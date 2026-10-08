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
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from patch_broker import PatchBroker, PATCH_SCHEMA
from filesystem_layout import require_disjoint
from provisioning import operator_ceiling, protected_host_paths
from project_lineage import ensure_project_lineage, lineage_id, normalize_entry as normalize_lineage_entry, project_state_dir, workspace_state_dir
from generation_runtime import GenerationStore
from source_transport import normalize_palette_call

REPO = Path(os.environ.get("PALETTE_REPO", Path(__file__).absolute().parents[2])).resolve()
BASE = Path(os.environ["OPERATOR_WORKSPACE"]).resolve()
LEGACY = Path(os.environ.get("PALETTE_STATE_DIR", str(Path.home() / ".local/share/operator-surfaces/palette-state" / hashlib.sha256(str(BASE).encode()).hexdigest()))).resolve()
LEGACY_RECEIPTS = LEGACY.with_name(LEGACY.name + ".receipts")
ROOT = Path(os.environ.get("PALETTE_WORKSPACE_STATE_ROOT", str(Path.home() / ".local/share/operator-surfaces/palette-workspaces"))).resolve()
GENERATIONS = GenerationStore(ROOT / "runtime-generations", REPO, os.environ.get("PALETTE_HOST"))
generation_lock = threading.RLock()
ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
MAX_LIVE = int(os.environ.get("PALETTE_MAX_LIVE_WORKSPACES", "4"))
IDLE_SECONDS = float(os.environ.get("PALETTE_WORKSPACE_IDLE_SECONDS", "900"))
if MAX_LIVE < 1 or IDLE_SECONDS <= 0:
    raise ValueError("Workspace limits must be positive")
require_disjoint([BASE], [REPO, ROOT, LEGACY, LEGACY_RECEIPTS, os.environ.get("PALETTE_SCRATCH_ROOT", str(ROOT / "scratch"))])
require_disjoint([ROOT, LEGACY, LEGACY_RECEIPTS], [REPO])
require_disjoint([LEGACY, LEGACY_RECEIPTS], [ROOT])


def workspace_protected_paths():
    protected = [BASE, REPO, ROOT, LEGACY, LEGACY_RECEIPTS,
                 Path(os.environ.get("PALETTE_SCRATCH_ROOT", str(ROOT / "scratch")))]
    protected.extend(Path(root) for root in os.environ.get("PALETTE_READ_ROOTS", "").split(":") if root)
    protected.extend(protected_host_paths(operator_ceiling()))
    depot = os.environ.get("JULIA_DEPOT_PATH", str(Path.home() / ".julia"))
    protected.extend(Path(path) for path in depot.split(os.pathsep) if path)
    julia = os.environ.get("PALETTE_JULIA_BIN") or shutil.which("julia")
    if julia:
        protected.append(Path(julia).expanduser().resolve().parent.parent)
    for name in ("PALETTE_HOST", "PALETTE_TASK_TOOLS", "PALETTE_TASK_ENV", "PALETTE_PACKAGE_DEPOT",
                 "PALETTE_RUNTIME_ROOT", "PALETTE_SOCKET", "PALETTE_CAPABILITY_CEILING",
                 "PALETTE_REPAIR_CONFIG"):
        value = os.environ.get(name)
        if value:
            protected.append(Path(value).expanduser())
    try:
        patch_roots = json.loads(os.environ.get("PALETTE_PATCH_ROOTS", "{}"))
    except json.JSONDecodeError as exc:
        raise ValueError("PALETTE_PATCH_ROOTS must be a JSON object") from exc
    if not isinstance(patch_roots, dict):
        raise ValueError("PALETTE_PATCH_ROOTS must be a JSON object")
    for target, raw_path in patch_roots.items():
        if not isinstance(target, str) or not isinstance(raw_path, str) or not Path(raw_path).is_absolute():
            raise ValueError("PALETTE_PATCH_ROOTS must map target IDs to absolute paths")
        protected.append(Path(raw_path).expanduser())
    return protected


