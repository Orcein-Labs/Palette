#!/usr/bin/env python3
"""Host-side, one-attempt file patch grants. Approval is never a tool argument."""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import ctypes
import difflib
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import selectors
import stat
import subprocess
import sys
import tempfile
import time
import uuid
from filesystem_layout import require_disjoint

PATCH_SCHEMA = {"type": "object", "properties": {"action": {"enum": ["read", "prepare", "status", "apply"]}, "target": {"type": "string"}, "path": {"type": "string"}, "request_id": {"type": "string"}, "changes": {"type": "array", "maxItems": 16, "items": {"type": "object", "properties": {"path": {"type": "string"}, "before_sha256": {"type": ["string", "null"]}, "content": {"type": "string"}}, "required": ["path", "before_sha256", "content"], "additionalProperties": False}}}, "required": ["action"], "additionalProperties": False}
MAX_BYTES = 65536
REQUEST_FIELDS = ("request_id", "workspace_id", "target", "root", "root_identity", "changes", "preview", "created_at", "expiry", "allowed_operations")
AUTO_APPLY_CLASSES = frozenset({"runtime", "tools", "recipes", "adapters"})
IMMUTABLE_CREDENTIAL_COMPONENTS = frozenset({".ssh", ".aws", ".gnupg", "credentials", "secrets"})


def credential_location(path):
    """Catch secrets even when the configured patch root is narrowed inside one."""
    parts = [part.casefold() for part in Path(path).parts]
    basename = parts[-1] if parts else ""
    return (any(part in IMMUTABLE_CREDENTIAL_COMPONENTS for part in parts[:-1]) or
            basename in {".ssh", ".aws", ".gnupg"} or basename.startswith(".env") or
            any(token in basename for token in ("credential", "secret")))


def classify_patch_path(relative, configured_prefixes=()):
    """Classify a path using host-owned prefixes and an immutable authority denylist.

    Prefix configuration is supplied by the operator at broker construction; it
    is never read from a patch request. The denylist wins over every prefix.
    """
    parts = parent_path_parts(relative)
    lowered = [part.casefold() for part in parts]
    joined = "/".join(lowered)
    basename = lowered[-1]
    protected_dirs = ("runtime/security", "runtime/host", "runtime/capabilities", "runtime/plugins",
                      "src/security", "src/host", "host/security", ".github", ".buildkite",
                      "config", "configs", "deployment", "deploy", "infra", ".ssh", ".aws", ".gnupg")
    protected_names = {
        "operator_mcp.py", "operator_workspace_router.py", "runtime_registry.py",
        "patch_broker.py", "repair_broker.py", "broker.py", "sandbox.py",
        "launch_worker.py", "install_operator_plugin.py", "serve_palette.py",
        "capability.json",
        "project.toml", "manifest.toml", "startup.jl", "localpreferences.toml",
        "pyproject.toml", "setup.py", "setup.cfg", "tox.ini", "pytest.ini",
    }
    authority_tokens = ("credential", "secret", "privilege", "ceiling", "authority", "sandbox", "broker")
    credential_dirs = {".ssh", ".aws", ".gnupg", "credentials", "secrets"}
    if (any(part in credential_dirs for part in lowered[:-1]) or basename in {".ssh", ".aws", ".gnupg"} or
            any(token in basename for token in ("credential", "secret")) or
            basename.startswith(".env")):
        return {"class": "forbidden", "self_apply": False, "reason": "credential or secret path"}
    if any(joined == item or joined.startswith(item + "/") for item in protected_dirs):
        return {"class": "protected", "self_apply": False, "reason": "authority path"}
    protected_components = {"security", "host", "capabilities", "plugins", "scripts",
                            "config", "configs", "deployment", "deploy", "infra",
                            ".ssh", ".aws", ".gnupg"}
    if any(part in protected_components for part in lowered[:-1]):
        return {"class": "protected", "self_apply": False, "reason": "authority directory"}
    if (any(part in {"test", "tests", "verification", "fixtures"} for part in lowered[:-1]) or
            basename.startswith(("test_", "test-", "verify_", "verify-")) or
            basename in {"conftest.py", "pytest.ini", "tox.ini"}):
        return {"class": "protected", "self_apply": False, "reason": "test or verification path"}
    if (basename in protected_names or basename.startswith((".env", "config", "settings")) or
            basename.endswith((".pth", ".jllpath")) or
            any(token in basename for token in authority_tokens)):
        return {"class": "protected", "self_apply": False, "reason": "authority or load-path file"}
    for class_name, prefix in configured_prefixes:
        prefix_parts = parent_path_parts(prefix)
        if parts[:len(prefix_parts)] == prefix_parts:
            return {"class": class_name, "self_apply": True, "reason": "operator-configured ordinary path"}
    return {"class": "unknown", "self_apply": False, "reason": "no operator-configured ordinary path class"}


