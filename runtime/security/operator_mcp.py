#!/usr/bin/env python3
"""Palette-only MCP consumer of the existing Rust session and preparation paths."""
import hashlib
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import sys
import threading
import time
import uuid

from prewarm_depot import prepare, preparation_identity, preparation_status, resolve_real_julia_binary

REPO = os.environ.get("PALETTE_REPO", str(Path(__file__).absolute().parents[2]))
HOST = os.environ.get("PALETTE_HOST", str(Path(REPO, "runtime/host/target/release/palette-host")))
WORKSPACE = str(Path(os.environ["OPERATOR_WORKSPACE"]).absolute())
STATE_DIR = Path(os.environ.get("PALETTE_STATE_DIR", str(Path.home() / ".local/share/operator-surfaces/palette-state" /
    hashlib.sha256(WORKSPACE.encode()).hexdigest()))).absolute()
proc = None
frames = queue.Queue()
hello = None
session_notice = None
preparation = None
binding_view = {}
binding_epoch = None
connection_failure = None



def operator_ceiling():
    """The operator session's capability ceiling. PALETTE_HOST_COMMANDS names
    a JSON file of host commands ({"<name>": {"argv": [...], "cwd": "/abs",
    "timeout_s": 600, "extra_args": false}}) the kernel may run outside the
    sandbox through the broker; without it the ceiling is empty."""
    path = os.environ.get("PALETTE_HOST_COMMANDS")
    if not path:
        return {}
    commands = json.loads(Path(path).read_text())
    if not isinstance(commands, dict):
        raise RuntimeError(f"PALETTE_HOST_COMMANDS must name a JSON object of commands: {path}")
    return {"host_command": {"commands": commands}}

def project_result(event, *, full=False, ephemeral=False):
    global binding_view, binding_epoch
    result = dict(event)
    rows = event.get("bindings")
    if isinstance(rows, list) and not ephemeral:
        current = {row.split(" (", 1)[0]: row for row in rows}
        previous = binding_view if binding_epoch == event.get("epoch") else {}
        changed = [row for name, row in current.items() if previous.get(name) != row]
        absent = [name for name in previous if name not in current]
        if not full:
            result.pop("bindings", None)
            if changed or absent:
                result["binding_changes"] = {"new_or_changed": changed, "no_longer_listed": absent}
            if len(rows) >= 200:
                result["binding_observation"] = "Inventory lists at most 200 bindings; no longer listed does not prove deletion. Inspect varinfo() for the full world."
        binding_view, binding_epoch = current, event.get("epoch")
    if full:
        return result
    if result.get("data") is not None:
        result.pop("display", None)
    if "costs" in result and result.get("success") is not False:
        result.pop("costs", None)
    for key in ("display", "error", "output", "revival", "reloads", "retired_snapshot", "call", "costs", "transport_shortened"):
        if result.get(key) is None or result.get(key) == "":
            result.pop(key, None)
    if result.get("interrupted") is False:
        result.pop("interrupted", None)
    result.pop("request_id", None)
    return result


def tool_failure(exc):
    """All tool responses retain a JSON payload for routed and direct clients."""
    event = {"success": False,
             "error": f"{type(exc).__name__}: {exc}. Check connection status and restoration evidence; work may not have completed."}
    if connection_failure is not None:
        event["connection_failure"] = connection_failure
    return {"content": [{"type": "text", "text": json.dumps(event, ensure_ascii=False)}],
            "isError": True}


def send_child(msg):
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()


def pump(child, destination):
    try:
        for line in child.stdout:
            try:
                frame = json.loads(line)
            except json.JSONDecodeError as exc:
                raise RuntimeError(f"Invalid operator JSON frame ({len(line)} characters): {line[:256]!r}") from exc
            destination.put(frame)
    except Exception as exc:
        destination.put(exc)
    finally:
        destination.put(EOFError("operator exited"))


def receive(deadline):
    item = frames.get(timeout=max(0.001, deadline - time.monotonic()))
    if isinstance(item, Exception):
        raise item
    return item


