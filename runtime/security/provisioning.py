#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Load trusted operator capability configuration for Palette sessions.

The capability file is a host-owned JSON object selected by
``PALETTE_CAPABILITY_CEILING``. It contains the ceiling directly. Tool and
model requests never select or modify this file; session owners load it once
and pass the resulting value to the broker, which freezes its own copy.
"""
from __future__ import annotations

import json
import os
import argparse
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from typing import Any


_PACKAGE_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
_PATH_ARGUMENT_OPTIONS = {
    "--executable", "--artifact", "--library", "--library-dir", "--model",
    "--model-path", "--weights", "--weights-file", "--weights-path",
}
_CATEGORIES = {
    "external_fs_write", "fs_digest", "host_command", "host_request",
    "network_access", "package_management", "spawn_child_worker",
}


class OperatorConfigError(ValueError):
    """The trusted operator ceiling is malformed or uses an unknown grant."""


def _string_list(value: Any, where: str, *, absolute: bool = False) -> list[str]:
    if not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value):
        raise OperatorConfigError(f"{where} must be a list of non-empty strings")
    if absolute and any(not Path(item).is_absolute() for item in value):
        raise OperatorConfigError(f"{where} entries must be absolute paths")
    return list(value)


def _validate_host_commands(value: Any) -> None:
    if not isinstance(value, dict):
        raise OperatorConfigError("host_command.commands must be an object")
    for name, spec in value.items():
        if not isinstance(name, str) or not name or not isinstance(spec, dict):
            raise OperatorConfigError("host_command entries must map non-empty names to objects")
        argv = _string_list(spec.get("argv"), f"host_command.commands.{name}.argv")
        if not Path(argv[0]).is_absolute():
            raise OperatorConfigError(f"host_command.commands.{name}.argv[0] must be an absolute executable path")
        cwd = spec.get("cwd")
        if not isinstance(cwd, str) or not Path(cwd).is_absolute():
            raise OperatorConfigError(f"host_command.commands.{name}.cwd must be an absolute path")
        timeout = spec.get("timeout_s", 60)
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not 0 < timeout <= 14400:
            raise OperatorConfigError(f"host_command.commands.{name}.timeout_s must be in (0, 14400]")
        if "extra_args" in spec and not isinstance(spec["extra_args"], bool):
            raise OperatorConfigError(f"host_command.commands.{name}.extra_args must be boolean")


def validate_ceiling(value: Any) -> dict[str, Any]:
    """Validate a capability map and return an independent JSON-shaped copy."""
    if not isinstance(value, dict):
        raise OperatorConfigError("capability ceiling must be a JSON object")
    unknown = set(value) - _CATEGORIES
    if unknown:
        raise OperatorConfigError(f"unknown capability categories: {sorted(unknown)}")

    # A JSON round trip both detaches nested values from a caller and rejects
    # Python-only objects that could not have come from the operator file.
    try:
        ceiling = json.loads(json.dumps(value))
    except (TypeError, ValueError) as exc:
        raise OperatorConfigError(f"capability ceiling is not JSON data: {exc}") from exc

    for category, grant in ceiling.items():
        if not isinstance(grant, dict):
            raise OperatorConfigError(f"{category} grant must be an object")
        if category == "package_management":
            packages = _string_list(grant.get("allowed_packages"), "package_management.allowed_packages")
            invalid = [name for name in packages if not _PACKAGE_NAME.fullmatch(name)]
            if invalid:
                raise OperatorConfigError(f"package_management.allowed_packages has invalid Julia identifiers: {invalid}")
            if len(set(packages)) != len(packages):
                raise OperatorConfigError("package_management.allowed_packages must not contain duplicates")
            if "offline" in grant and not isinstance(grant["offline"], bool):
                raise OperatorConfigError("package_management.offline must be boolean when provided")
        elif category == "host_request":
            types = _string_list(grant.get("allowed_types"), "host_request.allowed_types")
            if len(set(types)) != len(types):
                raise OperatorConfigError("host_request.allowed_types must not contain duplicates")
        elif category == "external_fs_write":
            _string_list(grant.get("allowed_dirs"), "external_fs_write.allowed_dirs", absolute=True)
        elif category == "fs_digest":
            _string_list(grant.get("allowed_paths"), "fs_digest.allowed_paths", absolute=True)
        elif category == "network_access":
            if not isinstance(grant.get("allowed"), bool):
                raise OperatorConfigError("network_access.allowed must be boolean")
            hosts = grant.get("allowed_hosts")
            if hosts is not None:
                _string_list(hosts, "network_access.allowed_hosts")
        elif category == "host_command":
            _validate_host_commands(grant.get("commands"))
        elif category == "spawn_child_worker" and grant:
            raise OperatorConfigError("spawn_child_worker grant must be an empty object")
    return ceiling


def operator_ceiling(config_path: str | Path | None = None) -> dict[str, Any]:
    """Read and validate the host-owned ceiling; unset configuration denies all.

    ``config_path`` is for the trusted session owner and test fixtures. It must
    never be taken from an MCP request or model-controlled payload. The default
    source is ``PALETTE_CAPABILITY_CEILING``; an absent variable grants nothing.
    """
    selected = config_path if config_path is not None else os.environ.get("PALETTE_CAPABILITY_CEILING")
    if selected is None or str(selected).strip() == "":
        parsed = {}
    else:
        path = Path(selected).expanduser()
        try:
            parsed = json.loads(path.read_text(encoding="utf-8"))
        except OSError as exc:
            raise OperatorConfigError(f"cannot read PALETTE_CAPABILITY_CEILING {path}: {exc}") from exc
        except json.JSONDecodeError as exc:
            raise OperatorConfigError(f"invalid JSON in PALETTE_CAPABILITY_CEILING {path}: {exc}") from exc
    if not isinstance(parsed, dict):
        raise OperatorConfigError("capability ceiling must be a JSON object")
    legacy_path = os.environ.get("PALETTE_HOST_COMMANDS")
    if legacy_path:
        try:
            legacy = json.loads(Path(legacy_path).expanduser().read_text(encoding="utf-8"))
        except OSError as exc:
            raise OperatorConfigError(f"cannot read PALETTE_HOST_COMMANDS {legacy_path}: {exc}") from exc
        except json.JSONDecodeError as exc:
            raise OperatorConfigError(f"invalid JSON in PALETTE_HOST_COMMANDS {legacy_path}: {exc}") from exc
        if not isinstance(legacy, dict):
            raise OperatorConfigError("PALETTE_HOST_COMMANDS must contain an object")
        ceiling_commands = parsed.get("host_command", {})
        if not isinstance(ceiling_commands, dict):
            raise OperatorConfigError("host_command grant must be an object")
        commands = ceiling_commands.get("commands", {})
        if not isinstance(commands, dict):
            raise OperatorConfigError("host_command.commands must be an object")
        merged = dict(commands)
        for name, spec in legacy.items():
            if name in merged and merged[name] != spec:
                raise OperatorConfigError(f"PALETTE_HOST_COMMANDS conflicts with host_command.commands entry {name!r}")
            merged[name] = spec
        parsed["host_command"] = {**ceiling_commands, "commands": merged}
    return validate_ceiling(parsed)


def protected_host_paths(
    ceiling: dict[str, Any], *, include_package_depot: bool = True,
    include_runtime_root: bool = True,
) -> list[Path]:
    """Return canonical trusted config/store and host-command paths.

    Config files are protected as exact files. Host-selected binaries,
    artifacts, libraries, and command working roots also protect their parents.
    """
    checked = validate_ceiling(ceiling)
    paths: set[Path] = set()
    for key in (
        "PALETTE_CAPABILITY_CEILING", "PALETTE_HOST_COMMANDS",
        "PALETTE_REPAIR_CONFIG", "PALETTE_PACKAGE_DEPOT", "PALETTE_RUNTIME_ROOT",
    ):
        if ((key == "PALETTE_PACKAGE_DEPOT" and not include_package_depot) or
                (key == "PALETTE_RUNTIME_ROOT" and not include_runtime_root)):
            continue
        configured = os.environ.get(key)
        if configured:
            paths.add(Path(configured).expanduser().resolve(strict=False))
    read_roots = os.environ.get("PALETTE_READ_ROOTS")
    if read_roots:
        from filesystem_layout import parse_read_roots
        paths.update(Path(root).resolve(strict=False) for root in parse_read_roots(read_roots))
    commands = checked.get("host_command", {}).get("commands", {})
    for spec in commands.values():
        argv = spec["argv"]
        candidates = [spec["cwd"], *_configured_path_arguments(argv)]
        for candidate in candidates:
            path = Path(candidate)
            if path.is_absolute():
                resolved = path.resolve(strict=False)
                paths.add(resolved)
                paths.add(resolved.parent)
    return sorted(paths, key=str)


def _configured_path_arguments(argv: list[str]) -> list[str]:
    """Extract artifact paths from frozen host argv, including runner wrappers.

    The generic runner encodes configured backend arguments as
    ``--fixed-arg=<value>``. Unwrap only those operator-owned tokens; runtime
    arguments supplied later through ``extra_args`` are never passed here.
    """
    direct: list[str] = []
    fixed: list[str] = []
    i = 0
    while i < len(argv):
        item = argv[i]
        if item == "--fixed-arg":
            if i + 1 < len(argv):
                fixed.append(argv[i + 1])
                i += 2
                continue
        elif item.startswith("--fixed-arg="):
            fixed.append(item.split("=", 1)[1])
            i += 1
            continue
        direct.append(item)
        i += 1

    result: list[str] = []
    for tokens in (direct, fixed):
        i = 0
        while i < len(tokens):
            item = tokens[i]
            if Path(item).is_absolute():
                result.append(item)
            option, separator, value = item.partition("=")
            # Host-command argv is frozen operator configuration. Any
            # option-value token may name a fixed artifact (`--tokenizer=`,
            # `--config=`, etc.), so path protection must not depend on a
            # model-runner-specific option vocabulary.
            if separator and Path(value).is_absolute():
                result.append(value)
            elif item in _PATH_ARGUMENT_OPTIONS and i + 1 < len(tokens):
                if Path(tokens[i + 1]).is_absolute():
                    result.append(tokens[i + 1])
                i += 1
            i += 1
    return result


def _existing_file(value: str) -> str:
    path = Path(value).expanduser().resolve()
    if not path.is_file():
        raise argparse.ArgumentTypeError(f"File does not exist: {path}")
    return str(path)


def _existing_executable(value: str) -> str:
    path = Path(_existing_file(value))
    if not os.access(path, os.X_OK):
        raise argparse.ArgumentTypeError(f"File is not executable: {path}")
    return str(path)


def _existing_directory(value: str) -> str:
    path = Path(value).expanduser().resolve()
    if not path.is_dir():
        raise argparse.ArgumentTypeError(f"Directory does not exist: {path}")
    return str(path)


def _toolchain_config(args: argparse.Namespace) -> dict[str, Any]:
    argv = [sys.executable, str(Path(__file__).resolve()), "run",
            "--executable", args.executable, "--cwd", args.cwd,
            "--timeout", str(args.timeout)]
    if args.library_dir:
        argv.extend(["--library-dir", args.library_dir])
    for value in args.fixed_arg:
        argv.append(f"--fixed-arg={value}")
    for value in args.artifact:
        argv.extend(["--artifact", value])
    argv.extend(["--input-mode", args.input_mode])
    for value in args.input_prefix:
        argv.append(f"--input-prefix={value}")
    argv.append("--")
    return {args.name: {"argv": argv, "cwd": args.cwd, "timeout_s": args.timeout,
                        "extra_args": args.input_mode == "text"}}


def _run_toolchain(args: argparse.Namespace) -> int:
    forwarded = list(args.arguments)
    if forwarded and forwarded[0] == "--":
        forwarded.pop(0)
    if args.input_mode == "none":
        if forwarded:
            raise SystemExit("this configured toolchain takes no worker-supplied input")
    elif len(forwarded) != 1:
        raise SystemExit("text mode requires exactly one positional input string")
    if any("\0" in item or len(item.encode()) > 4096 for item in forwarded):
        raise SystemExit("input must be at most 4096 bytes without NUL")
    if forwarded and forwarded[0].startswith("-"):
        raise SystemExit("text input may not begin with '-' because that could override configured options")
    command = [args.executable, *args.fixed_arg, *args.artifact,
               *args.input_prefix, *forwarded]
    env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
    if args.library_dir:
        env["LD_LIBRARY_PATH"] = args.library_dir
    # Give the selected process an empty, private home instead of ambient
    # credentials, model settings, package state, or host service config.
    with tempfile.TemporaryDirectory(prefix="palette-toolchain-") as home:
        env["HOME"] = home
        env["TMPDIR"] = home
        try:
            completed = subprocess.run(command, cwd=args.cwd, env=env,
                                       stdin=subprocess.DEVNULL, timeout=args.timeout)
        except subprocess.TimeoutExpired as exc:
            raise SystemExit(f"configured toolchain exceeded its {args.timeout:g}s timeout") from exc
        except OSError as exc:
            raise SystemExit(f"could not start configured toolchain: {exc}") from exc
    return completed.returncode


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="action", required=True)

    config = subparsers.add_parser("config", help="emit one generic fixed toolchain command")
    config.add_argument("--name", required=True)
    config.add_argument("--executable", required=True, type=_existing_executable)
    config.add_argument("--cwd", required=True, type=_existing_directory)
    config.add_argument("--library-dir", type=_existing_directory)
    config.add_argument("--fixed-arg", action="append", default=[])
    config.add_argument("--artifact", action="append", type=_existing_file, default=[])
    config.add_argument("--input-mode", choices=("none", "text"), default="none")
    config.add_argument("--input-prefix", action="append", default=[],
                        help="fixed backend arguments placed before the one text input")
    config.add_argument("--timeout", type=float, default=600)

    run = subparsers.add_parser("run", help=argparse.SUPPRESS)
    run.add_argument("--executable", required=True, type=_existing_executable)
    run.add_argument("--cwd", required=True, type=_existing_directory)
    run.add_argument("--library-dir", type=_existing_directory)
    run.add_argument("--fixed-arg", action="append", default=[])
    run.add_argument("--artifact", action="append", type=_existing_file, default=[])
    run.add_argument("--input-mode", choices=("none", "text"), default="none")
    run.add_argument("--input-prefix", action="append", default=[])
    run.add_argument("--timeout", type=float, default=600)
    run.add_argument("arguments", nargs=argparse.REMAINDER)

    args = parser.parse_args()
    if args.action == "config":
        if not args.name or any(ch in args.name for ch in "\0/\\"):
            parser.error("--name must be a non-empty command name without path separators")
        if not 0 < args.timeout <= 14400:
            parser.error("--timeout must be in (0, 14400]")
        if args.input_mode == "text" and not args.input_prefix:
            parser.error("--input-mode text requires a fixed --input-prefix")
        if args.input_mode == "none" and args.input_prefix:
            parser.error("--input-prefix requires --input-mode text")
        print(json.dumps(_toolchain_config(args), indent=2))
        return 0
    if not 0 < args.timeout <= 14400:
        parser.error("--timeout must be in (0, 14400]")
    return _run_toolchain(args)


if __name__ == "__main__":
    raise SystemExit(main())
