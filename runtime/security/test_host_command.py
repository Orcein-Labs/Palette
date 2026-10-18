"""host_command: operator-named host commands, run outside the sandbox.

Every case runs against both brokers -- broker.py in-process and the Rust
`palette-host broker` over its socket -- so the two stay in parity.
"""
from __future__ import annotations

import json
import os
import select
import socket
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from broker import Broker, ceiling_is_subset  # noqa: E402
from host_adapter import HOST_BIN, _RustBrokerServer  # noqa: E402


def pid_dead(pid, timeout=5.0):
    """Whether `pid` has exited (a zombie counts), waiting on its pidfd: the
    kernel signals the descriptor when the process exits."""
    try:
        fd = os.pidfd_open(int(pid))
    except ProcessLookupError:
        return True
    try:
        return bool(select.select([fd], [], [], timeout)[0])
    finally:
        os.close(fd)


def read_fifo(path):
    """Blocks until a writer opens the FIFO and closes it: a deterministic
    'the command has reached this point'."""
    with open(path) as f:
        return f.read().strip()


def write_fifo(path, text="go"):
    """Blocks until the command opens the FIFO to read, then releases it."""
    with open(path, "w") as f:
        f.write(text + "\n")


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
        for fifo in ("go", "started", "long-ready"):
            os.mkfifo(self.tmp / fifo)
        d = str(self.tmp)
        self.b = self.Broker({"host_command": {"commands": {
            "echo": {"argv": ["/bin/echo", "fixed"], "cwd": d, "extra_args": True},
            "pwd": {"argv": ["/bin/pwd"], "cwd": d},
            "relative": {"argv": ["sh", "-c", "true"], "cwd": d},
            "hang": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $! > helper.pid; wait"], "cwd": d, "timeout_s": 0.5},
            "spawn": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $!"], "cwd": d, "timeout_s": 30},
            "job": {"argv": ["/bin/sh", "-c", "echo $$ > started; read line < go; echo done"], "cwd": d},
            "long": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $! > long-ready; wait"], "cwd": d},
            "ticks": {"argv": ["/bin/sh", "-c", "echo one; echo $$ > started; read line < go; echo two; echo err >&2"], "cwd": d},
            "loud": {"argv": ["/bin/sh", "-c", "head -c 40000 /dev/zero | tr '\\0' x; echo"], "cwd": d},
            "sleep": {"argv": ["/bin/sleep", "300"], "cwd": d, "timeout_s": 600},
            "quick": {"argv": ["/bin/true"], "cwd": d},
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
        self.assertIn("run, start, poll, cancel or list", reason({"name": "pwd", "action": "sudo"}))
        # A request may shorten a command's timeout, never extend it.
        self.assertIn("(0, 60]", reason({"name": "pwd", "timeout_s": 61}))
        self.assertIn("timeout_s", reason({"name": "pwd", "timeout_s": 0}))
        self.assertIn("timeout_s", reason({"name": "pwd", "timeout_s": "5"}))
        self.assertIn("timeout_s", reason({"name": "pwd", "timeout_s": True}))
        self.assertEqual(reason({"name": "pwd", "timeout_s": 5}), "APPROVED")
        self.assertIn("cancel requires a string 'job'", reason({"action": "cancel"}))

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
        shell = read_fifo(self.tmp / "started")          # the job is running, blocked on `go`
        self.assertTrue(self.b({"action": "poll", "job": job})["result"]["running"])
        write_fifo(self.tmp / "go")
        self.assertTrue(pid_dead(shell))
        r = self.b({"action": "poll", "job": job})
        self.assertEqual(r["result"]["stdout"], "done\n", r)
        self.assertIn("no running", self.b({"action": "poll", "job": job})["reason"])
        self.b({"name": "long", "action": "start"})
        helper = read_fifo(self.tmp / "long-ready")
        self.b.close()
        self.assertTrue(pid_dead(helper), "a job outlived its session")


    def test_poll_streams_output_since_the_last_poll(self):
        job = self.b({"name": "ticks", "action": "start"})["result"]["job"]
        read_fifo(self.tmp / "started")                  # "one" is written; the job waits on `go`
        seen, deadline = "", time.monotonic() + 10
        while seen != "one\n" and time.monotonic() < deadline:
            r = self.b({"action": "poll", "job": job})["result"]
            self.assertTrue(r["running"], r)
            self.assertEqual(r["name"], "ticks")
            seen += r["stdout"]
        self.assertEqual(seen, "one\n")
        self.assertEqual(self.b({"action": "poll", "job": job})["result"]["stdout"], "")
        write_fifo(self.tmp / "go")
        rest, err, deadline = "", "", time.monotonic() + 10
        while time.monotonic() < deadline:
            r = self.b({"action": "poll", "job": job})["result"]
            rest, err = rest + r["stdout"], err + r["stderr"]
            if not r["running"]:
                break
        self.assertFalse(r["running"], r)
        self.assertEqual(rest, "two\n", "the final result repeated output a poll had returned")
        self.assertEqual(err, "err\n")
        self.assertFalse(r["cancelled"])

    def test_cancel_kills_the_job_group_and_list_names_jobs(self):
        job = self.b({"name": "long", "action": "start"})["result"]["job"]
        helper = read_fifo(self.tmp / "long-ready")
        listed = self.b({"action": "list"})["result"]["jobs"]
        self.assertEqual([(j["job"], j["name"], j["running"]) for j in listed], [(job, "long", True)])
        r = self.b({"action": "cancel", "job": job})["result"]
        self.assertTrue(r["cancelled"], r)
        self.assertFalse(r["running"])
        self.assertFalse(r["timed_out"])
        self.assertTrue(pid_dead(helper), "a cancelled job's helper survived")
        self.assertEqual(self.b({"action": "list"})["result"]["jobs"], [])
        self.assertIn("no running", self.b({"action": "cancel", "job": job})["reason"])
        # Cancelling a job that already finished reports it finished, not cancelled.
        job = self.b({"name": "quick", "action": "start"})["result"]["job"]
        deadline = time.monotonic() + 10
        while self.b({"action": "list"})["result"]["jobs"][0]["running"] and time.monotonic() < deadline:
            pass                                         # each broker round trip paces the loop
        r = self.b({"action": "cancel", "job": job})["result"]
        self.assertEqual((r["cancelled"], r["returncode"]), (False, 0), r)

    def test_reports_output_the_tail_dropped(self):
        r = self.b({"name": "loud"})["result"]
        self.assertEqual(len(r["stdout"]), 16 * 1024)
        self.assertEqual(r["stdout_skipped_bytes"], 40001 - 16 * 1024)
        self.assertNotIn("stderr_skipped_bytes", r)

    def test_request_timeout_shortens_the_ceiling_timeout(self):
        started = time.monotonic()
        r = self.b({"name": "sleep", "timeout_s": 0.5})
        self.assertTrue(r["result"]["timed_out"], r)
        self.assertLess(time.monotonic() - started, 5)


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
