#!/usr/bin/env python3
# Scheduled for removal: the Python host is kept for one more release, only as
# the conformance oracle. See "Python host: scheduled for removal" in
# runtime/docs/authority.md.
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

Each worker gets its own private writable depot, mounted as the single
`JULIA_DEPOT_PATH` entry. A split depot path (a writable entry before a
read-only one) was tried and caused the cold-start problem it was meant to
solve; see _clone_depot's docstring for the root cause. When preparation
published a snapshot (see publish_depot_snapshot) and bwrap mounts
overlays, the private depot is a kernel overlay of that snapshot: still one
depot path entry, so there is no cascade, and nothing is copied. Otherwise
it is a reflink copy of the snapshot, or of the live depot when there is
none. A separate durable operator package store may be present as a later
read-only depot entry for broker-provisioned packages.

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
import threading
import time
import zipfile
import zlib
from contextlib import contextmanager
from pathlib import Path
from filesystem_layout import parse_read_roots, require_disjoint, require_preserved_roots, require_read_roots, trusted_tools_dir


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

    create_session_depot uses this copy only when there is no snapshot of
    the depot. A kernel overlay of a snapshot is also a single depot entry,
    so it avoids the cascade too, without copying.
    """
    dest = _new_session_depot_dir(clone_root)
    try:
        _copy_tree(real_depot, dest, DEPOT_CLONE_ATTEMPTS)
    except BaseException:
        remove_session_depot(dest)
        raise
    return dest


def _new_session_depot_dir(clone_root: str) -> str:
    dest = tempfile.mkdtemp(dir=clone_root, prefix="depot-")
    try:
        record = _clone_owner_record()
        if record is not None:
            Path(dest + CLONE_OWNER_SUFFIX).write_text(record, encoding="ascii")
    except BaseException:
        remove_session_depot(dest)
        raise
    return dest


OVERLAY_LOWER, OVERLAY_UPPER, OVERLAY_WORK = "lower", "upper", "work"
# The snapshot fds this process's overlay depots hold, by depot.
_snapshot_holds: dict[str, int] = {}
_snapshot_holds_lock = threading.Lock()


def _layer_overlay(snapshot: Path, dest: str) -> None:
    os.symlink(snapshot, os.path.join(dest, OVERLAY_LOWER))
    os.mkdir(os.path.join(dest, OVERLAY_UPPER))
    os.mkdir(os.path.join(dest, OVERLAY_WORK))


def overlay_depot_layers(depot: str) -> tuple[str, str, str] | None:
    """The real lower, upper and work directories of an overlay depot, or
    None for a copied one."""
    if not os.path.islink(os.path.join(depot, OVERLAY_LOWER)):
        return None
    layers = tuple(os.path.realpath(os.path.join(depot, name))
                   for name in (OVERLAY_LOWER, OVERLAY_UPPER, OVERLAY_WORK))
    for layer in layers:
        if not os.path.isdir(layer):
            raise RuntimeError(f"depot overlay layer is not a directory: {layer}")
    return layers


def _overlay_supported(depot: str) -> bool:
    """Whether bwrap mounts the overlay `depot` describes. bwrap before 0.8
    has no `--overlay`, and some kernels refuse unprivileged overlay mounts.
    A probe that hangs counts as no, so the session copies instead of waiting."""
    try:
        lower, upper, work = overlay_depot_layers(depot)
        return subprocess.run(["bwrap", "--unshare-user", "--ro-bind", "/", "/", "--overlay-src", lower,
                               "--overlay", upper, work, lower, "--", "true"],
                              stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL, timeout=10).returncode == 0
    except (OSError, RuntimeError, TypeError, subprocess.TimeoutExpired):
        return False


# Every directory this host owns outside its own scratch root -- a depot clone
# `depot-XXXXXXXX`, a default session root `palette-<id>`, a default receipts
# directory -- has a sidecar `<dir>.owner` naming the process that made it as
# "<pid namespace> <pid> <start time>". Owners remove their directory when they
# finish; the sidecar lets a later session remove the directories of owners
# that were killed first. The Rust host writes the same record.
CLONE_OWNER_SUFFIX = ".owner"
DEFAULT_RECEIPT_RETENTION_DAYS = 30
# How many times a live depot clone is tried before the session gives up.
DEPOT_CLONE_ATTEMPTS = 3


def _process_start_time(pid: int) -> str | None:
    """Field 22 of /proc/<pid>/stat, counted after the parenthesized command name."""
    try:
        stat_line = Path(f"/proc/{pid}/stat").read_text(encoding="ascii", errors="replace")
    except OSError:
        return None
    fields = stat_line[stat_line.rfind(")") + 1:].split()
    return fields[19] if len(fields) > 19 else None


def _pid_namespace() -> str | None:
    try:
        return os.readlink("/proc/self/ns/pid")
    except OSError:
        return None


def _clone_owner_record() -> str | None:
    namespace, start = _pid_namespace(), _process_start_time(os.getpid())
    if namespace is None or start is None:
        return None
    return f"{namespace} {os.getpid()} {start}\n"


def _clone_owner_alive(record: str) -> bool:
    """False only for a well-formed record from this pid namespace whose
    process has exited or whose pid now belongs to a different process."""
    fields = record.split()
    if len(fields) != 3 or not fields[1].isdigit():
        return True
    namespace, pid, start = fields
    if _pid_namespace() != namespace:
        return True
    return _process_start_time(int(pid)) == start


def record_owner(directory: str | os.PathLike) -> None:
    """Write this process's owner record for `directory`."""
    record = _clone_owner_record()
    if record is not None:
        Path(str(directory) + CLONE_OWNER_SUFFIX).write_text(record, encoding="ascii")


