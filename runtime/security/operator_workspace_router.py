#!/usr/bin/env python3
"""Route independent Palette worlds and broker exact, explicitly approved patches."""
from __future__ import annotations

import copy
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from patch_broker import PatchBroker, PATCH_SCHEMA
from filesystem_layout import require_disjoint
from provisioning import operator_ceiling, protected_host_paths

REPO = Path(os.environ.get("PALETTE_REPO", Path(__file__).absolute().parents[2])).resolve()
BASE = Path(os.environ["OPERATOR_WORKSPACE"]).resolve()
LEGACY = Path(os.environ.get("PALETTE_STATE_DIR", str(Path.home() / ".local/share/operator-surfaces/palette-state" / hashlib.sha256(str(BASE).encode()).hexdigest()))).resolve()
LEGACY_RECEIPTS = LEGACY.with_name(LEGACY.name + ".receipts")
ROOT = Path(os.environ.get("PALETTE_WORKSPACE_STATE_ROOT", str(Path.home() / ".local/share/operator-surfaces/palette-workspaces"))).resolve()
ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
MAX_LIVE = int(os.environ.get("PALETTE_MAX_LIVE_WORKSPACES", "4"))
IDLE_SECONDS = float(os.environ.get("PALETTE_WORKSPACE_IDLE_SECONDS", "900"))
if MAX_LIVE < 1 or IDLE_SECONDS <= 0:
    raise ValueError("Workspace limits must be positive")
require_disjoint([BASE], [REPO, ROOT, LEGACY, LEGACY_RECEIPTS, os.environ.get("PALETTE_SCRATCH_ROOT", str(ROOT / "scratch"))])
require_disjoint([ROOT, LEGACY, LEGACY_RECEIPTS], [REPO])
require_disjoint([LEGACY, LEGACY_RECEIPTS], [ROOT])


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        json.dump(value, stream, sort_keys=True)
        stream.flush()
        os.fsync(stream.fileno())
    try:
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


