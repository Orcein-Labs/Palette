#!/usr/bin/env python3
"""
Palette contained worker launcher -- the first trust domain.

Builds a bubblewrap sandbox around a real, unrestricted `julia` process:
full language semantics inside (eval, ccall, run, Base, Pkg, metaprogramming
-- nothing about the language itself is restricted), bounded by the OS
outside it (namespace isolation, not a language-level blacklist).

The launcher binds only what's needed, read-only unless a path needs to
be writable, clears ambient credentials (--clearenv), and disables network
access unless explicitly enabled. See runtime/docs/authority.md for the
current host and worker authority contracts. Inherited source attribution
and license grants are retained in NOTICE and runtime/licenses/.

Depot handling is NOT a layered writable-over-readonly overlay (that was
tried and, on this substrate, actively caused the cold-start problem it
was meant to solve -- see _clone_depot's docstring for the root cause,
found by direct reproduction). Each worker gets its own real, cheap
(reflink) clone of the depot, bound writable as the depot's one and only
`JULIA_DEPOT_PATH` entry.

Paths are bound at IDENTICAL guest paths to their host paths (no
remapping). Julia's own package manifests and precompile cache embed
absolute host paths; remapping them would invalidate the whole prewarmed
depot and force full recompilation inside every sandboxed launch -- the
the cold-start tax that depot preparation is designed to avoid.
"""
from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from filesystem_layout import parse_read_roots, require_disjoint, require_preserved_roots, require_read_roots


def _clone_depot(real_depot: str, clone_root: str) -> str:
    """Make a real, independent, cheap copy of the depot for one worker's
    exclusive writable use.

    Root cause this works around (found by direct reproduction outside any
    sandbox): a split `JULIA_DEPOT_PATH`
    ("writable:readonly") makes Julia recompute a different "desired
    build_id" for stdlib packages (observed via JULIA_DEBUG=loading) the
    moment the first entry is a fresh/empty writable directory. That
    mismatch cascades and invalidates the entire precompiled dependency
    chain (JSON, Parsers, ...) on every single launch -- a ~40s tax
    regardless of how thoroughly the read-only base depot was prewarmed.
    A *single*-entry, writable depot has no such mismatch, but must be a
    real, independent copy (not the live shared depot bound writable --
    that would let one worker's script corrupt every other session's
    cache) at the SAME absolute path Julia's existing compiled caches
    already reference (package source paths are embedded as absolutes),
    which is why the guest bind target below is `real_depot`, not
    `clone_root`.

    `cp --reflink=auto` on a CoW filesystem (btrfs here) makes this
    near-free: no data is actually duplicated, and a write inside the
    sandbox diverges only the touched blocks. On a non-CoW filesystem it
    falls back to a real byte copy, still correct, just slower (fails
    closed -- correctness first, worth revisiting if that path matters
    later, but not yet proven to be slow enough here to justify a
    filesystem-specific special case).
    """
    dest = tempfile.mkdtemp(dir=clone_root, prefix="depot-")
    shutil.rmtree(dest)  # cp needs the destination to not exist yet
    try:
        subprocess.run(["cp", "-a", "--reflink=auto", real_depot, dest], check=True, stdin=subprocess.DEVNULL)
    except BaseException:
        shutil.rmtree(dest, ignore_errors=True)
        raise
    return dest


def resolve_real_julia_binary() -> str:
    """Resolve past the juliaup shim to the real julia binary -- the shim
    itself does version-selection logic this sandbox doesn't need or want
    to grant filesystem access to resolve.

    `stdin=subprocess.DEVNULL` is not optional here: confirmed by direct
    testing that `julia -e ...` invoked WITHOUT an explicit stdin (the
    default: inherit the caller's own stdin fd) silently breaks the
    CALLER's own subsequent reads from its own stdin -- invisible for
    every one-shot caller so far (nothing before runtime/security/session_cli.py
    depended on a live stdin pipe surviving past this call), but fatal for
    a persistent process like session_cli.py, which reads turn requests
    from its own stdin for its entire lifetime. This one call, running
    inside a session's broker thread the moment any ephemeral turn
    requested spawn_child_worker, would have corrupted that whole
    session's ability to receive any further turn after the first one.
    """
    # An explicit binary wins. Asked under a fresh HOME, the juliaup shim
    # installs and returns its current default release: an endurance run got
    # Julia 1.13.1 while the depot was built for 1.12.6.
    pinned = os.environ.get("PALETTE_JULIA_BIN")
    if pinned:
        if not Path(pinned).is_file():
            raise RuntimeError(f"PALETTE_JULIA_BIN is not a file: {pinned}")
        return pinned
    out = subprocess.run(
        ["julia", "-e", "print(Sys.BINDIR)"], capture_output=True, text=True, check=True, stdin=subprocess.DEVNULL,
    )
    bindir = out.stdout.strip()
    return str(Path(bindir) / "julia")


