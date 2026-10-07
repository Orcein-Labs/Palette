#!/usr/bin/env python3
"""
Precompile every Julia stdlib and project dependency into the real depot,
compatible with Palette.

A worker loads Palette before any turn runs, and Palette's cache pins the
stdlib caches it was built against (Logging, Dates, ...) from the depot
itself rather than the ones Julia ships. Every shipped stdlib cache that
depends on a different copy is then rejected, and so is a project package
built outside this environment, so the first `using Test` in
a session took ~30s and `using Pkg` ~70-80s -- redone in every session,
since each worker writes to a throwaway depot clone. Loading everything once
here, in the worker's own sandbox with the real depot writable, leaves caches
in the real depot that every later clone inherits. It has to be the sandbox:
a cache records the absolute source paths it was built from, and a prewarm
that could see paths the worker cannot built caches every worker rejected.

Run it where sessions will run, with the same mounts; a harness that places
the Julia runtime or this repo at other paths must run it there.

Run once per depot, and again after Palette or the project changes:
    python3 runtime/security/prewarm_depot.py --project-dir <project>
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from launch_worker import build_bwrap_argv, default_depot, package_store_paths, resolve_real_julia_binary  # noqa: E402

PREWARM = Path(__file__).with_name("prewarm_workload.jl").read_text()


def preparation_identity(project_dir: str, repo_dir: str, julia_bin: str) -> dict:
    project, repo = Path(project_dir).absolute(), Path(repo_dir).absolute()
    files = {str(p): hashlib.sha256(p.read_bytes()).hexdigest()
             for p in sorted(set([*repo.joinpath("src").rglob("*.jl"),
                                  *project.joinpath("src").rglob("*.jl"),
                                  *repo.joinpath("runtime", "scripts").glob("*.jl"),
                                  *project.glob("*Project.toml"), *project.glob("*Manifest.toml")]))}
    package_root = Path(os.environ.get("PALETTE_PACKAGE_DEPOT", str(Path.home() / ".palette/packages"))).expanduser().resolve()
    package_environment = package_root / "environment"
    optional_files = {str(p): hashlib.sha256(p.read_bytes()).hexdigest()
                      for p in sorted([*package_environment.glob("*Project.toml"), *package_environment.glob("*Manifest.toml")])}
    version = subprocess.run([julia_bin, "--startup-file=no", "--version"],
                             stdin=subprocess.DEVNULL, capture_output=True, text=True, check=True).stdout.strip()
    return {"project": str(project), "repo": str(repo), "source": files,
            "workload": hashlib.sha256(PREWARM.encode()).hexdigest(),
            "runtime": {"binary": julia_bin, "version": version, "machine": os.uname().machine},
            "task_environment": (hashlib.sha256(Path(os.environ["PALETTE_TASK_ENV"]).read_bytes()).hexdigest()
                                 if os.environ.get("PALETTE_TASK_ENV") else None),
            "package_environment": {"path": str(package_environment), "files": optional_files},
            "depot": default_depot()}


def preparation_path(identity: dict, state_dir: str | None = None) -> Path:
    key = hashlib.sha256(json.dumps([identity["project"], identity["repo"], identity["depot"]]).encode()).hexdigest()
    return Path(state_dir or str(Path(identity["depot"]) / ".palette-preparation")) / (key + ".json")


def preparation_status(identity: dict, state_dir: str | None = None) -> dict:
    path = preparation_path(identity, state_dir)
    try:
        record = json.loads(path.read_text())
    except FileNotFoundError:
        return {"state": "cold", "identity": identity}
    except (ValueError, OSError) as exc:
        return {"state": "failed", "error": f"preparation record unreadable: {exc}", "identity": identity}
    if not isinstance(record, dict):
        return {"state": "failed", "error": "preparation record is not an object", "identity": identity}
    if record.get("schema") != "palette.preparation.v1":
        return {"state": "failed", "error": "unknown preparation record format", "identity": identity}
    if record.get("state") not in ("preparing", "prepared", "failed") or not isinstance(record.get("cache_files", {}), dict):
        return {"state": "failed", "error": "invalid preparation state or cache inventory", "identity": identity}
    if record.get("identity") != identity:
        return {**record, "state": "stale", "current_identity": identity}
    if record.get("state") == "prepared" and any(not Path(p).is_file() or
            hashlib.sha256(Path(p).read_bytes()).hexdigest() != digest
            for p, digest in record.get("cache_files", {}).items()):
        return {**record, "state": "stale", "error": "prepared cache missing or changed"}
    if record.get("state") == "preparing":
        try:
            os.kill(record["pid"], 0)
            if process_start(record["pid"]) != record.get("process_start"):
                return {**record, "state": "failed", "error": "preparation process identity changed"}
        except (ProcessLookupError, FileNotFoundError, KeyError, TypeError):
            return {**record, "state": "failed", "error": "preparation process disappeared"}
    if record.get("state") == "prepared" and record.get("returncode") == 0:
        record.setdefault("stderr", record.get("error") or "")
        record["error"] = None
    return record


def process_start(pid: int) -> str:
    # PID reuse must not make an interrupted preparation look alive.
    stat = Path(f"/proc/{pid}/stat").read_text()
    return stat[stat.rfind(")") + 2:].split()[19]


def prepare(project_dir: str, repo_dir: str, *, state_dir: str | None = None,
            host_bin: str | None = None, force: bool = False) -> dict:
    with tempfile.TemporaryDirectory(prefix="palette-prewarm-") as workspace:
        return _prepare_in_workspace(project_dir, repo_dir, workspace, state_dir=state_dir,
                                     host_bin=host_bin, force=force)


def _prepare_in_workspace(project_dir: str, repo_dir: str, workspace: str, *, state_dir: str | None = None,
                          host_bin: str | None = None, force: bool = False) -> dict:
    julia_bin = resolve_real_julia_binary()
    identity = preparation_identity(project_dir, repo_dir, julia_bin)
    path = preparation_path(identity, state_dir)
    package_root, package_depot, package_environment = package_store_paths(
        protected_paths=[identity["project"], identity["repo"], state_dir or "", path.parent, workspace])
    path.parent.mkdir(parents=True, exist_ok=True)
    def save(record):
        with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as out:
            json.dump(record, out)
        os.replace(out.name, path)
    with open(str(path) + ".lock", "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        previous = preparation_status(identity, state_dir)
        if previous["state"] == "prepared" and not force:
            return {**previous, "reused": True}
        record = {"schema": "palette.preparation.v1", "state": "preparing", "identity": identity,
                  "pid": os.getpid(), "process_start": process_start(os.getpid()),
                  "started_at": time.time(), "previous_state": previous["state"]}
        save(record)
        started = time.monotonic()
        try:
            if host_bin:
                argv = [host_bin, "prewarm", "--project-dir", project_dir, "--repo-dir", repo_dir]
            else:
                argv = build_bwrap_argv(workspace_dir=workspace, broker_socket_dir=None,
                    project_dir=identity["project"], repo_dir=identity["repo"], julia_bin=julia_bin,
                    julia_depot=identity["depot"], network_enabled=False,
                    package_store_root=package_root, package_depot_dir=package_depot,
                    package_environment_dir=package_environment)
                argv += ["--", julia_bin, "--startup-file=no", "-e", PREWARM]
            result = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True, text=True)
            record.update(state="prepared" if result.returncode == 0 else "failed",
                          returncode=result.returncode, output=result.stdout, stderr=result.stderr,
                          error=None if result.returncode == 0 else result.stderr or result.stdout)
            loaded_caches = next((json.loads(line.split("=", 1)[1]) for line in result.stdout.splitlines()
                                  if line.startswith("palette_preparation_caches=")), [])
            record["cache_files"] = {p: hashlib.sha256(Path(p).read_bytes()).hexdigest() for p in loaded_caches}
            record["cache_tracking"] = "loaded_images"
        except Exception as exc:
            record.update(state="failed", error=f"{type(exc).__name__}: {exc}")
        record["seconds"] = time.monotonic() - started
        record["finished_at"] = time.time()
        save(record)
        return record


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--project-dir", required=True)
    ap.add_argument("--repo-dir", default=str(Path(__file__).resolve().parents[2]))
    ap.add_argument("--state-dir")
    ap.add_argument("--host-bin")
    ap.add_argument("--status", action="store_true")
    ap.add_argument("--force", action="store_true")
    args = ap.parse_args()
    record = (preparation_status(preparation_identity(args.project_dir, args.repo_dir,
                  resolve_real_julia_binary()), args.state_dir) if args.status else
              prepare(args.project_dir, args.repo_dir, state_dir=args.state_dir,
                      host_bin=args.host_bin, force=args.force))
    print(json.dumps(record, ensure_ascii=False))
    return 1 if record["state"] == "failed" else 0


if __name__ == "__main__":
    sys.exit(main())