@contextmanager
def registry():
    ROOT.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (ROOT / "registry.lock").open("a+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        path = ROOT / "registry.json"
        value = json.loads(path.read_text()) if path.exists() else {"format": 1, "workspaces": {}, "active": {}}
        if value.get("format") != 1 or not isinstance(value.get("workspaces"), dict):
            raise ValueError("Unsupported workspace registry")
        value.setdefault("active", {})
        before = json.dumps(value, sort_keys=True)
        yield value
        if before != json.dumps(value, sort_keys=True):
            atomic_json(path, value)


def context_key(context):
    return hashlib.sha256((str(BASE) + "\0" + context).encode()).hexdigest()


def default_entry():
    return {"workspace_id": "default", "scope": "project", "workspace_dir": str(BASE), "project_dir": str(BASE), "state_dir": str(LEGACY), "legacy": True}


def visible(entry, context):
    return (entry["scope"] == "open" or
            entry["scope"] == "project" and entry["project_dir"] == str(BASE) or
            entry["scope"] == "thread" and bool(context) and entry["owner_context"] == context)


def resolve(args):
    context = args.get("context_id")
    with registry() as reg:
        identifier = args.get("workspace_id") or reg["active"].get(context_key(context) if context else "") or "default"
        entry = default_entry() if identifier == "default" else reg["workspaces"].get(identifier)
        if entry is None:
            raise ValueError("Unknown workspace_id")
        if not ID_RE.fullmatch(identifier):
            raise ValueError("Invalid workspace_id in registry")
        expected_state = LEGACY if identifier == "default" else ROOT / "states" / identifier
        if entry.get("workspace_dir") != str(BASE) or Path(entry.get("state_dir", "")).resolve() != expected_state.resolve():
            raise PermissionError("Workspace registry paths differ from the configured layout")
        if not visible(entry, context):
            raise PermissionError("Workspace is outside this context's scope")
        return dict(entry)


class Child:
    def __init__(self, entry):
        self.entry = entry
        self.proc = None
        self.lease = None
        self.frames = queue.Queue()
        self.last_used = time.monotonic()
        self.lock = threading.RLock()

    def start(self):
        if self.proc is not None and self.proc.poll() is None:
            return
        self.stop()
        state = Path(self.entry["state_dir"]).resolve()
        require_disjoint([self.entry["workspace_dir"]], [REPO, ROOT, LEGACY])
        require_disjoint([state], [REPO] + [ROOT / name for name in ("patches", "registry.json", "registry.lock", "client-capabilities.json", "scratch")])
        state.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        # Outside the state mount; a worker cannot unlink its ownership lock.
        self.lease = state.with_name(state.name + ".owner.lock").open("a+")
        try:
            fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            self.lease.close()
            self.lease = None
            raise RuntimeError("Workspace already owned by another router; close it there before attaching here")
        env = dict(os.environ, OPERATOR_WORKSPACE=self.entry["workspace_dir"], PALETTE_STATE_DIR=str(state), PALETTE_WORKSPACE_ID=self.entry["workspace_id"])
        scratch = Path(env.get("PALETTE_SCRATCH_ROOT", str(ROOT / "scratch"))) / hashlib.sha256(str(state).encode()).hexdigest()
        scratch.mkdir(parents=True, exist_ok=True, mode=0o700)
        env["PALETTE_SCRATCH_ROOT"] = str(scratch)
        self.frames = queue.Queue()
        try:
            self.proc = subprocess.Popen([sys.executable, str(REPO / "runtime/security/operator_mcp.py")], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr, text=True, bufsize=1, cwd=self.entry["workspace_dir"], env=env, pass_fds=(self.lease.fileno(),))
            threading.Thread(target=self.pump, args=(self.proc, self.frames), daemon=True).start()
            self.exchange("initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "palette-router", "version": "0.3.0"}})
        except Exception:
            self.stop()
            raise

    @staticmethod
    def pump(proc, frames):
        try:
            while True:
                line = proc.stdout.readline(1048577)
                if not line:
                    raise EOFError("Workspace child exited; last call completion may be unknown")
                if len(line) > 1048576:
                    raise ValueError("Workspace child frame exceeded 1 MiB")
                frames.put(json.loads(line))
        except Exception as exc:
            frames.put(exc)

    def exchange(self, method, params):
        rid = uuid.uuid4().hex
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}) + "\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + 210
        while True:
            frame = self.frames.get(timeout=max(0.001, deadline - time.monotonic()))
            if isinstance(frame, Exception):
                raise frame
            if frame.get("id") == rid:
                if "error" in frame:
                    raise RuntimeError(frame["error"])
                return frame["result"]

    def call(self, name, args):
        with self.lock:
            self.start()
            try:
                return self.exchange("tools/call", {"name": name, "arguments": args})
            except Exception:
                self.stop()
                raise
            finally:
                self.last_used = time.monotonic()

    def stop(self):
        with self.lock:
            proc, self.proc = self.proc, None
            try:
                if proc:
                    try:
                        proc.stdin.close()
                    except BrokenPipeError:
                        pass
                    try:
                        proc.wait(timeout=40)
                    except subprocess.TimeoutExpired:
                        proc.terminate()
                        try:
                            proc.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            proc.kill()
                            proc.wait()
                    proc.stdout.close()
            finally:
                if self.lease:
                    self.lease.close()
                    self.lease = None


children = {}


def child_for(entry):
    for child in children.values():
        if child.proc and time.monotonic() - child.last_used >= IDLE_SECONDS:
            child.stop()
    key = entry["state_dir"]
    child = children.setdefault(key, Child(entry))
    if not child.proc and sum(c.proc is not None and c.proc.poll() is None for c in children.values()) >= MAX_LIVE:
        raise RuntimeError(f"Live workspace limit ({MAX_LIVE}); close an unused workspace first")
    return child


