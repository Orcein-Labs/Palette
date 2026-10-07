#!/usr/bin/env python3
"""Wire a local Palette plugin directly to the repository-owned MCP consumer."""
import argparse
import json
import os
from pathlib import Path
import sys
import tempfile
import shutil
import shlex
from filesystem_layout import require_disjoint

ROOT = Path(__file__).resolve().parents[2]


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as file:
        temporary = Path(file.name)
        json.dump(value, file, indent=2)
        file.write("\n")
    try:
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plugin-dir", required=True)
    parser.add_argument("--workspace-dir", required=True)
    parser.add_argument("--repo-dir", default=str(ROOT))
    parser.add_argument("--format", choices=("codex", "portable"), default="codex")
    parser.add_argument("--bin-dir", help="Optional directory for a palette-mcp executable on PATH")
    args = parser.parse_args()
    repo, plugin = Path(args.repo_dir).resolve(), Path(args.plugin_dir).resolve()
    require_disjoint([args.workspace_dir], [repo, plugin] + ([args.bin_dir] if args.bin_dir else []))
    host = repo / "runtime/host/target/release/palette-host"
    client = repo / "runtime/security/palette_mcp_client.py"
    for path in (repo / "Project.toml", repo / "Manifest.toml", client, host):
        if not path.is_file():
            parser.error(f"missing {path}; instantiate the local project and build the Rust host before installing")
    if not os.access(host, os.X_OK):
        parser.error(f"host is not executable: {host}")
    manifest_path = plugin / ".codex-plugin/plugin.json"
    manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {
        "name": "palette", "version": "0.3.0", "description": "Persistent Julia Palette with offline preparation and existing state revival.",
        "interface": {"displayName": "Palette", "shortDescription": "Persistent local Julia workbench"}}
    if manifest.get("name") != "palette" or (args.format == "codex" and (plugin / "plugin.json").exists()):
        parser.error("target must be a Palette plugin using the Codex compatibility manifest")
    config_path = plugin / ".mcp.json"
    config = json.loads(config_path.read_text()) if config_path.exists() else {"mcpServers": {}}
    if set(config.get("mcpServers", {})) - {"palette"}:
        parser.error("target contains other servers; use a dedicated Palette plugin")
    entry = config.setdefault("mcpServers", {}).setdefault("palette", {})
    runtime_dir = os.environ.get("XDG_RUNTIME_DIR")
    default_socket = Path(runtime_dir) / "palette/daemon.sock" if runtime_dir else None
    socket = Path(os.environ.get("PALETTE_SOCKET", default_socket)).expanduser().resolve() if os.environ.get("PALETTE_SOCKET") or default_socket else None
    client_env = {**entry.get("env", {}), "PALETTE_REPO": str(repo), "PALETTE_HOST": str(host),
                  "OPERATOR_WORKSPACE": str(Path(args.workspace_dir).resolve()), "PALETTE_SERVICE": "palette.service"}
    if socket:
        client_env["PALETTE_SOCKET"] = str(socket)
    entry.update(command=sys.executable, args=[str(client)],
        env=client_env)
    entry.setdefault("startup_timeout_sec", 200)
    entry.setdefault("tool_timeout_sec", 210)
    manifest["mcpServers"] = "./.mcp.json"
    if args.format == "portable":
        manifest_path = plugin / "plugin.json"
        portable = json.loads(manifest_path.read_text()) if manifest_path.exists() else json.loads((repo / "runtime/plugins/palette/plugin.json").read_text())
        if portable.get("name") != "palette":
            parser.error("target must be a Palette portable plugin")
        config_path = plugin / "mcp.json"
        existing = json.loads(config_path.read_text()) if config_path.exists() else {}
        if set(existing.get("mcpServers", {})) - {"palette"}:
            parser.error("target contains other portable servers")
        prior = existing.get("mcpServers", {}).get("palette", {})
        entry["env"] = {**prior.get("env", {}), **entry["env"]}
        entry.pop("startup_timeout_sec", None)
        entry.pop("tool_timeout_sec", None)
        config = {**existing, "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
                  "mcpServers": {**existing.get("mcpServers", {}), "palette": {"type": "stdio", **entry}}}
        manifest = portable
        assets = plugin / "assets"
        assets.mkdir(parents=True, exist_ok=True)
        icon = repo / "runtime/plugins/palette/assets/palette.svg"
        if icon.resolve() != (assets / "palette.svg").resolve():
            shutil.copyfile(icon, assets / "palette.svg")
    write_json(config_path, config)
    write_json(manifest_path, manifest)
    if args.bin_dir:
        bindir = Path(args.bin_dir).resolve()
        bindir.mkdir(parents=True, exist_ok=True)
        launcher = bindir / "palette-mcp"
        if launcher.exists():
            old = launcher.read_text()
            if not old.startswith("#!/bin/sh\n# Palette-managed launcher\n"):
                parser.error("refusing to overwrite an unmanaged palette-mcp executable")
        launcher.write_text("#!/bin/sh\n# Palette-managed launcher\nexec " + shlex.quote(sys.executable) + " " + shlex.quote(str(client)) + ' "$@"\n')
        launcher.chmod(0o755)
    print(json.dumps({"plugin": str(plugin), "repo": str(repo), "workspace": entry["env"]["OPERATOR_WORKSPACE"],
        "preparation": "checked on first operator start", "state": "existing state directory wired by operator",
        "socket": str(socket) if socket else "${XDG_RUNTIME_DIR}/palette/daemon.sock", "service": "palette.service"}))


if __name__ == "__main__":
    main()
