#!/usr/bin/env python3
"""
Executable tests for runtime/security/session.py -- Palette's persistent worker.

Same evidence standard as runtime/security/test_authority.py: real sandboxes, real
broker, real host filesystem checks, never the worker's own self-report
alone for anything that claims to cross the authority boundary.

Run:
    python3 runtime/security/test_session.py
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
from host_adapter import HOST_BIN, PaletteSession, SessionDeadError  # noqa: E402

REPO_DIR = str(Path(__file__).resolve().parents[2])
PROJECT_DIR = os.environ.get(
    "PALETTE_TEST_PROJECT_DIR",
    REPO_DIR,
)


def _skip_if_no_bwrap():
    if shutil.which("bwrap") is None:
        raise unittest.SkipTest("bubblewrap (bwrap) not found on PATH")


def _skip_if_no_project():
    if not Path(PROJECT_DIR).exists():
        raise unittest.SkipTest(f"no Julia dev project at {PROJECT_DIR}")


class SessionTestCase(unittest.TestCase):
    def setUp(self):
        _skip_if_no_bwrap()
        _skip_if_no_project()


class TestPersistentMind(SessionTestCase):
    def test_state_persists_across_turns(self):
        with PaletteSession(project_dir=PROJECT_DIR, ceiling={}) as s:
            r1 = s.turn("x = 42")
            self.assertTrue(r1["success"])
            r2 = s.turn("x + 1")
            self.assertTrue(r2["success"])
            self.assertEqual(r2["data"], 43)

    def test_ephemeral_does_not_leak_into_persistent_mind(self):
        with PaletteSession(project_dir=PROJECT_DIR, ceiling={}) as s:
            r1 = s.turn("helper(y) = y * 10; helper(3)", ephemeral=True)
            self.assertTrue(r1["success"])
            self.assertEqual(r1["data"], 30)
            r2 = s.turn("@isdefined(helper)")
            self.assertTrue(r2["success"])
            self.assertFalse(r2["data"])

    def test_ephemeral_child_self_corruption_cannot_reach_the_parent(self):
        """The regression this architecture exists to make possible.

        Earlier, same-process ephemeral isolation (Palette.EphemeralTool's
        AST guard) was proven, by direct adversarial testing, insufficient:
        `Core.eval(Base, :(function show(io::IO, x::Int) ... end))` -- a
        value only assembled at runtime -- successfully extended Base.show
        with zero syntactic trace in the submitted source for any
        parse-time check to catch. No static checker can fully police a
        language with reflective eval.

        The fix was architectural, not a smarter checker: `EPHEMERAL`
        turns now run in a real, disposable, OS-sandboxed child process
        (spawn_child_worker, already adversarially proven -- see
        runtime/docs/authority.md), not in this process. The exact
        same payload that broke same-process isolation is preserved here
        deliberately, per the architecture decision: it MUST still succeed
        locally (proving this is still full, unbounded Julia -- nothing
        was crippled to make this "safe"), but the persistent session's
        own Base.show, checked immediately after in a real subsequent
        turn on the SAME session, must be provably unaffected.
        """
        with PaletteSession(project_dir=PROJECT_DIR, ceiling={}) as s:
            before = s.turn("sprint(show, 42)")
            self.assertEqual(before["data"], "42")

            payload = (
                'Core.eval(Base, :(function show(io::IO, x::Int); print(io, "PWNED"); end)); '
                "sprint(show, 42)"
            )
            child = s.turn(payload, ephemeral=True)
            self.assertTrue(child["success"], child)
            self.assertEqual(child["data"], "PWNED")  # the exploit DOES succeed locally -- full power preserved

            after = s.turn("sprint(show, 42)")
            self.assertTrue(after["success"], after)
            self.assertEqual(after["data"], "42")  # but never reaches the persistent process


class TestSessionLifecycle(SessionTestCase):
    def test_two_sessions_get_different_epochs(self):
        with PaletteSession(project_dir=PROJECT_DIR, ceiling={}) as s1, PaletteSession(project_dir=PROJECT_DIR, ceiling={}) as s2:
            self.assertNotEqual(s1.epoch, s2.epoch)
            for current in (s1, s2):
                self.assertTrue(Path(current.turn("pwd()")["data"]).is_relative_to(tempfile.gettempdir()))

    def test_killed_worker_is_detected_not_hung_or_silently_respawned(self):
        s = PaletteSession(project_dir=PROJECT_DIR, ceiling={})
        try:
            if HOST_BIN:
                for pid in s.worker_pids():
                    os.kill(pid, 9)
            else:
                s._proc.kill()
                s._proc.wait()
            with self.assertRaises(SessionDeadError):
                s.turn("1 + 1")
        finally:
            s.close()

    def test_session_starts_under_a_deep_session_root(self):
        """The broker's Unix socket lived under the session root; a long HOME
        pushed its path past 108 bytes and no kernel could start."""
        deep = Path(tempfile.mkdtemp(prefix="palette-deep-")) / ("d" * 60) / ("e" * 60)
        s = PaletteSession(project_dir=PROJECT_DIR, ceiling={}, workspace_dir=str(deep))
        try:
            if not HOST_BIN:
                sock_dir = s._broker_sock_dir
                self.assertLessEqual(len(str(Path(sock_dir) / "broker.sock").encode()), 100)
            self.assertEqual(s.turn("1 + 1")["data"], 2)
        finally:
            s.close()
            if HOST_BIN:
                self.assertFalse(deep.exists(), "the session's scratch root outlived it")
            shutil.rmtree(deep.parent.parent, ignore_errors=True)
        if not HOST_BIN:
            self.assertFalse(Path(sock_dir).exists())

    def test_turn_after_close_raises(self):
        s = PaletteSession(project_dir=PROJECT_DIR, ceiling={})
        s.close()
        with self.assertRaises(SessionDeadError):
            s.turn("1 + 1")

    def test_construction_failure_does_not_leak_the_depot_clone(self):
        """A real startup failure must remove its own depot and scratch root."""
        previous_depot = os.environ.get("JULIA_DEPOT_PATH")
        try:
            for copy_fails in (False, True):
                with self.subTest(copy_fails=copy_fails), tempfile.TemporaryDirectory(prefix="palette-startup-failure-") as tmp:
                    depot = Path(tmp, "depot")
                    depot.mkdir()
                    blocked = depot / "unreadable"
                    blocked.write_text("copy must fail" if copy_fails else "copy succeeds")
                    blocked.chmod(0 if copy_fails else 0o600)
                    root = Path(tmp, "session")
                    root.mkdir()
                    (root / "broker").write_text("not a directory")
                    os.environ["JULIA_DEPOT_PATH"] = str(depot)
                    error = SessionDeadError if HOST_BIN else subprocess.CalledProcessError if copy_fails else FileExistsError
                    with self.assertRaises(error):
                        PaletteSession(project_dir=PROJECT_DIR, ceiling={}, workspace_dir=str(root))
                    clone_root = Path(tmp, ".palette-depot-clones")
                    self.assertTrue(clone_root.is_dir(), "startup never reached depot cloning")
                    self.assertEqual(list(clone_root.iterdir()), [], "depot clone leaked")
                    self.assertFalse(root.exists(), "scratch root leaked")
        finally:
            if previous_depot is None:
                os.environ.pop("JULIA_DEPOT_PATH", None)
            else:
                os.environ["JULIA_DEPOT_PATH"] = previous_depot



class TestAuthorityUnderPersistence(SessionTestCase):
    """The whole point: keeping the worker warm must not change what it's
    authorized to do. Verified against real host filesystem state, not the
    worker's own self-report."""

    def setUp(self):
        super().setUp()
        self.tmp = tempfile.mkdtemp(prefix="palette-session-test-")
        self.allowed_sub = os.path.join(self.tmp, "sub")
        os.makedirs(self.allowed_sub)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_approved_write_from_persistent_turn_reaches_real_host(self):
        target = os.path.join(self.allowed_sub, "out.txt")
        with PaletteSession(
            project_dir=PROJECT_DIR, ceiling={"external_fs_write": {"allowed_dirs": [self.allowed_sub]}}
        ) as s:
            r = s.turn(f'Palette.request_capability("external_fs_write", Dict("path" => "{target}", "content" => "hi"))')
            self.assertTrue(r["success"])
            self.assertTrue(r["data"]["approved"])
        self.assertTrue(os.path.isfile(target))
        self.assertEqual(Path(target).read_text(), "hi")

    def test_denied_write_from_persistent_turn_never_reaches_real_host(self):
        denied_target = os.path.join(self.tmp, "outside.txt")  # sibling of allowed_sub, not inside it
        with PaletteSession(
            project_dir=PROJECT_DIR, ceiling={"external_fs_write": {"allowed_dirs": [self.allowed_sub]}}
        ) as s:
            r = s.turn(f'Palette.request_capability("external_fs_write", Dict("path" => "{denied_target}", "content" => "no"))')
            self.assertTrue(r["success"])  # the REQUEST was well-formed; the CAPABILITY was denied
            self.assertFalse(r["data"]["approved"])
        self.assertFalse(os.path.isfile(denied_target))

    def test_ephemeral_default_ceiling_is_empty_not_inherited(self):
        """The new, safer default: an ephemeral child gets NO broker-
        mediated authority unless the caller explicitly widens it, even
        if the parent session's own ceiling would allow more."""
        target = os.path.join(self.allowed_sub, "should_not_exist.txt")
        with PaletteSession(
            project_dir=PROJECT_DIR, ceiling={"external_fs_write": {"allowed_dirs": [self.allowed_sub]}}
        ) as s:
            r = s.turn(
                f'Palette.request_capability("external_fs_write", Dict("path" => "{target}", "content" => "eph"))',
                ephemeral=True,  # no ephemeral_ceiling given -- defaults to {}
            )
            self.assertTrue(r["success"])
            self.assertFalse(r["data"]["approved"])
        self.assertFalse(os.path.isfile(target))

    def test_ephemeral_turn_reaches_the_broker_when_explicitly_widened(self):
        target = os.path.join(self.allowed_sub, "from_ephemeral.txt")
        ceiling = {"external_fs_write": {"allowed_dirs": [self.allowed_sub]}}
        with PaletteSession(project_dir=PROJECT_DIR, ceiling=ceiling) as s:
            r = s.turn(
                f'Palette.request_capability("external_fs_write", Dict("path" => "{target}", "content" => "eph"))',
                ephemeral=True,
                ephemeral_ceiling=ceiling,  # explicit: C_child == C_caller here, still validated as a real subset
            )
            self.assertTrue(r["success"])
            self.assertTrue(r["data"]["approved"])
        self.assertEqual(Path(target).read_text(), "eph")