def stop():
    global proc
    if proc is None:
        return
    child, proc = proc, None
    child.stdin.close()
    try:
        # The Rust owner gives its last snapshot up to 30s before killing it.
        child.wait(timeout=35)
    except subprocess.TimeoutExpired:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
    finally:
        child.stdout.close()


def prepare_session():
    global preparation
    print(json.dumps({"phase": "preparation", "state": "preparing"}), file=sys.stderr, flush=True)
    started = time.monotonic()
    record = prepare(REPO, REPO, host_bin=HOST)
    preparation = {k: record[k] for k in ("state", "reused", "previous_state") if k in record}
    preparation["seconds_this_start"] = time.monotonic() - started
    preparation["compile_prepare_seconds"] = 0.0 if record.get("reused") else record.get("seconds")
    preparation["recorded_preparation_seconds"] = record.get("seconds")
    preparation["identity"] = hashlib.sha256(json.dumps(record["identity"], sort_keys=True).encode()).hexdigest()
    if record["state"] != "prepared":
        raise RuntimeError("preparation failed: " + str(record.get("error", record.get("output", "")))[:2000])
    return preparation


def start(*, prepared=True):
    global proc, frames, hello, session_notice, preparation
    if proc is not None:
        if proc.poll() is not None:
            stop()
            raise RuntimeError("operator died; restoration has not been established")
        return
    if prepared:
        prepare_session()
    else:
        identity = preparation_identity(REPO, REPO, resolve_real_julia_binary())
        preparation = {"state": preparation_status(identity)["state"], "automatic_preparation": False}
    frames = queue.Queue()
    Path(WORKSPACE).mkdir(parents=True, exist_ok=True)
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    command = [HOST, "session", "--project-dir", REPO, "--repo-dir", REPO,
               "--workspace-dir", WORKSPACE, "--state-dir", str(STATE_DIR), "--ceiling", json.dumps(operator_ceiling())]
    if os.environ.get("PALETTE_SCRATCH_ROOT"):
        command += ["--scratch-root", os.environ["PALETTE_SCRATCH_ROOT"]]
    started = time.monotonic()
    proc = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=sys.stderr, text=True, errors="replace", bufsize=1,
                            cwd=WORKSPACE, env=dict(os.environ))
    threading.Thread(target=pump, args=(proc, frames), daemon=True).start()
    try:
        hello = receive(time.monotonic() + 180)
        if hello.get("kind") != "HELLO":
            raise RuntimeError("operator startup failed: " + json.dumps(hello))
    except Exception:
        stop()
        raise
    session_notice = {"epoch": hello["epoch"], "host_startup_seconds": time.monotonic() - started,
                      "preparation": preparation, "snapshot_present": hello.get("revival", False),
                      "restoration": "uncertain" if hello.get("revival") else "fresh_empty_world"}
    if connection_failure is not None:
        session_notice["previous_connection_failure"] = connection_failure
        session_notice["recovery_warning"] = "Restoration describes the saved snapshot. Effects after that snapshot are not restored; their precise contents are unknown."


def execute(args):
    global session_notice, connection_failure
    args = dict(args)
    view = args.pop("view", "quiet")
    if view not in ("quiet", "full"):
        raise ValueError("view must be quiet or full")
    rid = uuid.uuid4().hex
    try:
        event = receive_result(rid, args)
        if event.get("session_dead") is True and args.get("ephemeral") is not True:
            connection_failure = {"epoch": hello.get("epoch"), "error": event.get("error"), "call": event.get("call")}
            # The CLI exits after its terminal result. Retire its pipes before
            # the next independent call starts a session from the saved state.
            stop()
        return project_result(event, full=view == "full", ephemeral=args.get("ephemeral") is True)
    except Exception as exc:
        connection_failure = {"epoch": hello.get("epoch") if hello else None, "error": str(exc), "completion": "unknown"}
        stop()
        raise


def receive_result(rid, args):
    global session_notice, connection_failure
    send_child({"request_id": rid, **args})
    deadline = time.monotonic() + 90
    while True:
        event = receive(deadline)
        if event.get("event") == "host_request":
            send_child({"host_reply": event["id"], "reply": {"status": "error",
                "error": "This Palette operator has no Cyan harness host services configured."}})
            continue
        if event.get("request_id") == rid:
            if hello.get("capabilities", {}).get("revival_observation_v1") and isinstance(event.get("revival"), dict):
                if session_notice is not None:
                    session_notice["restoration"] = event["revival"]["state"]
            if session_notice is not None:
                event["session"] = session_notice
                session_notice = None
                connection_failure = None
            return event


