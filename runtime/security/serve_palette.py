#!/usr/bin/env python3
"""Launch Palette's stdio MCP adapter from any working directory."""
import os
from pathlib import Path
import runpy
import sys


def main():
    repo = Path(os.environ.get("PALETTE_REPO", Path(__file__).resolve().parents[2])).resolve()
    host = Path(os.environ.get("PALETTE_HOST", repo / "runtime/host/target/release/palette-host"))
    for required in (repo / "Project.toml", repo / "Manifest.toml", repo / "runtime/security/operator_workspace_router.py", host):
        if not required.is_file():
            raise SystemExit(f"Palette setup incomplete: missing {required}; instantiate Julia and build the host before starting")
    if not os.access(host, os.X_OK):
        raise SystemExit(f"Palette host is not executable: {host}")
    data = Path(os.environ.get("PALETTE_DATA_HOME", Path.home() / ".local/share/palette")).resolve()
    os.environ.setdefault("PALETTE_REPO", str(repo))
    os.environ.setdefault("PALETTE_HOST", str(host))
    os.environ.setdefault("OPERATOR_WORKSPACE", str(data / "workspace"))
    os.environ.setdefault("PALETTE_STATE_DIR", str(data / "state"))
    os.environ.setdefault("PALETTE_WORKSPACE_STATE_ROOT", str(data / "worlds"))
    # A private instance keeps its packages beside its other data instead of
    # sharing the host-wide store, as the installers already arrange.
    os.environ.setdefault("PALETTE_PACKAGE_DEPOT", str(data / "packages"))
    sys.path.insert(0, str(repo / "runtime/security"))
    runpy.run_path(str(repo / "runtime/security/operator_workspace_router.py"), run_name="__main__")


if __name__ == "__main__":
    main()
