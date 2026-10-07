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
from provisioning import operator_ceiling, protected_host_paths
from runtime_registry import RuntimeRegistry
from patch_broker import PatchBroker
from repair_broker import RepairBroker

REPO = os.environ.get("PALETTE_REPO", str(Path(__file__).absolute().parents[2]))
HOST = os.environ.get("PALETTE_HOST", str(Path(REPO, "runtime/host/target/release/palette-host")))
WORKSPACE = str(Path(os.environ["OPERATOR_WORKSPACE"]).absolute())
STATE_DIR = Path(os.environ.get("PALETTE_STATE_DIR", str(Path.home() / ".local/share/operator-surfaces/palette-state" /
    hashlib.sha256(WORKSPACE.encode()).hexdigest()))).absolute()
RECEIPTS_DIR = STATE_DIR.with_name(STATE_DIR.name + ".receipts")
proc = None
frames = queue.Queue()
hello = None
session_notice = None
preparation = None
binding_view = {}
binding_epoch = None
connection_failure = None
FROZEN_CEILING = operator_ceiling()
RUNTIME_ROOT = Path(os.environ.get("PALETTE_RUNTIME_ROOT", str(Path(REPO, "runtime/capabilities")))).expanduser().resolve()
runtime_registry = RuntimeRegistry(RUNTIME_ROOT, FROZEN_CEILING, workspace=WORKSPACE, state_dir=STATE_DIR,
                                   repo=REPO, host_binary=HOST)
refresh_pending = None
PATCH_STATE_ROOT = Path(os.environ.get("PALETTE_WORKSPACE_STATE_ROOT",
    str(Path.home() / ".local/share/operator-surfaces/palette-workspaces"))).expanduser().resolve()
PROTECTED_PATHS = [*protected_host_paths(FROZEN_CEILING, include_runtime_root=False), STATE_DIR, RECEIPTS_DIR, PATCH_STATE_ROOT]
try:
    PATCH_ROOTS = json.loads(os.environ.get("PALETTE_PATCH_ROOTS", "{}"))
    if not isinstance(PATCH_ROOTS, dict):
        raise ValueError("PALETTE_PATCH_ROOTS must contain a JSON object")
    repair_config_path = os.environ.get("PALETTE_REPAIR_CONFIG")
    REPAIR_CONFIG = json.loads(Path(repair_config_path).expanduser().read_text(encoding="utf-8")) if repair_config_path else {}
    if not isinstance(REPAIR_CONFIG, dict) or set(REPAIR_CONFIG) - {"test_recipes", "auto_apply_prefixes", "sandbox_source_paths"}:
        raise ValueError("PALETTE_REPAIR_CONFIG has unsupported fields")
    PATCH_BROKER = PatchBroker(PATCH_STATE_ROOT / "patches", PATCH_ROOTS,
                               REPAIR_CONFIG.get("auto_apply_prefixes", {}),
                               protected_paths=PROTECTED_PATHS,
                               registry_root=RUNTIME_ROOT,
                               sandbox_source_paths=REPAIR_CONFIG.get("sandbox_source_paths", {}))
    REPAIR_BROKER = RepairBroker(PATCH_BROKER, REPAIR_CONFIG.get("test_recipes", {}),
                                 REPAIR_CONFIG.get("auto_apply_prefixes", {}),
                                 protected_paths=PROTECTED_PATHS)
except (OSError, json.JSONDecodeError, TypeError, ValueError) as exc:
    raise RuntimeError(f"Invalid trusted repair configuration: {exc}") from exc



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
        return {"stopped": False, "returncode": None, "timed_out": False}
    child, proc = proc, None
    timed_out = False
    child.stdin.close()
    try:
        # The Rust owner gives its last snapshot up to 30s before killing it.
        child.wait(timeout=35)
    except subprocess.TimeoutExpired:
        timed_out = True
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
    finally:
        child.stdout.close()
    return {"stopped": True, "returncode": child.returncode, "timed_out": timed_out}


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
               "--workspace-dir", WORKSPACE, "--state-dir", str(STATE_DIR),
               "--receipts-dir", str(RECEIPTS_DIR), "--ceiling", json.dumps(FROZEN_CEILING)]
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
    global session_notice, connection_failure, refresh_pending
    send_child({"request_id": rid, **args})
    deadline = time.monotonic() + 90
    while True:
        event = receive(deadline)
        if event.get("event") == "host_request":
            data = event.get("data", {})
            if isinstance(data, dict) and data.get("type") == "palette.runtime":
                try:
                    action = data.get("action")
                    payload = data.get("payload", {})
                    if not isinstance(payload, dict):
                        raise ValueError("palette.runtime payload must be an object")
                    if action == "refresh" and args.get("ephemeral") is True:
                        raise PermissionError("runtime refresh is unavailable to disposable workers")
                    if action == "repair":
                        result = REPAIR_BROKER.call(payload, os.environ.get("PALETTE_WORKSPACE_ID", "default"),
                                                    None, None, None)
                    else:
                        result = runtime_registry.handle(action, payload, epoch=hello.get("epoch"))
                    if action == "refresh":
                        result["deferred"] = True
                        result["snapshot"] = "pending_current_turn_completion"
                        refresh_pending = {"request_id": event["id"], "requested": dict(result),
                                           "previous_epoch": hello.get("epoch"), "previous_generation": result["previous_generation"]}
                    send_child({"host_reply": event["id"], "reply": {"status": "ok", "result": result}})
                except Exception as exc:
                    send_child({"host_reply": event["id"], "reply": {"status": "error", "error": f"{type(exc).__name__}: {exc}"}})
            else:
                send_child({"host_reply": event["id"], "reply": {"status": "error",
                    "error": "This Palette operator has no host service for that request type."}})
            continue
        if event.get("request_id") == rid:
            if hello.get("capabilities", {}).get("revival_observation_v1") and isinstance(event.get("revival"), dict):
                if session_notice is not None:
                    session_notice["restoration"] = event["revival"]["state"]
            if session_notice is not None:
                event["session"] = session_notice
                session_notice = None
                connection_failure = None
            if refresh_pending is not None:
                event = complete_runtime_refresh(event)
            return event