def _last_modified(directory: str) -> float:
    """The newest modification time of `directory` and its direct entries."""
    newest = os.lstat(directory).st_mtime
    try:
        for entry in os.scandir(directory):
            try:
                newest = max(newest, entry.stat().st_mtime)
            except OSError:
                pass
    except OSError:
        pass
    return newest


def sweep_orphaned_dirs(root: str | os.PathLike, prefix: str, min_age_seconds: float = 0.0) -> None:
    """Remove the directories `<prefix>*` in `root` whose owner has died and
    that nobody has written to for at least `min_age_seconds`. A directory
    without a readable owner record, or one that is not a plain directory, is
    left alone."""
    for directory in _expired_orphaned_dirs(root, prefix, min_age_seconds):
        remove_owned_dir(directory)


def _expired_orphaned_dirs(root: str | os.PathLike, prefix: str, min_age_seconds: float) -> list[str]:
    """The directories `sweep_orphaned_dirs` would remove. Owner records whose
    directory is already gone are removed here."""
    expired: list[str] = []
    try:
        entries = list(os.scandir(root))
    except OSError:
        return expired
    for entry in entries:
        dir_name = entry.name[:-len(CLONE_OWNER_SUFFIX)] if entry.name.endswith(CLONE_OWNER_SUFFIX) else None
        if not dir_name or not dir_name.startswith(prefix) or not entry.is_file(follow_symlinks=False):
            continue
        try:
            record = Path(entry.path).read_text(encoding="ascii")
        except (OSError, UnicodeDecodeError):
            continue
        if _clone_owner_alive(record):
            continue
        directory = os.path.join(root, dir_name)
        try:
            mode = os.lstat(directory).st_mode
        except FileNotFoundError:
            Path(entry.path).unlink(missing_ok=True)
            continue
        except OSError:
            continue
        if stat.S_ISDIR(mode):
            try:
                idle = time.time() - _last_modified(directory)
            except OSError:
                continue
            if idle >= min_age_seconds:
                expired.append(directory)
    return sorted(expired)


def sweep_orphaned_depot_clones(clone_root: str) -> None:
    """Remove the clones in `clone_root` whose owner has died."""
    sweep_orphaned_dirs(clone_root, "depot-")


def remove_owned_dir(directory: str | os.PathLike) -> None:
    """Remove an owned directory and then its owner record."""
    _unlock_overlay_work(directory)
    shutil.rmtree(directory, ignore_errors=True)
    Path(str(directory) + CLONE_OWNER_SUFFIX).unlink(missing_ok=True)