def parent_path_parts(relative):
    if not isinstance(relative, str) or not relative:
        raise ValueError("File path must be relative")
    parts = tuple(relative.split("/"))
    if any(part in ("", ".", "..", ".git") for part in parts) or PurePosixPath(relative).is_absolute():
        raise ValueError("Traversal and Git metadata are forbidden")
    return parts


def request_digest(record):
    immutable = {key: record[key] for key in REQUEST_FIELDS}
    immutable["status"] = "pending"
    return digest(json.dumps(immutable, sort_keys=True).encode())


def digest(data):
    return hashlib.sha256(data).hexdigest()


def write_json(path, value):
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as stream:
        tmp = Path(stream.name)
        json.dump(value, stream)
        stream.flush()
        os.fsync(stream.fileno())
    try:
        os.replace(tmp, path)
    finally:
        tmp.unlink(missing_ok=True)


def parent_fd(root_fd, relative):
    parts = parent_path_parts(relative)
    fd = os.dup(root_fd)
    try:
        for part in parts[:-1]:
            next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        return fd, parts[-1]
    except Exception:
        os.close(fd)
        raise


def file_snapshot(fd, name):
    try:
        file_fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
    except FileNotFoundError:
        return None, None, None
    try:
        metadata = os.fstat(file_fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise ValueError("Only regular files without hardlinks may be patched")
        with os.fdopen(os.dup(file_fd), "rb") as stream:
            data = stream.read(MAX_BYTES + 1)
        if len(data) > MAX_BYTES:
            raise ValueError("Existing file exceeds patch size limit")
        return digest(data), data, stat.S_IMODE(metadata.st_mode)
    finally:
        os.close(file_fd)


def confine_writes(root_fd):
    """Mandatory Landlock >=3 on Linux: the helper can write only the approved tree."""
    if sys.platform != "linux" or platform.machine() not in ("x86_64", "aarch64"):
        raise RuntimeError("Patch application requires supported Linux Landlock")
    libc = ctypes.CDLL(None, use_errno=True)
    abi = libc.syscall(444, 0, 0, 1)
    if abi < 3:
        raise RuntimeError("Patch application requires Landlock ABI >=3")
    handled = ((1 << 15) - 1) & ~(1 | 4 | 8)
    attr = ctypes.c_uint64(handled)
    rules = libc.syscall(444, ctypes.byref(attr), ctypes.sizeof(attr), 0)
    if rules < 0:
        raise OSError(ctypes.get_errno(), "landlock_create_ruleset")
    class Rule(ctypes.Structure):
        _pack_ = 1
        _fields_ = [("allowed_access", ctypes.c_uint64), ("parent_fd", ctypes.c_int32)]
    rule = Rule(2 | 32 | 256 | 16384, root_fd)
    try:
        if libc.syscall(445, rules, 1, ctypes.byref(rule), 0) != 0 or libc.prctl(38, 1, 0, 0, 0) != 0 or libc.syscall(446, rules, 0) != 0:
            raise OSError(ctypes.get_errno(), "Landlock enforcement failed")
    finally:
        os.close(rules)


def apply_files(record):
    touched = []
    root_fd = os.open(record["root"], os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        metadata = os.fstat(root_fd)
        if [metadata.st_dev, metadata.st_ino] != record["root_identity"]:
            raise ValueError("Approved root identity changed")
        confine_writes(root_fd)
        parents = []
        try:
            # Check every precondition before the first mutation.
            for change in record["changes"]:
                fd, name = parent_fd(root_fd, change["path"])
                parents.append((fd, name, change))
                before, _, _ = file_snapshot(fd, name)
                if before != change["before_sha256"]:
                    raise ValueError("Before hash conflict: " + change["path"])
            for fd, name, change in parents:
                before, _, mode = file_snapshot(fd, name)
                if before != change["before_sha256"]:
                    raise ValueError("Concurrent edit: " + change["path"])
                tmp = ".palette-patch-" + uuid.uuid4().hex
                file_fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=fd)
                try:
                    with os.fdopen(file_fd, "wb") as stream:
                        stream.write(change["content"].encode())
                        stream.flush()
                        os.fchmod(stream.fileno(), mode if mode is not None else 0o644)
                        os.fsync(stream.fileno())
                    # Keep atomic replacement within this descriptor-relative directory.
                    check, _, _ = file_snapshot(fd, name)
                    if check != before:
                        raise ValueError("Concurrent edit before commit: " + change["path"])
                    os.replace(tmp, name, src_dir_fd=fd, dst_dir_fd=fd)
                    touched.append({"path": change["path"], "before_sha256": before, "after_sha256": digest(change["content"].encode())})
                    os.fsync(fd)
                finally:
                    try:
                        os.unlink(tmp, dir_fd=fd)
                    except FileNotFoundError:
                        pass
            return {"status": "applied", "files_touched": touched, "atomicity": "per-file; target must be quiescent for a multi-file patch"}
        except Exception as exc:
            return {"status": "partial" if touched else "failed", "files_touched": touched, "error": str(exc)}
        finally:
            for fd, _, _ in parents:
                os.close(fd)
    finally:
        os.close(root_fd)


class PatchBroker:
    def __init__(self, storage, roots, auto_apply_prefixes=None, protected_paths=(),
                 registry_root=None, sandbox_source_paths=None):
        self.storage = Path(storage).resolve()
        self.roots = {key: str(Path(path).resolve(strict=True)) for key, path in roots.items()}
        require_disjoint(self.roots.values(), [self.storage])
        self.protected_paths = tuple(Path(path).resolve(strict=False) for path in protected_paths)
        self.registry_root = None
        self.sandbox_source_paths = {}
        if registry_root is not None:
            configured_root = Path(registry_root).expanduser()
            if configured_root.is_symlink():
                raise ValueError("Operator runtime registry root must not be a symlink")
            self.registry_root = configured_root.resolve(strict=False)
            if self.registry_root.exists() and not self.registry_root.is_dir():
                raise ValueError("Operator runtime registry root must be a directory")
            self.registry_root_has_symlink_ancestor = configured_root.absolute() != self.registry_root
        else:
            self.registry_root_has_symlink_ancestor = False
        if sandbox_source_paths and self.registry_root is None:
            raise ValueError("Sandbox source paths require the operator runtime registry root")
        self.auto_apply_prefixes = {}
        for target, entries in (auto_apply_prefixes or {}).items():
            if target not in self.roots:
                raise ValueError("Auto-apply classes require a configured patch target")
            normalized = []
            for class_name, prefix in entries:
                if class_name not in AUTO_APPLY_CLASSES:
                    raise ValueError("Unknown auto-apply path class")
                parent_path_parts(prefix)
                normalized.append((class_name, prefix))
            self.auto_apply_prefixes[target] = tuple(normalized)
        for target, entries in (sandbox_source_paths or {}).items():
            if target not in self.roots:
                raise ValueError("Sandbox source paths require a configured patch target")
            raw_root = Path(roots[target]).expanduser()
            root = Path(self.roots[target])
            if raw_root.is_symlink() or raw_root.absolute() != root:
                raise ValueError("Sandbox source target must not be a symlink")
            if (self.registry_root is None or not self.registry_root.is_dir() or
                    root.parent != self.registry_root):
                raise ValueError("Sandbox source target must be one immediate capability directory under the runtime registry")
            if root.is_symlink() or not root.is_dir():
                raise ValueError("Sandbox source target must be a real capability directory")
            if self.registry_root_has_symlink_ancestor:
                raise ValueError("Sandbox source registry path cannot contain symlinked ancestors")
            if credential_location(root):
                raise ValueError("Sandbox source target cannot be inside a credential or secret path")
            root_info = root.stat()
            registry_info = self.registry_root.stat()
            if (root_info.st_uid != os.geteuid() or registry_info.st_uid != os.geteuid() or
                    root_info.st_mode & 0o022 or registry_info.st_mode & 0o022):
                raise ValueError("Sandbox source registry must be operator-owned and not group/world writable")
            if not isinstance(entries, (tuple, list)) or not entries:
                raise ValueError("Sandbox source paths must list exact Julia source filenames")
            filenames = set()
            for relative in entries:
                parts = parent_path_parts(relative)
                if len(parts) != 1 or not parts[0].endswith(".jl"):
                    raise ValueError("Sandbox source paths must be immediate .jl files under a capability directory")
                source = root / parts[0]
                try:
                    metadata = source.lstat()
                except OSError as exc:
                    raise ValueError(f"Configured sandbox source is unavailable: {relative}") from exc
                if (not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1 or
                        metadata.st_uid != os.geteuid() or metadata.st_mode & 0o022):
                    raise ValueError("Configured sandbox source must be a regular file without hardlinks")
                manifest_path = root / "capability.json"
                try:
                    manifest_info = manifest_path.lstat()
                    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError) as exc:
                    raise ValueError("Sandbox source target requires a readable capability manifest") from exc
                if (not isinstance(manifest, dict) or not stat.S_ISREG(manifest_info.st_mode) or
                        manifest_info.st_nlink != 1 or manifest_info.st_uid != os.geteuid() or
                        manifest_info.st_mode & 0o022 or manifest.get("source") != parts[0]):
                    raise ValueError("Configured sandbox source must match its immutable capability manifest")
                class_name = self._configured_class(target, relative)
                if class_name not in {"adapters", "tools"}:
                    raise ValueError("Sandbox source requires an explicit adapters or tools path class")
                filenames.add(relative)
            self.sandbox_source_paths[target] = frozenset(filenames)
        self.storage.mkdir(parents=True, exist_ok=True, mode=0o700)

    def _configured_class(self, target, relative):
        parts = parent_path_parts(relative)
        for class_name, prefix in self.auto_apply_prefixes.get(target, ()):
            prefix_parts = parent_path_parts(prefix)
            if parts[:len(prefix_parts)] == prefix_parts:
                return class_name
        return None

    def _is_sandbox_source(self, target, relative):
        if relative not in self.sandbox_source_paths.get(target, ()):
            return False
        class_name = self._configured_class(target, relative)
        if class_name not in {"adapters", "tools"}:
            return False
        root = Path(self.roots[target])
        source = root / relative
        manifest_path = root / "capability.json"
        try:
            root_info = root.lstat()
            registry_info = self.registry_root.lstat()
            source_info = source.lstat()
            manifest_info = manifest_path.lstat()
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return False
        return (not stat.S_ISLNK(root_info.st_mode) and stat.S_ISDIR(root_info.st_mode) and
                root_info.st_uid == os.geteuid() and not root_info.st_mode & 0o022 and
                stat.S_ISDIR(registry_info.st_mode) and registry_info.st_uid == os.geteuid() and
                not registry_info.st_mode & 0o022 and
                stat.S_ISREG(source_info.st_mode) and source_info.st_nlink == 1 and
                source_info.st_uid == os.geteuid() and not source_info.st_mode & 0o022 and
                stat.S_ISREG(manifest_info.st_mode) and manifest_info.st_nlink == 1 and
                manifest_info.st_uid == os.geteuid() and not manifest_info.st_mode & 0o022 and
                isinstance(manifest, dict) and
                manifest.get("source") == relative)

    def classify(self, target, relative):
        if target not in self.roots:
            raise PermissionError("Target is not in the host-configured patch roots")
        candidate = Path(self.roots[target]).joinpath(*parent_path_parts(relative)).absolute()
        if credential_location(candidate):
            return {"class": "forbidden", "self_apply": False, "reason": "credential or secret path"}
        result = classify_patch_path(relative, self.auto_apply_prefixes.get(target, ()))
        if result["class"] == "forbidden":
            return result
        sandbox_source = self._is_sandbox_source(target, relative)
        if sandbox_source and result["class"] == "protected" and result.get("reason") not in {"authority path", "authority directory"}:
            return result
        if sandbox_source and result["class"] == "protected":
            result = {"class": self._configured_class(target, relative), "self_apply": True,
                      "reason": "operator-configured sandboxed capability source"}
        # The deployed runtime trust chain is protected by canonical location,
        # including when an operator configures a narrower subtree as a target.
        runtime_dir = Path(__file__).resolve().parent.parent
        protected = (runtime_dir / "security", runtime_dir / "host",
                     runtime_dir / "plugins", runtime_dir / "scripts")
        deployed_registry = runtime_dir / "capabilities"
        # The canonical deployed registry stays immutable unless this exact
        # registry was explicitly selected and the candidate is an exact,
        # validated sandbox source. A configured registry elsewhere must not
        # make the repository's registry patchable.
        in_deployed_registry = candidate == deployed_registry or candidate.is_relative_to(deployed_registry)
        explicit_registry_source = (
            self.registry_root is not None and
            self.registry_root == deployed_registry.resolve(strict=False) and
            sandbox_source
        )
        if in_deployed_registry and not explicit_registry_source:
            return {"class": "protected", "self_apply": False, "reason": "deployed runtime trust chain"}
        if any(candidate == path or candidate.is_relative_to(path) for path in protected):
            return {"class": "protected", "self_apply": False, "reason": "deployed runtime trust chain"}
        if any(candidate == path or candidate.is_relative_to(path) for path in self.protected_paths):
            return {"class": "protected", "self_apply": False, "reason": "configured protected host path"}
        if self.registry_root and candidate.is_relative_to(self.registry_root) and not sandbox_source:
            return {"class": "protected", "self_apply": False, "reason": "operator runtime registry"}
        return result

    @contextmanager
    def locked(self):
        with (self.storage / "broker.lock").open("a+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            yield

    def path(self, identifier):
        if not isinstance(identifier, str) or len(identifier) != 32 or any(c not in "0123456789abcdef" for c in identifier):
            raise ValueError("Invalid patch request_id")
        return self.storage / (identifier + ".json")

    def receipt(self, record):
        with (self.storage / "receipts.jsonl").open("a") as stream:
            json.dump({key: value for key, value in record.items() if key not in ("changes", "preview")}, stream)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())

    def prepare(self, args, workspace):
        root = self.roots.get(args.get("target"))
        if root is None:
            raise PermissionError("Target is not in the host-configured patch roots")
        changes = args.get("changes")
        if not isinstance(changes, list) or not 1 <= len(changes) <= 16:
            raise ValueError("Expected 1..16 exact file changes")
        if len(json.dumps(changes).encode()) > MAX_BYTES:
            raise ValueError("Patch exceeds 64 KiB")
        root_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        preview = []
        seen = set()
        try:
            metadata = os.fstat(root_fd)
            for change in changes:
                if not isinstance(change, dict) or set(change) != {"path", "before_sha256", "content"} or not isinstance(change["content"], str):
                    raise ValueError("Each change needs path, before_sha256 and UTF-8 content only")
                if change["path"] in seen:
                    raise ValueError("Duplicate patch path")
                seen.add(change["path"])
                classification = self.classify(args["target"], change["path"])
                if classification["class"] == "forbidden":
                    raise PermissionError("Credential and secret files cannot be patched")
                fd, name = parent_fd(root_fd, change["path"])
                try:
                    before, data, _ = file_snapshot(fd, name)
                    if before != change["before_sha256"]:
                        raise ValueError("Before hash conflict: " + change["path"])
                    old = data.decode() if data is not None else ""
                    preview.extend(difflib.unified_diff(old.splitlines(True), change["content"].splitlines(True), fromfile=change["path"], tofile=change["path"]))
                finally:
                    os.close(fd)
            if len("".join(preview).encode()) > MAX_BYTES:
                raise ValueError("Review diff exceeds 64 KiB; split this patch")
            now = time.time()
            record = {"request_id": uuid.uuid4().hex, "workspace_id": workspace, "target": args["target"], "root": root, "root_identity": [metadata.st_dev, metadata.st_ino], "changes": changes, "preview": "".join(preview), "created_at": now, "expiry": now + 900, "allowed_operations": ["create", "modify"], "status": "pending"}
            record["patch_sha256"] = request_digest(record)
            with self.locked():
                write_json(self.path(record["request_id"]), record)
                self.receipt(record)
            return record
        finally:
            os.close(root_fd)

    def inspect(self, identifier, workspace=None):
        record = json.loads(self.path(identifier).read_text())
        if record.get("patch_sha256") != request_digest(record):
            raise PermissionError("Stored patch differs from its bound digest")
        if workspace is not None and record["workspace_id"] != workspace:
            raise PermissionError("Patch belongs to a different workspace")
        return record

    def reject_forbidden_changes(self, record):
        """Reclassify every bound path at each authority transition.

        Protected and unknown files can still be changed through an explicit
        exact-digest approval. Credential and secret paths are never eligible,
        even when a request was created before policy changed.
        """
        for change in record["changes"]:
            classification = self.classify(record["target"], change["path"])
            if classification["class"] == "forbidden":
                raise PermissionError("Credential and secret files cannot be patched")

    def approve(self, identifier, expected_digest, authority):
        with self.locked():
            record = self.inspect(identifier)
            if record["status"] != "pending" or time.time() >= record["expiry"] or expected_digest != record["patch_sha256"]:
                raise PermissionError("Patch is not pending, expired, or digest differs")
            record.update(status="approved", granted_by=authority, granted_at=time.time(), capability_id=uuid.uuid4().hex)
            write_json(self.path(identifier), record)
            self.receipt(record)

    @contextmanager
    def target_locked(self, root):
        # Shared across registries; held outside the approved target mount.
        path = Path(root)
        fd = os.open(path.parent / ("." + path.name + ".palette-patch.lock"), os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "a+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            yield

    def apply(self, identifier, workspace):
        root = self.inspect(identifier, workspace)["root"]
        with self.locked(), self.target_locked(root):
            record = self.inspect(identifier, workspace)
            if record["status"] != "approved" or time.time() >= record["expiry"]:
                raise PermissionError("No valid unused approval")
            if self.roots.get(record["target"]) != record["root"]:
                raise PermissionError("Target configuration changed")
            self.reject_forbidden_changes(record)
            if record.get("granted_by", "").startswith("isolated_tested_repair:"):
                # Auto-approval is only valid while every path still has its
                # frozen neutral classification. Protected changes use the
                # ordinary explicit administrator flow.
                if any(not self.classify(record["target"], change["path"]).get("self_apply")
                       for change in record["changes"]):
                    raise PermissionError("Repair auto-apply classification changed before apply")
            # Consume before effects. Process failure cannot make this grant reusable.
            record["status"] = "consumed_completion_unknown"
            write_json(self.path(identifier), record)
            self.receipt(record)
            try:
                proc = subprocess.run([sys.executable, "-B", str(Path(__file__).resolve()), "apply-helper"], input=json.dumps(record), text=True, capture_output=True, timeout=30)
                if proc.returncode != 0:
                    raise RuntimeError(proc.stderr[-2000:])
                result = json.loads(proc.stdout)
            except Exception as exc:
                result = {"status": "consumed_completion_unknown", "error": str(exc)}
            record.update(result)
            write_json(self.path(identifier), record)
            self.receipt(record)
            return {key: value for key, value in record.items() if key not in ("changes", "preview")}

    def call(self, args, workspace, capabilities, source, destination):
        action = args["action"]
        if action == "read":
            root = self.roots.get(args.get("target"))
            if root is None:
                raise PermissionError("Target is not host-configured")
            classification = self.classify(args["target"], args.get("path"))
            if classification["class"] == "forbidden":
                raise PermissionError("Credential and secret files cannot be read")
            root_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                fd, name = parent_fd(root_fd, args.get("path"))
                try:
                    before, data, _ = file_snapshot(fd, name)
                    return {"target": args["target"], "path": args["path"], "sha256": before, "content": data.decode() if data is not None else None}
                finally:
                    os.close(fd)
            finally:
                os.close(root_fd)
        if action == "prepare":
            return self.prepare(args, workspace)
        record = self.inspect(args.get("request_id"), workspace)
        if action == "status":
            return record
        if record["status"] == "pending":
            self.reject_forbidden_changes(record)
            elicitation = capabilities.get("elicitation")
            if not isinstance(elicitation, dict) or elicitation and "form" not in elicitation:
                return {"status": "approval_required", "request_id": record["request_id"], "patch_sha256": record["patch_sha256"], "reason": "Client does not advertise form elicitation. Local administrator must review and approve the exact pending request; Julia and MCP tool arguments cannot grant approval."}
            rid = "approve-" + uuid.uuid4().hex
            request = {"jsonrpc": "2.0", "id": rid, "method": "elicitation/create", "params": {"mode": "form", "message": f"Apply exactly this one-shot patch to {record['root']} for workspace {workspace}? Digest {record['patch_sha256']}. Expires in 15 minutes.\n{record['preview']}", "requestedSchema": {"type": "object", "properties": {"confirm": {"type": "boolean", "title": "Approve this exact patch once"}}, "required": ["confirm"]}}}
            print(json.dumps(request), file=destination, flush=True)
            # Bound the client response. Tool requests during confirmation are refused.
            selector = selectors.DefaultSelector()
            try:
                selector.register(source, selectors.EVENT_READ)
                deadline = time.monotonic() + 120
                while selector.select(timeout=max(0, deadline - time.monotonic())):
                    raw = source.readline()
                    if not raw:
                        break
                    reply = json.loads(raw)
                    if reply.get("id") == rid and "method" not in reply:
                        result = reply.get("result", {})
                        if result.get("action") == "accept" and result.get("content", {}).get("confirm") is True:
                            self.approve(record["request_id"], record["patch_sha256"], "mcp_client_confirmed_user")
                            return self.apply(record["request_id"], workspace)
                        with self.locked():
                            self.receipt({**record, "approval_outcome": "not_approved"})
                        return {"status": "not_approved", "request_id": record["request_id"]}
                    if "id" in reply and "method" in reply:
                        print(json.dumps({"jsonrpc": "2.0", "id": reply["id"], "error": {"code": -32000, "message": "Approval is pending; retry this request afterward"}}), file=destination, flush=True)
                    if time.monotonic() >= deadline:
                        break
            finally:
                selector.close()
            return {"status": "approval_timeout", "request_id": record["request_id"]}
        return self.apply(record["request_id"], workspace)


def main():
    if sys.argv[1:] == ["apply-helper"]:
        try:
            result = apply_files(json.load(sys.stdin))
        except Exception as exc:
            result = {"status": "failed", "files_touched": [], "error": str(exc)}
        print(json.dumps(result))
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--storage", required=True)
    parser.add_argument("action", choices=["review", "approve"])
    parser.add_argument("request_id")
    parser.add_argument("--digest")
    args = parser.parse_args()
    broker = PatchBroker(args.storage, {})
    if args.action == "approve":
        if not args.digest:
            parser.error("Approval requires the reviewed --digest")
        broker.approve(args.request_id, args.digest, "local_administrator")
    print(json.dumps(broker.inspect(args.request_id), indent=2))


if __name__ == "__main__":
    main()