SANDBOX_USER = "palette"
# The sandbox sets these itself; a task environment cannot move them.
SANDBOX_OWNED_ENV = {"PATH", "HOME", "USER", "LOGNAME", "LANG", "JULIA_DEPOT_PATH", "JULIA_PROJECT", "JULIA_LOAD_PATH",
                     "JULIA_PKG_OFFLINE", "PALETTE_REPO_DIR", "PALETTE_STATE_DIR", "PALETTE_BROKER_SOCKET"}


def read_task_env(path: str | None) -> dict[str, str]:
    """A benchmark's toolchain activation (OCAMLLIB, GOROOT, CONDA_PREFIX, ...),
    as KEY=VALUE lines, given to the kernel as the other contestants get it."""
    if not path:
        return {}
    env = {}
    for line in Path(path).read_text().splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        key, sep, value = line.partition("=")
        if not sep or not key.strip().isidentifier():
            raise RuntimeError(f"PALETTE_TASK_ENV: not a KEY=VALUE line: {line!r}")
        if key.strip() not in SANDBOX_OWNED_ENV:
            env[key.strip()] = value
    return env


def identity_files() -> Path:
    """passwd and group files naming the sandbox's uid 1000, written once per host."""
    etc = Path(tempfile.gettempdir()) / f"palette-etc-{os.getuid()}"
    passwd = f"{SANDBOX_USER}:x:1000:1000:Palette sandbox:/run/palette/home:/bin/bash\n"
    group = f"{SANDBOX_USER}:x:1000:\n"
    if not (etc / "passwd").is_file() or (etc / "passwd").read_text() != passwd:
        etc.mkdir(mode=0o755, exist_ok=True)
        for name, text in (("passwd", passwd), ("group", group)):
            tmp = etc / f".{name}.{os.getpid()}"
            tmp.write_text(text)
            os.chmod(tmp, 0o644)
            os.replace(tmp, etc / name)
    return etc


