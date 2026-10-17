#!/usr/bin/env python3
# Scheduled for removal: the Python host is kept for one more release, only as
# the conformance oracle. See "Python host: scheduled for removal" in
# runtime/docs/authority.md.
"""
Palette session bridge -- a thin, persistent stdio wrapper around
`PaletteSession`, for a host process in a DIFFERENT language (Node, via
Prime-Agent's `baseToolsFactory`) to spawn ONCE per agent session and drive
for that session's whole lifetime, instead of shelling out fresh per call.

This is the missing piece between what Palette already proves
(runtime/security/session.py's PaletteSession: real state persistence, real epoch
handshake, real authority fence, all adversarially tested) and an actual
chassis integration: Prime-Agent's tool boundary is TypeScript calling a
subprocess, not a Python API. PaletteSession itself does not change here --
this only exposes it over one more stdio hop, mirroring (structurally, not
literally) the same "newline-delimited JSON, one line per turn" shape
runtime/scripts/session_loop.jl already uses one layer down, for the same reason:
simple to spawn, simple to pipe, no new transport to build.

Protocol (all newline-delimited JSON):

  Request:   {"request_id": "...", "code": "...", "ephemeral": bool?, "ceiling": {...}?, "payload": str | {name: str}?}
  Response:  {"request_id": "...", "success": bool, "data": ..., "display": str?, "output": str?,
              "error": ..., "epoch": "...", "interrupted": bool, "call": int?, "bindings": [str]?, "session_dead": true?}
             `call` numbers a persistent turn; Palette.output(call) returns everything it printed.
  HELLO:     {"kind": "HELLO", "epoch": "...", "session_id": "..."}  -- printed once, at startup

Mid-call host requests (kernel -> host, while a call runs):
  Event:     {"event": "host_request", "id": "...", "data": {"type": "...", ...payload}}
  Reply:     {"host_reply": "<id>", "reply": {"status": "ok", "result": ...} | {"status": "error", "error": "..."}}
  The kernel reaches this only through its broker, and only for request types
  its ceiling names (category `host_request`); see HostBridge below. A reply
  may arrive while a call is still running, so stdin is read by its own
  thread. A reply for an unknown or finished request is dropped; stdin closing
  fails every waiting request.

`ephemeral`/`ceiling` map directly onto `PaletteSession.turn()`'s own
`ephemeral`/`ephemeral_ceiling` parameters -- see runtime/security/session.py for
what they mean and why the default ceiling is `{}` (full language power,
zero broker-mediated authority) rather than inherited automatically.

A malformed request line is answered with an error response (not a crash);
this process exits only when stdin closes (the host process closing its
end, e.g. on agent session dispose) or the underlying PaletteSession dies
(SessionDeadError from a turn is reported as an error response, not
silently swallowed -- the caller decides whether that's fatal to ITS
session too).
"""
from __future__ import annotations

import argparse
import json
import queue
import signal
import sys
import threading
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from session import PaletteSession, SessionDeadError  # noqa: E402


def _payload(value):
    """Text, or an object of named texts; anything else is dropped."""
    if isinstance(value, str):
        return value
    if isinstance(value, dict) and all(isinstance(k, str) and isinstance(v, str) for k, v in value.items()):
        return value
    return None


_out_lock = threading.Lock()


def _respond(resp: dict) -> None:
    # Responses (main thread) and host-request events (broker threads) share
    # stdout; one lock keeps every line whole.
    with _out_lock:
        print(json.dumps(resp), flush=True)


class HostBridge:
    """The host half of mid-call requests: send a request event, wait for the
    matching reply. Called from broker threads while the main thread is inside
    a call."""

    def __init__(self, timeout: float):
        self.timeout = timeout
        self._pending: dict[str, list] = {}
        self._lock = threading.Lock()
        self._closed = False

    def __call__(self, rtype: str, payload: dict) -> dict:
        rid = uuid.uuid4().hex
        done = threading.Event()
        slot = [done, None]
        with self._lock:
            if self._closed:
                return {"status": "error", "error": "the agent host has closed its connection"}
            self._pending[rid] = slot
        try:
            # The type goes last, so a payload key named "type" cannot reroute the request.
            _respond({"event": "host_request", "id": rid, "data": {**payload, "type": rtype}})
            if not done.wait(self.timeout):
                return {"status": "error", "error": f"the agent host did not answer {rtype!r} within {self.timeout:g}s"}
            return slot[1]
        finally:
            with self._lock:
                self._pending.pop(rid, None)

    def resolve(self, rid, reply) -> None:
        with self._lock:
            slot = self._pending.get(rid)
        if slot is None:
            return  # late, unknown or already timed out
        if not isinstance(reply, dict) or reply.get("status") not in ("ok", "error"):
            reply = {"status": "error", "error": "the agent host sent a malformed reply"}
        slot[1] = reply
        slot[0].set()

    def close(self) -> None:
        with self._lock:
            self._closed = True
            slots = list(self._pending.values())
        for slot in slots:
            slot[1] = {"status": "error", "error": "the agent host has closed its connection"}
            slot[0].set()