def _unlock_overlay_work(directory: str | os.PathLike) -> None:
    """The kernel leaves an overlay's `work/work` at mode 000, which stops
    rmtree descending into it."""
    work = os.path.join(directory, OVERLAY_WORK)
    try:
        if stat.S_ISDIR(os.lstat(work).st_mode) and stat.S_ISDIR(os.lstat(os.path.join(work, "work")).st_mode):
            os.chmod(os.path.join(work, "work"), 0o700)
    except OSError:
        pass


def remove_session_depot(clone: str) -> None:
    """Remove a session depot and its owner record, then release the
    snapshot an overlay depot holds."""
    remove_owned_dir(clone)
    with _snapshot_holds_lock:
        fd = _snapshot_holds.pop(clone, None)
    if fd is not None:
        os.close(fd)


def _copy_tree(source: str, dest: str, attempts: int) -> None:
    """`cp -a --reflink=auto source dest`, tried `attempts` times. A live
    depot may be precompiling: a Julia process removes its temporary files,
    and one removed between cp listing a directory and copying it fails the
    copy. A later attempt copies the settled tree."""
    try:
        for attempt in range(attempts):
            _unlock_overlay_work(dest)
            shutil.rmtree(dest, ignore_errors=True)  # cp needs the destination to not exist yet
            if attempt:
                time.sleep(0.2)
            try:
                subprocess.run(["cp", "-a", "--reflink=auto", source, dest], check=True,
                               stdin=subprocess.DEVNULL)
                return
            except subprocess.CalledProcessError:
                if attempt == attempts - 1:
                    raise
    except BaseException:
        shutil.rmtree(dest, ignore_errors=True)
        raise


# Snapshots of a prepared depot live in
# `<depot parent>/.palette-depot-snapshots/<depot name>/snap-<sequence>`. A
# successful prepare publishes one; no sandbox binds them, so no Julia process
# writes them. Session clones copy the newest one while holding a shared flock
# on its directory, and a snapshot is removed only once it is no longer the
# newest and an exclusive flock on it succeeds without waiting. Publishing and
# sweeping hold an exclusive flock on `.lock`. The Rust host uses the same
# layout and locks.
SNAPSHOT_PREFIX = "snap-"


def depot_snapshot_root(depot: str) -> Path | None:
    path = Path(os.path.normpath(depot))
    if not path.name:
        return None
    return path.parent / ".palette-depot-snapshots" / path.name


def _open_dir_nofollow(path: Path) -> int:
    return os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)


def _snapshot_names(root: Path) -> list[str]:
    """The snapshot names in `root`, oldest first."""
    names = [n for n in os.listdir(root)
             if n.startswith(SNAPSHOT_PREFIX) and n[len(SNAPSHOT_PREFIX):].isdecimal()
             and n[len(SNAPSHOT_PREFIX):].isascii()]
    return sorted(names, key=lambda n: int(n[len(SNAPSHOT_PREFIX):]))


def _acquire_depot_snapshot(depot: str) -> tuple[Path, int] | None:
    """The newest snapshot of `depot` and an fd holding a shared flock on
    it, or None when it has none. The caller closes the fd to release it."""
    root = depot_snapshot_root(depot)
    if root is None:
        return None
    try:
        if not stat.S_ISDIR(os.lstat(root).st_mode):
            raise OSError(f"{root} is not a directory")
    except FileNotFoundError:
        return None
    # A sweep can take the chosen snapshot between listing and locking, but
    # only after a newer one was published, so the next listing finds that.
    for _ in range(8):
        names = _snapshot_names(root)
        if not names:
            return None
        path = root / names[-1]
        try:
            fd = _open_dir_nofollow(path)
        except OSError:
            continue
        try:
            fcntl.flock(fd, fcntl.LOCK_SH)
            held, named = os.fstat(fd), None
            try:
                named = os.lstat(path)
            except OSError:
                pass
            if named is not None and (held.st_dev, held.st_ino) == (named.st_dev, named.st_ino):
                fd, kept = -1, fd
                return path, kept
        finally:
            if fd >= 0:
                os.close(fd)
    raise OSError(f"the depot snapshots in {root} kept changing")


