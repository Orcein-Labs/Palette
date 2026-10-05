"""host_command: operator-named host commands, run outside the sandbox.

Every case runs against both brokers -- broker.py in-process and the Rust
`palette-host broker` over its socket -- so the two stay in parity.
"""
from __future__ import annotations

import json
import os
import socket
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from broker import Broker, ceiling_is_subset  # noqa: E402
from host_adapter import HOST_BIN, _RustBrokerServer  # noqa: E402


def pid_dead(pid):
    for _ in range(100):
        try:
            stat = Path(f"/proc/{pid}/stat").read_text()
        except FileNotFoundError:
            return True
        if stat.rsplit(")", 1)[1].split()[0] == "Z":
            return True
        time.sleep(0.02)
    return False


class PythonBroker:
    def __init__(self, ceiling, tmp):
        self.b = Broker(ceiling, str(tmp / "receipts.jsonl"), "test")

    def __call__(self, params):
        return self.b.handle_request({"category": "host_command", "params": params})

    def close(self):
        self.b.close()


class RustBroker:
    def __init__(self, ceiling, tmp):
        self.sock = str(tmp / "broker.sock")
        self.server = _RustBrokerServer(self.sock, ceiling, str(tmp / "receipts.jsonl"), "test")

    def __call__(self, params):
        with socket.socket(socket.AF_UNIX) as s:
            s.connect(self.sock)
            s.sendall((json.dumps({"id": "t", "category": "host_command", "params": params}) + "\n").encode())
            return json.loads(s.makefile().readline())

    def close(self):
        self.server.shutdown()


class HostCommandParity:
    Broker = None

    def setUp(self):
        if self.Broker is RustBroker and not (HOST_BIN and Path(HOST_BIN).is_file()):
            raise unittest.SkipTest("no palette-host binary (set PALETTE_HOST_BIN)")
        self.tmp = Path(tempfile.mkdtemp(prefix="palette-hostcmd-")).resolve()
        d = str(self.tmp)
        self.b = self.Broker({"host_command": {"commands": {
            "echo": {"argv": ["/bin/echo", "fixed"], "cwd": d, "extra_args": True},
            "pwd": {"argv": ["/bin/pwd"], "cwd": d},
            "relative": {"argv": ["sh", "-c", "true"], "cwd": d},
            "hang": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $! > helper.pid; wait"], "cwd": d, "timeout_s": 0.5},
            "spawn": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $!"], "cwd": d, "timeout_s": 30},
            "job": {"argv": ["/bin/sh", "-c", "sleep 0.4; echo done"], "cwd": d},
            "long": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $! > long.pid; wait"], "cwd": d},
        }}}, self.tmp)

    def tearDown(self):
        self.b.close()

    def test_runs_a_named_command_with_fixed_argv_and_cwd(self):
        r = self.b({"name": "echo", "args": ["one", "two words"]})
        self.assertTrue(r["approved"], r)
        self.assertEqual(r["result"]["stdout"], "fixed one two words\n")
        self.assertEqual(r["result"]["returncode"], 0)
        self.assertEqual(self.b({"name": "pwd"})["result"]["stdout"].strip(), str(self.tmp))

    def test_refusals(self):
        def reason(params):
            return self.b(params).get("reason") or "APPROVED"
        self.assertIn("not in this session's ceiling", reason({"name": "rm"}))
        self.assertIn("takes no arguments", reason({"name": "pwd", "args": ["-P"]}))
        self.assertIn("list of strings", reason({"name": "pwd", "args": "-P"}))
        self.assertIn("absolute path", reason({"name": "relative"}))
        self.assertIn("run, start or poll", reason({"name": "pwd", "action": "sudo"}))

    def test_timeout_kills_the_whole_group(self):
        started = time.monotonic()
        r = self.b({"name": "hang"})
        self.assertTrue(r["result"]["timed_out"], r)
        self.assertLess(time.monotonic() - started, 5)
        self.assertTrue(pid_dead((self.tmp / "helper.pid").read_text().strip()))

    def test_helpers_do_not_outlive_a_finished_command(self):
        started = time.monotonic()
        r = self.b({"name": "spawn"})
        self.assertEqual(r["result"]["returncode"], 0, r)
        self.assertLess(time.monotonic() - started, 5)
        self.assertTrue(pid_dead(r["result"]["stdout"].strip()))

    def test_start_poll_and_session_end(self):
        job = self.b({"name": "job", "action": "start"})["result"]["job"]
        self.assertTrue(self.b({"action": "poll", "job": job})["result"]["running"])
        time.sleep(0.7)
        r = self.b({"action": "poll", "job": job})
        self.assertEqual(r["result"]["stdout"], "done\n", r)
        self.assertIn("no running", self.b({"action": "poll", "job": job})["reason"])
        self.b({"name": "long", "action": "start"})
        time.sleep(0.2)
        helper = (self.tmp / "long.pid").read_text().strip()
        self.b.close()
        self.assertTrue(pid_dead(helper), "a job outlived its session")


class PythonHostCommandTest(HostCommandParity, unittest.TestCase):
    Broker = PythonBroker


class RustHostCommandTest(HostCommandParity, unittest.TestCase):
    Broker = RustBroker


class CeilingSubsetTest(unittest.TestCase):
    def test_child_keeps_only_parent_commands_unchanged(self):
        parent = {"host_command": {"commands": {"render": {"argv": ["/bin/true"], "cwd": "/tmp"}}}}
        self.assertTrue(ceiling_is_subset({"host_command": {"commands": {}}}, parent)[0])
        self.assertTrue(ceiling_is_subset(parent, parent)[0])
        self.assertFalse(ceiling_is_subset({"host_command": {"commands": {"render": {"argv": ["/bin/sh"], "cwd": "/tmp"}}}}, parent)[0])
        self.assertFalse(ceiling_is_subset({"host_command": {"commands": {"sh": {"argv": ["/bin/sh"], "cwd": "/tmp"}}}}, parent)[0])
        self.assertFalse(ceiling_is_subset(parent, {})[0])


if __name__ == "__main__":
    unittest.main()
