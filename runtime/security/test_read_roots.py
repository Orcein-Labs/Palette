"""PALETTE_READ_ROOTS: host directories bound read-only into the sandbox.

Mirrors runtime/host/src/sandbox.rs (parse_read_roots, check_read_roots).
"""
from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
from filesystem_layout import parse_read_roots, require_read_roots  # noqa: E402
import launch_worker  # noqa: E402


class ReadRootsTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="palette-read-roots-")).resolve()
        for name in ("data", "ws", "state", "home/depot", "repo", "project", "toolchain/bin"):
            (self.tmp / name).mkdir(parents=True)

    def test_parse_accepts_absolute_dirs_and_refuses_the_rest(self):
        a, b = self.tmp / "data", self.tmp / "repo"
        self.assertEqual(parse_read_roots(f"{a}::{b}"), [str(a), str(b)])
        self.assertEqual(parse_read_roots(None), [])
        for bad in ("relative/dir", str(self.tmp / "missing")):
            with self.assertRaises(PermissionError):
                parse_read_roots(bad)

    def test_a_root_never_contains_a_mount_or_sits_in_a_writable_one(self):
        writable = [str(self.tmp / "ws")]
        others = [str(self.tmp / "home/depot"), "/tmp", "/usr", "/run/palette"]
        require_read_roots([str(self.tmp / "data")], writable, others)
        for bad, why in ((self.tmp / "home", "contains"), (Path("/"), "contains"), (self.tmp, "contains"),
                         (self.tmp / "ws/inner", "inside the writable")):
            bad.mkdir(exist_ok=True)
            with self.assertRaises(PermissionError) as e:
                require_read_roots([str(bad)], writable, others)
            self.assertIn(why, str(e.exception))

    def test_argv_binds_each_root_read_only_at_its_own_path(self):
        julia_bin = self.tmp / "toolchain/bin/julia"
        julia_bin.write_text("")
        kw = dict(workspace_dir=str(self.tmp / "ws"), broker_socket_dir=None, project_dir=str(self.tmp / "project"),
                  repo_dir=str(self.tmp / "repo"), julia_bin=str(julia_bin), julia_depot=str(self.tmp / "home/depot"),
                  network_enabled=False, state_dir=str(self.tmp / "state"))
        data = str(self.tmp / "data")
        with mock.patch.dict(os.environ, {"PALETTE_READ_ROOTS": data}):
            argv = launch_worker.build_bwrap_argv(**kw)
        i = argv.index(data)
        self.assertEqual(argv[i - 1:i + 2], ["--ro-bind", data, data])
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("PALETTE_READ_ROOTS", None)
            self.assertNotIn(data, launch_worker.build_bwrap_argv(**kw))
        with mock.patch.dict(os.environ, {"PALETTE_READ_ROOTS": str(self.tmp)}):
            with self.assertRaises(PermissionError):
                launch_worker.build_bwrap_argv(**kw)


if __name__ == "__main__":
    unittest.main()
