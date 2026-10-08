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
(reflink) clone of the prepared depot, bound writable as the primary
`JULIA_DEPOT_PATH` entry. A separate durable operator package store may be
present as a later read-only depot entry for broker-provisioned packages.

Paths are bound at IDENTICAL guest paths to their host paths (no
remapping). Julia's own package manifests and precompile cache embed
absolute host paths; remapping them would invalidate the whole prewarmed
depot and force full recompilation inside every sandboxed launch -- the
the cold-start tax that depot preparation is designed to avoid.
"""
from __future__ import annotations

import argparse
import fcntl
import os
import shutil
import stat
import subprocess
import sys
import tempfile
from contextlib import contextmanager
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
                     "JULIA_PKG_OFFLINE", "PALETTE_PACKAGE_ENVIRONMENT", "PALETTE_REPO_DIR", "PALETTE_STATE_DIR", "PALETTE_BROKER_SOCKET"}


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
    package_store_root: str | None = None,
    package_depot_dir: str | None = None,
    package_environment_dir: str | None = None,
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
    if package_store_root:
        protected.append(package_store_root)
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
        # worker write access to every other session's cache. The separate
        # operator package store, when configured, follows read-only.
        "--bind", depot_clone_dir or julia_depot, julia_depot,
        # this repo's source (the Palette package) -- read-only, worker cannot modify it
        "--ro-bind", repo_dir, repo_dir,
        # the dev project (Project.toml/Manifest.toml naming Palette + IJulia + deps) -- read-only
        "--ro-bind", project_dir, project_dir,
        # bounded, writable workspace -- the sandbox-local effect envelope
        "--bind", workspace_dir, workspace_dir,
    ]
    if package_store_root:
        # Provisioned package sources and the broker-managed environment are
        # host-owned data. Workers can import them but cannot mutate them.
        argv += ["--ro-bind", package_store_root, package_store_root]
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
        "--setenv", "JULIA_DEPOT_PATH", f"{julia_depot}:{package_depot_dir}" if package_depot_dir else julia_depot,
        "--setenv", "JULIA_PROJECT", project_dir,
        # The session project stays loadable after turn code activates
        # another one; a turn's routine `Pkg.activate(".")` used to hide
        # every package it had. The operator package environment is appended
        # explicitly below.
        "--setenv", "JULIA_LOAD_PATH",
        f"@:{project_dir}:@v#.#:{package_environment_dir}:@stdlib" if package_environment_dir
        else f"@:{project_dir}:@v#.#:@stdlib",
        "--setenv", "PALETTE_PACKAGE_ENVIRONMENT", package_environment_dir or "",
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
    configured = os.environ.get("JULIA_DEPOT_PATH")
    if configured:
        primary = configured.split(os.pathsep, 1)[0]
        if primary:
            return primary
    return str(Path.home() / ".julia")


def build_package_installer_argv(
    *, julia_bin: str, package_store_root: str, package_depot_dir: str,
    package_environment_dir: str, base_depot: str, offline: bool,
    script: str, protected_paths: tuple[str, ...] | list[str] = (),
) -> list[str]:
    """Run operator-authorized Pkg work in a small filesystem namespace.

    Pkg executes package `deps/build.jl` scripts during installation. The
    broker therefore gives that process only the durable package store as a
    writable host mount, plus fresh tmpfs locations for HOME and temporary
    files. Julia, the prepared depot's source/registry/cache directories, and
    the minimum runtime libraries are read-only. The worker's project,
    workspace, broker socket, task tools, ambient HOME, and host config are
    not mounted into this process.
    """
    bwrap = shutil.which("bwrap")
    if bwrap is None:
        raise RuntimeError("bubblewrap (bwrap) is required for package installation")
    if Path(package_store_root).is_symlink() or Path(package_depot_dir).is_symlink() or Path(package_environment_dir).is_symlink():
        raise RuntimeError("package store, depot, and environment paths must not be symlinks")
    store = Path(package_store_root).resolve(strict=True)
    depot = Path(package_depot_dir).resolve(strict=True)
    environment = Path(package_environment_dir).resolve(strict=True)
    base = Path(base_depot).resolve(strict=True)
    julia = Path(julia_bin).resolve(strict=True)
    toolchain = julia.parent.parent
    if not store.is_dir() or not depot.is_dir() or not environment.is_dir() or not base.is_dir():
        raise RuntimeError("package store, package depot, environment, and prepared depot must be directories")
    if store not in depot.parents or store not in environment.parents:
        raise RuntimeError("package depot and environment must be inside the durable package store")
    sources = store / "sources"
    if sources.is_symlink() or (sources.exists() and (not sources.is_dir() or store not in sources.resolve().parents)):
        raise RuntimeError("package store sources must be a real directory contained in the store")
    _reject_symlink_tree(sources, "package store sources", allow_contained=True)
    if store == base or store in base.parents or base in store.parents:
        raise RuntimeError("package store and Julia base depot must be disjoint")
    if store == toolchain or store in toolchain.parents or toolchain in store.parents:
        raise RuntimeError("package store and Julia toolchain must be disjoint")
    for configured in protected_paths:
        protected = Path(configured).resolve(strict=False)
        if store == protected or store in protected.parents or protected in store.parents:
            raise RuntimeError(f"package store must be disjoint from protected host path {protected}")
    for system_path in (Path("/usr"), Path("/lib64"), Path("/etc"), Path("/proc"), Path("/dev"), Path("/run")):
        if store == system_path or store in system_path.parents or system_path in store.parents:
            raise RuntimeError(f"package store must be disjoint from {system_path}")

    args = [
        bwrap, "--unshare-user", "--uid", "1000", "--gid", "1000",
        "--disable-userns", "--assert-userns-disabled", "--unshare-pid", "--unshare-ipc",
    ]
    if offline:
        args.append("--unshare-net")
    args += [
        "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
        "--dir", "/etc", "--dir", "/run/palette", "--tmpfs", "/run/palette", "--dir", "/run/palette/home",
        "--clearenv", "--die-with-parent", "--new-session", "--cap-drop", "ALL",
        "--ro-bind", "/usr", "/usr", "--symlink", "usr/lib", "/lib",
        "--ro-bind", "/lib64", "/lib64", "--symlink", "usr/bin", "/bin",
        "--ro-bind", "/etc/ld.so.cache", "/etc/ld.so.cache",
    ]
    # Mount targets nested below HOME or /tmp need empty guest parent dirs;
    # those directories are created in the namespace and reveal no host data.
    guest_dirs: set[str] = set()
    for mount in (toolchain, store, depot, environment, *(base / part for part in ("compiled", "packages", "artifacts"))):
        if mount.exists():
            for path in (mount, *reversed(mount.parents)):
                value = str(path)
                if value in ("/", "/usr", "/lib64", "/etc", "/proc", "/dev", "/tmp", "/run"):
                    continue
                guest_dirs.add(value)
    for directory in sorted(guest_dirs, key=lambda item: (item.count("/"), item)):
        args += ["--dir", directory]

    args += ["--ro-bind", str(toolchain), str(toolchain), "--ro-bind", str(store), str(store)]
    for name in ("compiled", "packages", "artifacts"):
        source = base / name
        if source.is_dir():
            args += ["--ro-bind", str(source), str(source)]
    args += ["--bind", str(depot), str(depot), "--bind", str(environment), str(environment)]
    identity = identity_files()
    args += [
        "--ro-bind", str(identity / "passwd"), "/etc/passwd",
        "--ro-bind", str(identity / "group"), "/etc/group",
    ]
    if not offline:
        for source, target in (("/etc/hosts", "/etc/hosts"), ("/etc/resolv.conf", "/etc/resolv.conf"),
                               ("/etc/nsswitch.conf", "/etc/nsswitch.conf"), ("/etc/ssl/certs", "/etc/ssl/certs")):
            if Path(source).exists():
                args += ["--ro-bind", source, target]
    path = f"{toolchain}/bin:/usr/bin:/bin"
    depot_path = f"{depot}:{base}"
    args += [
        "--setenv", "HOME", "/run/palette/home", "--setenv", "TMPDIR", "/tmp",
        "--setenv", "PATH", path, "--setenv", "LANG", "C.UTF-8",
        "--setenv", "JULIA_DEPOT_PATH", depot_path,
        "--setenv", "JULIA_PROJECT", str(environment),
        "--setenv", "JULIA_PKG_PRECOMPILE_AUTO", "0",
    ]
    if offline:
        args += ["--setenv", "JULIA_PKG_OFFLINE", "true"]
    args += ["--chdir", str(environment), "--", str(julia), "--startup-file=no", "-e", script]
    return args


@contextmanager
def package_store_lock(root: str | Path):
    """Serialize durable package-store initialization and Pkg mutations."""
    lock_path = Path(root) / ".package-store.lock"
    fd = os.open(lock_path, os.O_CREAT | os.O_APPEND | os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0), 0o600)
    with os.fdopen(fd, "a") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(lock.fileno(), fcntl.LOCK_UN)


def _reject_symlink_tree(path: Path, label: str, *, allow_contained: bool = False) -> None:
    """Reject symlinks below a host tree before binding or copying it."""
    try:
        root_info = path.lstat()
    except FileNotFoundError:
        return
    root = path.resolve(strict=True) if stat.S_ISDIR(root_info.st_mode) else path.parent.resolve(strict=True)

    def visit(node: Path) -> None:
        info = node.lstat()
        if stat.S_ISLNK(info.st_mode):
            target = node.resolve(strict=False)
            if not allow_contained or (target != root and root not in target.parents):
                raise RuntimeError(f"{label} must not contain escaping symlinks: {node}")
        elif stat.S_ISREG(info.st_mode):
            return
        elif stat.S_ISDIR(info.st_mode):
            for entry in node.iterdir():
                visit(entry)
        else:
            raise RuntimeError(f"{label} contains a non-regular filesystem entry: {node}")

    visit(path)


def package_store_paths(
    root: str | None = None,
    *,
    protected_paths: list[str | Path] | tuple[str | Path, ...] = (),
    ceiling: dict | None = None,
) -> tuple[str, str, str]:
    """Validate, then prepare, the durable host-owned package store.

    Every authority and containment check happens before mkdir, chmod, lock
    creation, or source seeding. In particular, an operator config path may
    not alias the store, and pre-existing store children may not redirect
    initialization through symlinks.
    """
    configured = root or os.environ.get("PALETTE_PACKAGE_DEPOT")
    path = Path(configured).expanduser() if configured else Path.home() / ".palette" / "packages"
    if not path.is_absolute():
        raise RuntimeError("PALETTE_PACKAGE_DEPOT must be an absolute path")
    lexical_root = Path(os.path.abspath(path))
    if lexical_root.is_symlink():
        raise RuntimeError("PALETTE_PACKAGE_DEPOT must not be a symlink")
    path = lexical_root.resolve(strict=False)
    depot = path / "depot"
    environment = path / "environment"
    for child in (depot, environment, path / "sources", depot / "registries", depot / "packages",
                  path / ".seeded", path / ".package-store.lock"):
        if child.is_symlink():
            raise RuntimeError(f"package store path must not be a symlink: {child}")
    _reject_symlink_tree(path / "sources", "package store sources", allow_contained=True)
    for tree in (depot / "registries", depot / "packages"):
        _reject_symlink_tree(tree, "package store directory", allow_contained=True)

    from provisioning import operator_ceiling, protected_host_paths
    frozen_ceiling = operator_ceiling() if ceiling is None else ceiling
    protected = set(protected_host_paths(frozen_ceiling, include_package_depot=False))
    protected.update(Path(p).expanduser().resolve(strict=False) for p in protected_paths)
    protected.add(Path(__file__).resolve().parents[2])
    for item in protected:
        if path == item or path in item.parents or item in path.parents:
            raise RuntimeError(f"PALETTE_PACKAGE_DEPOT overlaps protected host path: {item}")

    base_depot = Path(default_depot()).resolve()
    if path == base_depot or path in base_depot.parents or base_depot in path.parents:
        raise RuntimeError("PALETTE_PACKAGE_DEPOT must be disjoint from the Julia base depot")
    for system in (Path("/usr"), Path("/etc"), Path("/dev"), Path("/proc"), Path("/run"), Path("/lib64")):
        if path == system or path in system.parents or system in path.parents:
            raise RuntimeError(f"PALETTE_PACKAGE_DEPOT must be disjoint from {system}")
    if path == Path("/tmp") or path in Path("/tmp").parents:
        raise RuntimeError("PALETTE_PACKAGE_DEPOT must not contain the host /tmp mount")
    julia_toolchain = Path(resolve_real_julia_binary()).resolve(strict=True).parent.parent
    if path == julia_toolchain or path in julia_toolchain.parents or julia_toolchain in path.parents:
        raise RuntimeError("PALETTE_PACKAGE_DEPOT must be disjoint from the Julia toolchain")
    marker = path / ".seeded"
    seed_needed = not marker.exists() and base_depot.is_dir() and any(base_depot.iterdir())
    if seed_needed:
        for directory in ("registries", "packages"):
            source = base_depot / directory
            destination = depot / directory
            if source.is_dir():
                _reject_symlink_tree(source, "prepared package source", allow_contained=True)
                _reject_symlink_tree(destination, "package seed destination")

    # No mutation before the prospective canonical paths have passed all
    # disjointness and symlink checks above.
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    path = path.resolve(strict=True)
    os.chmod(path, 0o700)
    depot = path / "depot"
    environment = path / "environment"
    depot.mkdir(mode=0o700, exist_ok=True)
    environment.mkdir(mode=0o700, exist_ok=True)
    depot = depot.resolve(strict=True)
    environment = environment.resolve(strict=True)
    if path not in depot.parents or path not in environment.parents:
        raise RuntimeError("package depot and environment must remain inside PALETTE_PACKAGE_DEPOT")
    # Seed registry metadata and package sources only. Artifacts and compiled
    # caches can be multi-gigabyte and are unnecessary for source-only packages
    # such as Parsers. A package requiring an uncached artifact fails under the
    # broker's offline Pkg policy.
    with package_store_lock(path):
        if seed_needed and not marker.exists():
            for directory in ("registries", "packages"):
                source = base_depot / directory
                if not source.is_dir():
                    continue
                destination = depot / directory
                _reject_symlink_tree(destination, "package seed destination")
                if destination.is_symlink():
                    raise RuntimeError(f"package store seed destination must not be a symlink: {destination}")
                destination.mkdir(mode=0o700, exist_ok=True)
                completed = subprocess.run(["cp", "-a", "--reflink=auto", f"{source}/.", str(destination)],
                                           stdin=subprocess.DEVNULL, capture_output=True, text=True)
                if completed.returncode != 0:
                    raise RuntimeError(f"could not seed durable package {directory}: {completed.stderr[-1000:]}")
            marker.write_text("seeded registries and package sources from the configured Julia depot\n", encoding="utf-8")
    return str(path), str(depot), str(environment)


def create_session_depot(real_depot: str | None = None, clone_root: str | None = None) -> str:
    """Create the private writable prewarmed depot used for worker caches."""
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
    package_store_root: str | None = None,
    package_depot_dir: str | None = None,
    package_environment_dir: str | None = None,
    timeout: float = 60.0,
) -> subprocess.CompletedProcess:
    julia_bin = resolve_real_julia_binary()
    depot = julia_depot or default_depot()
    if package_store_root is None and package_depot_dir is None and package_environment_dir is None:
        package_store_root, package_depot_dir, package_environment_dir = package_store_paths(
            protected_paths=[workspace_dir, project_dir, repo_dir, broker_socket_dir or ""])
    Path(workspace_dir).mkdir(parents=True, exist_ok=True)

    # A session owner can reuse its private writable clone for warm worker
    # caches. Broker-authorized packages live in the separate durable store.
    # Standalone launches get a fresh clone that is destroyed when they end.
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
            package_store_root=package_store_root,
            package_depot_dir=package_depot_dir,
            package_environment_dir=package_environment_dir,
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
