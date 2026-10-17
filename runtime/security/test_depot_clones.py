"""Session depot clones, default session roots and default receipts
directories record their owner, and those of dead owners are swept."""
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from launch_worker import (  # noqa: E402
    CLONE_OWNER_SUFFIX, _append_receipts, _clone_owner_alive, _clone_owner_record, _pid_namespace,
    create_receipts_dir, create_session_depot, identity_base, identity_files, private_dir, receipt_archive_dir,
    receipt_retention_days, remove_session_depot, retire_receipts, sweep_orphaned_dirs,
)


def owner_path(clone) -> Path:
    return Path(str(clone) + CLONE_OWNER_SUFFIX)


class DepotCloneOwnershipTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="palette-depot-clones-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        self.depot = self.root / "depot"
        self.depot.mkdir()
        (self.depot / "cache").write_text("warm")
        self.clones = self.root / ".palette-depot-clones"

    def test_clones_record_their_owner_and_dead_owners_are_swept(self):
        live = create_session_depot(str(self.depot))
        self.assertEqual(Path(live, "cache").read_text(), "warm")
        record = owner_path(live).read_text()
        self.assertTrue(_clone_owner_alive(record), f"own record judged dead: {record}")

        namespace = _pid_namespace()
        child = subprocess.Popen(["true"])
        reaped = child.pid
        child.wait()

        def orphan(name, record):
            clone = self.clones / name
            clone.mkdir()
            (clone / "cache").write_text("stale")
            owner_path(clone).write_text(record)
            return clone

        exited = orphan("depot-exited", f"{namespace} {reaped} 1\n")
        reused = orphan("depot-reused", f"{namespace} {os.getpid()} 0\n")
        foreign = orphan("depot-foreign", f"pid:[1] {reaped} 1\n")
        garbled = orphan("depot-garbled", "not a record")
        unrecorded = self.clones / "depot-unrecorded"
        unrecorded.mkdir()
        lone_record = self.clones / "depot-gone.owner"
        lone_record.write_text(f"{namespace} {reaped} 1\n")
        target = self.root / "elsewhere"
        target.mkdir()
        link = self.clones / "depot-link"
        link.symlink_to(target)
        owner_path(link).write_text(f"{namespace} {reaped} 1\n")

        second = create_session_depot(str(self.depot))
        for gone in (exited, reused, lone_record, owner_path(exited), owner_path(reused)):
            self.assertFalse(os.path.lexists(gone), f"{gone} survived the sweep")
        for kept in (live, second, foreign, garbled, unrecorded, target):
            self.assertTrue(Path(kept).is_dir(), f"{kept} was swept")
        self.assertTrue(link.is_symlink())

        remove_session_depot(live)
        self.assertFalse(os.path.lexists(live) or os.path.lexists(owner_path(live)))

    def test_a_failed_clone_leaves_no_owner_record(self):
        with self.assertRaises(subprocess.CalledProcessError):
            create_session_depot(str(self.root / "no-such-depot"))
        self.assertEqual(list(self.clones.iterdir()), [], "a failed clone left residue")


def _reaped_pid() -> int:
    child = subprocess.Popen(["true"])
    child.wait()
    return child.pid


class OwnedDirSweepTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="palette-owned-dirs-"))
        self.addCleanup(shutil.rmtree, self.root, True)

    def make(self, name, record, age_days):
        directory = self.root / name
        directory.mkdir()
        receipts = directory / "receipts.jsonl"
        receipts.write_text("{}\n")
        when = time.time() - age_days * 86_400
        os.utime(receipts, (when, when))
        os.utime(directory, (when, when))
        owner_path(directory).write_text(record)
        return directory

    def test_owned_dirs_are_swept_by_prefix_once_idle_long_enough(self):
        dead = f"{_pid_namespace()} {_reaped_pid()} 1\n"
        stale = self.make("palette-stale", dead, 40)
        recent = self.make("palette-recent", dead, 2)
        live = self.make("palette-live", _clone_owner_record(), 40)
        other_prefix = self.make("depot-stale", dead, 40)

        sweep_orphaned_dirs(self.root, "palette-", 30 * 86_400)
        self.assertFalse(os.path.lexists(stale) or os.path.lexists(owner_path(stale)))
        for kept in (recent, live, other_prefix):
            self.assertTrue(kept.is_dir() and owner_path(kept).exists(), f"{kept} was swept")
        sweep_orphaned_dirs(self.root, "palette-")
        self.assertFalse(os.path.lexists(recent), "a dead owner's dir survived a zero-age sweep")
        self.assertTrue(live.is_dir() and other_prefix.is_dir())

    def test_receipts_dirs_are_private_owned_and_retention_is_parsed_strictly(self):
        root = self.root / "receipts"
        directory = create_receipts_dir(root, "palette-0123456789abcdef", 1)
        self.assertEqual(root.stat().st_mode & 0o777, 0o700)
        self.assertEqual(directory.stat().st_mode & 0o777, 0o700)
        self.assertTrue(_clone_owner_alive(owner_path(directory).read_text()))
        with self.assertRaises(FileExistsError):
            create_receipts_dir(root, "palette-0123456789abcdef", 0)
        self.assertTrue(directory.is_dir(), "a live session's receipts were pruned")

        self.assertEqual(receipt_retention_days(None), 30)
        self.assertEqual(receipt_retention_days("7"), 7)
        self.assertEqual(receipt_retention_days("0"), 0)
        for bad in ("", "-1", "+7", "1.5", "\u0667", "forever", "99999999"):
            with self.assertRaises(ValueError, msg=bad):
                receipt_retention_days(bad)


