"""
The host the tests run against: the Rust `palette-host` binary, from
PALETTE_HOST_BIN or else this checkout's release build. Every entry point the
tests use -- the session bridge, one-shot workers, the broker, the
ceiling-subset rule, a whole session, the kernel-free control actions -- goes
through it.

Test support only: nothing here runs in a session.
"""
from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
HOST_BIN = os.environ.get("PALETTE_HOST_BIN") or str(HERE.parent / "host/target/release/palette-host")

import launch_worker as _launch  # noqa: E402

create_session_depot = _launch.create_session_depot
remove_session_depot = _launch.remove_session_depot


def scratch_root() -> str:
    return str(Path(tempfile.gettempdir()) / f"palette-adapter-{uuid.uuid4().hex}")


def session_cmd(root: str | None = None) -> list[str]:
    """The command that starts the session bridge (its arguments follow)."""
    return [HOST_BIN, "session", "--scratch-root", root or scratch_root()]


def control(action, directory, **args):
    """palette_control's kernel-free actions (checkpoints, restore, receipts) via `palette-host control`."""
    out = subprocess.run([HOST_BIN, "control", action, str(directory), json.dumps(args)],
                         capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=60)
    if out.returncode != 0:
        message = out.stderr.strip().removeprefix("palette-host: ")
        raise (ValueError if message.startswith("ValueError: ") else RuntimeError)(message.removeprefix("ValueError: "))
    return json.loads(out.stdout)


def run_worker(*, workspace_dir, project_dir, repo_dir, script, broker_socket_dir=None, network_enabled=False,
               depot_clone_dir=None, timeout=60.0, **_ignored):
    argv = [HOST_BIN, "run-worker", "--workspace", workspace_dir, "--project", project_dir, "--repo", repo_dir,
            "--script", script, "--timeout", str(timeout)]
    if broker_socket_dir:
        argv += ["--broker-socket-dir", broker_socket_dir]
    if depot_clone_dir:
        argv += ["--depot-clone-dir", depot_clone_dir]
    if network_enabled:
        argv += ["--network"]
    out = subprocess.run(argv, capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=timeout + 120)
    if out.returncode != 0:
        raise RuntimeError(f"palette-host run-worker failed: {out.stderr}")
    r = json.loads(out.stdout)
    if r["timed_out"]:
        raise subprocess.TimeoutExpired(argv, timeout, r["stdout"], r["stderr"])
    return subprocess.CompletedProcess(argv, r["returncode"], r["stdout"], r["stderr"])


class _RustBrokerServer:
    def __init__(self, sock_path, ceiling, receipts, session_id, depot_dir=None, project_dir=None, repo_dir=None,
                 child_timeout=90.0):
        argv = [HOST_BIN, "broker", "--socket", sock_path, "--ceiling", json.dumps(ceiling), "--receipts", receipts,
                "--session-id", session_id, "--child-timeout", str(child_timeout)]
        for flag, val in (("--depot", depot_dir), ("--project", project_dir), ("--repo", repo_dir)):
            if val:
                argv += [flag, val]
        self.proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        try:
            ready = self.proc.stdout.readline()
            if json.loads(ready or "{}").get("kind") != "READY":
                raise RuntimeError(f"broker did not start: {ready!r}")
        except Exception:
            self.shutdown()
            raise

    def shutdown(self):
        try:
            if self.proc.poll() is None:
                self.proc.stdin.close()
                try:
                    self.proc.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait()
        finally:
            self.proc.stdin.close()
            self.proc.stdout.close()

    def server_close(self):
        self.shutdown()


