"""Host-side index of sandboxed Julia capability source.

The registry reports content and authority facts and returns source text to the
Julia worker for execution. It never imports or calls capability code.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tomllib
from typing import Any

from provisioning import _configured_path_arguments


CAPABILITY_ID = re.compile(r"^[A-Za-z][A-Za-z0-9_.-]{0,63}$")
JULIA_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
CAPABILITY_FIELDS = {"id", "version", "source", "entrypoint", "dependencies", "requires", "description"}
FORBIDDEN_FIELDS = {"ceiling", "credentials", "credential", "callback", "callbacks", "host_callback", "host_callbacks", "host_command", "argv", "secret"}
AUTHORITY_CATEGORIES = {"external_fs_write", "fs_digest", "host_command", "network_access", "package_management", "spawn_child_worker"}


class RuntimeRegistryError(ValueError):
    """A capability registry entry is invalid or outside its trusted root."""


def _json_digest(value: Any) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()).hexdigest()


def _strings(value: Any, field: str, pattern: re.Pattern[str] | None = None) -> list[str]:
    if not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value):
        raise RuntimeRegistryError(f"{field} must be a list of non-empty strings")
    result = list(value)
    if pattern and any(not pattern.fullmatch(item) for item in result):
        raise RuntimeRegistryError(f"{field} contains an invalid identifier")
    if len(set(result)) != len(result):
        raise RuntimeRegistryError(f"{field} must not contain duplicates")
    return result


def _reject_forbidden(value: Any, where: str = "manifest") -> None:
    if isinstance(value, dict):
        for key, child in value.items():
            if isinstance(key, str) and key.lower() in FORBIDDEN_FIELDS:
                raise RuntimeRegistryError(f"{where} contains forbidden field {key!r}")
            _reject_forbidden(child, where)
    elif isinstance(value, list):
        for child in value:
            _reject_forbidden(child, where)


class RuntimeRegistry:
    """A workspace-local active snapshot over a trusted deployment directory."""

    def __init__(self, root: str | Path, ceiling: dict[str, Any], *, workspace: str | Path, state_dir: str | Path,
                 repo: str | Path | None = None, host_binary: str | Path | None = None):
        self.root = Path(root).expanduser().resolve()
        self.workspace = Path(workspace).expanduser().resolve()
        self.state_dir = Path(state_dir).expanduser().resolve()
        self.repo = Path(repo).expanduser().resolve() if repo else None
        self.host_binary = Path(host_binary).expanduser().resolve() if host_binary else None
        if self.root == self.workspace or self.root in self.workspace.parents or self.workspace in self.root.parents:
            raise RuntimeRegistryError("runtime root must be disjoint from the workspace")
        if self.root == self.state_dir or self.root in self.state_dir.parents or self.state_dir in self.root.parents:
            raise RuntimeRegistryError("runtime root must be disjoint from session state")
        self.ceiling = json.loads(json.dumps(ceiling))
        self.adapter_identity = self._adapter_identity()
        self.active, self.active_generation = self._load()
        self.staged: tuple[dict[str, dict[str, Any]], str] | None = None
        self.runtime_identity = self._identity(self.active)

    def _adapter_identity(self) -> dict[str, str]:
        observed: dict[str, str] = {}
        root = self.repo / "runtime/security" if self.repo else None
        if root and root.is_dir():
            for path in sorted(root.glob("*.py")):
                if path.name.startswith("test_"):
                    continue
                observed[str(path.relative_to(self.repo))] = hashlib.sha256(path.read_bytes()).hexdigest()
        return observed

    def _identity(self, entries: dict[str, dict[str, Any]]) -> dict[str, str]:
        content = {name: {"manifest": entry["manifest"], "source_sha256": entry["source_sha256"]}
                   for name, entry in sorted(entries.items())}
        protected: dict[str, str] = {}
        if self.repo:
            for root, pattern in ((self.repo / "src", "*.jl"), (self.repo / "runtime/host/src", "*.rs")):
                if root.is_dir():
                    for path in sorted(root.rglob(pattern)):
                        protected[str(path.relative_to(self.repo))] = hashlib.sha256(path.read_bytes()).hexdigest()
            project = self.repo / "Project.toml"
            if project.is_file():
                protected["Project.toml"] = hashlib.sha256(project.read_bytes()).hexdigest()
        if self.host_binary and self.host_binary.is_file():
            protected["palette-host"] = hashlib.sha256(self.host_binary.read_bytes()).hexdigest()
        version = "unknown"
        if self.repo:
            try:
                version = str(tomllib.loads((self.repo / "Project.toml").read_text()).get("version", "unknown"))
            except (OSError, ValueError):
                pass
        adapter_digest = _json_digest(self.adapter_identity)
        return {"version": version, "content_sha256": _json_digest({"runtime": protected,
                "adapter_content_sha256": adapter_digest}), "adapter_content_sha256": adapter_digest}

    def _load(self) -> tuple[dict[str, dict[str, Any]], str]:
        entries: dict[str, dict[str, Any]] = {}
        if not self.root.exists():
            return entries, _json_digest(entries)
        if not self.root.is_dir():
            raise RuntimeRegistryError("runtime root is not a directory")
        for path in sorted(self.root.glob("*/capability.json")):
            manifest_path = path.resolve(strict=True)
            if self.root not in manifest_path.parents:
                raise RuntimeRegistryError("capability manifest escapes runtime root")
            try:
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError) as exc:
                raise RuntimeRegistryError(f"cannot read capability manifest {path.parent.name}: {exc}") from exc
            if not isinstance(manifest, dict):
                raise RuntimeRegistryError(f"capability manifest {path} must be an object")
            _reject_forbidden(manifest)
            unknown = set(manifest) - CAPABILITY_FIELDS
            missing = {"id", "version", "source", "entrypoint"} - set(manifest)
            if unknown or missing:
                raise RuntimeRegistryError(f"capability manifest fields invalid (unknown={sorted(unknown)}, missing={sorted(missing)})")
            identifier = manifest["id"]
            if not isinstance(identifier, str) or not CAPABILITY_ID.fullmatch(identifier) or identifier != path.parent.name:
                raise RuntimeRegistryError("capability id must match its directory name")
            if identifier in entries:
                raise RuntimeRegistryError(f"duplicate capability id {identifier!r}")
            if not isinstance(manifest["version"], str) or not manifest["version"]:
                raise RuntimeRegistryError(f"capability {identifier!r} version must be non-empty")
            entrypoint = manifest["entrypoint"]
            if not isinstance(entrypoint, str) or not JULIA_NAME.fullmatch(entrypoint):
                raise RuntimeRegistryError(f"capability {identifier!r} entrypoint must be a Julia identifier")
            source_name = manifest["source"]
            if not isinstance(source_name, str) or not source_name or Path(source_name).is_absolute():
                raise RuntimeRegistryError(f"capability {identifier!r} source must be a relative path")
            source_path = (manifest_path.parent / source_name).resolve(strict=True)
            if manifest_path.parent.resolve() not in source_path.parents or not source_path.is_file() or source_path.suffix != ".jl":
                raise RuntimeRegistryError(f"capability {identifier!r} source must be a Julia file inside its directory")
            dependencies = _strings(manifest.get("dependencies", []), f"{identifier}.dependencies", JULIA_NAME)
            requires = _strings(manifest.get("requires", []), f"{identifier}.requires")
            if any(not (item in AUTHORITY_CATEGORIES or
                        item.startswith("host_request:") and bool(item.removeprefix("host_request:")))
                   for item in requires):
                raise RuntimeRegistryError(f"capability {identifier!r} declares an unknown required capability")
            authorized = all(self._grants(requirement) for requirement in requires)
            description = manifest.get("description", "")
            if not isinstance(description, str):
                raise RuntimeRegistryError(f"capability {identifier!r} description must be a string")
            manifest_copy = dict(manifest)
            manifest_copy["dependencies"] = dependencies
            manifest_copy["requires"] = requires
            manifest_copy["description"] = description
            source = source_path.read_text(encoding="utf-8")
            entries[identifier] = {"manifest": manifest_copy, "source": source,
                                   "source_sha256": hashlib.sha256(source.encode()).hexdigest(),
                                   "authorized": authorized}
        generation = _json_digest({name: {"manifest": entry["manifest"], "source_sha256": entry["source_sha256"]}
                                   for name, entry in sorted(entries.items())})
        return entries, generation

    def _grants(self, requirement: str) -> bool:
        if requirement.startswith("host_request:"):
            host_type = requirement.removeprefix("host_request:")
            return bool(host_type and host_type in self.ceiling.get("host_request", {}).get("allowed_types", []))
        grant = self.ceiling.get(requirement)
        if requirement == "network_access":
            return isinstance(grant, dict) and grant.get("allowed") is True
        if requirement == "package_management":
            return isinstance(grant, dict) and bool(grant.get("allowed_packages"))
        if requirement == "host_command":
            return isinstance(grant, dict) and bool(grant.get("commands"))
        if requirement == "external_fs_write":
            return isinstance(grant, dict) and bool(grant.get("allowed_dirs"))
        if requirement == "fs_digest":
            return isinstance(grant, dict) and bool(grant.get("allowed_paths"))
        return isinstance(grant, dict) and requirement in AUTHORITY_CATEGORIES

    def _available(self) -> tuple[dict[str, dict[str, Any]], str]:
        return self._load()

    def _toolchains(self) -> list[dict[str, str]]:
        installed: dict[str, dict[str, str]] = {}
        commands = self.ceiling.get("host_command", {}).get("commands", {})
        for name, spec in commands.items():
            argv = spec.get("argv", [])
            cwd = spec.get("cwd")
            executable = Path(argv[0]) if argv else None
            artifacts = [Path(item) for item in _configured_path_arguments(argv)]
            ready = bool(executable and executable.is_file() and os.access(executable, os.X_OK) and
                         isinstance(cwd, str) and Path(cwd).is_dir() and
                         all(path.exists() for path in artifacts))
            installed[name] = {"status": "ready" if ready else "missing", "source": "operator_host_command"}
        native = {"julia": os.environ.get("PALETTE_JULIA") or shutil.which("julia"),
                  "python": sys.executable, "rust": shutil.which("cargo")}
        for name, executable in native.items():
            ready = bool(executable and Path(executable).is_file() and os.access(executable, os.X_OK))
            installed.setdefault(name, {"status": "ready" if ready else "missing", "source": "runtime_host"})
        return [{"name": name, **installed[name]} for name in sorted(installed)]

    @staticmethod
    def _summary(entries: dict[str, dict[str, Any]], *, active: dict[str, dict[str, Any]]) -> list[dict[str, Any]]:
        output = []
        for identifier, entry in sorted(entries.items()):
            manifest = entry["manifest"]
            active_entry = active.get(identifier)
            state = "active" if active_entry and active_entry["source_sha256"] == entry["source_sha256"] and active_entry["manifest"] == manifest else (
                "available_update" if active_entry else "available")
            output.append({"id": identifier, "version": manifest["version"],
                           "content_sha256": entry["source_sha256"], "description": manifest["description"],
                           "dependencies": manifest["dependencies"], "required_capabilities": manifest["requires"],
                           "authorized": entry["authorized"],
                           "state": state})
        return output

    def discover(self, epoch: str | None) -> dict[str, Any]:
        available, available_generation = self._available()
        return {"epoch": epoch, "runtime": dict(self.runtime_identity),
                "capability_generation": self.active_generation,
                "available_generation": available_generation,
                "installed_toolchains": self._toolchains(),
                "capabilities": self._summary(self.active, active=self.active),
                "available_capabilities": self._summary(available, active=self.active),
                "authorized_capabilities": sorted(name for name, entry in available.items() if entry["authorized"]),
                "unauthorized_capabilities": sorted(name for name, entry in available.items() if not entry["authorized"]),
                "authorized_ceiling": self._authorized_summary(),
                "dependencies_by_capability": {name: entry["manifest"]["dependencies"] for name, entry in available.items()}}

    def _authorized_summary(self) -> dict[str, Any]:
        summary: dict[str, Any] = {}
        for name, grant in self.ceiling.items():
            if name == "host_request":
                summary[name] = {"allowed_types": sorted(grant.get("allowed_types", []))}
            elif name == "package_management":
                summary[name] = {"allowed_packages": sorted(grant.get("allowed_packages", [])),
                                 "offline": grant.get("offline", False)}
            elif name == "host_command":
                summary[name] = {"command_names": sorted(grant.get("commands", {}))}
            elif name in ("external_fs_write", "fs_digest"):
                key = "allowed_dirs" if name == "external_fs_write" else "allowed_paths"
                summary[name] = {"configured_path_count": len(grant.get(key, []))}
            elif name == "network_access":
                summary[name] = {"allowed": grant.get("allowed") is True,
                                 "configured_host_count": len(grant.get("allowed_hosts") or [])}
            elif name == "spawn_child_worker":
                summary[name] = {"enabled": True}
        return summary

    def invoke(self, payload: dict[str, Any]) -> dict[str, Any]:
        identifier = payload.get("id")
        if not isinstance(identifier, str) or identifier not in self.active:
            raise RuntimeRegistryError("unknown capability in active generation")
        entry = self.active[identifier]
        if not entry["authorized"]:
            raise PermissionError(f"capability {identifier!r} requires authority outside the operator ceiling")
        return {"id": identifier, "generation": self.active_generation,
                "manifest": entry["manifest"], "source": entry["source"],
                "source_sha256": entry["source_sha256"]}

    def prepare_refresh(self, epoch: str | None) -> dict[str, Any]:
        previous = self.active_generation
        entries, generation = self._load()
        self.staged = (entries, generation)
        return {"previous_generation": previous, "capability_generation": generation,
                "available_generation": generation, "changed": previous != generation,
                "epoch_before_refresh": epoch}

    def commit_refresh(self) -> dict[str, Any]:
        if self.staged is None:
            raise RuntimeRegistryError("no staged runtime generation is ready")
        previous = self.active_generation
        self.active, self.active_generation = self.staged
        self.staged = None
        self.runtime_identity = self._identity(self.active)
        return {"previous_generation": previous, "capability_generation": self.active_generation,
                "changed": previous != self.active_generation}

    def cancel_refresh(self) -> None:
        self.staged = None

    def handle(self, action: str, payload: dict[str, Any], *, epoch: str | None = None) -> dict[str, Any]:
        if action == "discover":
            return self.discover(epoch)
        if action == "invoke":
            return self.invoke(payload)
        if action == "refresh":
            return self.prepare_refresh(epoch)
        raise RuntimeRegistryError(f"unknown runtime action {action!r}")