# 2020-02-29T12:00:00Z: a leap day, years past any retention.
LEAP_DAY = 1_582_977_600


class ReceiptArchiveTest(unittest.TestCase):
    def setUp(self):
        self.base = Path(tempfile.mkdtemp(prefix="palette-receipt-archive-"))
        self.addCleanup(shutil.rmtree, self.base, True)
        self.root = self.base / "receipts"
        self.archive = receipt_archive_dir(self.root)
        self.dead = f"{_pid_namespace()} {_reaped_pid()} 1\n"

    def make(self, name, files, record, when, root=None):
        """A closed session's receipts dir with `files`, owned by `record`,
        all last modified at `when`."""
        directory = (root or self.root) / name
        directory.mkdir(parents=True)
        for rel, data in files.items():
            path = directory / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
            os.utime(path, (when, when))
            os.utime(path.parent, (when, when))
        os.utime(directory, (when, when))
        owner_path(directory).write_text(record)
        return directory

    def kept(self, directory):
        return directory.is_dir() and owner_path(directory).exists()

    def members(self, zip_path):
        with zipfile.ZipFile(zip_path) as zf:
            self.assertIsNone(zf.testzip())
            return {info.filename: zf.read(info) for info in zf.infolist()}

    def test_expired_receipts_are_zipped_by_month_and_then_removed(self):
        self.assertEqual(self.archive, self.base / "docs" / "receipts")
        blob = bytes(range(256)) * 300
        first = self.make("palette-aaaa", {"receipts.jsonl": b'{"a":1}\n', "sub/blob.bin": blob}, self.dead, LEAP_DAY)
        live = self.make("palette-live", {"receipts.jsonl": b"live\n"}, _clone_owner_record(), LEAP_DAY)
        recent = self.make("palette-recent", {"receipts.jsonl": b"recent\n"}, self.dead, time.time() - 2 * 86_400)

        session = create_receipts_dir(self.root, "palette-new1", 30)
        zip_path = self.archive / "2020-02.zip"
        self.assertFalse(os.path.lexists(first) or os.path.lexists(owner_path(first)))
        for kept in (live, recent, session):
            self.assertTrue(self.kept(kept), f"{kept} was touched")
        self.assertEqual((recent / "receipts.jsonl").read_bytes(), b"recent\n")
        self.assertEqual(self.archive.stat().st_mode & 0o777, 0o700)
        self.assertEqual(zip_path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.members(zip_path),
                         {"palette-aaaa/receipts.jsonl": b'{"a":1}\n', "palette-aaaa/sub/blob.bin": blob})
        if shutil.which("unzip"):
            self.assertEqual(subprocess.run(["unzip", "-tq", zip_path], capture_output=True).returncode, 0)

        # A later session appends to the same month and starts a new one for another.
        second = self.make("palette-bbbb", {"receipts.jsonl": b'{"b":2}\n'}, self.dead, LEAP_DAY + 43_199)
        march = self.make("palette-cccc", {"receipts.jsonl": b'{"c":3}\n'}, self.dead, LEAP_DAY + 43_200)
        create_receipts_dir(self.root, "palette-new2", 30)
        self.assertFalse(os.path.lexists(second) or os.path.lexists(march))
        self.assertEqual(self.members(zip_path), {"palette-aaaa/receipts.jsonl": b'{"a":1}\n',
                                                  "palette-aaaa/sub/blob.bin": blob,
                                                  "palette-bbbb/receipts.jsonl": b'{"b":2}\n'})
        self.assertEqual(self.members(self.archive / "2020-03.zip"), {"palette-cccc/receipts.jsonl": b'{"c":3}\n'})
        self.assertEqual(sorted(p.name for p in self.archive.iterdir()), [".lock", "2020-02.zip", "2020-03.zip"])

    def test_an_archive_failure_keeps_the_receipts(self):
        zip_path = self.archive / "2020-02.zip"
        linked = self.make("palette-link", {"receipts.jsonl": b"x\n"}, self.dead, LEAP_DAY)
        (linked / "elsewhere").symlink_to("/etc/hostname")
        os.utime(linked, (LEAP_DAY, LEAP_DAY))
        with self.assertRaisesRegex(ValueError, "not a regular file or directory"):
            retire_receipts(linked, self.archive)
        self.assertTrue(self.kept(linked) and not zip_path.exists())
        (linked / "elsewhere").unlink()
        os.utime(linked, (LEAP_DAY, LEAP_DAY))

        zip_path.write_bytes(b"not a zip")
        with self.assertRaises(zipfile.BadZipFile):
            retire_receipts(linked, self.archive)
        self.assertTrue(self.kept(linked) and zip_path.read_bytes() == b"not a zip")

        zip_path.unlink()
        zip_path.symlink_to(self.base / "decoy.zip")
        with self.assertRaises(OSError):
            retire_receipts(linked, self.archive)
        self.assertTrue(self.kept(linked) and not (self.base / "decoy.zip").exists())
        zip_path.unlink()

        retire_receipts(self.make("palette-small", {"r": b"1\n"}, self.dead, LEAP_DAY), self.archive)
        before = zip_path.read_bytes()
        big = self.make("palette-big", {"r": bytes(4096)}, self.dead, LEAP_DAY)
        with self.assertRaisesRegex(ValueError, "4 GiB"):
            retire_receipts(big, self.archive, limit=2048)
        self.assertTrue(self.kept(big) and zip_path.read_bytes() == before)

        blocked = self.base / "blocked"
        stuck = self.make("palette-stuck", {"r": b"1\n"}, self.dead, LEAP_DAY, root=blocked / "receipts")
        (blocked / "docs").write_bytes(b"a file where the archive dir goes")
        create_receipts_dir(blocked / "receipts", "palette-new", 1)
        self.assertTrue(self.kept(stuck))
        self.assertEqual((stuck / "r").read_bytes(), b"1\n")

    def test_an_interrupted_retirement_is_not_archived_twice(self):
        directory = self.make("palette-retry", {"r": b"1\n"}, self.dead, LEAP_DAY)
        self.archive.mkdir(mode=0o700, parents=True)
        zip_path = self.archive / "2020-02.zip"
        _append_receipts(zip_path, directory, 0xFFFFFFFF)  # archived, then the host died before removing it
        retire_receipts(directory, self.archive)
        self.assertFalse(directory.exists())
        self.assertEqual(self.members(zip_path), {"palette-retry/r": b"1\n"})

        changed = self.make("palette-retry", {"r": b"2\n"}, self.dead, LEAP_DAY)
        with self.assertRaisesRegex(ValueError, "already holds a different"):
            retire_receipts(changed, self.archive)
        self.assertTrue(changed.is_dir())


