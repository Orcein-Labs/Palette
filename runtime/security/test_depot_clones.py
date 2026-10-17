"""Session depot clones record their owner, and clones of dead owners are swept."""
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from launch_worker import (  # noqa: E402
    CLONE_OWNER_SUFFIX, _clone_owner_alive, _pid_namespace, create_session_depot, remove_session_depot,
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


if __name__ == "__main__":
    unittest.main()