def workspace_control(args):
    action, context = args["action"], args.get("context_id")
    if action == "create":
        scope = args.get("scope", "thread")
        if scope == "thread" and not context:
            raise ValueError("Thread scope requires context_id (a routing key, not authenticated identity)")
        identifier = args.get("workspace_id", "ws-" + uuid.uuid4().hex)
        if not ID_RE.fullmatch(identifier) or identifier == "default":
            raise ValueError("Invalid or reserved workspace_id")
        with registry() as reg:
            if identifier in reg["workspaces"]:
                raise ValueError("workspace_id already exists")
            state = ROOT / "states" / identifier
            state.mkdir(parents=True, mode=0o700)
            entry = {"workspace_id": identifier, "scope": scope, "owner_context": context if scope == "thread" else None, "project_dir": str(BASE) if scope == "project" else None, "workspace_dir": str(BASE), "state_dir": str(state)}
            reg["workspaces"][identifier] = entry
            if context and args.get("attach", True):
                reg["active"][context_key(context)] = identifier
        return {**entry, "active_for_context": bool(context and args.get("attach", True))}
    if action == "list":
        with registry() as reg:
            return {"workspaces": [default_entry()] + [e for e in reg["workspaces"].values() if visible(e, context)], "identity": "caller-supplied context routing; not authentication"}
    entry = resolve(args)
    if action == "attach":
        if not context or not args.get("workspace_id"):
            raise ValueError("Attach requires context_id and workspace_id")
        with registry() as reg:
            reg["active"][context_key(context)] = entry["workspace_id"]
        return {"active_workspace_id": entry["workspace_id"]}
    child = children.get(entry["state_dir"])
    if action == "close":
        if child:
            child.stop()
        return {"workspace_id": entry["workspace_id"], "closed": True, "state_retained": True}
    return {**entry, "running": bool(child and child.proc and child.proc.poll() is None), "local_adapter_pid": child.proc.pid if child and child.proc and child.proc.poll() is None else None, "observation": "running describes this router; another router may hold ownership"}


def text_result(value, error=False):
    return {"content": [{"type": "text", "text": json.dumps(value, ensure_ascii=False)}], "isError": error}


# Reuse the proven adapter schemas without importing its process-owning globals.
PALETTE_SCHEMA = {"type": "object", "properties": {"code": {"type": "string"}, "view": {"enum": ["quiet", "full"]}, "payload": {"oneOf": [{"type": "string"}, {"type": "object", "additionalProperties": {"type": "string"}}]}, "ephemeral": {"type": "boolean"}}, "required": ["code"], "additionalProperties": False}
CONTROL_SCHEMA = {"type": "object", "properties": {"action": {"enum": ["status", "prepare", "restart"]}, "restore": {"type": "boolean"}, "prepare": {"type": "boolean"}}, "required": ["action"], "additionalProperties": False}
WORKSPACE_SCHEMA = {"type": "object", "properties": {"action": {"enum": ["create", "attach", "list", "status", "close"]}, "scope": {"enum": ["thread", "project", "open"]}, "attach": {"type": "boolean"}}, "required": ["action"], "additionalProperties": False}
for schema in (PALETTE_SCHEMA, CONTROL_SCHEMA, WORKSPACE_SCHEMA, PATCH_SCHEMA):
    schema["properties"].update(workspace_id={"type": "string", "maxLength": 64}, context_id={"type": "string", "maxLength": 256})
TOOLS = [{"name": name, "description": description, "inputSchema": schema, "annotations": {"readOnlyHint": False, "destructiveHint": True, "openWorldHint": False}} for name, description, schema in [
    ("palette", "Persistent Julia world. Explicit workspace_id wins over context_id's active workspace. Without either, uses the shared legacy world. context_id is a caller-chosen routing key, not verified identity. Files may remain shared. Quiet/full projection, payload and scratch retain their existing semantics.", PALETTE_SCHEMA),
    ("palette_control", "Status, offline preparation or restart of exactly the selected workspace. Missing routing fields select the shared legacy world. restore=false retires that world's snapshot.", CONTROL_SCHEMA),
    ("palette_workspace", "Create/attach/list/status/close thread, project or explicitly open worlds. Thread scope compares supplied context IDs; it does not authenticate callers. Close retains snapshots. Live worlds are limited; idle ones are stopped on subsequent calls.", WORKSPACE_SCHEMA),
    ("palette_patch", "Read bounded target files, prepare, inspect or apply exact file replacements in a host-configured target. Application requires explicit user approval through supported client elicitation or the local administrator CLI. No argument grants authority. Before hashes and expiry are rechecked. Multi-file application is not atomic; use a quiescent target.", PATCH_SCHEMA)]]


