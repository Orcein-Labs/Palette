"""Process-level proof that the daemon outlives its disposable socket clients."""
from __future__ import annotations

import json
import os
from pathlib import Path
import selectors
import socket
import stat
import subprocess
import tempfile
import threading
import unittest

REPO = Path(__file__).resolve().parents[2]
HOST = REPO / "runtime/host/target/release/palette-host"


def descendants(pid):
    try:
        children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
    except OSError:
        return []
    result = [int(child) for child in children]
    for child in tuple(result):
        result.extend(descendants(child))
    return result


def julia_pid(root_pid):
    for pid in descendants(root_pid):
        try:
            command = Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\0", b" ")
        except OSError:
            continue
        if b"runtime/scripts/session_loop.jl" in command:
            return pid
    raise AssertionError("Julia session process is absent from the daemon's workspace process tree")


class PaletteDaemonProcess(unittest.TestCase):
    def setUp(self):
        self.assertTrue(HOST.is_file(), "Build release palette-host before running daemon integration tests")
        self.tmp = tempfile.TemporaryDirectory(prefix="palette-daemon-")
        root = Path(self.tmp.name)
        self.project = root / "project"
        self.project.mkdir()
        self.sockpath = root / "run/palette/daemon.sock"
        self.env = {**os.environ, "PALETTE_REPO": str(REPO), "PALETTE_HOST": str(HOST),
                    "OPERATOR_WORKSPACE": str(self.project), "PALETTE_STATE_DIR": str(root / "legacy"),
                    "PALETTE_WORKSPACE_STATE_ROOT": str(root / "worlds"),
                    "PALETTE_SCRATCH_ROOT": str(root / "scratch")}
        self.start_daemon()

    def start_daemon(self):
        self.proc = subprocess.Popen([str(HOST), "mcp", "--socket", str(self.sockpath)],
                                     cwd=self.project, env=self.env, stdout=subprocess.DEVNULL,
                                     stderr=subprocess.PIPE, text=True)
        with selectors.DefaultSelector() as selector:
            selector.register(self.proc.stderr, selectors.EVENT_READ)
            self.assertTrue(selector.select(15), "daemon readiness deadline expired")
        ready = self.proc.stderr.readline()
        self.assertIn("Palette daemon listening", ready)
        with socket.socket(socket.AF_UNIX) as probe:
            probe.connect(str(self.sockpath))

    def request(self, name, args):
        frame = self.request_frame(name, args)
        self.assertIn("result", frame, frame)
        result = frame["result"]
        self.assertFalse(result.get("isError"), result)
        return json.loads(result["content"][0]["text"])

    def request_frame(self, name, args):
        with socket.socket(socket.AF_UNIX) as conn:
            conn.connect(str(self.sockpath))
            stream = conn.makefile("rwb", buffering=0)
            msg = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                   "params": {"name": name, "arguments": args}}
            stream.write((json.dumps(msg) + "\n").encode())
            return json.loads(stream.readline())

    def stop_daemon(self):
        self.proc.terminate()
        self.proc.wait(timeout=60)
        self.assertEqual(self.proc.returncode, 128 + 15)
        self.proc.stderr.close()

    def tearDown(self):
        if getattr(self, "proc", None) and self.proc.poll() is None:
            self.proc.terminate()
            self.proc.wait(timeout=60)
        if getattr(self, "proc", None) and self.proc.stderr:
            self.proc.stderr.close()
        self.tmp.cleanup()

    def test_disconnect_restart_and_revive(self):
        daemon_pid = self.proc.pid
        self.assertEqual(stat.S_IMODE(self.sockpath.stat().st_mode), 0o600)
        proxy = subprocess.run([str(HOST), "mcp", "connect"],
                               input=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}) + "\n",
                               text=True, capture_output=True, timeout=20, env={**self.env, "PALETTE_SOCKET": str(self.sockpath)})
        self.assertEqual(proxy.returncode, 0, proxy.stderr)
        self.assertEqual(len(json.loads(proxy.stdout)["result"]["tools"]), 4)
        created = self.request("palette_workspace", {"action": "create", "workspace_id": "durable", "scope": "open"})
        self.assertEqual(created["workspace_id"], "durable")
        mutation = self.request("palette", {"workspace_id": "durable", "code": "daemon_value = 431"})
        self.assertTrue(mutation.get("epoch"))
        workspace_status = self.request("palette_workspace", {"action": "status", "workspace_id": "durable"})
        adapter_pid = workspace_status["local_adapter_pid"]
        kernel_pid = julia_pid(adapter_pid)
        self.assertEqual(self.request("palette", {"workspace_id": "durable", "code": "daemon_value"})["data"], 431)

        # request() closes its client socket; reconnect reaches the same daemon and world.
        self.assertIsNone(self.proc.poll())
        self.assertEqual(self.proc.pid, daemon_pid)
        status = self.request("palette_workspace", {"action": "status", "workspace_id": "durable"})
        self.assertEqual(status["local_adapter_pid"], adapter_pid)
        value = self.request("palette", {"workspace_id": "durable", "code": "daemon_value"})
        self.assertEqual(value["data"], 431)
        self.assertEqual(value["epoch"], mutation["epoch"])
        self.assertEqual(julia_pid(adapter_pid), kernel_pid)
        self.stop_daemon()
        self.assertFalse(self.sockpath.exists())
        self.start_daemon()
        self.assertNotEqual(self.proc.pid, daemon_pid)
        self.assertEqual(self.request("palette", {"workspace_id": "durable", "code": "daemon_value"})["data"], 431)

    def test_concurrent_starts_reserve_live_capacity(self):
        self.stop_daemon()
        self.env["PALETTE_MAX_LIVE_WORKSPACES"] = "1"
        self.start_daemon()
        self.request("palette_workspace", {"action": "create", "workspace_id": "race-a", "scope": "open"})
        self.request("palette_workspace", {"action": "create", "workspace_id": "race-b", "scope": "open"})

        barrier = threading.Barrier(3)
        outcomes = {}

        def start_workspace(workspace_id):
            barrier.wait()
            outcomes[workspace_id] = self.request_frame(
                "palette", {"workspace_id": workspace_id, "code": "42"})

        starters = [threading.Thread(target=start_workspace, args=(workspace_id,))
                    for workspace_id in ("race-a", "race-b")]
        for thread in starters:
            thread.start()
        barrier.wait()
        for thread in starters:
            thread.join(timeout=240)
            self.assertFalse(thread.is_alive(), "concurrent workspace start hung")
        successful = {workspace_id: frame for workspace_id, frame in outcomes.items()
                      if "result" in frame and not frame["result"].get("isError")}
        rejected = next(frame for frame in outcomes.values() if frame["result"].get("isError"))
        self.assertEqual(len(successful), 1, outcomes)
        denied_payload = json.loads(rejected["result"]["content"][0]["text"])
        self.assertIn("Live workspace limit (1)", denied_payload["error"])

    def test_rejects_unsafe_existing_path(self):
        # A non-socket file or symlink is never unlinked/replaced by startup.
        self.stop_daemon()
        self.sockpath.write_text("owned")
        result = subprocess.run([str(HOST), "mcp", "--socket", str(self.sockpath)],
                                env=self.env, capture_output=True, text=True, timeout=15)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.sockpath.read_text(), "owned")
        self.sockpath.unlink()
        target = Path(self.tmp.name) / "target"
        target.write_text("owned")
        self.sockpath.symlink_to(target)
        result = subprocess.run([str(HOST), "mcp", "--socket", str(self.sockpath)],
                                env=self.env, capture_output=True, text=True, timeout=15)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.sockpath.is_symlink())
        self.assertEqual(target.read_text(), "owned")
        self.sockpath.unlink()
        shared = Path(self.tmp.name) / "shared"
        shared.mkdir(mode=0o770)
        shared.chmod(0o770)
        private_child = shared / "private"
        private_child.mkdir(mode=0o700)
        result = subprocess.run([str(HOST), "mcp", "--socket", str(private_child / "daemon.sock")],
                                env=self.env, capture_output=True, text=True, timeout=15)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((private_child / "daemon.sock").exists())


if __name__ == "__main__":
    unittest.main()
