#!/usr/bin/env python3
"""
Palette persistent session -- "persistent mind, disposable machinery".

The rest of this codebase (launch_worker.py, broker.py) launches one
`julia -e script` process per call: real state persistence only within
that one process's lifetime. This module keeps ONE sandboxed worker alive
across many turns instead, running runtime/scripts/session_loop.jl, while reusing
every existing authority primitive unchanged: the same bwrap argv builder,
the same broker/ceiling model, the same depot-clone mechanism (now
session-lifetime, via create_session_depot, instead of one-shot).

Session identity is explicit, not assumed. `PaletteSession.epoch` is read
from the worker process's own first line of output (a fresh UUID it
generates itself on startup -- see session_loop.jl) -- never guessed,
never carried over from a previous process. If the worker process dies,
its epoch dies with it: `PaletteSession` does not silently relaunch or
reconnect. A caller holding a dead session's handle gets `SessionDeadError`
and must explicitly choose a new session or revival. Each session owns one
sandboxed worker with newline-delimited JSON over its own stdio.


The authority model is completely unchanged by persistence: the broker
still only sees Class-C requests over its own socket, exactly as for a
one-shot worker. Nothing about keeping the worker warm grants it anything
it didn't already have.
"""
from __future__ import annotations

import json
import os
import selectors
import shutil
import subprocess
import tempfile
import threading
import uuid
from pathlib import Path

from launch_worker import resolve_real_julia_binary, build_bwrap_argv, create_session_depot, package_store_paths
from filesystem_layout import require_disjoint

import broker as _broker

REPO_DIR = str(Path(__file__).resolve().parents[2])
SESSION_LOOP_SCRIPT = str(Path(REPO_DIR) / "runtime/scripts" / "session_loop.jl")


class SessionDeadError(Exception):
    """The worker process this session was talking to is no longer alive.
    Never raised speculatively -- only after direct evidence (a closed
    pipe, a dead poll() result, or an explicit close())."""


# How much longer than turn_timeout the host waits for a reply: the worker's
# own interrupt grace period plus slack. Only a turn that never yields gets
# here.
HARD_TIMEOUT_MARGIN_S = 12.0


class SessionTimeoutError(SessionDeadError):
    """A turn exceeded turn_timeout waiting for a response. Deliberately a
    SessionDeadError subclass, not a separate, recoverable condition: a
    timed-out worker is killed (see turn()) rather than left half-blocked
    on a response this call gave up reading, because the protocol here is
    strictly one-request-one-response with no request-id-based recovery --
    trying to "keep going" after a timeout would mean either leaving that
    stale response for the NEXT turn() call to misread as ITS OWN
    response, or building a request-id dispatch loop this module doesn't
    have. Killing and failing closed is simpler and correct."""


class SessionProtocolError(Exception):
    pass


def _readline_with_timeout(pipe, timeout: float) -> str:
    """`pipe.readline()` blocks forever on a hung/slow worker with no way
    to bound it -- confirmed: `turn_timeout` was stored on `PaletteSession`
    but never actually used anywhere, so a turn running `while true; end`
    (or anything just slow) hung the calling thread indefinitely, with no
    way for `close()` to interrupt it either (the call sits inside the
    session lock `turn()` already holds). `selectors` gives a real,
    enforced deadline on the underlying file descriptor before ever
    calling the blocking `readline()`."""
    sel = selectors.DefaultSelector()
    sel.register(pipe, selectors.EVENT_READ)
    try:
        if not sel.select(timeout=timeout):
            raise TimeoutError(f"no response within {timeout}s")
        return pipe.readline()
    finally:
        sel.close()