if __name__ == "__main__":
    unittest.main()


class TestReadRoots(SessionTestCase):
    """PALETTE_READ_ROOTS: the kernel reads the host directory at its own
    path, cannot write it, and sees nothing beside it."""

    def test_a_read_root_is_readable_not_writable_and_nothing_else_appears(self):
        outside = Path(tempfile.mkdtemp(prefix="palette-read-root-", dir=str(Path.home())))
        root = outside / "shared"
        root.mkdir()
        (root / "data.txt").write_text("from the host\n")
        (outside / "sibling.txt").write_text("not shared\n")
        try:
            with unittest.mock.patch.dict(os.environ, {"PALETTE_READ_ROOTS": str(root)}):
                with PaletteSession(project_dir=PROJECT_DIR, ceiling={}) as s:
                    r = s.turn(f'read({str(root / "data.txt")!r}, String)'.replace("'", '"'))
                    self.assertEqual(r["data"], "from the host\n")
                    r = s.turn(f'try; write({str(root / "new.txt")!r}, "x"); "wrote"; catch e; "refused"; end'.replace("'", '"'))
                    self.assertEqual(r["data"], "refused")
                    r = s.turn(f'isfile({str(outside / "sibling.txt")!r})'.replace("'", '"'))
                    self.assertIs(r["data"], False)
            self.assertFalse((root / "new.txt").exists())
        finally:
            shutil.rmtree(outside, ignore_errors=True)
