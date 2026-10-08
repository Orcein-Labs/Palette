#!/usr/bin/env python3
"""Install and control Palette's user-owned systemd services."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
UNIT_DIR = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "systemd/user"


def socket_path():
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if not runtime:
        raise ValueError("XDG_RUNTIME_DIR is required to locate Palette's user runtime socket")
    return Path(os.environ.get("PALETTE_SOCKET", Path(runtime) / "palette/daemon.sock")).expanduser().resolve()


def unit_quote(value):
    """Quote one systemd token, disabling specifier and environment expansion."""
    value = str(value)
    if "\n" in value or "\r" in value or "\0" in value:
        raise ValueError("unit values cannot contain newlines or NUL bytes")
    value = value.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%")
    value = value.replace("$", "\\x24").replace("`", "\\x60")
    return '"' + value + '"'


def unit_path(value):
    """Escape a filesystem path for directives that do not accept quoted words."""
    value = str(value)
    if not value.startswith("/") or "\n" in value or "\r" in value or "\0" in value:
        raise ValueError("systemd file paths must be absolute and contain no newlines or NUL bytes")
    escaped = (chr(92), chr(34), "#", "%", ";")
    return "".join("\\x%02x" % ord(char) if char.isspace() or char in escaped else char for char in value)


def operator_env_file(path):
    path = Path(path).expanduser().resolve()
    stat = path.stat()
    if stat.st_uid != os.getuid() or stat.st_mode & 0o777 != 0o600:
        raise ValueError("operator environment file must be owned by this user with mode 0600")
    reserved = {"PALETTE_REPO", "PALETTE_HOST", "PALETTE_SOCKET", "OPERATOR_WORKSPACE"}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key = line.removeprefix("export ").partition("=")[0].strip()
        if key in reserved:
            raise ValueError(f"operator environment file cannot override service identity: {key}")
    return path


def render_palette_unit(repo=ROOT, workspace=None, sock=None, python=sys.executable, env_file=None):
    repo, sock = Path(repo).resolve(), Path(sock or socket_path()).resolve()
    if workspace is None:
        raise ValueError("--workspace-dir is required because the daemon needs its default workspace")
    router = repo / "runtime/security/operator_workspace_router.py"
    if not router.is_file():
        raise ValueError(f"missing Palette daemon entrypoint: {router}")
    host = repo / "runtime/host/target/release/palette-host"
    if not host.is_file() or not os.access(host, os.X_OK):
        raise ValueError(f"missing or non-executable Palette host: {host}")
    values = {"PALETTE_REPO": repo, "PALETTE_HOST": host, "PALETTE_SOCKET": sock}
    values["OPERATOR_WORKSPACE"] = Path(workspace).expanduser().resolve()
    env_lines = "EnvironmentFile=" + unit_path(operator_env_file(env_file)) + "\n" if env_file else ""
    env_lines += "\n".join("Environment=" + key + "=" + unit_quote(value) for key, value in values.items())
    command = " ".join(unit_quote(value) for value in (python, router, "--socket", sock))
    return f"""[Unit]
Description=Palette Julia operator daemon
After=default.target

[Service]
Type=simple
ExecStart={command}
Restart=on-failure
RestartSec=2s
RuntimeDirectory=palette
RuntimeDirectoryMode=0700
UMask=0077
PrivateTmp=yes
NoNewPrivileges=yes
{env_lines}
UnsetEnvironment=CONTROL_PLANE_API_KEY OPENAI_API_KEY OPENAI_ADMIN_KEY PALETTE_HOST_COMMANDS
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
"""


def render_tunnel_unit(client, profile, env_file=None, profile_dir=None, sock=None):
    client = Path(client).expanduser().resolve()
    if not client.is_file() or not os.access(client, os.X_OK):
        raise ValueError(f"tunnel client must be an existing executable: {client}")
    if not profile or "\n" in profile:
        raise ValueError("an existing tunnel profile name is required")
    profile_args = []
    if profile_dir:
        profile_dir = Path(profile_dir).expanduser().resolve()
        if not profile_dir.is_dir() or not any((profile_dir / (profile + suffix)).is_file() for suffix in (".yaml", ".yml")):
            raise ValueError(f"tunnel profile must exist in the configured profile directory: {profile_dir}")
        profile_args = ["--profile-dir", profile_dir]
    env_line = ""
    if env_file:
        env_line = "EnvironmentFile=" + unit_path(operator_env_file(env_file)) + "\n"
    if sock is not None:
        env_line += "Environment=PALETTE_SOCKET=" + unit_quote(Path(sock).expanduser().resolve()) + "\n"
    command = " ".join(unit_quote(value) for value in (client, "run", "--profile", profile, *profile_args))
    return f"""[Unit]