def validate(args, schema):
    if not isinstance(args, dict) or set(args) - set(schema["properties"]) or any(k not in args for k in schema["required"]):
        raise ValueError("Invalid tool arguments")
    for key, value in args.items():
        spec = schema["properties"][key]
        if "enum" in spec and value not in spec["enum"]:
            raise ValueError("Invalid " + key)
        kind = spec.get("type")
        if kind == "string" and (not isinstance(value, str) or not value or len(value) > spec.get("maxLength", 1048576)):
            raise ValueError("Invalid " + key)
        if kind == "boolean" and not isinstance(value, bool):
            raise ValueError("Invalid " + key)


def main():
    capabilities = {}
    roots = json.loads(os.environ.get("PALETTE_PATCH_ROOTS", "{}"))
    require_disjoint(roots.values(), [ROOT, LEGACY, LEGACY_RECEIPTS])
    ceiling = operator_ceiling()
    repair_config_path = os.environ.get("PALETTE_REPAIR_CONFIG")
    repair_config = json.loads(Path(repair_config_path).expanduser().read_text(encoding="utf-8")) if repair_config_path else {}
    if not isinstance(repair_config, dict) or set(repair_config) - {"test_recipes", "auto_apply_prefixes", "sandbox_source_paths"}:
        raise ValueError("PALETTE_REPAIR_CONFIG has unsupported fields")
    auto_apply_prefixes = repair_config.get("auto_apply_prefixes", {})
    runtime_root = Path(os.environ.get("PALETTE_RUNTIME_ROOT", str(Path(__file__).resolve().parents[1] / "capabilities"))).expanduser().resolve()
    broker = PatchBroker(ROOT / "patches", roots, auto_apply_prefixes,
                         protected_paths=[*protected_host_paths(ceiling, include_runtime_root=False), ROOT, LEGACY, LEGACY_RECEIPTS],
                         registry_root=runtime_root,
                         sandbox_source_paths=repair_config.get("sandbox_source_paths", {}))
    for raw in sys.stdin:
        mid = None
        try:
            msg = json.loads(raw)
            mid, method = msg.get("id"), msg.get("method", "")
            if method.startswith("notifications/"):
                continue
            if method == "initialize":
                capabilities = msg.get("params", {}).get("capabilities", {})
                atomic_json(ROOT / "client-capabilities.json", {"elicitation": capabilities.get("elicitation"), "protocolVersion": msg.get("params", {}).get("protocolVersion")})
                result = {"protocolVersion": msg.get("params", {}).get("protocolVersion", "2024-11-05"), "capabilities": {"tools": {}}, "serverInfo": {"name": "palette", "version": "0.3.0"}, "instructions": "Select a logical workspace for each independent experiment. Unrouted calls share the legacy world. Context keys are supplied by callers, not authenticated thread identities."}
            elif method == "ping":
                result = {}
            elif method == "tools/list":
                result = {"tools": TOOLS}
            elif method == "tools/call":
                params = msg.get("params", {})
                name, args = params.get("name"), params.get("arguments", {})
                try:
                    tool = next((t for t in TOOLS if t["name"] == name), None)
                    if not tool:
                        raise ValueError("Unknown tool")
                    validate(args, tool["inputSchema"])
                    if name == "palette_workspace":
                        result = text_result(workspace_control(args))
                    else:
                        entry = resolve(args)
                        if name == "palette_patch":
                            result = text_result(broker.call(args, entry["workspace_id"], capabilities, sys.stdin, sys.stdout))
                        else:
                            forwarded = {k: v for k, v in args.items() if k not in ("workspace_id", "context_id")}
                            result = child_for(entry).call(name, forwarded)
                            value = json.loads(result["content"][0]["text"])
                            value.update(workspace_id=entry["workspace_id"], workspace_scope=entry["scope"], legacy_shared=entry.get("legacy", False))
                            result["content"][0]["text"] = json.dumps(value)
                except Exception as exc:
                    result = text_result({"success": False, "error": f"{type(exc).__name__}: {exc}"}, True)
            else:
                raise ValueError("Unsupported method")
            print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}), flush=True)
        except Exception as exc:
            print(json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": str(exc)}}), flush=True)


def stop_all():
    for child in children.values():
        child.stop()


def terminate(signum, frame):
    raise SystemExit(128 + signum)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGHUP, terminate)
    try:
        main()
    finally:
        stop_all()
