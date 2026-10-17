"""Session depot clones, default session roots and default receipts
directories record their owner, and those of dead owners are swept."""
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from launch_worker import (  # noqa: E402
    CLONE_OWNER_SUFFIX, _clone_owner_alive, _clone_owner_record, _pid_namespace, create_receipts_dir,
    create_session_depot, receipt_retention_days, remove_session_depot, sweep_orphaned_dirs,
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


if __name__ == "__main__":
    unittest.main()
