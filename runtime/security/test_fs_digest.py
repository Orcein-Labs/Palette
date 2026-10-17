#!/usr/bin/env python3
"""
The broker's fs_digest category: a host-side digest the sandbox cannot forge.

Direct Broker-class tests (no sockets): digest correctness, ceiling denial,
path-scope denial, per-path filesystem conditions, and receipts written
regardless of outcome (kernel receipts die with the kernel; host receipts
do not).

Run:
    python3 runtime/security/test_fs_digest.py
"""
from __future__ import annotations

import hashlib
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from broker import Broker  # noqa: E402


class FsDigestTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="fs-digest-test-")
        self.receipts = os.path.join(self.tmp, "receipts.jsonl")
        self.dir = os.path.join(self.tmp, "scoped")
        os.makedirs(self.dir)
        self.f1 = os.path.join(self.dir, "a.txt")
        self.f2 = os.path.join(self.dir, "b.bin")
        with open(self.f1, "wb") as fh:
            fh.write(b"hello digest\n")
        with open(self.f2, "wb") as fh:
            fh.write(bytes(range(256)) * 100)  # 25.6 KB — spans read chunks
        self.ceiling = {"fs_digest": {"allowed_paths": [self.dir]}}
        self.broker = Broker(ceiling=self.ceiling, receipt_log_path=self.receipts, session_id="t")

    def _expected(self, path):
        h = hashlib.sha256()
        with open(path, "rb") as fh:
            for chunk in iter(lambda: fh.read(1024 * 1024), b""):
                h.update(chunk)
        return h.hexdigest()

    def test_digest_correct_for_files(self):
        resp = self.broker.handle_request(
            {"category": "fs_digest", "params": {"paths": [self.f1, self.f2]}})
        self.assertTrue(resp["approved"])
        d = resp["result"]["digests"]
        self.assertEqual(d[self.f1]["sha256"], self._expected(self.f1))
        self.assertEqual(d[self.f2]["sha256"], self._expected(self.f2))
        self.assertEqual(d[self.f1]["bytes"], os.path.getsize(self.f1))
        self.assertEqual(resp["result"]["observer"], "host")
        self.assertEqual(resp["result"]["algorithm"], "sha256")

    def test_per_path_conditions_not_request_failures(self):
        missing = os.path.join(self.dir, "missing.txt")
        resp = self.broker.handle_request(
            {"category": "fs_digest", "params": {"paths": [missing, self.dir]}})
        self.assertTrue(resp["approved"])
        d = resp["result"]["digests"]
        self.assertEqual(d[missing]["error_class"], "not_found")
        self.assertEqual(d[self.dir]["error_class"], "is_directory")

    def test_out_of_scope_path_denies_whole_request(self):
        outside = self.tmp  # exists, but NOT under allowed_paths (self.dir)
        resp = self.broker.handle_request(
            {"category": "fs_digest", "params": {"paths": [outside]}})
        self.assertFalse(resp["approved"])
        self.assertIn("allowed_paths", resp["reason"])

    def test_category_absent_from_ceiling_denies(self):
        broker = Broker(ceiling={}, receipt_log_path=self.receipts, session_id="t2")
        resp = broker.handle_request(
            {"category": "fs_digest", "params": {"paths": [self.f1]}})
        self.assertFalse(resp["approved"])

    def test_child_ceiling_cannot_widen_scope(self):
        child_cap = {"fs_digest": {"allowed_paths": [self.tmp]}}  # wider than parent
        ok, reason = __import__("broker").ceiling_is_subset(
            child_cap, {"fs_digest": {"allowed_paths": [self.dir]}})
        self.assertFalse(ok)

    def test_receipt_written_regardless_of_outcome(self):
        self.broker.handle_request(
            {"category": "fs_digest", "params": {"paths": [self.f1]}})
        broker_deny = Broker(ceiling={}, receipt_log_path=self.receipts, session_id="t3")
        broker_deny.handle_request(
            {"category": "fs_digest", "params": {"paths": [self.f1]}})
        with open(self.receipts) as fh:
            records = [json.loads(line) for line in fh if line.strip()]
        outcomes = [(r["category"], r["approved"]) for r in records
                    if r["category"] == "fs_digest"]
        self.assertIn(("fs_digest", True), outcomes)
        self.assertIn(("fs_digest", False), outcomes)


if __name__ == "__main__":
    unittest.main()