class _BrokerNamespace:
    """The broker surface the tests drive."""

    @staticmethod
    def ceiling_is_subset(requested, parent):
        out = subprocess.run([HOST_BIN, "subset", json.dumps(requested), json.dumps(parent)], capture_output=True,
                             text=True, check=True, stdin=subprocess.DEVNULL)
        r = json.loads(out.stdout)
        return r["ok"], r["reason"]

    @staticmethod
    def serve(sock_path, ceiling, receipts, session_id, stop_event=None, depot_dir=None, project_dir=None,
              repo_dir=None, child_timeout=90.0):
        server = _RustBrokerServer(sock_path, ceiling, receipts, session_id, depot_dir, project_dir, repo_dir,
                                   child_timeout)
        if stop_event is None:
            return server, None
        try:
            stop_event.wait()
        finally:
            server.shutdown()
        return server, None

    class Broker:
        """A broker process the test's calls go to over its socket."""

        def __init__(self, ceiling, receipts, session_id, **_kw):
            self._dir = Path(receipts).parent
            self._sock = str(Path("/tmp") / f"njl-adapter-{uuid.uuid4().hex[:10]}.sock")
            self._server = _RustBrokerServer(self._sock, ceiling, receipts, session_id)

        def _call(self, category, params):
            with socket.socket(socket.AF_UNIX) as s:
                s.connect(self._sock)
                s.sendall((json.dumps({"id": str(uuid.uuid4()), "category": category, "params": params}) + "\n").encode())
                with s.makefile() as reply:
                    resp = json.loads(reply.readline())
            if not resp["approved"]:
                raise RuntimeError(resp["reason"])
            return resp["result"]

        def handle_request(self, req):
            """One raw request and its raw reply, denials included."""
            with socket.socket(socket.AF_UNIX) as s:
                s.connect(self._sock)
                s.sendall((json.dumps(req) + "\n").encode())
                with s.makefile() as reply:
                    return json.loads(reply.readline())

        def _handle_network_access(self, params):
            return self._call("network_access", params)

        def __del__(self):
            # A broker that failed to start has no server to stop.
            server = getattr(self, "_server", None)
            if server is not None:
                server.shutdown()


B = _BrokerNamespace()


class SessionDeadError(Exception):
    pass


class PaletteSession:
    """A whole session through the host's session bridge."""

    def __init__(self, *, project_dir, ceiling, workspace_dir=None, turn_timeout=60.0, task_workspace_dir=None, **_ignored):
        workspace_dir = workspace_dir or scratch_root()
        argv = session_cmd(workspace_dir) + ["--project-dir", project_dir, "--ceiling", json.dumps(ceiling),
                                "--turn-timeout", str(turn_timeout)]
        if task_workspace_dir is not None:
            argv += ["--workspace-dir", task_workspace_dir]
        self.proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     text=True, bufsize=1)
        hello = json.loads(self.proc.stdout.readline() or "{}")
        if hello.get("kind") != "HELLO":
            self.close()
            raise SessionDeadError(f"session did not start: {hello}")
        self.epoch, self.session_id = hello["epoch"], hello["session_id"]
        self._n = 0
        self._closed = False
        self.scratch_root = workspace_dir

    def turn(self, code, *, ephemeral=False, ephemeral_ceiling=None, payload=None, **_kw):
        if self._closed:
            raise SessionDeadError(f"session {self.session_id} was explicitly closed")
        if self.proc.poll() is not None:
            raise SessionDeadError("the session bridge exited")
        self._n += 1
        req = {"request_id": str(self._n), "code": code, "ephemeral": ephemeral}
        if ephemeral_ceiling is not None:
            req["ceiling"] = ephemeral_ceiling
        if payload is not None:
            req["payload"] = payload
        try:
            self.proc.stdin.write(json.dumps(req) + "\n")
            self.proc.stdin.flush()
        except (BrokenPipeError, OSError) as e:
            raise SessionDeadError(str(e)) from e
        line = self.proc.stdout.readline()
        if not line:
            raise SessionDeadError("the session bridge exited")
        resp = json.loads(line)
        if resp.get("session_dead"):
            raise SessionDeadError(resp.get("error"))
        return resp

    def worker_pids(self) -> list[int]:
        """The bridge's children: the sandbox's bwrap, whose kill ends the worker."""
        try:
            kids = Path(f"/proc/{self.proc.pid}/task/{self.proc.pid}/children").read_text().split()
        except OSError:
            return []
        return [int(k) for k in kids]

    def close(self):
        if getattr(self, "_closed", False):
            return
        self._closed = True
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        for pipe in (self.proc.stdout, self.proc.stderr):
            try:
                pipe.close()
            except Exception:
                pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

