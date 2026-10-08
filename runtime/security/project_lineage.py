#!/usr/bin/env python3
"""Canonical durable state identity for Palette project-scoped workspaces.

Project scope is a continuity boundary, not a caller/workspace-name boundary.
All aliases selecting the same host-configured project_root_id converge on one
state directory. Thread/open workspaces keep their independent state dirs.

Migration is intentionally fail-closed:
- a legacy state held by another router is never moved;
- two non-empty legacy project states are never merged implicitly;
- registry paths outside the old or canonical layouts are rejected.
"""
from __future__ import annotations

import fcntl
import os
from pathlib import Path
import re

ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")


class LineageError(RuntimeError):
    pass


class LineageBusy(LineageError):
    pass


class LineageConflict(LineageError):
    pass


def _valid(identifier: str, field: str) -> str:
    if not isinstance(identifier, str) or not ID_RE.fullmatch(identifier):
        raise ValueError(f"Invalid {field}")
    return identifier


def lineage_id(scope: str, project_root_id: str, workspace_id: str) -> str:
    """Stable logical identity exposed to clients and kernels."""
    if scope == "project":
        return "project:" + _valid(project_root_id, "project_root_id")
    if scope in {"thread", "open"}:
        return "workspace:" + _valid(workspace_id, "workspace_id")
    raise ValueError("Invalid workspace scope")


def project_state_dir(root: Path, project_root_id: str) -> Path:
    root = Path(root).resolve()
    return root / "lineages" / _valid(project_root_id, "project_root_id")


def workspace_state_dir(root: Path, workspace_id: str) -> Path:
    root = Path(root).resolve()
    return root / "states" / _valid(workspace_id, "workspace_id")


def desired_state_dir(root: Path, entry: dict) -> Path:
    if entry["scope"] == "project":
        return project_state_dir(root, entry["project_root_id"])
    return workspace_state_dir(root, entry["workspace_id"])


def owner_lock_path(state_dir: Path) -> Path:
    state = Path(state_dir).resolve()
    return state.with_name(state.name + ".owner.lock")


def _is_nonempty_dir(path: Path) -> bool:
    if not path.exists():
        return False
    if path.is_symlink() or not path.is_dir():
        raise PermissionError(f"Palette state path must be a real directory: {path}")
    return next(path.iterdir(), None) is not None


def _acquire_migration_lock(state_dir: Path):
    """Acquire the legacy owner's exact lock inode before moving its state."""
    lock_path = owner_lock_path(state_dir)
    lock_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    stream = lock_path.open("a+")
    try:
        fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as exc:
        stream.close()
        raise LineageBusy(
            f"Project lineage migration is blocked by an active legacy owner: {state_dir}"
        ) from exc
    return stream


def ensure_project_lineage(registry: dict, root: Path, project_root_id: str) -> tuple[Path, str]:
    """Migrate/normalize every project alias for one configured project root."""
    root = Path(root).resolve()
    project_root_id = _valid(project_root_id, "project_root_id")
    target = project_state_dir(root, project_root_id)
    lid = lineage_id("project", project_root_id, project_root_id)

    aliases: list[tuple[str, dict, Path]] = []
    for identifier, raw in registry.get("workspaces", {}).items():
        if raw.get("scope") != "project" or raw.get("project_root_id", "default") != project_root_id:
            continue
        identifier = _valid(identifier, "workspace_id")
        legacy = workspace_state_dir(root, identifier)
        recorded = Path(raw.get("state_dir", "")).resolve()
        if recorded not in {legacy, target}:
            raise PermissionError(
                f"Workspace registry state path differs from the canonical project lineage: {recorded}"
            )
        aliases.append((identifier, raw, legacy))

    legacy_nonempty: list[Path] = []
    for _, _, legacy in aliases:
        if legacy != target and _is_nonempty_dir(legacy):
            legacy_nonempty.append(legacy)

    target_nonempty = _is_nonempty_dir(target)
    if target_nonempty and legacy_nonempty:
        raise LineageConflict(
            f"Canonical project lineage and legacy workspace state are both non-empty for {project_root_id}"
        )
    if len(legacy_nonempty) > 1:
        raise LineageConflict(
            f"Multiple non-empty legacy project states exist for {project_root_id}; refusing to choose"
        )

    lock = None
    source = legacy_nonempty[0] if legacy_nonempty else None
    try:
        if source is not None:
            lock = _acquire_migration_lock(source)
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            if target.exists():
                if target.is_symlink() or not target.is_dir():
                    raise PermissionError(f"Palette lineage path must be a real directory: {target}")
                if next(target.iterdir(), None) is not None:
                    raise LineageConflict(f"Project lineage became non-empty during migration: {target}")
                target.rmdir()
            os.replace(source, target)
        else:
            target.mkdir(parents=True, exist_ok=True, mode=0o700)

        for _, entry, legacy in aliases:
            entry["lineage_id"] = lid
            entry["state_dir"] = str(target)
            if legacy != target and legacy.exists() and not _is_nonempty_dir(legacy):
                legacy.rmdir()
        return target, lid
    finally:
        if lock is not None:
            fcntl.flock(lock, fcntl.LOCK_UN)
            lock.close()


def normalize_entry(registry: dict, root: Path, entry: dict) -> dict:
    """Return an entry with canonical lineage identity/state."""
    value = dict(entry)
    value.setdefault("project_root_id", "default")
    scope = value.get("scope")
    identifier = _valid(value.get("workspace_id"), "workspace_id")
    expected_lid = lineage_id(scope, value["project_root_id"], identifier)
    recorded_lid = value.get("lineage_id")
    if recorded_lid is not None and recorded_lid != expected_lid:
        raise PermissionError("Workspace registry lineage_id differs from the configured layout")

    if scope == "project":
        state, expected_lid = ensure_project_lineage(
            registry, root, value["project_root_id"]
        )
    else:
        state = workspace_state_dir(root, identifier)
        recorded = Path(value.get("state_dir", "")).resolve()
        if recorded != state:
            raise PermissionError("Workspace registry state path differs from the configured layout")

    value["lineage_id"] = expected_lid
    value["state_dir"] = str(state)
    return value