class PaletteSession:
    """One persistent, sandboxed Palette worker plus its own broker,
    ceiling, and depot clone -- a real Julia process kept alive across
    many `turn()` calls, not a fresh one per call."""

    def __init__(
        self,
        *,
        project_dir: str,
        ceiling: dict,
        repo_dir: str = REPO_DIR,
        workspace_dir: str | None = None,
        network_enabled: bool = False,
        receipts_dir: str | None = None,
        turn_timeout: float = 60.0,
        task_workspace_dir: str | None = None,
        startup_timeout: float = 180.0,
        state_dir: str | None = None,
    ):
        """`workspace_dir` is this session's own scratch root: created here
        and deleted on close. `task_workspace_dir` is a directory the caller
        owns (an agent's task checkout): bound writable as the worker's
        working directory and never deleted. Without it the worker works in
        a private scratch workspace under the scratch root."""
        self.session_id = f"palette-{uuid.uuid4().hex[:16]}"
        self.project_dir = project_dir
        self.repo_dir = repo_dir
        self.turn_timeout = turn_timeout
        self._stderr_tail = ""
        self._lock = threading.Lock()
        self._closed = False
        self._next_request_id = 0

        # Everything from here through the HELLO handshake is wrapped in one
        # try/except: confirmed by direct testing that a failure ANYWHERE
        # in this window (e.g. the broker's socket bind failing right after
        # a real, several-GB depot clone was already made) leaked the
        # depot clone, the broker's thread and socket, and the workspace
        # directories permanently, since nothing called cleanup before this
        # fix. _teardown() is hasattr-guarded specifically so it's safe to
        # call here no matter how early the failure happened.
        try:
            scratch = Path(workspace_dir or Path.home() / ".palette-sessions" / self.session_id).resolve()
            planned_receipts_dir = receipts_dir or str(scratch / "broker")
            package_root, package_depot, package_environment = package_store_paths(
                ceiling=ceiling,
                protected_paths=[
                    scratch, project_dir, repo_dir, state_dir or "",
                    task_workspace_dir or "", planned_receipts_dir,
                ],
            )
            require_disjoint([scratch], [package_root])
            if state_dir:
                require_disjoint([state_dir], [scratch])
            if task_workspace_dir is not None:
                task_workspace = Path(task_workspace_dir).resolve(strict=True)
                if not task_workspace.is_dir():
                    raise ValueError(f"task workspace is not a directory: {task_workspace}")
                require_disjoint([task_workspace], [scratch])
                self.workspace_dir = str(task_workspace)
            self._tmp_root = scratch
            self._tmp_root.mkdir(parents=True, exist_ok=True)
            if task_workspace_dir is None:
                self.workspace_dir = str(self._tmp_root / "workspace")
                Path(self.workspace_dir).mkdir(parents=True, exist_ok=True)

            self.depot_clone_dir = create_session_depot()
            self.package_store_root = package_root

            # A Unix socket path is limited to 108 bytes. Under a long HOME the
            # session root pushed it past that and no kernel could start
            # ("AF_UNIX path too long"); the socket then gets a short private
            # directory of its own. Receipts stay in the session root.
            broker_root = self._tmp_root / "broker"
            broker_root.mkdir(parents=True, exist_ok=True)
            if len(str(broker_root / "broker.sock").encode()) > 100:
                self._short_sock_dir = tempfile.mkdtemp(prefix="nj-")
                self._broker_sock_dir = self._short_sock_dir
            else:
                self._broker_sock_dir = str(broker_root)
            receipts_path = str(Path(receipts_dir or broker_root) / "receipts.jsonl")
            require_disjoint([self.workspace_dir] + ([state_dir] if state_dir else []),
                [broker_root, Path(receipts_path).parent, self.depot_clone_dir, self.package_store_root])
            self._broker_server, self._broker = _broker.serve(
                str(Path(self._broker_sock_dir) / "broker.sock"),
                ceiling, receipts_path, self.session_id,
                depot_dir=self.depot_clone_dir,
                package_depot_dir=package_depot,
                package_environment_dir=package_environment,
                project_dir=self.project_dir,
                repo_dir=self.repo_dir,
                # Leaves the child's own shutdown and the reply time to land
                # inside the turn.
                child_timeout=max(5.0, turn_timeout - 10.0),
            )

            julia_bin = resolve_real_julia_binary()
            argv = build_bwrap_argv(
                workspace_dir=self.workspace_dir,
                broker_socket_dir=self._broker_sock_dir,
                project_dir=self.project_dir,
                repo_dir=self.repo_dir,
                julia_bin=julia_bin,
                julia_depot=os.environ.get("JULIA_DEPOT_PATH", str(Path.home() / ".julia")).split(":")[-1],
                depot_clone_dir=self.depot_clone_dir,
                package_store_root=package_root,
                package_depot_dir=package_depot,
                package_environment_dir=package_environment,
                network_enabled=network_enabled,
                state_dir=state_dir,
            )
            # Ephemeral-turn provenance goes to the sandbox's private home; the
            # working directory may be the caller's task workspace.
            argv += ["--setenv", "PALETTE_PROVENANCE_DIR", "/run/palette/home/.palette"]
            # kernelinfo() reports the limit a call runs under.
            argv += ["--setenv", "PALETTE_TURN_TIMEOUT", f"{turn_timeout:g}"]
            # The sandbox starts with a clear environment; the limits on saved
            # state pass through when the host sets them (src/revival.jl).
            for name in ("PALETTE_STATE_MAX_BINDING_BYTES", "PALETTE_STATE_MAX_BYTES"):
                if name in os.environ:
                    argv += ["--setenv", name, os.environ[name]]
            argv += ["--", julia_bin, "--startup-file=no", str(Path(self.repo_dir) / "runtime/scripts/session_loop.jl")]
            self._proc = subprocess.Popen(
                argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                # Turn output is arbitrary bytes; one invalid byte used to
                # raise UnicodeDecodeError here and take the bridge down.
                text=True, errors="replace", bufsize=1,
            )
            # Nothing else reads the worker's stderr; left undrained, 64 KiB
            # of warnings fills the pipe and blocks the worker mid-turn.
            self._stderr_thread = threading.Thread(target=self._drain_stderr, daemon=True)
            self._stderr_thread.start()

            try:
                hello_line = _readline_with_timeout(self._proc.stdout, startup_timeout)
            except TimeoutError:
                self._proc.kill()
                self._proc.wait()
                raise SessionDeadError(f"worker did not produce HELLO within {startup_timeout}s")
            if not hello_line:
                self._proc.wait()
                self._stderr_thread.join(timeout=5)
                raise SessionDeadError(f"worker exited before HELLO: {self._stderr_tail[-2000:]}")
            hello = json.loads(hello_line)
            if hello.get("kind") != "HELLO" or not isinstance(hello.get("epoch"), str):
                raise SessionProtocolError(f"malformed HELLO: {hello_line!r}")
            self.epoch = hello["epoch"]
            self.capabilities = hello.get("capabilities", {})
        except BaseException:
            self._teardown()
            raise

    def _drain_stderr(self):
        try:
            for chunk in iter(lambda: self._proc.stderr.read(4096), ""):
                self._stderr_tail = (self._stderr_tail + chunk)[-8000:]
        except (OSError, ValueError):
            pass

    def is_alive(self) -> bool:
        return not self._closed and self._proc.poll() is None

    def turn(self, code: str, *, ephemeral: bool = False, ephemeral_ceiling: dict | None = None,
             payload: str | dict[str, str] | None = None, include_map: bool = False,
             digest: bool = True) -> dict:
        """Send one script to the persistent worker and block for its one
        response. Raises SessionDeadError if the process is gone -- never
        silently relaunches; a new process would mean a new epoch, i.e. a
        different session, and this call has no authority to decide that
        on the caller's behalf.

        `ephemeral_ceiling` (ignored unless `ephemeral=True`) is the
        capability ceiling the disposable child worker gets -- validated
        as a real subset of this session's own ceiling by the broker
        (`ceiling_is_subset`), same as any other `spawn_child_worker`
        request. Defaults to `{}`: full, unbounded Julia language power
        inside the child, zero broker-mediated authority, the safer
        default for code whose actual needs aren't known ahead of time.

        `payload` is bound as `PAYLOAD` for this turn: text, such as a file's
        contents, that the turn uses without it passing through Julia's
        string syntax, or named texts (`{"old": ..., "new": ...}`) that the
        turn reads as `PAYLOAD["old"]`.

        The worker enforces `turn_timeout` itself: it interrupts a turn that
        is waiting (sleep, a subprocess, I/O) and keeps the kernel. This call
        waits `HARD_TIMEOUT_MARGIN_S` longer before killing the worker, which
        only compute that never yields should reach.
        """
        with self._lock:
            if self._closed:
                raise SessionDeadError(f"session {self.session_id} was explicitly closed")
            if self._proc.poll() is not None:
                raise SessionDeadError(
                    f"session {self.session_id} worker process exited (code {self._proc.returncode}) "
                    f"since epoch {self.epoch} was issued"
                )
            self._next_request_id += 1
            request_id = str(self._next_request_id)
            req = {"request_id": request_id, "kind": "EPHEMERAL" if ephemeral else "EXECUTE", "code": code,
                   "timeout_s": self.turn_timeout}
            if ephemeral and ephemeral_ceiling is not None:
                req["ceiling"] = ephemeral_ceiling
            if payload is not None:
                req["payload"] = payload
            if include_map:
                req["map"] = True
            if not digest:
                req["digest"] = False
            try:
                self._proc.stdin.write(json.dumps(req) + "\n")
                self._proc.stdin.flush()
                line = _readline_with_timeout(self._proc.stdout, self.turn_timeout + HARD_TIMEOUT_MARGIN_S)
            except TimeoutError:
                # TimeoutError IS-A OSError in Python's builtin hierarchy --
                # this branch must come before the (BrokenPipeError, OSError)
                # one below, or it silently never fires (confirmed: it
                # didn't, on the first version of this fix). Killed, not
                # left pending: this protocol is strictly one
                # request/one response, so a timed-out turn's eventual
                # response (if the worker ever produces one) would
                # otherwise sit in the pipe and be misread as the NEXT
                # turn's response. Killing the process is what makes this
                # session's failure mode simple and correct instead of
                # silently corrupting the next call.
                self._proc.kill()
                self._proc.wait()
                raise SessionTimeoutError(
                    f"The call exceeded its {self.turn_timeout:g}s limit and did not respond to the interrupt: "
                    f"compute that never yields cannot be interrupted, so the kernel was stopped. "
                    f"Every binding is gone; files written to the workspace remain."
                )
            except (BrokenPipeError, OSError) as e:
                raise SessionDeadError(f"session {self.session_id} pipe broke: {e}") from e
            if not line:
                self._proc.wait()
                self._stderr_thread.join(timeout=5)
                tail = self._stderr_tail[-2000:].strip()
                raise SessionDeadError(
                    f"session {self.session_id} kernel process exited during the turn "
                    f"(exit code {self._proc.returncode})" + (f": {tail}" if tail else "")
                )
            try:
                resp = json.loads(line)
            except json.JSONDecodeError as e:
                # The channel is out of sync; nothing after this line can be
                # trusted to answer this request.
                self._proc.kill()
                self._proc.wait()
                raise SessionDeadError(f"session {self.session_id} sent a non-JSON protocol line: {line[:200]!r}") from e
            if resp.get("epoch") != self.epoch:
                # Not expected to be reachable given one dedicated process
                # per session and no respawning. Still verify the epoch so
                # a response from another process cannot satisfy this turn.
                raise SessionDeadError(
                    f"epoch mismatch: session issued for {self.epoch!r}, response carries {resp.get('epoch')!r}"
                )
            if resp.get("request_id") != request_id:
                raise SessionProtocolError(f"response request_id {resp.get('request_id')!r} != sent {request_id!r}")
            if resp.get("kernel_exit"):
                # The worker answered and then exited on purpose; waiting here
                # means the next turn cannot race that exit.
                try:
                    self._proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self._proc.kill()
                    self._proc.wait()
                raise SessionDeadError(resp.get("error") or "the kernel stopped")
            return resp

    def _teardown(self):
        # Defensive against EVERY attribute below, not just self._proc: this
        # is also called from __init__'s own except clause (see below) if
        # construction fails partway through -- confirmed by direct testing
        # that a failure between create_session_depot() and the worker's
        # subprocess.Popen (e.g. the broker's socket bind failing) used to
        # leak the depot clone, the broker's thread and socket, and the
        # workspace directories permanently, since nothing called cleanup
        # at all before this fix, and _teardown() itself would have raised
        # AttributeError if called before self._proc existed.
        if hasattr(self, "_proc"):
            try:
                if self._proc.poll() is None:
                    try:
                        self._proc.stdin.close()
                    except Exception:
                        pass
                    # EOF lets the kernel finish saving the last call's state
                    # (src/revival.jl), which for a new data type means
                    # compiling its serializer: several seconds, once.
                    try:
                        self._proc.wait(timeout=30)
                    except subprocess.TimeoutExpired:
                        self._proc.kill()
                        self._proc.wait()
            finally:
                # subprocess.Popen does not close its pipe file objects on
                # its own just because the process exited -- confirmed
                # leaking real file descriptors (ResourceWarning on every
                # session in the test suite) until explicitly closed here.
                for pipe in (self._proc.stdin, self._proc.stdout, self._proc.stderr):
                    try:
                        pipe.close()
                    except Exception:
                        pass
        if hasattr(self, "_broker_server"):
            try:
                self._broker_server.shutdown()
                self._broker_server.server_close()
            except Exception:
                pass
        if hasattr(self, "depot_clone_dir"):
            shutil.rmtree(self.depot_clone_dir, ignore_errors=True)
        if hasattr(self, "_tmp_root"):
            shutil.rmtree(self._tmp_root, ignore_errors=True)
        if hasattr(self, "_short_sock_dir"):
            shutil.rmtree(self._short_sock_dir, ignore_errors=True)

    def close(self):
        with self._lock:
            if self._closed:
                return
            self._closed = True
            self._teardown()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
