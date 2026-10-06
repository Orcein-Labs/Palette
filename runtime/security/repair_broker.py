#!/usr/bin/env python3
"""Tested repairs with a disposable, zero-authority verification worker.

The model may suggest file contents, but the operator supplies patch roots,
ordinary path classes, and fixed test recipes. Candidate code runs only in a
Bubblewrap sandbox with networking disabled, immutable candidate source and
private temporary storage.
"""
from __future__ import annotations

import hashlib
import hmac
import json
import os
from concurrent.futures import Future
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import threading
import time
import uuid

from patch_broker import AUTO_APPLY_CLASSES, classify_patch_path, digest, file_snapshot, parent_fd, parent_path_parts
from filesystem_layout import require_disjoint

MAX_FILES = 16
MAX_TREE_BYTES = 256 * 1024 * 1024
MAX_TEST_OUTPUT = 8192
TEST_TIMEOUT = 180


def canonical_changes(changes):
    return digest(json.dumps(changes, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode())


def canonical_recipe(recipe):
    return digest(json.dumps(recipe, sort_keys=True, separators=(",", ":")).encode())


def normalize_recipe(recipe):
    if not isinstance(recipe, dict) or set(recipe) - {"id", "argv", "read_roots", "julia_depots"} or not {"id", "argv"} <= set(recipe):
        raise ValueError("Each host test recipe needs id, argv and optional read_roots/julia_depots")
    if not isinstance(recipe["id"], str) or not recipe["id"] or len(recipe["id"]) > 128:
        raise ValueError("Invalid trusted test recipe id")
    argv = recipe["argv"]
    if not isinstance(argv, (tuple, list)) or not argv or len(argv) > 64 or any(not isinstance(item, str) or "\x00" in item for item in argv):
        raise ValueError("Trusted test recipe argv must be a fixed nonempty string list")
    roots = {}
    for field in ("read_roots", "julia_depots"):
        values = recipe.get(field, [])
        if not isinstance(values, list) or len(values) > 16 or any(not isinstance(value, str) or not Path(value).is_absolute() for value in values):
            raise ValueError(f"{field} must contain bounded absolute host directories")
        paths = [Path(value).resolve(strict=True) for value in values]
        if any(not path.is_dir() for path in paths):
            raise ValueError(f"{field} must contain directories")
        roots[field] = list(dict.fromkeys(map(str, paths)))
    if set(roots["julia_depots"]) - set(roots["read_roots"]):
        raise ValueError("Julia depots must be explicitly included in read_roots")
    return {"id": recipe["id"], "argv": list(argv), **roots}


def copy_stage(source, destination, classify=classify_patch_path):
    """Copy regular files only; symlink or special-file trees fail closed."""
    total = 0
    count = 0
    source = Path(source)
    destination = Path(destination)
    for base, dirs, files in os.walk(source, followlinks=False):
        relative = Path(base).relative_to(source)
        dirs[:] = [name for name in dirs if name not in (".git", ".codex", "__pycache__", ".julia")]
        dst_dir = destination / relative
        dst_dir.mkdir(parents=True, exist_ok=True)
        for name in list(dirs):
            mode = (Path(base) / name).lstat().st_mode
            if not os.path.isdir(Path(base) / name) or os.path.islink(Path(base) / name):
                raise ValueError("Repair test trees cannot contain symlinked directories")
            (dst_dir / name).mkdir(exist_ok=True)
        for name in files:
            src = Path(base) / name
            metadata = src.lstat()
            if not os.path.isfile(src) or os.path.islink(src):
                raise ValueError("Repair test trees can contain regular files only")
            total += metadata.st_size
            count += 1
            if total > MAX_TREE_BYTES or count > 20000:
                raise ValueError("Repair test source exceeds the staged tree limit")
            relative_file = (relative / name).as_posix()
            if classify(relative_file)["class"] == "forbidden":
                continue
            target = dst_dir / name
            shutil.copyfile(src, target, follow_symlinks=False)
            # Verification runs with the stage mounted read-only. Keep the
            # host's private copy writable so read-only deployed sources can
            # still receive a proposed candidate before that mount is made.
            os.chmod(target, (metadata.st_mode & 0o777) | stat.S_IWUSR)
    return total


def write_candidate(stage, changes):
    root_fd = os.open(stage, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for change in changes:
            fd, name = parent_fd(root_fd, change["path"])
            try:
                current, _, mode = file_snapshot(fd, name)
                if current != change["before_sha256"]:
                    raise ValueError("Staged source differs from proposed before hash: " + change["path"])
                flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW
                file_fd = os.open(name, flags, mode or 0o644, dir_fd=fd)
                with os.fdopen(file_fd, "wb") as stream:
                    stream.write(change["content"].encode())
            finally:
                os.close(fd)
    finally:
        os.close(root_fd)


def run_isolated_test(stage, recipe):
    bwrap = shutil.which("bwrap")
    if not bwrap:
        raise RuntimeError("Repair testing requires Bubblewrap; no unsandboxed fallback is available")
    stage = str(Path(stage).resolve(strict=True))
    argv = [
        bwrap, "--unshare-user", "--uid", "65534", "--gid", "65534",
        "--disable-userns", "--unshare-net", "--unshare-pid", "--unshare-ipc",
        "--cap-drop", "ALL", "--die-with-parent", "--new-session",
        "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
        "--ro-bind", "/usr", "/usr",
    ]
    for host_path in ("/lib", "/lib64", "/bin", "/etc/ld.so.cache"):
        if os.path.lexists(host_path):
            argv += ["--ro-bind-try", host_path, host_path]
    for root in recipe["read_roots"]:
        argv += ["--ro-bind", root, root]
    argv += [
        # Candidate and verification files are immutable during execution.
        # Any test scratch must live under the worker's private /tmp.
        "--ro-bind", stage, stage,
        "--clearenv", "--setenv", "PATH", "/usr/local/bin:/usr/bin:/bin",
        "--setenv", "HOME", "/tmp", "--setenv", "TMPDIR", "/tmp",
        "--setenv", "PYTHONDONTWRITEBYTECODE", "1", "--setenv", "LANG", "C.UTF-8",
        "--setenv", "JULIA_PKG_OFFLINE", "true", "--setenv", "JULIA_DEPOT_PATH", ":".join(["/tmp/julia-depot", *recipe["julia_depots"]]),
        "--chdir", stage, "--", *recipe["argv"],
    ]
    started = time.monotonic()
    try:
        proc = subprocess.run(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                              stderr=subprocess.STDOUT, text=True, timeout=TEST_TIMEOUT,
                              check=False, env={})
        output = proc.stdout[-MAX_TEST_OUTPUT:]
        return {"status": "passed" if proc.returncode == 0 else "failed",
                "exit_code": proc.returncode, "output": output,
                "duration_seconds": round(time.monotonic() - started, 3)}
    except subprocess.TimeoutExpired as exc:
        output = exc.stdout or b""
        if isinstance(output, bytes):
            output = output.decode("utf-8", "replace")
        return {"status": "failed", "exit_code": None,
                "output": output[-MAX_TEST_OUTPUT:] + "\n[test timed out]",
                "duration_seconds": round(time.monotonic() - started, 3)}


class RepairBroker:
    """Host-side repair workflow. All authority inputs are constructor-owned."""

    def __init__(self, patch_broker, test_recipes, auto_apply_prefixes=None, protected_paths=()):
        self.patches = patch_broker
        if protected_paths:
            self.patches.protected_paths = tuple(dict.fromkeys((*self.patches.protected_paths,
                *(Path(path).resolve(strict=False) for path in protected_paths))))
        self.storage = self.patches.storage / "repairs"
        self.storage.mkdir(parents=True, exist_ok=True, mode=0o700)
        with self.patches.locked():
            self.signing_key = self._load_signing_key()
        self.test_recipes = {target: normalize_recipe(recipe) for target, recipe in test_recipes.items()}
        if set(self.test_recipes) - set(self.patches.roots):
            raise ValueError("Repair test recipes require configured patch targets")
        for target, recipe in self.test_recipes.items():
            require_disjoint(recipe["read_roots"], [self.patches.roots[target], self.patches.storage])
            for root in recipe["read_roots"]:
                if Path(root) in (Path("/"), Path("/home"), Path.home(), Path("/etc"), Path("/proc"), Path("/dev")):
                    raise PermissionError("Repair read roots must name narrow toolchain or dependency directories")
        self.jobs = {}
        self.jobs_lock = threading.Lock()
        if auto_apply_prefixes:
            # Bind trusted class configuration once; model arguments cannot alter it.
            for target, entries in auto_apply_prefixes.items():
                if target not in self.patches.roots:
                    raise ValueError("Auto-apply classes require a configured patch target")
                existing = self.patches.auto_apply_prefixes.get(target, ())
                normalized = []
                for entry in entries:
                    if not isinstance(entry, (tuple, list)) or len(entry) != 2:
                        raise ValueError("Each auto-apply prefix must be a path-class/prefix pair")
                    class_name, prefix = entry
                    if class_name not in AUTO_APPLY_CLASSES:
                        raise ValueError("Unknown auto-apply path class")
                    parent_path_parts(prefix)
                    normalized.append((class_name, prefix))
                normalized = tuple(normalized)
                existing = tuple(tuple(entry) for entry in existing)
                if existing and normalized != existing:
                    raise ValueError("Repair and patch broker path classes must agree")
                if not existing:
                    self.patches.auto_apply_prefixes[target] = normalized

    def record_path(self, repair_id):
        if not isinstance(repair_id, str) or len(repair_id) != 32 or any(char not in "0123456789abcdef" for char in repair_id):
            raise ValueError("Invalid repair_id")
        return self.storage / (repair_id + ".json")

    def _load_signing_key(self):
        path = self.storage / ".repair-signing-key"
        try:
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        except FileExistsError:
            metadata = path.stat(follow_symlinks=False)
            if not path.is_file() or metadata.st_nlink != 1 or metadata.st_mode & 0o077:
                raise PermissionError("Repair signing key must be a private, unlinked regular file")
            key = path.read_bytes()
            if len(key) != 32:
                raise PermissionError("Repair signing key has an invalid length")
            return key
        else:
            key = os.urandom(32)
            with os.fdopen(fd, "wb") as stream:
                stream.write(key)
                stream.flush()
                os.fsync(stream.fileno())
            return key

    def _signature(self, record):
        payload = {key: value for key, value in record.items() if key != "record_hmac"}
        return hmac.new(self.signing_key, json.dumps(payload, sort_keys=True, separators=(",", ":")).encode(), hashlib.sha256).hexdigest()

    def read(self, repair_id, workspace=None):
        record = json.loads(self.record_path(repair_id).read_text())
        if not hmac.compare_digest(self._signature(record), record.get("record_hmac", "")):
            raise PermissionError("Stored repair differs from its bound digest")
        if canonical_changes(record.get("changes")) != record.get("changes_sha256"):
            raise PermissionError("Stored repair contents differ from the tested change digest")
        if workspace is not None and record["workspace_id"] != workspace:
            raise PermissionError("Repair belongs to a different workspace")
        return record

    def store(self, record):
        record["record_hmac"] = self._signature(record)
        temp = self.record_path(record["repair_id"]).with_suffix(".tmp")
        temp.write_text(json.dumps(record, sort_keys=True))
        os.chmod(temp, 0o600)
        os.replace(temp, self.record_path(record["repair_id"]))

    def classify_changes(self, target, changes):
        return [{"path": change["path"], **self.patches.classify(target, change["path"])} for change in changes]

    def detect(self, args):
        target = args.get("target")
        root = self.patches.roots.get(target)
        paths = args.get("paths")
        if root is None or not isinstance(paths, list) or not 1 <= len(paths) <= MAX_FILES:
            raise ValueError("detect requires a configured target and 1..16 paths")
        root_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            found = []
            for relative in paths:
                fd, name = parent_fd(root_fd, relative)
                try:
                    before, data, _ = file_snapshot(fd, name)
                    found.append({"path": relative, "sha256": before,
                                  "bytes": len(data) if data is not None else 0,
                                  **self.patches.classify(target, relative)})
                finally:
                    os.close(fd)
            return {"status": "detected", "target": target, "files": found}
        finally:
            os.close(root_fd)

    def propose(self, args, workspace):
        target = args.get("target")
        if target not in self.test_recipes:
            raise PermissionError("No operator-configured fixed test recipe for target")
        changes = args.get("changes")
        if not isinstance(changes, list) or not 1 <= len(changes) <= MAX_FILES:
            raise ValueError("propose requires 1..16 exact file changes")
        # The ordinary patch broker provides descriptor-relative path checks,
        # before hashes, bounded diffs, receipts, expiry, and approval storage.
        patch = self.patches.prepare({"target": target, "changes": changes}, workspace)
        repair_id = uuid.uuid4().hex
        record = {
            "repair_id": repair_id, "workspace_id": workspace, "target": target,
            "patch_request_id": patch["request_id"], "patch_sha256": patch["patch_sha256"],
            "changes_sha256": canonical_changes(changes), "changes": changes,
            "recipe_id": self.test_recipes[target]["id"],
            "recipe_sha256": canonical_recipe(self.test_recipes[target]), "created_at": time.time(),
            "classifications": self.classify_changes(target, changes),
            "test": {"status": "not_run"}, "status": "proposed",
        }
        self.store(record)
        return {key: record[key] for key in ("status", "repair_id", "patch_request_id", "patch_sha256", "changes_sha256", "classifications")}

    def test(self, repair_id, workspace):
        record = self.read(repair_id, workspace)
        patch_record = self.patches.inspect(record["patch_request_id"], workspace)
        if (patch_record["patch_sha256"] != record["patch_sha256"] or
                canonical_changes(patch_record["changes"]) != record["changes_sha256"]):
            raise PermissionError("Test candidate differs from its pending patch request")
        recipe = self.test_recipes.get(record["target"])
        if (recipe is None or recipe["id"] != record["recipe_id"] or
                canonical_recipe(recipe) != record["recipe_sha256"]):
            raise PermissionError("Trusted test recipe changed since proposal")
        source = self.patches.roots[record["target"]]
        with tempfile.TemporaryDirectory(prefix="palette-repair-") as directory:
            stage = Path(directory) / "source"
            stage.mkdir(mode=0o700)
            copy_stage(source, stage, lambda relative: self.patches.classify(record["target"], relative))
            write_candidate(stage, record["changes"])
            result = run_isolated_test(stage, recipe)
        record["test"] = {**result, "recipe_id": recipe["id"], "tested_changes_sha256": record["changes_sha256"], "tested_at": time.time()}
        record["status"] = "tested" if result["status"] == "passed" else "test_failed"
        self.store(record)
        return {"status": record["status"], "repair_id": repair_id, "patch_sha256": record["patch_sha256"], "test": record["test"]}

    def apply(self, repair_id, workspace, capabilities=None, source=None, destination=None):
        record = self.read(repair_id, workspace)
        if (record["status"] != "tested" or record["test"].get("status") != "passed" or
                record["test"].get("tested_changes_sha256") != record["changes_sha256"]):
            raise PermissionError("Only the exact successfully tested proposal can be applied")
        recipe = self.test_recipes.get(record["target"])
        if (recipe is None or recipe["id"] != record["recipe_id"] or
                canonical_recipe(recipe) != record["recipe_sha256"]):
            raise PermissionError("Trusted test recipe changed since proposal")
        patch_record = self.patches.inspect(record["patch_request_id"], workspace)
        if (patch_record["patch_sha256"] != record["patch_sha256"] or
                patch_record["status"] not in ("pending", "approved") or
                canonical_changes(patch_record["changes"]) != record["changes_sha256"]):
            raise PermissionError("Tested patch is no longer the pending exact patch")
        classifications = self.classify_changes(record["target"], record["changes"])
        if classifications != record["classifications"]:
            raise PermissionError("Operator path classification changed since proposal")
        if any(row["class"] == "forbidden" for row in classifications):
            raise PermissionError("Credential and secret files cannot be patched")
        if all(row["self_apply"] for row in classifications):
            if patch_record["status"] == "pending":
                self.patches.approve(record["patch_request_id"], record["patch_sha256"], "isolated_tested_repair:" + record["test"]["recipe_id"])
            outcome = self.patches.apply(record["patch_request_id"], workspace)
        else:
            outcome = self.patches.call({"action": "apply", "request_id": record["patch_request_id"]}, workspace, capabilities or {}, source, destination)
        record["apply"] = outcome
        # Keep a successful test live while the trusted operator decides. A
        # later call may consume that exact externally approved request.
        record["status"] = "tested" if outcome.get("status") in ("approval_required", "approval_timeout", "not_approved") else outcome.get("status", "approval_pending")
        self.store(record)
        return {"repair_id": repair_id, "patch_sha256": record["patch_sha256"], "status": record["status"], "apply": outcome}

    def start_test(self, repair_id, workspace):
        with self.jobs_lock:
            if repair_id in self.jobs and not self.jobs[repair_id].done():
                raise PermissionError("Repair verification is already running")
            record = self.read(repair_id, workspace)
            if record["status"] not in ("proposed", "test_failed", "tested"):
                raise PermissionError("Repair is not available for verification")
            record["status"] = "testing"
            self.store(record)
            future = Future()
            self.jobs[repair_id] = future

        def verify():
            try:
                future.set_result(self.test(repair_id, workspace))
            except Exception as exc:
                failed = self.read(repair_id, workspace)
                failed.update(status="test_failed", test={"status": "failed", "error": str(exc)})
                self.store(failed)
                future.set_exception(exc)

        threading.Thread(target=verify, daemon=True, name="palette-repair-" + repair_id).start()
        return {"status": "testing", "repair_id": repair_id}

    def wait_test(self, repair_id, workspace):
        self.read(repair_id, workspace)
        return self.jobs[repair_id].result(timeout=TEST_TIMEOUT + 30)

    def call(self, args, workspace, capabilities=None, source=None, destination=None):
        action = args.get("action")
        if action == "detect":
            return self.detect(args)
        if action == "classify":
            changes = args.get("changes")
            target = args.get("target")
            if not isinstance(changes, list) or target not in self.patches.roots:
                raise ValueError("classify requires a configured target and changes")
            return {"status": "classified", "files": self.classify_changes(target, changes)}
        if action == "propose":
            return self.propose(args, workspace)
        repair_id = args.get("repair_id")
        if action == "test":
            return self.start_test(repair_id, workspace)
        if action == "apply":
            return self.apply(repair_id, workspace, capabilities, source, destination)
        if action == "status":
            record = self.read(repair_id, workspace)
            if record["status"] == "testing" and repair_id not in self.jobs:
                record["status"] = "interrupted_completion_unknown"
                self.store(record)
            return {key: value for key, value in record.items() if key != "changes"}
        raise ValueError("Unknown repair action")
