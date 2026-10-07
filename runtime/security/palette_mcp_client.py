#!/usr/bin/env python3
"""Disposable stdio MCP client for the persistent Palette Unix socket daemon."""
from __future__ import annotations

import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time


def socket_path() -> Path:
    explicit = os.environ.get("PALETTE_SOCKET")
    if explicit:
        return Path(explicit).expanduser()
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if not runtime:
        raise RuntimeError("PALETTE_SOCKET is unset and XDG_RUNTIME_DIR is unavailable")
    return Path(runtime) / "palette" / "daemon.sock"


def verify_peer_uid(client: socket.socket) -> None:
    if hasattr(socket, "SO_PEERCRED"):
        _, uid, _ = struct.unpack("3i", client.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
    elif hasattr(client, "getpeereid"):
        uid, _ = client.getpeereid()
    else:
        raise RuntimeError("This platform cannot verify the Palette daemon's Unix socket credentials")
    if uid != os.getuid():
        raise PermissionError("Palette socket is served by a different user")


def connect(path: Path) -> socket.socket:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        client.connect(str(path))
        verify_peer_uid(client)
    except Exception:
        client.close()
        raise
    return client


def start_service(path: Path) -> socket.socket:
    try:
        return connect(path)
    except OSError as first:
        systemctl = shutil.which("systemctl")
        if not systemctl:
            raise RuntimeError(f"Palette daemon is unavailable at {path}; systemctl --user is unavailable") from first
        result = subprocess.run([systemctl, "--user", "start", "palette.service"],
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True, timeout=30)
        if result.returncode:
            detail = result.stderr.strip() or result.stdout.strip() or f"exit {result.returncode}"
            raise RuntimeError(f"Palette daemon is unavailable and systemd could not start palette.service: {detail}") from first
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                return connect(path)
            except OSError:
                time.sleep(0.05)
        raise RuntimeError(f"palette.service started but Palette did not accept connections at {path}") from first


def copy_stdin(sock: socket.socket) -> None:
    try:
        while True:
            data = sys.stdin.buffer.read1(65536)
            if not data:
                break
            sock.sendall(data)
    except (BrokenPipeError, OSError):
        pass
    finally:
        try:
            sock.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def copy_stdout(sock: socket.socket) -> None:
    try:
        while True:
            data = sock.recv(65536)
            if not data:
                return
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
    except (BrokenPipeError, OSError):
        return


def main() -> int:
    try:
        path = socket_path()
        sock = start_service(path)
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as exc:
        print(f"Palette MCP client: {exc}", file=sys.stderr)
        return 1
    with sock:
        output = threading.Thread(target=copy_stdout, args=(sock,), daemon=True)
        output.start()
        copy_stdin(sock)
        output.join()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
