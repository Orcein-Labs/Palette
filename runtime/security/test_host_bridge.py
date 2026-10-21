#!/usr/bin/env python3
"""
Mid-call host requests: the kernel asks the agent host to act (an RLM child,
for instance) while a call runs, through its broker, and only for the request
types its ceiling names. These tests play the host on the session bridge's stdio.

Run:
    python3 runtime/security/test_host_bridge.py
"""
from __future__ import annotations

import json
import os
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from host_adapter import B, session_cmd  # noqa: E402

from test_session_cli import PROJECT_DIR, _skip_if_no_bwrap, _skip_if_no_project  # noqa: E402

RLM_TYPES = ["rlm.run", "rlm.collect", "rlm.list_subagents"]
CHILD = {"rlm_child_id": "c1", "name": "w", "session_dir": "/s/c1", "model": "openai/luna"}


def _ok(result):
    return {"status": "ok", "result": result}


class Host:
    """The session bridge with a given ceiling, driven the way the agent host drives it."""

    def __init__(self, ceiling: dict, turn_timeout: float = 20):
        self.workspace = tempfile.mkdtemp(prefix="palette-bridge-")
        self.proc = subprocess.Popen(
            [*session_cmd(), "--project-dir", PROJECT_DIR, "--ceiling", json.dumps(ceiling),
             "--workspace-dir", self.workspace, "--turn-timeout", str(turn_timeout)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1,
        )
        try:
            hello = json.loads(self.proc.stdout.readline())
            assert hello.get("kind") == "HELLO", hello
        except BaseException:
            self.close()
            raise
        self.n = 0

    def send(self, msg: dict) -> None:
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def read(self, timeout: float = 60) -> dict | None:
        ready, _, _ = select.select([self.proc.stdout], [], [], timeout)
        if not ready:
            return None
        line = self.proc.stdout.readline()
        return json.loads(line) if line else None

    def call(self, code: str, answer=lambda data: None, **extra) -> tuple[dict, list]:
        """Run one call; answer each host request with `answer(data)` (None: no reply).
        Returns the call's response and the requests seen."""
        self.n += 1
        rid = f"r{self.n}"
        self.send({"request_id": rid, "code": code, **extra})
        seen = []
        while True:
            msg = self.read()
            assert msg is not None, "no response"
            if msg.get("event") == "host_request":
                seen.append(msg["data"])
                reply = answer(msg["data"])
                if reply is not None:
                    self.send({"host_reply": msg["id"], "reply": reply})
                continue
            assert msg.get("request_id") == rid, msg
            return msg, seen

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        finally:
            self.proc.stdout.close()
            self.proc.stderr.close()
            shutil.rmtree(self.workspace, ignore_errors=True)


def _rlm_host(data):
    if data["type"] == "rlm.run":
        return _ok(dict(CHILD, name=data["kwargs"]["name"]))
    if data["type"] == "rlm.collect":
        return _ok({"results": [{"rlm_child_id": "c1", "status": "done", "settled": True, "answer_preview": "42"}]})
    return {"status": "error", "error": f"no handler for {data['type']}"}


class TestHostBridge(unittest.TestCase):
    def setUp(self):
        _skip_if_no_bwrap()
        _skip_if_no_project()
        self.hosts = []

    def tearDown(self):
        for h in self.hosts:
            h.close()

    def host(self, ceiling, **kw):
        h = Host(ceiling, **kw)
        self.hosts.append(h)
        return h

    def test_spawn_and_collect_in_one_call(self):
        h = self.host({"host_request": {"allowed_types": RLM_TYPES}})
        r, seen = h.call('h = Palette.rlm.spawn("add 40 and 2"; name = "w"); '
                         'r = only(Palette.rlm.collect(h; timeout_ms = 1000)); (h.rlm_child_id, r.status, r.answer_preview)',
                         _rlm_host)
        self.assertTrue(r["success"], r)
        self.assertIn('("c1", "done", "42")', r["display"])
        self.assertEqual([d["type"] for d in seen], ["rlm.run", "rlm.collect"])
        self.assertEqual(seen[0]["prompt"], "add 40 and 2")
        self.assertEqual(seen[1]["targets"], ["c1"])
        self.assertEqual(seen[1]["timeout_ms"], 1000)

    def test_spawn_now_collect_in_a_later_call_and_from_a_background_task(self):
        h = self.host({"host_request": {"allowed_types": RLM_TYPES}})
        r, _ = h.call('h = Palette.rlm.spawn("x"; name = "w"); h.name', _rlm_host)
        self.assertTrue(r["success"], r)
        r, seen = h.call('only(Palette.rlm.collect(h)).answer_preview', _rlm_host)
        self.assertIn('"42"', r["display"])
        # A background collect whose request arrives after its call returned:
        # the host answers between calls, and the next call fetches it.
        r, seen = h.call('gate = Channel{Nothing}(1); job = @async (take!(gate); only(Palette.rlm.collect(h)).status); :started')
        self.assertTrue(r["success"], r)
        h.send({"request_id": "release", "code": "put!(gate, nothing); :released"})
        messages = [h.read(timeout=30), h.read(timeout=30)]
        self.assertTrue(all(messages), messages)
        msg = next(m for m in messages if m.get("event") == "host_request")
        self.assertTrue(next(m for m in messages if m.get("request_id") == "release")["success"])
        self.assertEqual(msg.get("event"), "host_request", msg)
        h.send({"host_reply": msg["id"], "reply": _rlm_host(msg["data"])})
        r, _ = h.call("fetch(job)")
        self.assertIn('"done"', r["display"])

    def test_denied_without_the_permission_and_nothing_reaches_the_host(self):
        h = self.host({})
        r, seen = h.call('Palette.rlm.spawn("x"; name = "w")', _rlm_host)
        self.assertFalse(r["success"])
        self.assertIn("host_request not in this session's ceiling", json.dumps(r))
        self.assertEqual(seen, [])

    def test_a_type_outside_allowed_types_is_denied(self):
        h = self.host({"host_request": {"allowed_types": ["rlm.collect"]}})
        r, seen = h.call('Palette.rlm.spawn("x"; name = "w")', _rlm_host)
        self.assertFalse(r["success"])
        self.assertIn("'rlm.run' is not in this session's allowed_types", json.dumps(r))
        self.assertEqual(seen, [])
        r, seen = h.call('Palette.host_request("rlm.collect", Dict("type" => "rlm.run", "targets" => []))',
                         lambda d: _ok({"results": []}))
        self.assertTrue(r["success"], r)
        self.assertEqual(seen[0]["type"], "rlm.collect", "a payload key must not reroute the request")

    def test_the_hosts_error_and_a_malformed_reply_reach_julia_truthfully(self):
        h = self.host({"host_request": {"allowed_types": RLM_TYPES}})
        r, _ = h.call('Palette.rlm.spawn("x"; name = "w")', lambda d: {"status": "error", "error": "name 'w' is taken"})
        self.assertFalse(r["success"])
        self.assertIn("name 'w' is taken", json.dumps(r))
        r, _ = h.call('Palette.rlm.spawn("x"; name = "w")', lambda d: {"surprise": True})
        self.assertFalse(r["success"])
        self.assertIn("malformed reply", json.dumps(r))

    def test_an_unanswered_request_times_out_and_a_late_reply_is_dropped(self):
        h = self.host({"host_request": {"allowed_types": RLM_TYPES}}, turn_timeout=8)
        ids = []
        r, seen = h.call('Palette.rlm.spawn("x"; name = "w")', lambda d: ids.append(d) or None)
        self.assertFalse(r["success"])
        self.assertIn("did not answer 'rlm.run' within 3s", json.dumps(r))
        self.assertFalse(r.get("session_dead"), "the kernel must survive an unanswered request")
        h.send({"host_reply": "no-such-request", "reply": _ok(CHILD)})
        r, _ = h.call("1 + 1")
        self.assertTrue(r["success"], r)
        self.assertIn("2", r["display"])

    def test_the_host_closing_its_end_fails_a_waiting_request_and_the_bridge_exits(self):
        h = self.host({"host_request": {"allowed_types": RLM_TYPES}})
        h.send({"request_id": "r1", "code": 'Palette.rlm.spawn("x"; name = "w")'})
        msg = h.read()
        self.assertEqual(msg.get("event"), "host_request")
        h.proc.stdin.close()
        t0 = time.time()
        h.proc.wait(timeout=60)
        self.assertLess(time.time() - t0, 60)

    def test_ephemeral_code_cannot_reach_the_host(self):
        h = self.host({"host_request": {"allowed_types": RLM_TYPES}})
        r, seen = h.call('Palette.rlm.spawn("x"; name = "w")', _rlm_host, ephemeral=True,
                         ceiling={"host_request": {"allowed_types": ["rlm.run"]}})
        self.assertFalse(r["success"])
        self.assertIn("no agent host is attached", json.dumps(r))
        self.assertEqual(seen, [], "an ephemeral worker must not reach the agent host")


class TestBrokerHostRequest(unittest.TestCase):
    """The broker's host_request category on its own: a standalone broker has
    no agent host attached, so these check the policy and the receipts."""

    def request(self, sock, category, params):
        with socket.socket(socket.AF_UNIX) as s:
            s.connect(sock)
            s.sendall((json.dumps({"id": "rq", "category": category, "params": params}) + "\n").encode())
            with s.makefile() as stream:
                return json.loads(stream.readline())

    def test_policy_and_receipts_without_a_host(self):
        d = tempfile.mkdtemp(prefix="njl-hr-")
        sock, receipts = os.path.join(d, "broker.sock"), os.path.join(d, "receipts.jsonl")
        server, _ = B.serve(sock, {"host_request": {"allowed_types": ["rlm.run"]}}, receipts, "s1")
        try:
            r = self.request(sock, "host_request", {"type": "rlm.run", "payload": {}})
            self.assertFalse(r["approved"])
            self.assertIn("no agent host is attached", r["reason"])
            r = self.request(sock, "host_request", {"type": "rlm.delete_subagent", "payload": {}})
            self.assertIn("'rlm.delete_subagent' is not in this session's allowed_types", r["reason"])
            r = self.request(sock, "host_request", {"type": "rlm.run", "payload": [1]})
            self.assertIn("'payload' must be an object", r["reason"])
            # A request cannot attach a host of its own.
            r = self.request(sock, "host_request", {"type": "rlm.run", "payload": {}, "host_bridge": "x"})
            self.assertFalse(r["approved"])
        finally:
            server.shutdown()
            server.server_close()
            rs = [json.loads(l) for l in Path(receipts).read_text().splitlines()]
            shutil.rmtree(d)
        self.assertEqual(len(rs), 4, "every request is receipted")
        self.assertTrue(all(r["category"] == "host_request" and r["approved"] is False for r in rs))

    def test_without_the_category_nothing_is_allowed(self):
        d = tempfile.mkdtemp(prefix="njl-hr-")
        sock = os.path.join(d, "broker.sock")
        server, _ = B.serve(sock, {}, os.path.join(d, "receipts.jsonl"), "s1")
        try:
            r = self.request(sock, "host_request", {"type": "rlm.run", "payload": {}})
            self.assertIn("host_request not in this session's ceiling", r["reason"])
        finally:
            server.shutdown()
            server.server_close()
            shutil.rmtree(d)

    def test_child_types_must_be_an_explicit_subset(self):
        parent = {"host_request": {"allowed_types": ["rlm.run", "rlm.collect"]}}
        self.assertTrue(B.ceiling_is_subset({"host_request": {"allowed_types": ["rlm.collect"]}}, parent)[0])
        self.assertFalse(B.ceiling_is_subset({"host_request": {"allowed_types": ["rlm.delete_subagent"]}}, parent)[0])
        self.assertFalse(B.ceiling_is_subset({"host_request": {}}, parent)[0])
        self.assertFalse(B.ceiling_is_subset({"host_request": {"allowed_types": ["rlm.run"]}}, {})[0])


if __name__ == "__main__":
    unittest.main()
