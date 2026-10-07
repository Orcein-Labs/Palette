#!/usr/bin/env python3
"""Prepare and launch a server-owned Palette runtime over MCP stdio."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys

from filesystem_layout import require_disjoint

FIELDS = {"format", "repo", "data", "depot", "ceiling", "runtime_root", "host_commands", "read_roots", "repair_config", "patch_roots"}


def absolute(value, name):
    if not isinstance(value, str) or not Path(value).is_absolute():
        raise ValueError(f"{name} must be an absolute server path")
    return Path(value).resolve()


def load_profile(path):
    profile_path = Path(path).resolve(strict=True)
    profile = json.loads(profile_path.read_text())
    if not isinstance(profile, dict) or set(profile) - FIELDS or profile.get("format") != 1:
        raise ValueError("Unsupported server profile")
    paths = {key: absolute(profile[key], key) for key in ("repo", "data", "depot", "ceiling", "runtime_root")}
    paths["profile"] = profile_path
    # Configuration, source, packages and saved worlds cannot enter a worker's
    # writable workspace. These checks happen before creating any directories.
    workspace = paths["data"] / "workspace"
    protected = [paths[k] for k in ("repo", "depot", "ceiling", "runtime_root", "profile")]
    require_disjoint([paths["data"]], protected)
    require_disjoint([workspace], [paths["data"] / "state", paths["data"] / "worlds", paths["data"] / "packages"])
    if profile.get("host_commands"):
        paths["host_commands"] = absolute(profile["host_commands"], "host_commands")
        require_disjoint([paths["data"]], [paths["host_commands"]])
    if profile.get("repair_config"):
        paths["repair_config"] = absolute(profile["repair_config"], "repair_config")
        require_disjoint([paths["data"]], [paths["repair_config"]])
    patch_roots = profile.get("patch_roots", {})
    if not isinstance(patch_roots, dict) or any(not isinstance(key, str) or not key for key in patch_roots):
        raise ValueError("patch_roots must map target names to absolute paths")
    paths["patch_roots"] = {key: str(absolute(value, "patch root")) for key, value in patch_roots.items()}
    require_disjoint(paths["patch_roots"].values(), [paths["data"], paths["depot"], paths["profile"], paths["ceiling"]])
    if "repair_config" in paths:
        require_disjoint(paths["patch_roots"].values(), [paths["repair_config"]])
    read_roots = profile.get("read_roots", [])
    if not isinstance(read_roots, list):
        raise ValueError("read_roots must be a list")
    paths["read_roots"] = [absolute(value, "read root") for value in read_roots]
    return paths


def environment(paths):
    # Transport/model credentials are not inherited into a generic runtime.
    env = {key: os.environ[key] for key in ("HOME", "PATH", "LANG", "LC_ALL") if key in os.environ}
    data = paths["data"]
    env.update(PALETTE_REPO=str(paths["repo"]),
               PALETTE_HOST=str(paths["repo"] / "runtime/host/target/release/palette-host"),
               PALETTE_DATA_HOME=str(data), OPERATOR_WORKSPACE=str(data / "workspace"),
               PALETTE_STATE_DIR=str(data / "state"), PALETTE_WORKSPACE_STATE_ROOT=str(data / "worlds"),
               PALETTE_SCRATCH_ROOT=str(data / "scratch"),
               PALETTE_PACKAGE_DEPOT=str(data / "packages"),
               PALETTE_CAPABILITY_CEILING=str(paths["ceiling"]),
               PALETTE_RUNTIME_ROOT=str(paths["runtime_root"]),
               JULIA_DEPOT_PATH=str(paths["depot"]), JULIA_PKG_OFFLINE="true")
    if "host_commands" in paths:
        env["PALETTE_HOST_COMMANDS"] = str(paths["host_commands"])
    if "repair_config" in paths:
        env["PALETTE_REPAIR_CONFIG"] = str(paths["repair_config"])
    if paths["patch_roots"]:
        env["PALETTE_PATCH_ROOTS"] = json.dumps(paths["patch_roots"])
    if paths["read_roots"]:
        env["PALETTE_READ_ROOTS"] = ":".join(map(str, paths["read_roots"]))
    return env


def prepare(paths):
    for binary in ("julia", "cargo", "bwrap"):
        if shutil.which(binary) is None:
            raise RuntimeError(f"Missing server prerequisite: {binary}")
    for key in ("data", "depot", "runtime_root"):
        paths[key].mkdir(parents=True, exist_ok=True, mode=0o700)
    # First-time provisioning is an administrator action, before the sandbox
    # opens. Interactive workers retain offline networking after preparation.
    env = environment(paths)
    env["JULIA_PKG_OFFLINE"] = "false"
    repo = paths["repo"]
    commands = [
        ["julia", "--startup-file=no", f"--project={repo}", "-e", "using Pkg; Pkg.instantiate()"],
        ["cargo", "build", "--release", "--locked", "--manifest-path", str(repo / "runtime/host/Cargo.toml")],
        [sys.executable, str(repo / "runtime/security/prewarm_depot.py"), "--project-dir", str(repo),
         "--host-bin", env["PALETTE_HOST"]],
    ]
    for command in commands:
        subprocess.run(command, cwd=repo, env=env, stdin=subprocess.DEVNULL, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("init", "inspect", "prepare", "launch", "client"))
    parser.add_argument("--profile", required=True, type=Path)
    parser.add_argument("--repo", default="/opt/palette")
    parser.add_argument("--data", default="/var/lib/palette")
    parser.add_argument("--depot", default="/var/cache/palette/depot")
    parser.add_argument("--ceiling", default="/etc/palette/ceiling.json")
    parser.add_argument("--runtime-root", default="/opt/palette-capabilities")
    parser.add_argument("--ssh-target")
    args = parser.parse_args()
    if args.action == "init":
        profile = {"format": 1, "repo": args.repo, "data": args.data, "depot": args.depot,
                   "ceiling": args.ceiling, "runtime_root": args.runtime_root, "read_roots": []}
        args.profile.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        # Never replace an administrator's profile during an update.
        with args.profile.open("x") as stream:
            json.dump(profile, stream, indent=2)
            stream.write("\n")
        args.profile.chmod(0o600)
    paths = load_profile(args.profile)
    if args.action in ("init", "inspect"):
        print(json.dumps({"profile": str(paths["profile"]), "repo": str(paths["repo"]),
                          "workspace": str(paths["data"] / "workspace"),
                          "state": str(paths["data"] / "state"), "compute": "server", "edge_required": False}))
    elif args.action == "prepare":
        prepare(paths)
    elif args.action == "client":
        if not args.ssh_target or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.@-]*", args.ssh_target):
            parser.error("client requires a valid --ssh-target")
        command = ["python3", str(paths["repo"] / "runtime/security/server_runtime.py"),
                   "launch", "--profile", str(paths["profile"])]
        print(json.dumps({"mcpServers": {"palette": {"command": "ssh", "args": ["-T", "--", args.ssh_target,
                          shlex.join(command)]}}}, indent=2))
    else:
        env = environment(paths)
        entrypoint = paths["repo"] / "runtime/security/serve_palette.py"
        os.execve(sys.executable, [sys.executable, str(entrypoint)], env)


if __name__ == "__main__":
    main()