def snapshot_evidence(call):
    manifest_path = STATE_DIR / "manifest.json"
    last_call_path = STATE_DIR / "last_call"
    result = {"manifest_present": manifest_path.is_file(), "manifest_call": None,
              "last_completed_call": None, "workspace_matches": False}
    if not manifest_path.is_file():
        return result
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        saved_call = manifest.get("call")
        result["manifest_call"] = saved_call
        result["workspace_matches"] = manifest.get("workspace") == str(Path(WORKSPACE).resolve())
        result["manifest_sha256"] = hashlib.sha256(manifest_path.read_bytes()).hexdigest()
        last = last_call_path.read_text(encoding="utf-8").strip() if last_call_path.is_file() else ""
        result["last_completed_call"] = int(last) if last.isdigit() else None
        result["covers_completed_turn"] = isinstance(saved_call, int) and saved_call >= call and (
            result["last_completed_call"] is None or result["last_completed_call"] >= call)
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        result["read_error"] = f"{type(exc).__name__}: {exc}"
        result["covers_completed_turn"] = False
    return result


def complete_runtime_refresh(turn_event):
    global refresh_pending, connection_failure
    pending, refresh_pending = refresh_pending, None
    before_epoch = pending["previous_epoch"]
    call = turn_event.get("call")
    stopped = stop()
    snapshot = snapshot_evidence(call) if isinstance(call, int) else {"covers_completed_turn": False, "manifest_present": False}
    try:
        snapshot_committed = (stopped.get("returncode") == 0 and not stopped.get("timed_out") and
                              snapshot.get("covers_completed_turn") is True and snapshot.get("workspace_matches") is True)
        commit = runtime_registry.commit_refresh() if snapshot_committed else None
        if commit is None:
            runtime_registry.cancel_refresh()
        start(prepared=False)
        revived = execute({"code": "nothing", "view": "full"})
        revival = revived.get("revival")
        after_epoch = hello.get("epoch") if hello else None
        saved_call = revival.get("saved_call") if isinstance(revival, dict) else None
        observed = isinstance(saved_call, int) and isinstance(call, int) and saved_call >= call
        success = (snapshot_committed and commit is not None and
                   commit["capability_generation"] == pending["requested"]["capability_generation"] and
                   after_epoch is not None and after_epoch != before_epoch and observed)
        refresh = {**pending["requested"], **(commit or {}), "deferred": False, "success": success,
                   "snapshot": {**snapshot, "stop_returncode": stopped.get("returncode"),
                                "stop_timed_out": stopped.get("timed_out")},
                   "previous_epoch": before_epoch, "epoch": after_epoch,
                   "revival": revival, "observed_saved_call": saved_call}
        turn_event["runtime_refresh"] = refresh
        if not success:
            turn_event["success"] = False
            turn_event["runtime_refresh"]["error"] = "snapshot commit or revival of the completed turn was not established"
            turn_event["error"] = (str(turn_event.get("error") or "") +
                                   " Runtime refresh did not establish snapshot and revival; inspect runtime_refresh evidence.").strip()
        return turn_event
    except Exception as exc:
        runtime_registry.cancel_refresh()
        connection_failure = {"epoch": before_epoch, "error": str(exc), "completion": "runtime_refresh_uncertain"}
        turn_event["success"] = False
        turn_event["runtime_refresh"] = {**pending["requested"], "deferred": False, "success": False,
            "snapshot": {**snapshot, "stop_returncode": stopped.get("returncode"), "stop_timed_out": stopped.get("timed_out")},
            "previous_epoch": before_epoch, "epoch": hello.get("epoch") if hello else None,
            "error": f"{type(exc).__name__}: {exc}"}
        turn_event["error"] = (str(turn_event.get("error") or "") +
                                " Runtime refresh recovery is uncertain; inspect runtime_refresh evidence.").strip()
        stop()
        return turn_event


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
