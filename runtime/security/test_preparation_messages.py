"""A failed or outdated preparation reads as what it is, not as a lost connection."""
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from prewarm_depot import error_summary, prepare, preparation_path, preparation_status  # noqa: E402
from unittest import mock  # noqa: E402


class ErrorSummaryTest(unittest.TestCase):
    def test_colour_codes_are_removed(self):
        self.assertEqual(error_summary("\x1b[91m\x1b[1mERROR:\x1b[22m\x1b[39m ZMQ failed\n"), "ERROR: ZMQ failed")

    def test_a_long_error_keeps_its_cause_and_its_end(self):
        uuid = "c2e4d1a0-0000-4000-8000-000000000001"
        text = "ERROR: The following 1 direct dependency failed to precompile:\n" + "x" * 50_000 + f"\nZMQ [{uuid}]"
        summary = error_summary(text)
        self.assertLess(len(summary), 4200)
        self.assertTrue(summary.startswith("ERROR: The following 1 direct dependency"))
        self.assertTrue(summary.endswith(f"ZMQ [{uuid}]"))
        self.assertIn("characters left out", summary)



class TimeoutTest(unittest.TestCase):
    def test_a_hung_prewarm_fails_with_its_output_so_far(self):
        with tempfile.TemporaryDirectory() as tmp:
            host = Path(tmp, "host")
            host.write_text("#!/bin/sh\necho 'Precompiling Stuck' >&2\nexec sleep 30\n")
            host.chmod(0o755)
            Path(tmp, "project").mkdir()
            with mock.patch.dict(os.environ, {"PALETTE_PACKAGE_DEPOT": str(Path(tmp, "packages"))}):
                record = prepare(str(Path(tmp, "project")), str(HERE.parents[1]), state_dir=str(Path(tmp, "state")),
                                 host_bin=str(host), timeout_s=1)
        self.assertEqual(record["state"], "failed")
        self.assertTrue(record["timed_out"])
        self.assertIn("did not finish in 1 s", record["error"])
        self.assertIn("Precompiling Stuck", record["error"])
        self.assertLess(record["seconds"], 10)

class StaleStatusTest(unittest.TestCase):
    def record(self, root, **fields):
        identity = {"project": "/p", "repo": "/r", "depot": str(root), "source": {"a.jl": "1"}}
        record = {"schema": "palette.preparation.v1", "state": "failed", "identity": identity,
                  "error": "old failure", "cache_files": {}, **fields}
        path = preparation_path(identity, str(root))
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(record))
        return identity

    def test_a_changed_source_drops_the_old_error_and_says_why(self):
        with tempfile.TemporaryDirectory() as root:
            identity = self.record(root)
            status = preparation_status({**identity, "source": {"a.jl": "2"}}, root)
            self.assertEqual(status["state"], "stale")
            self.assertIsNone(status["error"])
            self.assertIn("changed", status["reason"])

    def test_a_missing_cache_is_a_reason_not_an_error(self):
        with tempfile.TemporaryDirectory() as root:
            identity = self.record(root, state="prepared", error=None,
                                   cache_files={str(Path(root, "gone.ji")): "0"})
            status = preparation_status(identity, root)
            self.assertEqual((status["state"], status["error"]), ("stale", None))
            self.assertEqual(status["reason"], "prepared cache missing or changed")


if __name__ == "__main__":
    unittest.main()