@contextmanager
def _hold_depot_snapshot(depot: str):
    """Yields the newest snapshot of `depot` held under a shared flock, or
    None when it has none."""
    held = _acquire_depot_snapshot(depot)
    if held is None:
        yield None
        return
    try:
        yield held[0]
    finally:
        os.close(held[1])


def _snapshot_lock(root: Path) -> int:
    return os.open(root / ".lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)


def publish_depot_snapshot(depot: str) -> Path:
    """Copy `depot` into a new snapshot, then sweep the older ones. Run it
    only right after the depot was prepared, while nothing else prepares it."""
    root = depot_snapshot_root(depot)
    if root is None:
        raise OSError(f"{depot} has no parent")
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    if not stat.S_ISDIR(os.lstat(root).st_mode):
        raise OSError(f"{root} is not a directory")
    lock = _snapshot_lock(root)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        _remove_abandoned_snapshot_dirs(root)
        names = _snapshot_names(root)
        sequence = int(names[-1][len(SNAPSHOT_PREFIX):]) + 1 if names else 1
        tmp = root / f"tmp-{os.urandom(4).hex()}"
        # Prepare just finished, but a user's own Julia may share the depot.
        _copy_tree(depot, str(tmp), DEPOT_CLONE_ATTEMPTS)
        snapshot = root / f"{SNAPSHOT_PREFIX}{sequence:020d}"
        try:
            os.rename(tmp, snapshot)
        except OSError:
            shutil.rmtree(tmp, ignore_errors=True)
            raise
        directory = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
        _sweep_depot_snapshots(root)
        return snapshot
    finally:
        os.close(lock)


def has_depot_snapshot(depot: str) -> bool:
    root = depot_snapshot_root(depot)
    try:
        return root is not None and stat.S_ISDIR(os.lstat(root).st_mode) and bool(_snapshot_names(root))
    except OSError:
        return False


def _remove_abandoned_snapshot_dirs(root: Path) -> None:
    """A `tmp-` or `trash-` directory seen under the publish lock belongs to
    a publish or sweep that died."""
    for entry in os.scandir(root):
        if entry.name.startswith(("tmp-", "trash-")) and entry.is_dir(follow_symlinks=False):
            shutil.rmtree(entry.path, ignore_errors=True)


def _sweep_depot_snapshots(root: Path) -> None:
    """Remove every snapshot but the newest that no clone holds. The caller
    holds the publish lock."""
    try:
        names = _snapshot_names(root)[:-1]
    except OSError:
        return
    for name in names:
        try:
            fd = _open_dir_nofollow(root / name)
        except OSError:
            continue
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            os.close(fd)
            continue
        try:
            # Renamed first, so no later clone can pick it while it is removed.
            trash = root / f"trash-{name}"
            os.rename(root / name, trash)
            shutil.rmtree(trash, ignore_errors=True)
        except OSError:
            pass
        finally:
            os.close(fd)


def _try_sweep_depot_snapshots(depot: str) -> None:
    """Sweep the snapshots of `depot` unless a publish or sweep is under way."""
    root = depot_snapshot_root(depot)
    try:
        lock = _snapshot_lock(root)
    except (OSError, TypeError):
        return
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        _sweep_depot_snapshots(root)
    except OSError:
        pass
    finally:
        os.close(lock)


def receipt_retention_days(value: str | None) -> int:
    """PALETTE_RECEIPT_RETENTION_DAYS: whole days, default 30; 0 keeps
    receipts in the session scratch root, deleted with it."""
    if value is None:
        return DEFAULT_RECEIPT_RETENTION_DAYS
    text = value.strip()
    if not (text.isascii() and text.isdigit()) or int(text) > 36_500:
        raise ValueError(f"PALETTE_RECEIPT_RETENTION_DAYS must be a whole number of days, got {value!r}")
    return int(text)


def default_receipts_root() -> tuple[Path, int] | None:
    """Where a session's broker receipts go when the caller names no receipts
    directory: `~/.palette/receipts/<session id>/`, kept after the session
    closes. Once older than the retention, a later session moves them into
    the monthly archive `~/.palette/docs/receipts/<YYYY-MM>.zip`. None when
    the retention is 0."""
    days = receipt_retention_days(os.environ.get("PALETTE_RECEIPT_RETENTION_DAYS"))
    if days == 0:
        return None
    return Path.home() / ".palette" / "receipts", days


def create_receipts_dir(root: str | os.PathLike, session_id: str, retention_days: float) -> Path:
    """Create `<root>/<session id>/` for one session's receipts, after
    archiving the receipts of closed sessions older than the retention.
    Receipts that cannot be archived are kept, and a later session tries
    again."""
    root = Path(root)
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    archive = receipt_archive_dir(root)
    for expired in _expired_orphaned_dirs(root, "palette-", retention_days * 86_400):
        try:
            retire_receipts(expired, archive)
        except (OSError, ValueError, zipfile.BadZipFile, zipfile.LargeZipFile) as exc:
            print(f"palette-host: keeping the expired receipts {expired}: {exc}", file=sys.stderr)
    directory = root / session_id
    directory.mkdir(mode=0o700)
    try:
        record_owner(directory)
    except BaseException:
        directory.rmdir()
        raise
    return directory


# Expired receipts are archived, not deleted: appended to
# `~/.palette/docs/receipts/<YYYY-MM>.zip` (the UTC month of the directory's
# last modification), named `<session dir>/<relative path>`, through a synced
# temporary file renamed over the archive, under an exclusive lock on `.lock`
# in the archive directory. The Rust host writes the same archive under the
# same lock. Stored entries, no ZIP64.
RECEIPT_ARCHIVE_LIMIT = 0xFFFFFFFF


def receipt_archive_dir(receipts_root: str | os.PathLike) -> Path:
    """Where expired receipts under `receipts_root` (~/.palette/receipts) are
    archived: ~/.palette/docs/receipts."""
    return Path(receipts_root).parent / "docs" / "receipts"


def _receipt_files(directory: Path) -> list[tuple[str, Path]]:
    """The regular files under `directory` as (archive name, path), sorted.
    Symlinks and special files are refused."""
    files: list[tuple[str, Path]] = []
    pending = [(directory.name, directory)]
    while pending:
        prefix, path = pending.pop()
        for entry in sorted(os.scandir(path), key=lambda e: e.name):
            name = f"{prefix}/{entry.name}"
            mode = entry.stat(follow_symlinks=False).st_mode
            if stat.S_ISDIR(mode):
                pending.append((name, Path(entry.path)))
            elif stat.S_ISREG(mode):
                files.append((name, Path(entry.path)))
            else:
                raise ValueError(f"refusing {entry.path}: not a regular file or directory")
    return sorted(files)


def _open_source(path: Path):
    handle = os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC), "rb")
    if not stat.S_ISREG(os.fstat(handle.fileno()).st_mode):
        handle.close()
        raise ValueError(f"refusing {path}: not a regular file")
    return handle