def configured_project_roots():
    try:
        configured = json.loads(os.environ.get("PALETTE_PROJECT_ROOTS", "{}"))
    except json.JSONDecodeError as exc:
        raise ValueError("PALETTE_PROJECT_ROOTS must be a JSON object") from exc
    if not isinstance(configured, dict):
        raise ValueError("PALETTE_PROJECT_ROOTS must be a JSON object")

    roots = {"default": BASE}
    protected = workspace_protected_paths()

    for identifier, raw_path in configured.items():
        if not isinstance(identifier, str) or not ID_RE.fullmatch(identifier) or identifier == "default":
            raise ValueError("PALETTE_PROJECT_ROOTS keys must be valid non-default project root IDs")
        if not isinstance(raw_path, str) or not Path(raw_path).is_absolute():
            raise ValueError(f"Project root {identifier} must be an absolute path")
        try:
            path = Path(raw_path).resolve(strict=True)
        except OSError as exc:
            raise ValueError(f"Project root {identifier} is unavailable") from exc
        if not path.is_dir():
            raise ValueError(f"Project root {identifier} is not a directory")
        require_disjoint([path], [*protected, *roots.values()])
        roots[identifier] = path
    return roots


PROJECT_ROOTS = configured_project_roots()


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
    return {"workspace_id": "default", "scope": "project", "project_root_id": "default", "workspace_dir": str(BASE), "project_dir": str(BASE), "state_dir": str(LEGACY), "lineage_id": "legacy:default", "legacy": True}


def normalized_entry(entry):
    value = dict(entry)
    value.setdefault("project_root_id", "default")
    return value