def build_bwrap_argv(
    *,
    workspace_dir: str,
    broker_socket_dir: str | None,
    project_dir: str,
    repo_dir: str,
    julia_bin: str,
    julia_depot: str,
    network_enabled: bool,
    depot_clone_dir: str | None = None,
    extra_ro_binds: list[str] | None = None,
    state_dir: str | None = None,
) -> list[str]:
    workspace_dir = str(Path(workspace_dir).resolve())
    state_dir = str(Path(state_dir).resolve()) if state_dir else None
    julia_toolchain_dir = str(Path(julia_bin).parent.parent)  # .../julia-1.12.6+0.x64.linux.gnu
    task_tools = os.environ.get("PALETTE_TASK_TOOLS")
    if task_tools and not Path(task_tools, "bin").is_dir():
        raise RuntimeError(f"PALETTE_TASK_TOOLS has no bin directory: {task_tools}")
    task_env = read_task_env(os.environ.get("PALETTE_TASK_ENV"))
    protected = [repo_dir, project_dir, julia_depot, julia_toolchain_dir, "/usr", "/lib64", "/etc", "/proc", "/dev", "/run/palette"]
    protected.extend(extra_ro_binds or [])
    if task_tools:
        protected.append(task_tools)
    if broker_socket_dir:
        protected.append(broker_socket_dir)
    if depot_clone_dir:
        protected.append(depot_clone_dir)
    writable = [workspace_dir] + ([state_dir] if state_dir else [])
    require_disjoint(writable, protected)
    require_preserved_roots(writable, ("/tmp", "/run"))
    read_roots = parse_read_roots(os.environ.get("PALETTE_READ_ROOTS"))
    require_read_roots(read_roots, writable, protected + ["/tmp", "/run"])

    argv = [
        "bwrap",
        "--unshare-user", "--uid", "1000", "--gid", "1000",
        "--disable-userns", "--assert-userns-disabled",
        # Process lifetime: a new PID namespace whose PID 1 is bwrap's own
        # reaper, with julia as its child. When julia exits the reaper exits,
        # and when PID 1 exits the kernel SIGKILLs everything left in the
        # namespace -- background runs, setsid double forks, daemons that
        # ignore TERM and HUP. --die-with-parent (below) ends it if the
        # supervising process dies first. Nothing a worker starts outlives
        # it; runtime/security/test_session_cli.py proves it for ephemeral children.
        # Not --as-pid-1: julia as PID 1 inherited every orphaned process and
        # never reaped it, so finished background jobs stayed as zombies that
        # `ps` and `pgrep` still reported.
        "--unshare-pid",
        "--unshare-ipc",
    ]
    argv += ["--unshare-net"] if not network_enabled else []
    argv += [
        "--proc", "/proc",
        "--dev", "/dev",
        "--tmpfs", "/tmp",
        "--dir", "/run/palette",
        "--tmpfs", "/run/palette",
        "--dir", "/run/palette/home",
        "--clearenv",
        "--die-with-parent",
        "--new-session",
        "--cap-drop", "ALL",
        # base OS -- read-only
        "--ro-bind", "/usr", "/usr",
        "--symlink", "usr/lib", "/lib",
        # Ubuntu and Arch place the ELF loader in different library layouts.
        # Bind the host directory instead of assuming /lib64 -> /usr/lib.
        "--ro-bind", "/lib64", "/lib64",
        "--symlink", "usr/bin", "/bin",
        "--ro-bind", "/etc/ld.so.cache", "/etc/ld.so.cache",
        # julia toolchain -- read-only
        "--ro-bind", julia_toolchain_dir, julia_toolchain_dir,
        # depot -- bound WRITABLE, at the depot's own real path (package
        # source paths are baked into precompiled caches as absolutes, so
        # the guest path must match, not just the content). The source is
        # this worker's own private clone (see _clone_depot), never the
        # live shared depot: a single writable entry avoids the split-
        # JULIA_DEPOT_PATH build-id cascade below, without handing the
        # worker write access to every other session's cache.
        "--bind", depot_clone_dir or julia_depot, julia_depot,
        # this repo's source (the Palette package) -- read-only, worker cannot modify it
        "--ro-bind", repo_dir, repo_dir,
        # the dev project (Project.toml/Manifest.toml naming Palette + IJulia + deps) -- read-only
        "--ro-bind", project_dir, project_dir,
        # bounded, writable workspace -- the sandbox-local effect envelope
        "--bind", workspace_dir, workspace_dir,
    ]
    # A benchmark runner can supply the task's own tools (the task image's
    # interpreter, bun, ...) as one read-only directory; it is mounted at the
    # same path and put first on PATH, as it is for every other contestant.
    if task_tools:
        argv += ["--ro-bind", task_tools, task_tools]
    for path in extra_ro_binds or []:
        argv += ["--ro-bind", path, path]
    # Host directories the operator lets the kernel read (PALETTE_READ_ROOTS).
    for path in read_roots:
        argv += ["--ro-bind", path, path]
    if broker_socket_dir:
        argv += ["--ro-bind", broker_socket_dir, broker_socket_dir]
    # A user the sandbox's uid resolves to: whoami, initdb, ssh and git's
    # identity lookups failed without one.
    etc = identity_files()
    argv += ["--ro-bind", str(etc / "passwd"), "/etc/passwd", "--ro-bind", str(etc / "group"), "/etc/group"]
    # The persistent kernel's saved state (src/revival.jl), outside the task
    # workspace so it never shows up among the task's files. Ephemeral
    # children are never given it.
    if state_dir:
        argv += ["--bind", state_dir, state_dir, "--setenv", "PALETTE_STATE_DIR", state_dir]
    argv += [
        "--setenv", "HOME", "/run/palette/home",
        "--setenv", "JULIA_DEPOT_PATH", julia_depot,
        "--setenv", "JULIA_PROJECT", project_dir,
        # The session project stays loadable after turn code activates
        # another one; a model's routine `Pkg.activate(".")` used to hide
        # every package it had. `@v#.#` is where package_management installs.
        "--setenv", "JULIA_LOAD_PATH", f"@:{project_dir}:@v#.#:@stdlib",
        "--setenv", "PALETTE_REPO_DIR", repo_dir,
        "--setenv", "PATH", f"{task_tools}/bin:{julia_toolchain_dir}/bin:/usr/bin:/bin" if task_tools
        else f"{julia_toolchain_dir}/bin:/usr/bin:/bin",
        "--setenv", "LANG", "en_US.UTF-8",
        "--setenv", "USER", SANDBOX_USER,
        "--setenv", "LOGNAME", SANDBOX_USER,
        "--chdir", workspace_dir,
    ]
    for key, value in task_env.items():
        argv += ["--setenv", key, value]
    if broker_socket_dir:
        argv += ["--setenv", "PALETTE_BROKER_SOCKET", str(Path(broker_socket_dir) / "broker.sock")]
    if not network_enabled:
        # Without it Pkg.add spends ~18s per package on DNS retries before
        # failing, and a few adds in one turn outlast the turn limit.
        argv += ["--setenv", "JULIA_PKG_OFFLINE", "true"]
    return argv


