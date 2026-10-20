#!/usr/bin/env python3
"""
Tests for Execution X-Ray: Palette.xray(n) in the kernel, and the host
receipts of call n (receipt_query.py, palette_control action=receipts).
The Rust host stamps the receipts, so `cargo xtask verify` runs these on it only.

The join is the point: every capability request a call made appears in the
host's receipts for that call, and no receipt is attributed to a call that did
not make it.

Run:
    PALETTE_HOST_BIN=runtime/host/target/release/palette-host \\
    PALETTE_TEST_PROJECT_DIR=~/.palette-trial/project python3 runtime/security/test_xray.py
"""
from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import receipt_query  # noqa: E402
from test_revival import KernelTestCase  # noqa: E402

DIGEST = 'Palette.fs_digest([joinpath(pwd(), "a.txt")])'
DENIED = 'Palette.request_capability("network_access", Dict{String,Any}("host" => "example.com", "port" => 443))'

# Values the X-Ray bookkeeping could disturb if it leaked into the call: top-level
# loops and globals, exceptions, @timed (it nests with the compile timer X-Ray
# holds open), task-local state, and a closure compiled inside the call.
SAME_AS_PLAIN_JULIA = [
    'repr(sum(abs2, 1:10^5))',
    'acc = Ref(0); for i in 1:1000; acc[] += i; end; repr(acc[])',
    'repr(try error("boom") catch e; sprint(showerror, e) end)',
    't = @timed sum(1:10); repr((t.value, t.compile_time >= 0))',
    'repr(task_local_storage(:palette_probe, 7) do; task_local_storage(:palette_probe) * 6 end)',
    'h = x -> (x, typeof(x)); repr(map(h, (1, 2.0f0, "s")))',
    'repr(Base.Math.sincos(0.5))',
]


class XRayTest(KernelTestCase):
    def setUp(self):
        super().setUp()
        self.receipts = tempfile.mkdtemp(prefix="palette-xray-receipts-")
        self.addCleanup(shutil.rmtree, self.receipts, True)
        Path(self.ws, "a.txt").write_text("x\n")

    def kernel(self, state=None):
        return super().kernel(state, extra_args=["--receipts-dir", self.receipts],
                              ceiling={"fs_digest": {"allowed_paths": [self.ws]}})

    def test_every_request_of_a_call_joins_its_receipts(self):
        k = self.kernel()
        try:
            r1 = k.turn(f'{DIGEST}; {DENIED}; xs = zeros(10^6); sum(xs)')
            r2 = k.turn(DIGEST)
            x1 = k.turn(f'Palette.xray({r1["call"]})')
            x2 = k.turn(f'Palette.xray({r2["call"]})')
            epoch = k.hello["epoch"]
        finally:
            k.close()
        self.assertTrue(r1["success"], r1)
        self.assertEqual(r1["data"], 0.0)   # instrumentation does not change the result
        for r, x, n in ((r1, x1, 2), (r2, x2, 1)):
            self.assertTrue(x["success"], x)
            xray = x["data"]
            asked = [q["id"] for q in xray["capability_requests"]]
            self.assertEqual(len(asked), n, xray)
            found = receipt_query.query(self.receipts, call=r["call"], epoch=epoch)
            self.assertEqual([q["request_id"] for q in found["receipts"]], asked, found)   # 100% join, both ways
            self.assertEqual(found["call"]["session_request"], xray["session_request"])
        approved = {q["category"]: q["approved"] for q in x1["data"]["capability_requests"]}
        self.assertFalse(approved["network_access"])   # a denied request is receipted too
        seconds = x1["data"]["seconds"]
        for k_ in ("execution", "compile", "gc", "run"):
            self.assertIsInstance(seconds[k_], float, seconds)
        self.assertGreaterEqual(x1["data"]["allocated_bytes"], 8 * 10**6)
        self.assertIn("process-wide", x1["data"]["limits"])

    def test_instrumentation_does_not_change_results(self):
        plain = subprocess.run(["julia", "--startup-file=no", "-e",
                                "for c in ARGS; print(include_string(Main, c), '\\0'); end", *SAME_AS_PLAIN_JULIA],
                               capture_output=True, text=True, timeout=300)
        self.assertEqual(plain.returncode, 0, plain.stderr)
        expected = plain.stdout.split("\0")[:-1]
        self.assertEqual(len(expected), len(SAME_AS_PLAIN_JULIA), plain.stdout)
        k = self.kernel()
        try:
            got = [k.turn(code) for code in SAME_AS_PLAIN_JULIA]
            traced = [k.turn(code, context="traced") for code in SAME_AS_PLAIN_JULIA]
        finally:
            k.close()
        for r in got + traced:
            self.assertTrue(r["success"], r)
        self.assertEqual([r["data"] for r in got], expected)
        self.assertEqual([r["data"] for r in traced], expected)

    def test_call_record_carries_the_context(self):
        k = self.kernel()
        try:
            calls = [k.turn("1 + 1"), k.turn("1 + 1", context="traced"), k.turn("Palette.context!(:memoized)"),
                     k.turn("1 + 1"), k.turn("1 + 1", context="normal")]
            epoch = k.hello["epoch"]
        finally:
            k.close()
        contexts = [receipt_query.query(self.receipts, call=r["call"], epoch=epoch)["call"]["context"] for r in calls]
        self.assertEqual(contexts, ["normal", "traced", "normal", "memoized", "normal"])

    def test_call_numbers_restart_per_kernel(self):
        k = self.kernel()
        try:
            first = k.turn(DIGEST)
            e1 = k.hello["epoch"]
        finally:
            k.close()
        k = self.kernel(state=tempfile.mkdtemp(dir=self.state))   # a fresh state dir: a revived kernel would continue the numbering
        try:
            second = k.turn('nothing')
            e2 = k.hello["epoch"]
        finally:
            k.close()
        self.assertEqual(first["call"], second["call"])
        self.assertEqual(len(receipt_query.query(self.receipts, call=first["call"], epoch=e1)["receipts"]), 1)
        self.assertEqual(receipt_query.query(self.receipts, call=second["call"], epoch=e2)["receipts"], [])

    def test_unknown_call_is_an_error(self):
        k = self.kernel()
        try:
            r = k.turn('Palette.xray(99)')
        finally:
            k.close()
        self.assertFalse(r["success"])
        self.assertIn("99", r["error"])


if __name__ == "__main__":
    unittest.main()
