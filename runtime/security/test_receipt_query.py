#!/usr/bin/env python3
"""
Tests for the receipt query (receipt_query.py): join a kernel call to the
host receipts it caused, by session_request, without a kernel.

Run:
    cd runtime/security && python3 -m unittest test_receipt_query -v
"""
from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import receipt_query  # noqa: E402


def receipt(request_id, session_request, timestamp=1.0, category="fs_digest", receipt_id=None):
    return {"receipt_id": receipt_id or f"r-{request_id}-{timestamp}", "request_id": request_id,
            "session_id": "s", "timestamp": timestamp, "category": category, "params": {},
            "approved": True, "result": None, "reason": None, "session_request": session_request}


def call(epoch, session_request, n, started=1.0, finished=2.0):
    return {"session_id": "s", "epoch": epoch, "session_request": session_request, "kind": "EXECUTE",
            "call": n, "started": started, "finished": finished, "success": True}


def write_jsonl(path, records, raw_lines=()):
    lines = [json.dumps(r) for r in records] + list(raw_lines)
    path.write_text("".join(line + "\n" for line in lines))


class ReceiptQueryTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def write_receipts(self, records, raw_lines=()):
        write_jsonl(self.dir / "receipts.jsonl", records, raw_lines)

    def write_calls(self, records, raw_lines=()):
        write_jsonl(self.dir / "calls.jsonl", records, raw_lines)

    def test_call_join_keeps_only_receipts_of_that_call(self):
        self.write_receipts([
            receipt(1, "E1:1", 1.0),
            receipt(2, "E1:2", 2.0),
            receipt(3, "E1:2", 3.0),
            receipt(4, None, 4.0),
        ])
        self.write_calls([call("E1", "E1:2", 2)])
        out = receipt_query.query(self.dir, call=2)
        self.assertEqual([r["request_id"] for r in out["receipts"]], [2, 3])
        self.assertEqual(out["matched"], 2)
        self.assertEqual(out["returned"], 2)
        self.assertEqual(out["call"]["session_request"], "E1:2")
        self.assertEqual(out["unreadable_lines"], 0)
        self.assertIn("attributed to the call in flight", out["note"])

    def test_epoch_disambiguates_same_call_number(self):
        self.write_receipts([
            receipt(1, "E1:5", 1.0),
            receipt(2, "E2:5", 2.0),
        ])
        self.write_calls([
            call("E1", "E1:5", 2),
            call("E2", "E2:5", 2),
        ])
        first = receipt_query.query(self.dir, call=2, epoch="E1")
        self.assertEqual([r["session_request"] for r in first["receipts"]], ["E1:5"])
        self.assertEqual(first["call"]["epoch"], "E1")
        second = receipt_query.query(self.dir, call=2, epoch="E2")
        self.assertEqual([r["session_request"] for r in second["receipts"]], ["E2:5"])
        self.assertEqual(second["call"]["epoch"], "E2")

    def test_call_without_epoch_takes_the_last_such_record(self):
        self.write_receipts([receipt(1, "E1:5", 1.0), receipt(2, "E2:5", 2.0)])
        self.write_calls([call("E1", "E1:5", 2), call("E2", "E2:5", 2)])
        out = receipt_query.query(self.dir, call=2)
        self.assertEqual(out["call"]["epoch"], "E2")
        self.assertEqual([r["session_request"] for r in out["receipts"]], ["E2:5"])

    def test_kind_filter(self):
        self.write_receipts([
            receipt(1, "E1:1", 1.0, category="fs_digest"),
            receipt(2, "E1:1", 2.0, category="external_fs_write"),
            receipt(3, "E1:1", 3.0, category="fs_digest"),
        ])
        out = receipt_query.query(self.dir, kind="fs_digest")
        self.assertEqual([r["request_id"] for r in out["receipts"]], [1, 3])
        self.assertIsNone(out["call"])

    def test_call_and_kind_combine(self):
        self.write_receipts([
            receipt(1, "E1:1", 1.0, category="fs_digest"),
            receipt(2, "E1:1", 2.0, category="external_fs_write"),
            receipt(3, "E1:9", 3.0, category="fs_digest"),
        ])
        self.write_calls([call("E1", "E1:1", 1)])
        out = receipt_query.query(self.dir, call=1, kind="external_fs_write")
        self.assertEqual([r["request_id"] for r in out["receipts"]], [2])

    def test_time_bounds_are_inclusive(self):
        self.write_receipts([
            receipt(1, "E1:1", 10.0),
            receipt(2, "E1:1", 20.0),
            receipt(3, "E1:1", 30.0),
            receipt(4, "E1:1", 40.0),
        ])
        out = receipt_query.query(self.dir, since=20, until=30)
        self.assertEqual([r["request_id"] for r in out["receipts"]], [2, 3])
        out = receipt_query.query(self.dir, since=20.5)
        self.assertEqual([r["request_id"] for r in out["receipts"]], [3, 4])
        out = receipt_query.query(self.dir, until=10)
        self.assertEqual([r["request_id"] for r in out["receipts"]], [1])

    def test_limit_returns_the_tail_and_reports_totals(self):
        self.write_receipts([receipt(i, "E1:1", float(i)) for i in range(1, 8)])
        out = receipt_query.query(self.dir, limit=3)
        self.assertEqual([r["request_id"] for r in out["receipts"]], [5, 6, 7])
        self.assertEqual(out["matched"], 7)
        self.assertEqual(out["returned"], 3)
        out = receipt_query.query(self.dir, limit=500)
        self.assertEqual(out["returned"], 7)
        self.assertEqual(receipt_query.query(self.dir, limit=1)["receipts"][0]["request_id"], 7)

    def test_unreadable_lines_are_counted_and_skipped(self):
        self.write_receipts(
            [receipt(1, "E1:1", 1.0)],
            raw_lines=["not json", "[1, 2]", "42", '"text"', "{truncated"],
        )
        self.write_calls(
            [call("E1", "E1:1", 1)],
            raw_lines=["garbage"],
        )
        out = receipt_query.query(self.dir, call=1)
        self.assertEqual([r["request_id"] for r in out["receipts"]], [1])
        self.assertEqual(out["unreadable_lines"], 6)

    def test_missing_files_are_empty(self):
        out = receipt_query.query(self.dir)
        self.assertEqual(out["receipts"], [])
        self.assertEqual(out["matched"], 0)
        self.assertEqual(out["returned"], 0)
        self.assertIsNone(out["call"])
        self.assertEqual(out["unreadable_lines"], 0)

    def test_missing_directory_is_empty(self):
        out = receipt_query.query(self.dir / "does-not-exist", kind="fs_digest")
        self.assertEqual(out["receipts"], [])
        self.assertEqual(out["matched"], 0)

    def test_call_with_no_calls_file_is_an_error(self):
        self.write_receipts([receipt(1, "E1:1", 1.0)])
        with self.assertRaisesRegex(ValueError, r"^no record of call 1$"):
            receipt_query.query(self.dir, call=1)

    def test_unknown_call_error_names_the_epoch(self):
        self.write_calls([call("E1", "E1:1", 1)])
        with self.assertRaisesRegex(ValueError, r"^no record of call 7$"):
            receipt_query.query(self.dir, call=7)
        with self.assertRaisesRegex(ValueError, r"^no record of call 1 in epoch E9$"):
            receipt_query.query(self.dir, call=1, epoch="E9")

    def test_bad_limit_is_an_error(self):
        for bad in (0, -1, 501, True, "5", 2.0, None):
            with self.subTest(limit=bad), self.assertRaises(ValueError):
                receipt_query.query(self.dir, limit=bad)

    def test_bad_argument_types_are_errors(self):
        cases = [
            {"call": True}, {"call": "1"}, {"call": 1.0},
            {"epoch": 1}, {"kind": b"fs_digest"},
            {"since": "1"}, {"since": True}, {"until": False}, {"until": [1]},
        ]
        for kwargs in cases:
            with self.subTest(**kwargs), self.assertRaises(ValueError):
                receipt_query.query(self.dir, **kwargs)

    def test_since_and_until_accept_floats(self):
        self.write_receipts([receipt(1, "E1:1", 1.5), receipt(2, "E1:1", 2.5)])
        out = receipt_query.query(self.dir, since=1.5, until=2.0)
        self.assertEqual([r["request_id"] for r in out["receipts"]], [1])

    def test_query_does_not_modify_the_files(self):
        self.write_receipts([receipt(1, "E1:1", 1.0)], raw_lines=["junk"])
        self.write_calls([call("E1", "E1:1", 1)])
        before = {p.name: p.read_bytes() for p in self.dir.iterdir()}
        receipt_query.query(self.dir, call=1, limit=10)
        after = {p.name: p.read_bytes() for p in self.dir.iterdir()}
        self.assertEqual(before, after)


if __name__ == "__main__":
    unittest.main()