def default_depot() -> str:
    return os.environ.get("JULIA_DEPOT_PATH", str(Path.home() / ".julia")).split(":")[-1]


def create_session_depot(real_depot: str | None = None, clone_root: str | None = None) -> str:
    """Create one private, writable depot clone for a whole session (not
    one launch) to share. Exists as a public entry point, not just an
    internal `run_worker` detail, because `package_management` (see
    `runtime/security/broker.py`) needs the broker to install into the EXACT same
    directory a running worker already has bound -- a bind mount is a live
    view of a directory, not a snapshot, so a host-side write into a
    worker's depot clone while it's still running appears inside the
    sandbox immediately, with no new mount and no restart. That only works
    if there is one clone per session that both the worker and the broker
    agree on, instead of `run_worker` silently making (and destroying) a
    fresh one on every call, as it does for a caller that has no reason to
    share one.
    """
    depot = real_depot or default_depot()
    root = clone_root or str(Path(depot).parent / ".palette-depot-clones")
    Path(root).mkdir(parents=True, exist_ok=True)
    return _clone_depot(depot, root)


def run_worker(
    *,
    workspace_dir: str,
    project_dir: str,
    repo_dir: str,
    script: str,
    broker_socket_dir: str | None = None,
    network_enabled: bool = False,
    julia_depot: str | None = None,
    depot_clone_root: str | None = None,
    depot_clone_dir: str | None = None,
    timeout: float = 60.0,
) -> subprocess.CompletedProcess:
    julia_bin = resolve_real_julia_binary()
    depot = julia_depot or default_depot()
    Path(workspace_dir).mkdir(parents=True, exist_ok=True)

    # A caller that already owns a session-lifetime clone (see
    # create_session_depot) passes it in and keeps owning its cleanup --
    # e.g. so `package_management` can keep installing into it across
    # several launches. A caller with no such need gets the old
    # single-launch behavior: a fresh clone, destroyed when this call ends.
    owns_clone = depot_clone_dir is None
    if owns_clone:
        depot_clone_dir = create_session_depot(depot, depot_clone_root)
    try:
        argv = build_bwrap_argv(
            workspace_dir=workspace_dir,
            broker_socket_dir=broker_socket_dir,
            project_dir=project_dir,
            repo_dir=repo_dir,
            julia_bin=julia_bin,
            julia_depot=depot,
            depot_clone_dir=depot_clone_dir,
            network_enabled=network_enabled,
        )
        argv += ["--", julia_bin, "--startup-file=no", "-e", script]
        # stdin=DEVNULL: see resolve_real_julia_binary's docstring -- the
        # exact same class of bug (an unspecified stdin inherits the
        # caller's, and a `julia -e` invocation silently breaks that
        # caller's own future stdin reads), reachable here via
        # spawn_child_worker's broker handler running inside a persistent
        # session's own process.
        return subprocess.run(argv, capture_output=True, text=True, errors="replace", timeout=timeout, stdin=subprocess.DEVNULL)
    finally:
        if owns_clone:
            shutil.rmtree(depot_clone_dir, ignore_errors=True)


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--workspace", required=True)
    ap.add_argument("--project", required=True)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--script", required=True, help="Julia code to run with -e")
    ap.add_argument("--broker-socket-dir")
    ap.add_argument("--network", action="store_true")
    args = ap.parse_args()

    result = run_worker(
        workspace_dir=args.workspace,
        project_dir=args.project,
        repo_dir=args.repo,
        script=args.script,
        broker_socket_dir=args.broker_socket_dir,
        network_enabled=args.network,
    )
    print("--- stdout ---")
    print(result.stdout)
    print("--- stderr ---")
    print(result.stderr, file=sys.stderr)
    sys.exit(result.returncode)
