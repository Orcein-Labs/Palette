"""Host side of the Time Machine (src/time_machine.jl): list checkpoints
without a kernel, and ask the next kernel to restore one."""
import json
import os
from pathlib import Path
import re

from generation_runtime import _atomic_json

CHECKPOINT_NAME = re.compile(r"[A-Za-z0-9._-]{1,64}")   # as CHECKPOINT_NAME in src/time_machine.jl
KEPT_ACROSS_FRESH_WORLDS = ("checkpoints", "blobs")


def list_checkpoints(state_dir):
    """The world record and every complete checkpoint, oldest first."""
    state_dir = Path(state_dir)
    world = None
    try:
        world = json.loads((state_dir / "world.json").read_text())
    except (OSError, ValueError):
        pass
    records = []
    root = state_dir / "checkpoints"
    if root.is_dir():
        for entry in root.iterdir():
            try:
                r = json.loads((entry / "checkpoint.json").read_text())
            except (OSError, ValueError):
                continue
            records.append({k: r.get(k) for k in ("name", "call", "created", "note", "world")})
    records.sort(key=lambda r: (r["created"] or "", r["name"] or ""))
    lineage = None if world is None else {"id": world.get("id"), "parent": world.get("parent"), "created": world.get("created")}
    return {"world": lineage, "checkpoints": records}


def request_restore(state_dir, checkpoint, fork=False):
    """Writes restore.json, which the next kernel applies before revival."""
    if not isinstance(checkpoint, str) or not CHECKPOINT_NAME.fullmatch(checkpoint):
        raise ValueError("checkpoint must be a checkpoint name (1-64 letters, digits, '.', '_' or '-')")
    if not isinstance(fork, bool):
        raise ValueError("fork must be a boolean")
    state_dir = Path(state_dir)
    if not (state_dir / "checkpoints" / checkpoint / "checkpoint.json").is_file():
        names = [r["name"] for r in list_checkpoints(state_dir)["checkpoints"]]
        raise ValueError(f"no checkpoint is named {checkpoint!r}; checkpoints: {', '.join(names) or 'none'}")
    _atomic_json(state_dir / "restore.json", {"checkpoint": checkpoint, "fork": fork})


def carry_checkpoints(retired, state_dir):
    """A fresh world keeps the checkpoints of the one it replaced."""
    retired, state_dir = Path(retired), Path(state_dir)
    for name in KEPT_ACROSS_FRESH_WORLDS:
        if (retired / name).is_dir():
            state_dir.mkdir(parents=True, exist_ok=True)
            os.replace(retired / name, state_dir / name)