def visible(entry, context):
    project_root = PROJECT_ROOTS.get(entry.get("project_root_id", "default"))
    if project_root is None:
        return False
    return (entry["scope"] == "open" or
            entry["scope"] == "project" and entry["project_dir"] == str(project_root) or
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
        entry = normalized_entry(entry)
        project_root = PROJECT_ROOTS.get(entry["project_root_id"])
        if project_root is None:
            raise PermissionError("Workspace project root is not configured")
        if entry.get("scope") not in {"thread", "project", "open"}:
            raise PermissionError("Workspace scope is invalid")
        if identifier == "default":
            if Path(entry.get("state_dir", "")).resolve() != LEGACY.resolve():
                raise PermissionError("Workspace registry paths differ from the configured layout")
        else:
            entry = normalize_lineage_entry(reg, ROOT, entry)
        expected_project = str(project_root) if entry["scope"] == "project" else None
        if entry.get("workspace_dir") != str(project_root) or entry.get("project_dir") != expected_project:
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
        self.generation_id = None

    def start(self):
        if self.proc is not None and self.proc.poll() is None:
            return
        self.stop()
        state = Path(self.entry["state_dir"]).resolve()
        workspace = Path(self.entry["workspace_dir"]).resolve()
        generation = GENERATIONS.resolve()
        generation_repo = Path(generation["repo"]).resolve()
        generation_adapter = Path(generation["adapter"]).resolve()
        generation_host = Path(generation["host"]).resolve()
        protected = workspace_protected_paths() + [generation_repo, generation_adapter, generation_host]
        if workspace == BASE:
            protected = [path for path in protected if path != BASE]
        else:
            protected.append(BASE)
        require_disjoint([workspace], protected)
        require_disjoint([state], [REPO, generation_repo] + [ROOT / name for name in ("patches", "registry.json", "registry.lock", "client-capabilities.json", "scratch", "runtime-generations")])
        state.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        # Outside the state mount; a worker cannot unlink its ownership lock.
        self.lease = state.with_name(state.name + ".owner.lock").open("a+")
        try:
            fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            self.lease.close()
            self.lease = None
            raise RuntimeError("Workspace already owned by another router; close it there before attaching here")
        env = dict(os.environ, OPERATOR_WORKSPACE=self.entry["workspace_dir"], PALETTE_STATE_DIR=str(state), PALETTE_WORKSPACE_ID=self.entry.get("lineage_id", self.entry["workspace_id"]), PALETTE_REPO=str(generation_repo), PALETTE_HOST=str(generation_host))
        scratch = Path(env.get("PALETTE_SCRATCH_ROOT", str(ROOT / "scratch"))) / hashlib.sha256(str(state).encode()).hexdigest()
        scratch.mkdir(parents=True, exist_ok=True, mode=0o700)
        env["PALETTE_SCRATCH_ROOT"] = str(scratch)
        self.frames = queue.Queue()
        try:
            self.proc = subprocess.Popen([sys.executable, str(generation_adapter)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr, text=True, bufsize=1, cwd=self.entry["workspace_dir"], env=env, pass_fds=(self.lease.fileno(),))
            self.generation_id = generation["generation_id"]
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
        stop_idle_children(self)
        with self.lock:
            key = self.entry["state_dir"]
            if self.proc is not None and self.proc.poll() is not None:
                self.stop()
            reserved = False
            with children_lock:
                if self.proc is None:
                    occupied = {key for key, child in children.items()
                                if child.proc is not None and child.proc.poll() is None}
                    occupied.update(starting_children)
                    occupied.update(stopping_children)
                    if len(occupied) >= MAX_LIVE:
                        raise RuntimeError(f"Live workspace limit ({MAX_LIVE}); close an unused workspace first")
                    starting_children.add(key)
                    reserved = True
            if reserved:
                try:
                    self.start()
                finally:
                    with children_lock:
                        starting_children.discard(key)
            try:
                return self.exchange("tools/call", {"name": name, "arguments": args})
            except Exception:
                self.stop()
                raise
            finally:
                self.last_used = time.monotonic()

    def stop(self):
        with self.lock:
            proc = self.proc
            if proc is not None:
                with children_lock:
                    self.proc = None
                    stopping_children.add(self.entry["state_dir"])
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
                if proc:
                    with children_lock:
                        stopping_children.discard(self.entry["state_dir"])


children = {}
children_lock = threading.RLock()
starting_children = set()
stopping_children = set()


def stop_idle_children(except_child=None):
    with children_lock:
        candidates = [child for child in children.values()
                      if child is not except_child and child.proc is not None
                      and time.monotonic() - child.last_used >= IDLE_SECONDS]
    for child in candidates:
        if not child.lock.acquire(blocking=False):
            continue
        try:
            with children_lock:
                idle = (child.proc is not None
                        and time.monotonic() - child.last_used >= IDLE_SECONDS)
            if idle:
                child.stop()
        finally:
            child.lock.release()


def child_for(entry):
    with children_lock:
        key = entry["state_dir"]
        return children.setdefault(key, Child(entry))


def workspace_control(args):
    action, context = args["action"], args.get("context_id")
    if action != "create" and "project_root_id" in args:
        raise ValueError("project_root_id is only valid when creating a workspace")
    if action == "create":
        scope = args.get("scope", "thread")
        if scope == "thread" and not context:
            raise ValueError("Thread scope requires context_id (a routing key, not authenticated identity)")
        project_root_id = args.get("project_root_id", "default")
        project_root = PROJECT_ROOTS.get(project_root_id)
        if project_root is None:
            raise ValueError("Unknown project_root_id")
        identifier = args.get("workspace_id", "ws-" + uuid.uuid4().hex)
        if not ID_RE.fullmatch(identifier) or identifier == "default":
            raise ValueError("Invalid or reserved workspace_id")
        with registry() as reg:
            if identifier in reg["workspaces"]:
                raise ValueError("workspace_id already exists")
            if scope == "project":
                state = project_state_dir(ROOT, project_root_id)
            else:
                state = workspace_state_dir(ROOT, identifier)
            state.mkdir(parents=True, exist_ok=True, mode=0o700)
            entry = {"workspace_id": identifier, "scope": scope, "project_root_id": project_root_id, "owner_context": context if scope == "thread" else None, "project_dir": str(project_root) if scope == "project" else None, "workspace_dir": str(project_root), "state_dir": str(state), "lineage_id": lineage_id(scope, project_root_id, identifier)}
            reg["workspaces"][identifier] = entry
            if scope == "project":
                ensure_project_lineage(reg, ROOT, project_root_id)
                entry = dict(reg["workspaces"][identifier])
            if context and args.get("attach", True):
                reg["active"][context_key(context)] = identifier
        return {**entry, "active_for_context": bool(context and args.get("attach", True))}
    if action == "list":
        with registry() as reg:
            entries = [normalized_entry(e) for e in reg["workspaces"].values()]
            workspaces = [default_entry()] + [e for e in entries if visible(e, context)]
            roots = [{"project_root_id": identifier, "workspace_dir": str(path)}
                     for identifier, path in sorted(PROJECT_ROOTS.items())]
            return {"workspaces": workspaces, "available_project_roots": roots, "identity": "caller-supplied context routing; not authentication"}
    entry = resolve(args)
    if action == "attach":
        if not context or not args.get("workspace_id"):
            raise ValueError("Attach requires context_id and workspace_id")
        with registry() as reg:
            reg["active"][context_key(context)] = entry["workspace_id"]
        return {"active_workspace_id": entry["workspace_id"]}
    with children_lock:
        child = children.get(entry["state_dir"])
    if action == "close":
        # close is linearized against starts and waits for any in-flight call
        # to finish before stopping the worker. A later call may start it again.
        if child:
            with child.lock:
                proc = child.proc
                with children_lock:
                    current = children.get(entry["state_dir"])
                if current is child and proc:
                    child.stop()
        return {"workspace_id": entry["workspace_id"], "closed": True, "state_retained": True}
    if child:
        with child.lock:
            with children_lock:
                current = children.get(entry["state_dir"])
            proc = child.proc if current is child else None
            running = bool(proc and proc.poll() is None)
            adapter_pid = proc.pid if running else None
    else:
        running, adapter_pid = False, None
    return {**entry, "running": running, "local_adapter_pid": adapter_pid, "observation": "running describes this router; another router may hold ownership"}



def generation_control(args, entry):
    action = args["action"]
    if action == "generation_status":
        current = GENERATIONS.resolve()
        return {"generation_id": current["generation_id"], "available_generations": GENERATIONS.available()}

    if action != "activate_generation":
        raise ValueError("Unknown generation action")
    generation_id = args.get("generation_id")
    if not generation_id:
        raise ValueError("activate_generation requires generation_id")

    with generation_lock:
        previous_id = GENERATIONS.current_id()
        target = GENERATIONS.resolve(generation_id)
        if target["generation_id"] == previous_id:
            return {"generation_id": previous_id, "previous_generation_id": previous_id, "changed": False}

        stop_all()
        GENERATIONS.activate(generation_id)
        try:
            smoke = child_for(entry).call("palette", {"code": "nothing", "view": "full"})
            value = json.loads(smoke["content"][0]["text"])
            if smoke.get("isError") or value.get("success") is False:
                raise RuntimeError(value.get("error") or "new generation smoke call failed")
            return {
                "generation_id": generation_id,
                "previous_generation_id": previous_id,
                "changed": True,
                "smoke_epoch": value.get("epoch"),
                "lineage_id": entry.get("lineage_id"),
            }
        except Exception as exc:
            stop_all()
            GENERATIONS.activate(previous_id)
            rollback_error = None
            try:
                child_for(entry).call("palette", {"code": "nothing", "view": "full"})
            except Exception as rollback_exc:
                rollback_error = f"{type(rollback_exc).__name__}: {rollback_exc}"
            detail = f"generation activation failed and rolled back: {type(exc).__name__}: {exc}"
            if rollback_error:
                detail += f"; rollback restart also failed: {rollback_error}"
            raise RuntimeError(detail) from exc

def text_result(value, error=False):
    return {"content": [{"type": "text", "text": json.dumps(value, ensure_ascii=False)}], "isError": error}


# Reuse the proven adapter schemas without importing its process-owning globals.
PALETTE_SCHEMA = {"type": "object", "properties": {"code": {"type": "string"}, "source": {"type": "string"}, "view": {"enum": ["quiet", "full"]}, "payload": {"oneOf": [{"type": "string"}, {"type": "object", "additionalProperties": {"type": "string"}}]}, "ephemeral": {"type": "boolean"}}, "required": [], "additionalProperties": False}
CONTROL_SCHEMA = {"type": "object", "properties": {"action": {"enum": ["status", "prepare", "restart", "generation_status", "activate_generation"]}, "restore": {"type": "boolean"}, "prepare": {"type": "boolean"}, "generation_id": {"type": "string", "maxLength": 64}}, "required": ["action"], "additionalProperties": False}
WORKSPACE_SCHEMA = {"type": "object", "properties": {"action": {"enum": ["create", "attach", "list", "status", "close"]}, "scope": {"enum": ["thread", "project", "open"]}, "attach": {"type": "boolean"}, "project_root_id": {"type": "string", "maxLength": 64}}, "required": ["action"], "additionalProperties": False}
for schema in (PALETTE_SCHEMA, CONTROL_SCHEMA, WORKSPACE_SCHEMA, PATCH_SCHEMA):
    schema["properties"].update(workspace_id={"type": "string", "maxLength": 64}, context_id={"type": "string", "maxLength": 256})
TOOLS = [{"name": name, "description": description, "inputSchema": schema, "annotations": {"readOnlyHint": False, "destructiveHint": True, "openWorldHint": False}} for name, description, schema in [
    ("palette", "Persistent Julia world. source is transport-safe opaque Julia text; legacy code remains accepted. Explicit workspace_id wins over context_id's active workspace. Without either, uses the shared legacy world. context_id is a caller-chosen routing key, not verified identity. Files may remain shared. Quiet/full projection, payload and scratch retain their existing semantics.", PALETTE_SCHEMA),
    ("palette_control", "Status, offline preparation or restart of exactly the selected workspace. Missing routing fields select the shared legacy world. restore=false retires that world's snapshot.", CONTROL_SCHEMA),
    ("palette_workspace", "Create/attach/list/status/close thread, project or explicitly open worlds. On create, project_root_id selects a host-configured project directory; omit it for the legacy workspace. Paths are never accepted from tool arguments. Project-scoped aliases selecting the same root share one durable Julia lineage; thread/open worlds remain independent. Thread scope compares supplied context IDs; it does not authenticate callers. Close retains snapshots. Live worlds are limited; idle ones are stopped on subsequent calls.", WORKSPACE_SCHEMA),
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


def serve_stream(stdin, stdout):
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
    for raw in stdin:
        mid = None
        try:
            msg = json.loads(raw)
            mid, method = msg.get("id"), msg.get("method", "")
            if method.startswith("notifications/"):
                continue
            if method == "initialize":
                capabilities = msg.get("params", {}).get("capabilities", {})
                atomic_json(ROOT / "client-capabilities.json", {"elicitation": capabilities.get("elicitation"), "protocolVersion": msg.get("params", {}).get("protocolVersion")})
                result = {"protocolVersion": msg.get("params", {}).get("protocolVersion", "2024-11-05"), "capabilities": {"tools": {}}, "serverInfo": {"name": "palette", "version": "0.3.0"}, "instructions": "Select a logical workspace for each independent experiment. Omit project_root_id to keep using the legacy workspace; otherwise select a configured ID returned by palette_workspace list. Tool arguments cannot supply filesystem paths. Project-scoped workspaces on the same project root share one durable Julia lineage; thread/open workspaces remain independent. Context keys are supplied by callers, not authenticated thread identities."}
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
                    if name == "palette":
                        routed = {k: v for k, v in args.items() if k not in ("workspace_id", "context_id")}
                        normalized = normalize_palette_call(routed)
                        args = {**{k: v for k, v in args.items() if k in ("workspace_id", "context_id")}, **normalized}
                    if name == "palette_workspace":
                        result = text_result(workspace_control(args))
                    else:
                        entry = resolve(args)
                        if name == "palette_patch":
                            result = text_result(broker.call(args, entry.get("lineage_id", entry["workspace_id"]), capabilities, stdin, stdout))
                        elif name == "palette_control" and args["action"] in ("generation_status", "activate_generation"):
                            result = text_result(generation_control(args, entry))
                        else:
                            forwarded = {k: v for k, v in args.items() if k not in ("workspace_id", "context_id")}
                            result = child_for(entry).call(name, forwarded)
                            value = json.loads(result["content"][0]["text"])
                            child = child_for(entry)
                            value.update(workspace_id=entry["workspace_id"], workspace_scope=entry["scope"], project_root_id=entry["project_root_id"], lineage_id=entry.get("lineage_id"), generation_id=child.generation_id or GENERATIONS.current_id(), workspace_dir=entry["workspace_dir"], legacy_shared=entry.get("legacy", False))
                            result["content"][0]["text"] = json.dumps(value)
                except Exception as exc:
                    result = text_result({"success": False, "error": f"{type(exc).__name__}: {exc}"}, True)
            else:
                raise ValueError("Unsupported method")
            print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}), file=stdout, flush=True)
        except Exception as exc:
            print(json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": str(exc)}}), file=stdout, flush=True)


def _safe_socket_path(path):
    path = Path(path).expanduser().absolute()
    current = Path(path.anchor)
    directories = []
    for part in path.parts[1:-1]:
        current /= part
        try:
            st = current.lstat()
        except FileNotFoundError:
            current.mkdir(mode=0o700)
            st = current.lstat()
        if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
            raise ValueError("Palette socket path components must be real directories")
        directories.append((current, st))
    for index, (directory, st) in enumerate(directories):
        if stat.S_IMODE(st.st_mode) & 0o022:
            if not (stat.S_ISVTX & st.st_mode) or index + 1 == len(directories):
                raise PermissionError("Palette socket path has a writable non-sticky ancestor")
            child, child_stat = directories[index + 1]
            if child_stat.st_uid != os.getuid() or stat.S_IMODE(child_stat.st_mode) & 0o077:
                raise PermissionError("A sticky socket-path ancestor must contain a private directory owned by this user")
    parent_stat = path.parent.lstat()
    if parent_stat.st_uid != os.getuid() or stat.S_IMODE(parent_stat.st_mode) & 0o077:
        raise PermissionError("Palette socket parent must be owned by this user with private permissions")
    try:
        st = path.lstat()
    except FileNotFoundError:
        return path
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISSOCK(st.st_mode):
        raise ValueError("Palette socket path exists and is not a socket")
    if st.st_uid != os.getuid():
        raise PermissionError("Palette socket is owned by another user")
    # A stale socket may be removed; a live one is rejected by the connect probe.
    probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        probe.connect(str(path))
    except OSError:
        path.unlink()
    else:
        raise RuntimeError("Palette daemon socket is already accepting connections")
    finally:
        probe.close()
    return path


def serve_connection(conn):
    stream = None
    with conn:
        try:
            if peer_uid(conn) != os.getuid():
                return
            stream = conn.makefile("rwb", buffering=0)
            class TextInput:
                def fileno(self):
                    return stream.fileno()
                def readline(self, limit=-1):
                    line = stream.readline(1048577 if limit < 0 else limit)
                    if len(line) > 1048576:
                        raise ValueError("MCP frame exceeded 1 MiB")
                    return line.decode("utf-8")
                def __iter__(self):
                    while True:
                        line = stream.readline(1048577)
                        if not line:
                            return
                        if len(line) > 1048576:
                            raise ValueError("MCP frame exceeded 1 MiB")
                        yield line.decode("utf-8")
            class TextOutput:
                def write(self, value):
                    return stream.write(value.encode("utf-8"))
                def flush(self):
                    return None
            serve_stream(TextInput(), TextOutput())
        except (BrokenPipeError, ConnectionResetError, OSError):
            return
        except Exception as exc:
            print(f"Palette socket client rejected: {type(exc).__name__}: {exc}", file=sys.stderr, flush=True)
        finally:
            if stream is not None:
                stream.close()


def peer_uid(conn):
    if hasattr(socket, "SO_PEERCRED"):
        creds = conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
        return __import__("struct").unpack("3i", creds)[1]
    if hasattr(conn, "getpeereid"):
        return conn.getpeereid()[0]
    raise RuntimeError("This platform cannot verify Unix socket peer credentials")


def serve_socket(path):
    path = _safe_socket_path(path)
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    old_umask = os.umask(0o077)
    try:
        listener.bind(str(path))
    finally:
        os.umask(old_umask)
    os.chmod(path, 0o600)
    owned_inode = path.stat().st_ino
    listener.listen(16)
    listener.settimeout(1.0)
    print(f"Palette daemon listening on {path}", file=sys.stderr, flush=True)
    try:
        while True:
            try:
                conn, _ = listener.accept()
            except socket.timeout:
                continue
            threading.Thread(target=serve_connection, args=(conn,), daemon=True).start()
    finally:
        listener.close()
        try:
            st = path.lstat()
            if stat.S_ISSOCK(st.st_mode) and st.st_ino == owned_inode:
                path.unlink()
        except FileNotFoundError:
            pass


def stop_all():
    with children_lock:
        owned = list(children.values())
    for child in owned:
        child.stop()


def terminate(signum, frame):
    raise SystemExit(128 + signum)


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument("--socket", dest="socket_path")
    options = parser.parse_args()
    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGHUP, terminate)
    try:
        if options.socket_path:
            serve_socket(options.socket_path)
        else:
            serve_stream(sys.stdin, sys.stdout)
    finally:
        stop_all()