def _read_stdin(bridge: HostBridge, requests: queue.Queue) -> None:
    """Route each stdin line: host replies to the bridge, everything else to
    the main loop. None marks the end of input."""
    try:
        while True:
            # NOT `for line in sys.stdin:` -- iterating stdin block-buffers on a
            # pipe instead of yielding line by line, which hangs a live
            # request/response protocol. readline() reads exactly one line.
            line = sys.stdin.readline()
            if not line:
                break
            stripped = line.strip()
            if not stripped:
                continue
            try:
                msg = json.loads(stripped)
            except json.JSONDecodeError as e:
                requests.put(e)
                continue
            if isinstance(msg, dict) and "host_reply" in msg:
                bridge.resolve(msg.get("host_reply"), msg.get("reply"))
            else:
                requests.put(msg)
    finally:
        bridge.close()
        requests.put(None)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--project-dir", required=True)
    ap.add_argument("--repo-dir")
    ap.add_argument("--ceiling", default="{}", help="JSON object, or a path to a JSON file")
    ap.add_argument("--network", action="store_true")
    ap.add_argument("--turn-timeout", type=float, default=60.0)
    ap.add_argument("--startup-timeout", type=float, default=180.0)
    ap.add_argument("--workspace-dir", help="Task directory the worker works in; bound writable, never deleted")
    ap.add_argument("--scratch-root", help="Session scratch root; created at startup and removed on close")
    ap.add_argument("--receipts-dir", help="Private host broker receipts directory, outside writable worker mounts")
    ap.add_argument("--state-dir", help="Where the kernel saves its state after each completed call, and where a "
                    "new kernel revives it from; created if missing, never deleted")
    args = ap.parse_args()

    # Default SIGTERM handling exits without running `finally`, which leaked
    # the session's depot clone every time a host killed this bridge.
    def _terminate(signum, _frame):
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, _terminate)
    signal.signal(signal.SIGHUP, _terminate)

    ceiling_arg = args.ceiling
    ceiling_file = str(Path(ceiling_arg).resolve()) if Path(ceiling_arg).is_file() else None
    if ceiling_file:
        ceiling = json.loads(Path(ceiling_file).read_text())
    else:
        ceiling = json.loads(ceiling_arg)

    kwargs = {"project_dir": args.project_dir, "ceiling": ceiling, "ceiling_file": ceiling_file,
              "network_enabled": args.network, "turn_timeout": args.turn_timeout,
              "startup_timeout": args.startup_timeout, "task_workspace_dir": args.workspace_dir,
              "workspace_dir": args.scratch_root, "receipts_dir": args.receipts_dir}
    if args.repo_dir:
        kwargs["repo_dir"] = args.repo_dir

    try:
        revival = False
        if args.state_dir:
            Path(args.state_dir).mkdir(parents=True, exist_ok=True)
            kwargs["state_dir"] = str(Path(args.state_dir).resolve())
            manifest = Path(args.state_dir) / "manifest.json"
            # The kernel revives only a state saved in this same workspace, and
            # reports an unreadable one itself.
            workspace = str(Path(args.workspace_dir).resolve()) if args.workspace_dir else None
            try:
                saved = json.loads(manifest.read_text()) if manifest.is_file() else {}
            except (OSError, ValueError):
                saved = {"workspace": workspace}
            revival = manifest.is_file() and workspace is not None and saved.get("workspace") == workspace
        session = PaletteSession(**kwargs)
    except Exception as e:
        _respond({"kind": "ERROR", "error": f"failed to start PaletteSession: {e}"})
        return 1

    # A host request must be answered (or given up on) before the call itself
    # times out, so the kernel gets an error rather than being killed.
    bridge = HostBridge(timeout=max(1.0, args.turn_timeout - 5.0))
    session._broker.host_bridge = bridge
    requests: queue.Queue = queue.Queue()
    threading.Thread(target=_read_stdin, args=(bridge, requests), daemon=True).start()

    _respond({"kind": "HELLO", "epoch": session.epoch, "session_id": session.session_id,
              "revival": revival, "capabilities": session.capabilities})

    try:
        while True:
            req = requests.get()
            if req is None:
                break  # real EOF: the host process closed its end
            if isinstance(req, json.JSONDecodeError):
                _respond({"request_id": None, "success": False, "data": None, "error": f"malformed request: {req}"})
                continue
            if not isinstance(req, dict):
                _respond({"request_id": None, "success": False, "data": None, "error": "request must be a JSON object"})
                continue

            request_id = req.get("request_id")
            code = req.get("code")
            if not isinstance(code, str):
                _respond({"request_id": request_id, "success": False, "data": None,
                           "error": "request 'code' must be a string"})
                continue

            try:
                result = session.turn(
                    code,
                    ephemeral=bool(req.get("ephemeral", False)),
                    ephemeral_ceiling=req.get("ceiling"),
                    payload=_payload(req.get("payload")),
                    include_map=req.get("map") is True,
                    digest=req.get("digest") is not False,
                )
                _respond({
                    "request_id": request_id,
                    "success": result.get("success", False),
                    "data": result.get("data"),
                    "display": result.get("display"),
                    "output": result.get("output"),
                    "error": result.get("error"),
                    "epoch": result.get("epoch"),
                    "interrupted": bool(result.get("interrupted", False)),
                    "call": result.get("call"),
                    "bindings": result.get("bindings"),
                    "revival": result.get("revival"),
                    "reloads": result.get("reloads"),
                    "costs": result.get("costs"),
                    "transport_shortened": result.get("transport_shortened"),
                })
            except SessionDeadError as e:
                # `session_dead` lets the host stop sending turns now, rather
                # than racing this process's exit.
                _respond({"request_id": request_id, "success": False, "data": None, "error": str(e),
                          "session_dead": True})
                # The underlying worker is gone; every subsequent turn would
                # fail the same way. Nothing left to serve.
                break
    finally:
        session.close()

    return 0


if __name__ == "__main__":
    sys.exit(main())
