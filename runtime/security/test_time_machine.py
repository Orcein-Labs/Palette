#!/usr/bin/env python3
"""
Tests for the Time Machine (src/time_machine.jl, time_machine.py): named
checkpoints of the live world, restored or forked by a new kernel, with a
receipt of each identity the checkpoint recorded.

As in test_revival.py, the adversarial cases check that a restore does NOT
claim what is not true: a damaged checkpoint is refused, and code that is not
the checkpoint's is never reported as the same.

Run:
    PALETTE_TEST_PROJECT_DIR=~/.palette-trial/project python3 runtime/security/test_time_machine.py
"""
from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import time_machine  # noqa: E402
from test_revival import KernelTestCase  # noqa: E402

MEAN = "summarize(xs) = sum(xs) / length(xs)\n"
MAX = "summarize(xs) = maximum(xs)\n"


class TimeMachineTest(KernelTestCase):
    def checkpoint_names(self):
        return [c["name"] for c in time_machine.list_checkpoints(self.state)["checkpoints"]]

    def good_then_broken(self):
        """A world checkpointed as `good`, then changed in code, data and ENV."""
        ops = Path(self.ws, "ops.jl")
        ops.write_text(MEAN)
        r = self.session('include("ops.jl"); data = [1.0, 2.0, 3.0, 6.0]; ENV["TM_MODE"] = "good"; result = summarize(data)',
                         'Palette.checkpoint("good"; note="mean of the frame times")',
                         'nothing')
        self.assertIn("Saved checkpoint `good`", r[2]["output"])
        ops.write_text(MAX)
        self.session('include("ops.jl"); data = [9.0, 1.0]; ENV["TM_MODE"] = "broken"; result = summarize(data)')
        return ops

    def test_seven_step_restore_and_undo(self):
        self.good_then_broken()
        time_machine.request_restore(self.state, "good")
        out, r = self.revive('(summarize(data), result, ENV["TM_MODE"], Palette.revival()["checkpoint"])')
        self.assertTrue(r["success"], r)
        value, result, mode, receipt = r["data"]
        self.assertEqual((value, result, mode), (3.0, 3.0, "good"), out)
        self.assertIn("[time machine] Restored checkpoint `good`", out)
        self.assertTrue(receipt["identical"], receipt)
        self.assertEqual(receipt["environment"]["verdict"], "same")
        self.assertEqual([i["replayed_from"] for i in receipt["includes"]], ["checkpoint copy"])
        self.assertEqual([i["workspace"] for i in receipt["includes"]], ["differs"])
        self.assertTrue(all(d["verdict"] == "same" for d in receipt["definitions"] + receipt["data"]), receipt)
        self.assertIn("ops.jl", out)   # the report names the workspace file that differs
        self.assertEqual(Path(self.ws, "ops.jl").read_text(), MAX)   # and leaves it alone
        # Values computed from the checkpoint's copy are not stale: it is the code the world runs.
        self.assertNotIn("restored but stale", out)
        self.assertNotIn("[changed on disk", out)
        out, r = self.revive('(Palette.provenance(:result; strong=true)["freshness"], summarize(data))')
        self.assertEqual(r["data"], ["no_observed_staleness", 3.0], out)
        world = json.loads(Path(self.state, "world.json").read_text())
        self.assertEqual(world["history"][-1]["event"], "restore")
        # The world it replaced was kept, so the restore can be undone.
        auto = receipt["saved_before_restore"]
        self.assertIn(auto, self.checkpoint_names())
        time_machine.request_restore(self.state, auto)
        out, r = self.revive('c = Palette.revival()["checkpoint"]; (summarize(data), ENV["TM_MODE"], c["identical"], c["saved_before_restore"])')
        self.assertEqual(r["data"][:3], [9.0, "broken", True], out)
        self.assertRegex(r["data"][3], r"^auto-\d{8}-\d{6}-before-undo$")   # undoing an undo does not nest names

    def test_restore_survives_a_later_plain_revival(self):
        self.good_then_broken()
        time_machine.request_restore(self.state, "good")
        self.revive('nothing')
        out, r = self.revive('summarize(data)')
        self.assertEqual(r["data"], 3.0, out)   # still the checkpoint's code, not the workspace file's

    def test_fork_records_its_parent(self):
        self.good_then_broken()
        before = json.loads(Path(self.state, "world.json").read_text())["id"]
        time_machine.request_restore(self.state, "good", fork=True)
        out, r = self.revive('c = Palette.checkpoints(); w = c.world; (w["id"], w["parent"]["world"], w["parent"]["checkpoint"], c.checkpoints[1]["name"])')
        self.assertTrue(r["success"], r)
        self.assertIn("Forked a new world from checkpoint `good`", out)
        self.assertNotEqual(r["data"][0], before)
        self.assertEqual(r["data"][1:], [before, "good", "good"])

    def test_damaged_checkpoint_is_refused(self):
        self.good_then_broken()
        cdir = Path(self.state, "checkpoints", "good")
        record = json.loads((cdir / "checkpoint.json").read_text())
        data_file = cdir / record["data"][0]["file"]
        data_file.unlink()   # a hard link: replace it rather than write through to the live snapshot
        data_file.write_bytes(b"not the saved data")
        time_machine.request_restore(self.state, "good")
        out, r = self.revive('(summarize(data), Palette.revival()["checkpoint"]["event"])')
        self.assertIn("Restoring checkpoint `good` failed", out)
        self.assertIn("damaged", out)
        self.assertEqual(r["data"], [9.0, "restore_failed"], out)   # revived as it was

    def test_damaged_copy_of_source_is_refused(self):
        self.good_then_broken()
        record = json.loads(Path(self.state, "checkpoints", "good", "checkpoint.json").read_text())
        sha = next(iter(record["includes"][0]["files"].values()))
        Path(self.state, "blobs", sha).write_text("summarize(xs) = 0.0\n")
        time_machine.request_restore(self.state, "good")
        out, r = self.revive('summarize(data)')
        self.assertIn("Restoring checkpoint `good` failed", out)
        self.assertEqual(r["data"], 9.0, out)

    def test_code_changed_before_the_checkpoint_is_not_claimed_same(self):
        ops = Path(self.ws, "ops.jl")
        ops.write_text(MEAN)
        self.session('include("ops.jl"); data = [1.0, 2.0, 3.0, 6.0]; result = summarize(data)')
        ops.write_text(MAX)   # changed without include(): the world still runs MEAN
        self.session('Palette.checkpoint("after-edit")', 'nothing')
        record = json.loads(Path(self.state, "checkpoints", "after-edit", "checkpoint.json").read_text())
        self.assertEqual([i["kept"] for i in record["includes"]], [False])
        time_machine.request_restore(self.state, "after-edit")
        out, r = self.revive('Palette.revival()["checkpoint"]')
        receipt = r["data"]
        self.assertFalse(receipt["identical"], out)
        self.assertEqual([i["replayed_from"] for i in receipt["includes"]], ["workspace"])
        self.assertEqual([d["verdict"] for d in receipt["definitions"] if d["name"] == "summarize"], ["differs"])

    def test_names_listing_and_forgetting(self):
        Path(self.ws, "ops.jl").write_text(MEAN)
        r = self.session('include("ops.jl"); Palette.checkpoint("one")', 'nothing',
                         'Palette.checkpoint("one")', 'Palette.checkpoint("auto-x")', 'Palette.checkpoint("a/b")',
                         may_fail=True)
        self.assertIn("Saved checkpoint `one`", r[1]["output"])
        self.assertIn("exists", r[2]["error"])
        self.assertIn("auto-", r[3]["error"])
        self.assertIn("checkpoint name", r[4]["error"])
        self.assertEqual(self.checkpoint_names(), ["one"])
        with self.assertRaises(ValueError):
            time_machine.request_restore(self.state, "missing")
        with self.assertRaises(ValueError):
            time_machine.request_restore(self.state, "../one")
        out, r = self.revive('Palette.forget_checkpoint("one")')
        self.assertTrue(r["success"], r)
        self.assertEqual(self.checkpoint_names(), [])
        self.assertEqual(list(Path(self.state, "blobs").iterdir()), [], out)

    def test_failed_call_still_saves_its_checkpoint(self):
        r = self.session('Palette.checkpoint("boom"); error("stop here")', 'nothing', may_fail=True)
        # A call that fails still completes: its state, and the checkpoint, are saved.
        self.assertIn("Saved checkpoint `boom`", r[1]["output"])


class CarryCheckpointsTest(unittest.TestCase):
    def test_fresh_world_keeps_checkpoints(self):
        with tempfile.TemporaryDirectory() as tmp:
            old, new = Path(tmp, "old"), Path(tmp, "new")
            (old / "checkpoints" / "a").mkdir(parents=True)
            (old / "checkpoints" / "a" / "checkpoint.json").write_text(json.dumps({"name": "a", "created": "t"}))
            (old / "blobs").mkdir()
            (old / "manifest.json").write_text("{}")
            time_machine.carry_checkpoints(old, new)
            self.assertEqual(sorted(p.name for p in new.iterdir()), ["blobs", "checkpoints"])
            self.assertEqual([c["name"] for c in time_machine.list_checkpoints(new)["checkpoints"]], ["a"])


if __name__ == "__main__":
    unittest.main()
