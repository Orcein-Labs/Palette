"""Process-level proof that the daemon outlives its disposable socket clients."""
from __future__ import annotations

import json
import os
from pathlib import Path
import selectors
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
ROUTER = REPO / "runtime/security/operator_workspace_router.py"
HOST = REPO / "runtime/host/target/release/palette-host"
sys.path.insert(0, str(REPO / "runtime/security"))
import palette_mcp_client  # noqa: E402


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
        self.proc = subprocess.Popen([sys.executable, str(ROUTER), "--socket", str(self.sockpath)],
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
        proxy = subprocess.run([sys.executable, str(REPO / "runtime/security/palette_mcp_client.py")],
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
        result = subprocess.run([sys.executable, str(ROUTER), "--socket", str(self.sockpath)],
                                env=self.env, capture_output=True, text=True, timeout=15)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.sockpath.read_text(), "owned")
        self.sockpath.unlink()
        target = Path(self.tmp.name) / "target"
        target.write_text("owned")
        self.sockpath.symlink_to(target)
        result = subprocess.run([sys.executable, str(ROUTER), "--socket", str(self.sockpath)],
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
        result = subprocess.run([sys.executable, str(ROUTER), "--socket", str(private_child / "daemon.sock")],
                                env=self.env, capture_output=True, text=True, timeout=15)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((private_child / "daemon.sock").exists())

    def test_client_rejects_wrong_uid_peer(self):
        if hasattr(socket, "SO_PEERCRED"):
            peer = mock.Mock()
            peer.getsockopt.return_value = struct.pack("3i", 42, os.getuid() + 1, 7)
        else:
            class Peer:
                def getpeereid(self):
                    return os.getuid() + 1, os.getgid()
            peer = Peer()
        with self.assertRaises(PermissionError):
            palette_mcp_client.verify_peer_uid(peer)


class WorkspaceControlSynchronization(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="palette-workspace-control-")
        root = Path(self.tmp.name)
        self.project = root / "project"
        self.project.mkdir()
        self.env = {**os.environ, "PALETTE_REPO": str(REPO),
                    "OPERATOR_WORKSPACE": str(self.project),
                    "PALETTE_STATE_DIR": str(root / "legacy"),
                    "PALETTE_WORKSPACE_STATE_ROOT": str(root / "worlds"),
                    "PALETTE_SCRATCH_ROOT": str(root / "scratch")}

    def tearDown(self):
        sys.modules.pop("operator_workspace_router", None)
        self.tmp.cleanup()

    def test_close_waits_for_an_in_flight_call_and_status_observes_stopped_worker(self):
        with mock.patch.dict(os.environ, self.env):
            sys.modules.pop("operator_workspace_router", None)
            import operator_workspace_router as router

            entry = router.workspace_control({"action": "create", "workspace_id": "sync-world", "scope": "open"})

            class LiveProcess:
                pid = 12345
                stopped = False

                def poll(self):
                    return 0 if self.stopped else None

            class BlockingChild:
                def __init__(self):
                    self.lock = CloseTrackingLock()
                    self.proc = LiveProcess()

                def stop(self):
                    with self.lock:
                        self.proc.stopped = True
                        self.proc = None

            class CloseTrackingLock:
                def __init__(self):
                    self.inner = threading.RLock()
                    self.close_waiting = threading.Event()

                def acquire(self, *args, **kwargs):
                    if threading.current_thread().name == "close-world":
                        self.close_waiting.set()
                    return self.inner.acquire(*args, **kwargs)

                def release(self):
                    return self.inner.release()

                def __enter__(self):
                    self.acquire()
                    return self

                def __exit__(self, *_):
                    self.release()

            child = BlockingChild()
            router.children[entry["state_dir"]] = child
            in_flight = threading.Event()
            release_call = threading.Event()
            close_done = threading.Event()
            close_result = {}

            def hold_call_lock():
                with child.lock:
                    in_flight.set()
                    release_call.wait()

            def close_world():
                close_result["value"] = router.workspace_control({"action": "close", "workspace_id": "sync-world"})
                close_done.set()

            active_call = threading.Thread(target=hold_call_lock)
            active_call.start()
            closer = None
            try:
                self.assertTrue(in_flight.wait(5), "could not reserve the simulated in-flight call")
                closer = threading.Thread(target=close_world, name="close-world")
                closer.start()
                self.assertTrue(child.lock.close_waiting.wait(5), "close did not wait for the worker lock")
                self.assertFalse(close_done.is_set(), "close returned while the call still held the worker lock")
            finally:
                release_call.set()
                active_call.join(timeout=5)
                if closer:
                    closer.join(timeout=5)
            self.assertFalse(active_call.is_alive())
            self.assertFalse(closer and closer.is_alive())
            self.assertTrue(close_result["value"]["state_retained"])
            status = router.workspace_control({"action": "status", "workspace_id": "sync-world"})
            self.assertFalse(status["running"], status)

    def test_waiting_on_one_workspace_does_not_hold_registry_lock(self):
        with mock.patch.dict(os.environ, self.env):
            sys.modules.pop("operator_workspace_router", None)
            import operator_workspace_router as router

            entry = router.workspace_control({"action": "create", "workspace_id": "busy-world", "scope": "open"})
            waiting = threading.Event()

            class TrackedLock:
                def __init__(self):
                    self.inner = threading.RLock()

                def acquire(self, *args, **kwargs):
                    if threading.current_thread().name == "same-workspace-call":
                        waiting.set()
                    return self.inner.acquire(*args, **kwargs)

                def release(self):
                    return self.inner.release()

                def __enter__(self):
                    self.acquire()
                    return self

                def __exit__(self, *_):
                    self.release()

            class LiveProcess:
                pid = 54321

                def poll(self):
                    return None

            child = router.Child(entry)
            child.lock = TrackedLock()
            child.start = lambda: setattr(child, "proc", LiveProcess())
            child.exchange = lambda *_: {"ok": True}
            router.children[entry["state_dir"]] = child
            in_flight = threading.Event()
            release_call = threading.Event()

            def hold_worker_lock():
                with child.lock:
                    in_flight.set()
                    release_call.wait()

            holder = threading.Thread(target=hold_worker_lock, name="in-flight-call")
            holder.start()
            caller = None
            try:
                self.assertTrue(in_flight.wait(5), "could not reserve the simulated in-flight call")
                caller = threading.Thread(target=lambda: child.call("palette", {}), name="same-workspace-call")
                caller.start()
                self.assertTrue(waiting.wait(5), "second call did not wait on the workspace lock")
                acquired = router.children_lock.acquire(blocking=False)
                self.assertTrue(acquired, "a busy workspace held the global registry lock")
                router.children_lock.release()
            finally:
                release_call.set()
                holder.join(timeout=5)
                if caller:
                    caller.join(timeout=5)
            self.assertFalse(holder.is_alive())
            self.assertFalse(caller and caller.is_alive())

    def test_start_reservations_do_not_double_count_live_workers(self):
        with mock.patch.dict(os.environ, self.env):
            sys.modules.pop("operator_workspace_router", None)
            import operator_workspace_router as router

            router.MAX_LIVE = 2
            entries = [router.workspace_control({"action": "create", "workspace_id": name, "scope": "open"})
                       for name in ("starting-a", "starting-b")]

            class LiveProcess:
                def poll(self):
                    return None

            start_barrier = threading.Barrier(2)
            children = []
            for entry in entries:
                child = router.Child(entry)

                def start(worker=child):
                    worker.proc = LiveProcess()
                    start_barrier.wait(timeout=5)

                child.start = start
                child.exchange = lambda *_: {"ok": True}
                router.children[entry["state_dir"]] = child
                children.append(child)

            outcomes = {}
            errors = {}

            def call_worker(index, child):
                try:
                    outcomes[index] = child.call("palette", {})
                except Exception as exc:
                    errors[index] = exc

            callers = [threading.Thread(target=call_worker, args=(index, child))
                       for index, child in enumerate(children)]
            for caller in callers:
                caller.start()
            for caller in callers:
                caller.join(timeout=10)
            self.assertFalse(any(caller.is_alive() for caller in callers), "concurrent starts did not finish")
            self.assertFalse(errors, errors)
            self.assertEqual(outcomes, {0: {"ok": True}, 1: {"ok": True}})
            self.assertFalse(router.starting_children)

if __name__ == "__main__":
    unittest.main()