Description=Palette operator-owned Chat tunnel client
After=palette.service
Wants=palette.service

[Service]
Type=simple
ExecStart={command}
Restart=always
RestartSec=2s
UMask=0077
PrivateTmp=yes
NoNewPrivileges=yes
{env_line}StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
"""


def atomic_write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, name = tempfile.mkstemp(prefix="." + path.name, dir=path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w") as stream:
            stream.write(content)
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


def systemctl(action, unit):
    binary = os.environ.get("PALETTE_SYSTEMCTL") or shutil.which("systemctl")
    if not binary:
        raise RuntimeError("systemctl is not available")
    command = [binary, "--user", action]
    if unit:
        command.append(unit)
    return subprocess.run(command, check=True, text=True)


def status(unit, sock=None):
    binary = os.environ.get("PALETTE_SYSTEMCTL") or shutil.which("systemctl")
    if not binary:
        raise RuntimeError("systemctl is not available")
    result = subprocess.run([binary, "--user", "show", unit, "--no-page", "--property=ActiveState,SubState,MainPID,ExecMainStartTimestamp"], check=True, text=True, capture_output=True)
    fields = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
    pid = fields.get("MainPID", "0")
    identity = None
    if unit == "palette.service" and pid.isdigit() and int(pid) > 0:
        try:
            identity = Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1].split()[19]
        except (OSError, IndexError):
            identity = None
    print(json.dumps({"unit": unit, **fields, "process_start_ticks": identity,
                      "socket": str(sock or socket_path()) if unit == "palette.service" else None}))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("install", "start", "stop", "restart", "status"))
    parser.add_argument("--repo-dir", default=str(ROOT))
    parser.add_argument("--workspace-dir")
    parser.add_argument("--socket", default=os.environ.get("PALETTE_SOCKET"))
    parser.add_argument("--env-file", help="existing operator-owned 0600 daemon configuration file")
    parser.add_argument("--unit", choices=("palette.service", "palette-tunnel.service"), default="palette.service",
                        help="service to control for start, stop, restart, and status")
    parser.add_argument("--tunnel-client", help="existing tunnel-client executable; enables separate tunnel unit generation")
    parser.add_argument("--tunnel-profile", help="operator's existing tunnel profile")
    parser.add_argument("--tunnel-profile-dir", help="directory containing the existing tunnel profile")
    parser.add_argument("--tunnel-env-file", help="existing operator-owned 0600 credentials file")
    args = parser.parse_args(argv)
    try:
        if args.action == "install":
            sock = Path(args.socket).expanduser().resolve() if args.socket else socket_path()
            palette = UNIT_DIR / "palette.service"
            atomic_write(palette, render_palette_unit(args.repo_dir, args.workspace_dir, sock, env_file=args.env_file))
            if args.tunnel_client or args.tunnel_profile or args.tunnel_env_file:
                if not args.tunnel_client or not args.tunnel_profile:
                    parser.error("--tunnel-client and --tunnel-profile must be supplied together")
                atomic_write(UNIT_DIR / "palette-tunnel.service", render_tunnel_unit(
                    args.tunnel_client, args.tunnel_profile, args.tunnel_env_file,
                    profile_dir=args.tunnel_profile_dir, sock=sock))
            systemctl("daemon-reload", None)
            print(json.dumps({"installed": [str(palette)], "socket": str(sock), "tunnel_unit": str(UNIT_DIR / "palette-tunnel.service") if args.tunnel_client else None}))
        else:
            unit = args.unit
            if args.action == "status":
                status(unit, Path(args.socket).expanduser().resolve() if args.socket else None)
            else:
                systemctl(args.action, unit)
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as exc:
        parser.error(str(exc))


if __name__ == "__main__":
    main()