class PrivateDirectoryTest(unittest.TestCase):
    """The broker socket's directories and the sandbox identity files must be
    out of other local users' reach."""

    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="palette-private-dirs-"))
        self.addCleanup(shutil.rmtree, self.root, True)

    def mode(self, path):
        return os.lstat(path).st_mode & 0o777

    def test_private_dir_is_closed_to_other_users_and_refuses_a_symlink(self):
        nested = self.root / "a" / "b"
        private_dir(nested)
        self.assertEqual(self.mode(nested), 0o700)
        opened = self.root / "open"
        opened.mkdir(mode=0o755)
        opened.chmod(0o755)
        private_dir(opened)
        self.assertEqual(self.mode(opened), 0o700, "an existing open directory was not tightened")
        opened.chmod(0o755)
        (self.root / "link").symlink_to(opened)
        with self.assertRaisesRegex(RuntimeError, "refusing"):
            private_dir(self.root / "link")
        self.assertEqual(self.mode(opened), 0o755, "the symlink's target was changed")

    def test_identity_files_prefer_a_private_runtime_dir(self):
        runtime = self.root / "run"
        runtime.mkdir(mode=0o700)
        self.assertEqual(identity_base(str(runtime)), runtime)
        for bad in (None, "", "relative/run", str(self.root / "missing")):
            self.assertIsNone(identity_base(bad), bad)
        runtime.chmod(0o755)
        self.assertIsNone(identity_base(str(runtime)), "a runtime dir others can enter was used")
        runtime.chmod(0o700)
        (self.root / "link").symlink_to(runtime)
        self.assertIsNone(identity_base(str(self.root / "link")))

        previous = os.environ.get("XDG_RUNTIME_DIR")
        os.environ["XDG_RUNTIME_DIR"] = str(runtime)
        try:
            etc = identity_files()
        finally:
            if previous is None:
                os.environ.pop("XDG_RUNTIME_DIR")
            else:
                os.environ["XDG_RUNTIME_DIR"] = previous
        self.assertEqual(etc, runtime / f"palette-etc-{os.getuid()}")
        self.assertIn(":x:1000:1000:", (etc / "passwd").read_text())


if __name__ == "__main__":
    unittest.main()
