#!/usr/bin/env python3
"""Host-owned Palette backend generation registry."""
from __future__ import annotations

import json
import os
from pathlib import Path
import re
import tempfile

ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
FIELDS = {"format", "generation_id", "repo", "adapter", "host"}


class GenerationError(RuntimeError):
    pass


def _atomic_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with tempfile.NamedTemporaryFile("w", dir=path.parent, delete=False) as stream:
        tmp = Path(stream.name)
        json.dump(value, stream, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)


class GenerationStore:
    def __init__(self, root: Path, fallback_repo: Path, fallback_host: str | None = None):
        self.root = Path(root).resolve()
        self.manifests = self.root / "generations"
        self.pointer = self.root / "active-generation.json"
        self.fallback_repo = Path(fallback_repo).resolve()
        self.fallback_host = Path(fallback_host).expanduser().resolve() if fallback_host else None

    @staticmethod
    def _valid(identifier: str) -> str:
        if not isinstance(identifier, str) or not ID_RE.fullmatch(identifier):
            raise ValueError("Invalid generation_id")
        return identifier

    def current_id(self) -> str:
        try:
            raw = json.loads(self.pointer.read_text())
        except FileNotFoundError:
            return "builtin"
        except (OSError, json.JSONDecodeError, TypeError) as exc:
            raise GenerationError("Active generation pointer is malformed") from exc
        if not isinstance(raw, dict) or set(raw) != {"format", "generation_id"} or raw.get("format") != 1:
            raise GenerationError("Active generation pointer is malformed")
        return self._valid(raw["generation_id"])

    def resolve(self, generation_id: str | None = None) -> dict:
        generation_id = self.current_id() if generation_id is None else self._valid(generation_id)
        if generation_id == "builtin":
            repo = self.fallback_repo
            adapter = repo / "runtime/security/operator_mcp.py"
            host = self.fallback_host or repo / "runtime/host/target/release/palette-host"
            if not adapter.is_file():
                raise GenerationError(f"Builtin adapter is missing: {adapter}")
            return {
                "format": 1,
                "generation_id": "builtin",
                "repo": str(repo),
                "adapter": str(adapter),
                "host": str(host),
            }

        manifest_path = self.manifests / generation_id / "generation.json"
        try:
            raw = json.loads(manifest_path.read_text())
        except FileNotFoundError as exc:
            raise GenerationError(f"Unknown generation_id: {generation_id}") from exc
        except (OSError, json.JSONDecodeError, TypeError) as exc:
            raise GenerationError(f"Generation manifest is malformed: {generation_id}") from exc

        if not isinstance(raw, dict) or set(raw) - FIELDS or raw.get("format") != 1 or raw.get("generation_id") != generation_id:
            raise GenerationError(f"Generation manifest is malformed: {generation_id}")

        repo = Path(raw.get("repo", "")).expanduser()
        adapter = Path(raw.get("adapter") or repo / "runtime/security/operator_mcp.py").expanduser()
        host = Path(raw.get("host") or repo / "runtime/host/target/release/palette-host").expanduser()
        if not repo.is_absolute() or not adapter.is_absolute() or not host.is_absolute():
            raise GenerationError("Generation paths must be absolute")
        repo, adapter, host = repo.resolve(), adapter.resolve(), host.resolve()
        if not repo.is_dir() or not adapter.is_file():
            raise GenerationError(f"Generation files are incomplete: {generation_id}")

        return {
            "format": 1,
            "generation_id": generation_id,
            "repo": str(repo),
            "adapter": str(adapter),
            "host": str(host),
        }

    def activate(self, generation_id: str) -> dict:
        generation = self.resolve(generation_id)
        _atomic_json(self.pointer, {"format": 1, "generation_id": generation["generation_id"]})
        return generation

    def available(self) -> list[str]:
        result = ["builtin"]
        if self.manifests.is_dir():
            for entry in self.manifests.iterdir():
                if entry.is_dir() and ID_RE.fullmatch(entry.name) and (entry / "generation.json").is_file():
                    result.append(entry.name)
        return sorted(set(result))