def control(args):
    if any(key in args and not isinstance(args[key], bool) for key in ("restore", "prepare")):
        raise ValueError("restore and prepare must be booleans")
    action = args["action"]
    if action == "status":
        identity = preparation_identity(REPO, REPO, resolve_real_julia_binary())
        record = preparation_status(identity)
        return {"connected": proc is not None and proc.poll() is None, "epoch": hello.get("epoch") if hello else None,
                "state_dir": str(STATE_DIR), "preparation": {k: record[k] for k in ("state", "seconds", "error") if k in record}}
    if action == "prepare":
        return prepare_session()
    if action == "restart":
        previous_epoch = hello.get("epoch") if hello else None
        stop()
        retired = None
        if args.get("restore", True) is False and STATE_DIR.exists():
            retired = str(STATE_DIR.with_name(STATE_DIR.name + ".retired-" + uuid.uuid4().hex))
            os.replace(STATE_DIR, retired)
        start(prepared=args.get("prepare", True))
        result = execute({"code": "nothing"})
        result.update(previous_epoch=previous_epoch, retired_snapshot=retired)
        return result
    raise ValueError("unknown control action")


schema = {"type": "object", "properties": {"code": {"type": "string"}, "view": {"enum": ["quiet", "full"]}, "payload": {"oneOf": [
    {"type": "string"}, {"type": "object", "additionalProperties": {"type": "string"}}]},
    "ephemeral": {"type": "boolean"}}, "required": ["code"], "additionalProperties": False}
control_schema = {"type": "object", "properties": {"action": {"enum": ["status", "prepare", "restart"]},
    "restore": {"type": "boolean"}, "prepare": {"type": "boolean"}}, "required": ["action"], "additionalProperties": False}


def main():
    try:
        for raw in sys.stdin:
            mid = None
            try:
                msg = json.loads(raw)
                mid = msg.get("id")
                method = msg.get("method", "")
                if method.startswith("notifications/"):
                    continue
                if method == "initialize":
                    result = {"protocolVersion": msg.get("params", {}).get("protocolVersion", "2024-11-05"),
                              "capabilities": {"tools": {}}, "serverInfo": {"name": "palette", "version": "0.2.0"}}
                elif method == "ping":
                    result = {}
                elif method == "tools/list":
                    result = {"tools": [
                        {"name": "palette", "description": "Persistent Julia Palette through the Rust sandbox. Payload carries text verbatim. Scratch runs in a disposable process. Replies default to quiet; view=full retains response metadata. Inspect varinfo(), Api.provenance(:name), Api.changes(), Api.jobs(), and Api.costs() on demand. Offline preparation and existing state revival are wired; inspect restoration evidence after any epoch change.", "inputSchema": schema},
                        {"name": "palette_control", "description": "Inspect preparation, prepare offline, or restart. restore=false starts a clean world and retires the previous snapshot without deleting workspace files. prepare=false exercises the cold path.", "inputSchema": control_schema}]}
                elif method == "tools/call":
                    params = msg["params"]
                    args = params.get("arguments", {})
                    try:
                        if params["name"] == "palette_control":
                            event = control(args)
                        elif params["name"] == "palette" and isinstance(args.get("code"), str):
                            start()
                            event = execute(args)
                        else:
                            raise ValueError("expected a Palette tool and valid arguments")
                        result = {"content": [{"type": "text", "text": json.dumps(event, ensure_ascii=False)}],
                                  "isError": event.get("success") is False}
                    except Exception as exc:
                        result = tool_failure(exc)
                else:
                    print(json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Unknown method"}}), flush=True)
                    continue
                print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}), flush=True)
            except Exception as exc:
                print(json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": str(exc)}}), flush=True)
    finally:
        stop()


def terminate(signum, frame):
    raise SystemExit(128 + signum)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGHUP, terminate)
    main()