def _crc_and_size(path: Path) -> tuple[int, int]:
    crc, size = 0, 0
    with _open_source(path) as handle:
        while chunk := handle.read(1 << 16):
            crc, size = zlib.crc32(chunk, crc), size + len(chunk)
    return crc, size


def _append_receipts(target: Path, directory: Path, limit: int) -> None:
    """Append the files of `directory` to the archive `target`. Files already
    archived with the same contents are skipped, so a retry after a crash
    adds nothing twice."""
    files = _receipt_files(directory)
    try:
        existing = os.fdopen(os.open(target, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC), "rb")
    except FileNotFoundError:
        existing = None
    archived: dict[str, tuple[int, int]] = {}
    old_len = 0
    if existing is not None:
        with existing:
            if not stat.S_ISREG(os.fstat(existing.fileno()).st_mode):
                raise ValueError(f"refusing {target}: not a regular file")
            with zipfile.ZipFile(existing) as zf:
                for info in zf.infolist():
                    archived[info.filename] = (info.CRC, info.file_size)
            old_len = os.fstat(existing.fileno()).st_size
    fresh = []
    for name, path in files:
        crc, size = _crc_and_size(path)
        if name in archived:
            if archived[name] != (crc, size):
                raise ValueError(f"{target} already holds a different {name}")
            continue
        fresh.append((name, path, size))
    if not fresh:
        return
    projected = max(old_len, 22) + sum(76 + 2 * len(n.encode()) + size for n, _, size in fresh)
    if projected > limit or len(archived) + len(fresh) >= 0xFFFF:
        raise ValueError(f"{target} would pass 4 GiB or 65,534 entries, the limit without ZIP64")

    tmp = target.parent / f".{target.name}.{os.urandom(8).hex()}.tmp"
    out = os.fdopen(os.open(tmp, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600), "r+b")
    try:
        with out:
            if existing is not None:
                with _open_source(target) as source:
                    shutil.copyfileobj(source, out)
                out.seek(0)
            with zipfile.ZipFile(out, "a" if existing is not None else "w", zipfile.ZIP_STORED, allowZip64=False) as zf:
                for name, path, size in fresh:
                    with _open_source(path) as source:
                        mtime = os.fstat(source.fileno()).st_mtime
                        info = zipfile.ZipInfo(name, max(time.gmtime(mtime)[:6], (1980, 1, 1, 0, 0, 0)))
                        info.create_system = 3
                        info.external_attr = (os.fstat(source.fileno()).st_mode & 0xFFFF) << 16
                        info.file_size = size
                        with zf.open(info, "w") as member:
                            shutil.copyfileobj(source, member)
                    if info.file_size != size:
                        raise ValueError(f"{path} changed while it was archived")
            out.flush()
            os.fsync(out.fileno())
        os.replace(tmp, target)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise
    dir_fd = os.open(target.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(dir_fd)
    finally:
        os.close(dir_fd)


def retire_receipts(directory: str | os.PathLike, archive_dir: str | os.PathLike, limit: int = RECEIPT_ARCHIVE_LIMIT) -> None:
    """Archive the expired receipts directory `directory` into `archive_dir`
    and remove it. On any error the directory and its owner record are kept."""
    directory, archive_dir = Path(directory), Path(archive_dir)
    archive_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = os.lstat(archive_dir)
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        raise ValueError(f"refusing {archive_dir}: it must be a directory this user owns")
    if info.st_mode & 0o777 != 0o700:
        os.chmod(archive_dir, 0o700)
    lock = os.open(archive_dir / ".lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        # Another session may have archived and removed it while this one waited.
        try:
            if not stat.S_ISDIR(os.lstat(directory).st_mode):
                raise ValueError(f"{directory} is not a directory")
            os.lstat(str(directory) + CLONE_OWNER_SUFFIX)
        except FileNotFoundError:
            return
        month = time.strftime("%Y-%m", time.gmtime(_last_modified(str(directory))))
        _append_receipts(archive_dir / f"{month}.zip", directory, limit)
        remove_owned_dir(directory)
    finally:
        os.close(lock)


def resolve_real_julia_binary() -> str:
    """Resolve past the juliaup shim to the real julia binary -- the shim
    itself does version-selection logic this sandbox doesn't need or want
    to grant filesystem access to resolve.

    `stdin=subprocess.DEVNULL` is not optional here: confirmed by direct
    testing that `julia -e ...` invoked WITHOUT an explicit stdin (the
    default: inherit the caller's own stdin fd) silently breaks the
    CALLER's own subsequent reads from its own stdin -- invisible for
    every one-shot caller so far (nothing before the session bridge
    depended on a live stdin pipe surviving past this call), but fatal for
    a persistent process like the session bridge, which reads turn requests
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
                     "JULIA_PKG_OFFLINE", "PALETTE_PACKAGE_ENVIRONMENT", "PALETTE_REPO_DIR", "PALETTE_STATE_DIR", "PALETTE_BROKER_SOCKET",
                     "PALETTE_TRUSTED_TOOLS"}


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


def private_dir(path: str | os.PathLike) -> None:
    """Create `path` and its parents and make it a directory only this user
    can enter. A symlink, or a directory another user owns, is refused. The
    broker's socket is world-connectable for the sandbox, so the directory
    around it is what keeps other local users from reaching the broker."""
    path = Path(path)
    path.mkdir(parents=True, exist_ok=True)
    st = os.lstat(path)
    if not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid():
        raise RuntimeError(f"refusing {path}: it must be a directory owned by uid {os.getuid()}")
    if stat.S_IMODE(st.st_mode) != 0o700:
        os.chmod(path, 0o700)


def identity_base(runtime_dir: str | None) -> Path | None:
    """`$XDG_RUNTIME_DIR` when it is an absolute path to a directory this
    user owns that nobody else can enter."""
    if not runtime_dir or not os.path.isabs(runtime_dir):
        return None
    try:
        st = os.lstat(runtime_dir)
    except OSError:
        return None
    if stat.S_ISDIR(st.st_mode) and st.st_uid == os.getuid() and not st.st_mode & 0o077:
        return Path(runtime_dir)
    return None


def identity_files(base: str | Path | None = None) -> Path:
    """passwd and group files naming the sandbox's uid 1000, written once per host.

    They go in the user's private runtime directory when there is one, where
    no other user can claim the name first, and in the temp directory otherwise.

    The directory lives in a shared temp dir under a predictable name, so it
    is trusted only if this user owns it, it is a real directory (not a
    symlink), and nobody else can write to it. Otherwise another local user
    could pre-create it and choose the sandbox's passwd, or plant a symlink
    where the temp file is written.
    """
    uid = os.getuid()
    base = base or identity_base(os.environ.get("XDG_RUNTIME_DIR")) or tempfile.gettempdir()
    etc = Path(base) / f"palette-etc-{uid}"
    try:
        os.mkdir(etc, 0o755)
    except FileExistsError:
        pass
    st = os.lstat(etc)
    if not stat.S_ISDIR(st.st_mode) or st.st_uid != uid or st.st_mode & 0o022:
        raise RuntimeError(
            f"refusing sandbox identity dir {etc}: it must be a directory owned by uid {uid} "
            "and writable only by its owner")
    passwd = f"{SANDBOX_USER}:x:1000:1000:Palette sandbox:/run/palette/home:/bin/bash\n"
    group = f"{SANDBOX_USER}:x:1000:\n"

    def current(name: str) -> str | None:
        try:
            return (etc / name).read_text()
        except OSError:
            return None

    if current("passwd") != passwd or current("group") != group:
        for name, text in (("passwd", passwd), ("group", group)):
            tmp = etc / f".{name}.{os.getpid()}"
            try:
                os.unlink(tmp)
            except FileNotFoundError:
                pass
            fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
            with os.fdopen(fd, "w") as f:
                f.write(text)
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
    depot_overlay = overlay_depot_layers(depot_clone_dir) if depot_clone_dir else None
    if depot_clone_dir:
        protected.append(depot_clone_dir)
    if depot_overlay:
        protected.append(depot_overlay[0])
    if package_store_root:
        protected.append(package_store_root)
    writable = [workspace_dir] + ([state_dir] if state_dir else [])
    require_disjoint(writable, protected)
    require_preserved_roots(writable, ("/tmp", "/run"))
    trusted_tools = trusted_tools_dir(os.environ.get("PALETTE_TRUSTED_TOOLS"))
    read_roots = parse_read_roots(os.environ.get("PALETTE_READ_ROOTS")) + ([trusted_tools] if trusted_tools else [])
    require_read_roots(read_roots, writable, protected + ["/tmp", "/run"])

    # Every bind source is the resolved path, not the raw string: a symlink
    # swapped in after the checks above cannot redirect a mount. Guest-side
    # targets keep their configured paths.
    real = os.path.realpath

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
        "--ro-bind", real(julia_toolchain_dir), julia_toolchain_dir,
    ]
    # depot -- WRITABLE, at the depot's own real path (package source paths
    # are baked into precompiled caches as absolutes, so the guest path must
    # match, not just the content). The source is this worker's private
    # overlay or clone (see create_session_depot), never the live shared
    # depot: a single writable entry avoids the split-JULIA_DEPOT_PATH
    # build-id cascade (see _clone_depot), without handing the worker write
    # access to every other session's cache. The separate operator package
    # store, when configured, follows read-only.
    if depot_overlay:
        lower, upper, work = depot_overlay
        argv += ["--overlay-src", lower, "--overlay", upper, work, julia_depot]
    else:
        argv += ["--bind", real(depot_clone_dir or julia_depot), julia_depot]
    argv += [
        # this repo's source (the Palette package) -- read-only, worker cannot modify it
        "--ro-bind", real(repo_dir), repo_dir,
        # the dev project (Project.toml/Manifest.toml naming Palette + IJulia + deps) -- read-only
        "--ro-bind", real(project_dir), project_dir,
        # bounded, writable workspace -- the sandbox-local effect envelope
        "--bind", workspace_dir, workspace_dir,
    ]
    if package_store_root:
        # Provisioned package sources and the broker-managed environment are
        # host-owned data. Workers can import them but cannot mutate them.
        argv += ["--ro-bind", real(package_store_root), package_store_root]
    # A benchmark runner can supply the task's own tools (the task image's
    # interpreter, bun, ...) as one read-only directory; it is mounted at the
    # same path and put first on PATH, as it is for every other contestant.
    if task_tools:
        argv += ["--ro-bind", real(task_tools), task_tools]
    for path in extra_ro_binds or []:
        argv += ["--ro-bind", real(path), path]
    # Host directories the operator lets the kernel read (PALETTE_READ_ROOTS
    # and the trusted tool directory).
    for path in read_roots:
        argv += ["--ro-bind", path, path]
    if broker_socket_dir:
        argv += ["--ro-bind", real(broker_socket_dir), broker_socket_dir]
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
    if trusted_tools:
        argv += ["--setenv", "PALETTE_TRUSTED_TOOLS", trusted_tools]
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

    from provisioning import command_parents, operator_ceiling, protected_host_paths
    frozen_ceiling = operator_ceiling() if ceiling is None else ceiling
    if path in command_parents(frozen_ceiling):
        raise RuntimeError(f"PALETTE_PACKAGE_DEPOT is the parent of a host-command path: {path}")
    protected = set(protected_host_paths(frozen_ceiling, include_package_depot=False))
    # An empty or absent entry names nothing. Path("").resolve() is the cwd,
    # which would make the overlap check depend on where the host started.
    protected.update(Path(p).expanduser().resolve(strict=False) for p in protected_paths if p)
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


def create_session_depot(real_depot: str | None = None, clone_root: str | None = None,
                         overlay_supported=_overlay_supported) -> str:
    """Create the private writable prewarmed depot used for worker caches.

    With a snapshot of the depot and a bwrap that mounts overlays, the depot
    is an overlay: `lower` links to the snapshot, which stays held until
    remove_session_depot, and writes land in `upper` (see
    overlay_depot_layers). Otherwise it is a copy of the newest snapshot, or
    of the live depot when there is none; a live depot can change, so it is
    never an overlay's lower layer. Callers release it with
    remove_session_depot."""
    depot = real_depot or default_depot()
    root = clone_root or str(Path(depot).parent / ".palette-depot-clones")
    Path(root).mkdir(parents=True, exist_ok=True)
    sweep_orphaned_depot_clones(root)
    held = _acquire_depot_snapshot(depot)
    if held is None:
        return _clone_depot(depot, root)
    snapshot, fd = held
    try:
        dest = _new_session_depot_dir(root)
        try:
            _layer_overlay(snapshot, dest)
            layered = overlay_supported(dest)
        except OSError:
            layered = False
        if layered:
            with _snapshot_holds_lock:
                _snapshot_holds[dest] = fd
            fd = -1
        else:
            try:
                # Nothing writes a snapshot, so one attempt copies it or fails for real.
                _copy_tree(str(snapshot), dest, 1)
            except BaseException:
                remove_session_depot(dest)
                raise
    finally:
        if fd >= 0:
            os.close(fd)
    _try_sweep_depot_snapshots(depot)
    return dest


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
            protected_paths=[p for p in (workspace_dir, project_dir, repo_dir, broker_socket_dir) if p])
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
            remove_session_depot(depot_clone_dir)


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
