#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Configure and run local VibeThinker-3B through Palette's host-command broker."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import sys


def existing_file(value):
    path = Path(value).expanduser().resolve()
    if not path.is_file():
        raise argparse.ArgumentTypeError(f"File does not exist: {path}")
    return str(path)


def bounded_integer(low, high):
    def parse(value):
        number = int(value)
        if not low <= number <= high:
            raise argparse.ArgumentTypeError(f"Expected an integer in [{low}, {high}]")
        return number
    return parse


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("config", "run"))
    parser.add_argument("--llama-cli", required=True, type=existing_file)
    parser.add_argument("--model", required=True, type=existing_file)
    parser.add_argument("--library-dir", type=lambda p: str(Path(p).expanduser().resolve()))
    parser.add_argument("--threads", type=bounded_integer(1, 64), default=4)
    parser.add_argument("--tokens", type=bounded_integer(1, 2048), default=512)
    parser.add_argument("--context", type=bounded_integer(512, 32768), default=4096)
    parser.add_argument("--gpu-layers", type=bounded_integer(0, 99), default=0)
    parser.add_argument("--timeout", type=bounded_integer(1, 14400), default=180)
    parser.add_argument("prompt", nargs="?")
    args = parser.parse_args()
    if not os.access(args.llama_cli, os.X_OK):
        parser.error("--llama-cli must be executable")
    if args.library_dir and not Path(args.library_dir).is_dir():
        parser.error("--library-dir must be a directory")
    if args.action == "config":
        if args.prompt is not None:
            parser.error("config does not take a prompt")
        argv = [str(Path(sys.executable).resolve()), str(Path(__file__).resolve()),
                "run", "--llama-cli", args.llama_cli, "--model", args.model]
        for name in ("threads", "tokens", "context", "gpu_layers", "timeout"):
            argv += ["--" + name.replace("_", "-"), str(getattr(args, name))]
        if args.library_dir:
            argv += ["--library-dir", args.library_dir]
        # Caller-supplied argv is exclusively one positional prompt.
        argv.append("--")
        print(json.dumps({"vibethinker": {"argv": argv,
            "cwd": str(Path(__file__).resolve().parents[2]),
            "timeout_s": args.timeout, "extra_args": True}}, indent=2))
        return
    if args.prompt is None or not args.prompt.strip():
        parser.error("run requires one non-empty prompt after --")
    if len(args.prompt.encode()) > 4096 or "\0" in args.prompt:
        parser.error("prompt must be at most 4096 bytes without NUL")
    prompt = ("<|im_start|>system\nYou are a helpful assistant.<|im_end|>\n"
              f"<|im_start|>user\n{args.prompt}<|im_end|>\n<|im_start|>assistant\n")
    command = [args.llama_cli, "--model", args.model, "--threads", str(args.threads),
               "--ctx-size", str(args.context), "--n-predict", str(args.tokens),
               "--gpu-layers", str(args.gpu_layers), "--no-conversation", "--single-turn",
               "--no-display-prompt", "--simple-io", "--no-escape", "--prompt", prompt]
    env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
    if args.library_dir:
        env["LD_LIBRARY_PATH"] = args.library_dir
    # Replace this process so broker deadlines and teardown own inference too.
    os.execve(args.llama_cli, command, env)


if __name__ == "__main__":
    main()
