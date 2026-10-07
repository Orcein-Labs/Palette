#!/usr/bin/env python3
"""
Executable tests for Experiment 002's authority-fence claims.

These are deliberately NOT mocked. Every test launches a real bubblewrap
sandbox around a real julia process (or, for the ceiling-subset tests,
exercises the real broker logic directly). This is slow by design --
several tests pay a real, currently-unsolved ~40s Julia precompilation
cost per sandboxed launch (see runtime/docs/install.md) -- and
that slowness is itself honest evidence about the current state of the
depot problem, not something to hide by mocking it away.

Every "blocked" assertion checks REAL evidence (host filesystem state,
process exit codes, a second process's own PID), never just the worker's
own self-reported stdout -- see runtime/docs/authority.md's note on why two
early manual tests were false positives until checked this way.

Run:
    python3 runtime/security/test_authority.py            # full suite (slow: real sandboxes)
    python3 runtime/security/test_authority.py -k subset   # ceiling_is_subset only (fast, no sandbox)

Requires: bubblewrap (bwrap) on PATH and an instantiated test environment
with Palette, JSON, CSV, DataFrames, EzXML and IJulia. Set
PALETTE_TEST_PROJECT_DIR to that environment; see runtime/docs/install.md.
"""
from __future__ import annotations

import http.server
import hashlib
import json
import os
import shutil
import sys
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
# The broker and worker launcher under test: the Rust host when PALETTE_HOST_BIN is set.
from host_adapter import B, create_session_depot, run_worker  # noqa: E402
from launch_worker import build_package_installer_argv, default_depot, package_store_paths, resolve_real_julia_binary  # noqa: E402

REPO_DIR = str(Path(__file__).resolve().parents[2])

# A Julia project environment with `Palette` (this repo, dev-installed) and
# `IJulia` available. Point this at your own dev environment via env var;
# defaults to the repository; full conformance requires the extra test dependencies.
PROJECT_DIR = os.environ.get(
    "PALETTE_TEST_PROJECT_DIR",
    REPO_DIR,
)


def _julia_str(s: str) -> str:
    escaped = s.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def _julia_dict(d: dict) -> str:
    parts = []
    for k, v in d.items():
        if isinstance(v, str):
            v_lit = _julia_str(v)
        elif isinstance(v, bool):
            v_lit = "true" if v else "false"
        elif isinstance(v, (int, float)):
            v_lit = str(v)
        else:
            raise TypeError(f"unsupported value type for Julia Dict literal: {type(v)}")
        parts.append(f"{_julia_str(k)} => {v_lit}")
    return "Dict(" + ", ".join(parts) + ")"


def _skip_if_no_bwrap():
    if shutil.which("bwrap") is None:
        raise unittest.SkipTest("bubblewrap (bwrap) not found on PATH")


def _skip_if_no_project():
    if not Path(PROJECT_DIR).exists():
        raise unittest.SkipTest(
            f"no Julia dev project at {PROJECT_DIR} -- set PALETTE_TEST_PROJECT_DIR "
            "to a project with Palette (dev-installed from this repo) and IJulia"
        )


class SandboxTestCase(unittest.TestCase):
    """Common setup for tests that launch a real sandboxed worker."""

    def setUp(self):
        _skip_if_no_bwrap()
        _skip_if_no_project()
        self.tmp = tempfile.mkdtemp(prefix="palette-test-")
        self.workspace = os.path.join(self.tmp, "workspace")
        os.makedirs(self.workspace, exist_ok=True)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_script(self, script: str, broker_socket_dir: str | None = None, timeout: float = 90):
        return run_worker(
            workspace_dir=self.workspace,
            project_dir=PROJECT_DIR,
            repo_dir=REPO_DIR,
            script=script,
            broker_socket_dir=broker_socket_dir,
            timeout=timeout,
        )


class TestSandboxContainment(SandboxTestCase):
    def test_D04_workspace_cannot_mask_read_only_sources(self):
        alias = Path(self.tmp) / "source-alias"
        alias.symlink_to(REPO_DIR, target_is_directory=True)
        for workspace in (REPO_DIR, str(Path(REPO_DIR).parent), str(alias), PROJECT_DIR, "/tmp", "/proc", "/dev", "/run"):
            with self.subTest(workspace=workspace), self.assertRaisesRegex((PermissionError, RuntimeError), "overlap|masks"):
                run_worker(workspace_dir=workspace, project_dir=PROJECT_DIR,
                    repo_dir=REPO_DIR, script='println("unreachable")', timeout=30)

    def test_network_blocked_via_run(self):
        r = self.run_script('run(`curl -s -m 3 http://example.com`)')
        self.assertNotEqual(r.returncode, 0, "curl should fail with no network namespace")

    def test_network_blocked_via_raw_socket(self):
        r = self.run_script(
            'using Sockets; try; Sockets.connect("1.1.1.1", 80); println("ESCAPE"); '
            'catch e; println("blocked: ", sprint(showerror, e)); end'
        )
        self.assertNotIn("ESCAPE", r.stdout)
        self.assertIn("blocked", r.stdout)

    def test_sensitive_file_not_visible(self):
        r = self.run_script(
            'try; read("/etc/shadow", String); println("ESCAPE"); '
            'catch e; println("blocked: ", sprint(showerror, e)); end'
        )
        self.assertNotIn("ESCAPE", r.stdout)
        self.assertIn("No such file", r.stdout)

    def test_ephemeral_tmp_write_does_not_reach_host(self):
        marker = f"palette-test-{os.getpid()}-{time.time_ns()}"
        host_path = f"/tmp/{marker}.txt"
        self.assertFalse(os.path.exists(host_path))
        r = self.run_script(f'write("{host_path}", "pwned")')
        self.assertEqual(r.returncode, 0)
        # It "succeeded" inside the sandbox (its own ephemeral /tmp) -- the
        # only claim that matters is that it never reaches the real host.
        self.assertFalse(os.path.exists(host_path), "a /tmp write inside the sandbox must not reach the real host")

    def test_workspace_write_is_real_and_reaches_host(self):
        host_file = os.path.join(self.workspace, "positive-control.txt")
        self.assertFalse(os.path.exists(host_file))
        r = self.run_script(f'write("{host_file}", "legit sandbox-local write")')
        self.assertEqual(r.returncode, 0)
        self.assertTrue(os.path.exists(host_file), "a write inside the bounded workspace must reach the real host")
        self.assertEqual(Path(host_file).read_text(), "legit sandbox-local write")

    def test_eval_has_identical_power_and_failure_modes_as_direct_code(self):
        r = self.run_script(
            'direct = 1 + 1; evaled = Core.eval(Main, Meta.parse("1 + 1")); '
            'println("equal=", direct == evaled)'
        )
        self.assertIn("equal=true", r.stdout)

    def test_subprocess_inherits_network_restriction(self):
        r = self.run_script(
            'out = read(`sh -c "curl -s -m 3 http://example.com || echo NO_NETWORK_IN_CHILD"`, String); '
            'println(strip(out))'
        )
        self.assertIn("NO_NETWORK_IN_CHILD", r.stdout)

    def test_worker_cannot_self_spawn_a_new_sandbox(self):
        r = self.run_script(
            'try; run(`bwrap --unshare-user --uid 0 --gid 0 -- echo pwned`); println("ESCAPE"); '
            'catch e; println("blocked: ", sprint(showerror, e)); end'
        )
        self.assertNotIn("ESCAPE", r.stdout)


class TestBrokerMediatedCapabilities(SandboxTestCase):
    def setUp(self):
        super().setUp()
        self.broker_socket_dir = tempfile.mkdtemp(prefix="njl-bsock-")  # short path: AF_UNIX length limit
        self.approved_dir = os.path.join(self.tmp, "approved-out")
        os.makedirs(self.approved_dir, exist_ok=True)
        self.receipts_path = os.path.join(self.tmp, "receipts.jsonl")
        self.ceiling = {
            "external_fs_write": {"allowed_dirs": [self.approved_dir]},
            "network_access": {"allowed": False},
        }
        self._stop = threading.Event()
        self._thread = threading.Thread(
            target=B.serve,
            args=(
                os.path.join(self.broker_socket_dir, "broker.sock"),
                self.ceiling,
                self.receipts_path,
                "test-session",
                self._stop,
            ),
            daemon=True,
        )
        self._thread.start()
        time.sleep(0.3)

    def tearDown(self):
        self._stop.set()
        time.sleep(0.2)
        shutil.rmtree(self.broker_socket_dir, ignore_errors=True)
        super().tearDown()

    def _request(self, category: str, params: dict) -> str:
        # Build a Julia Dict(...) literal directly rather than round-tripping
        # through JSON.parse: JSON is only a transitive dependency of the
        # test project (via Palette), so `using JSON` isn't available to a
        # plain top-level script here -- same restriction Julia's Pkg
        # enforces on any transitive-only dependency.
        return f'using Palette; r = Palette.request_capability("{category}", {_julia_dict(params)}); println(r)'

    def test_approved_write_reaches_real_host(self):
        target = os.path.join(self.approved_dir, "from-worker.txt")
        r = self.run_script(
            self._request("external_fs_write", {"path": target, "content": "real write via broker"}),
            broker_socket_dir=self.broker_socket_dir,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn('"approved" => true', r.stdout)
        self.assertTrue(os.path.exists(target))
        self.assertEqual(Path(target).read_text(), "real write via broker")

    def test_denied_write_outside_ceiling_does_not_reach_host(self):
        target = "/tmp/definitely-not-in-ceiling.txt"
        self.assertFalse(os.path.exists(target))
        r = self.run_script(
            self._request("external_fs_write", {"path": target, "content": "nope"}),
            broker_socket_dir=self.broker_socket_dir,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn('"approved" => false', r.stdout)
        self.assertFalse(os.path.exists(target))

    def test_denied_network_request(self):
        r = self.run_script(
            self._request("network_access", {"url": "http://example.com"}),
            broker_socket_dir=self.broker_socket_dir,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn('"approved" => false', r.stdout)

    def test_direct_bypass_of_broker_still_blocked(self):
        """A denied capability isn't just declined by the nice API -- the
        same effect attempted directly, bypassing request_capability
        entirely, must still fail."""
        target = "/tmp/direct-bypass-attempt.txt"
        self.assertFalse(os.path.exists(target))
        r = self.run_script(f'write("{target}", "bypass")', broker_socket_dir=self.broker_socket_dir)
        self.assertEqual(r.returncode, 0)
        self.assertFalse(os.path.exists(target))

    def test_receipts_are_written_for_every_decision(self):
        r = self.run_script(
            self._request("network_access", {"url": "http://example.com"}),
            broker_socket_dir=self.broker_socket_dir,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        receipts = [json.loads(l) for l in Path(self.receipts_path).read_text().splitlines() if l.strip()]
        self.assertTrue(any(rec["category"] == "network_access" and rec["approved"] is False for rec in receipts))


class TestNetworkRedirectSafety(unittest.TestCase):
    """Direct Broker unit tests (no sandbox needed) -- regression for a
    real bypass found by direct exploitation: an allowlisted host that
    responds with an HTTP redirect to a DIFFERENT, non-allowlisted host
    was silently followed by urllib's default opener, reaching the
    disallowed host with the allowlist only ever having checked the FIRST
    url."""

    def setUp(self):
        class _Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(handler_self):
                if handler_self.path == "/redirect":
                    handler_self.send_response(302)
                    handler_self.send_header(
                        "Location", f"http://localhost:{handler_self.server.server_address[1]}/secret"
                    )
                    handler_self.end_headers()
                else:
                    handler_self.send_response(200)
                    handler_self.end_headers()
                    handler_self.wfile.write(b"SECRET REACHED")

            def log_message(handler_self, *args):
                pass

        self.httpd = http.server.HTTPServer(("127.0.0.1", 0), _Handler)
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.httpd.shutdown()
        self.httpd.server_close()

    def test_redirect_to_disallowed_host_is_not_followed(self):
        # allowlist covers the INITIAL host (127.0.0.1) but not the
        # redirect target (localhost) -- confirmed exploitable before the
        # fix: the redirect target's content ("SECRET REACHED") came back
        # with the allowlist never having been asked about "localhost" at all.
        broker = B.Broker(
            {"network_access": {"allowed": True, "allowed_hosts": ["127.0.0.1"]}},
            os.path.join(tempfile.mkdtemp(), "receipts.jsonl"), "redirect-test",
        )
        result = broker._handle_network_access({"url": f"http://127.0.0.1:{self.port}/redirect"})
        self.assertEqual(result["status"], 302)
        self.assertNotIn("SECRET", result["body_prefix"])


class TestCeilingSubset(unittest.TestCase):
    """Fast, no sandbox: the C_child ⊆ C_caller logic in isolation."""

    def setUp(self):
        self.parent = {
            "external_fs_write": {"allowed_dirs": ["/tmp/a"]},
            "network_access": {"allowed": False},
        }

    def test_identical_to_parent_is_allowed(self):
        ok, _ = B.ceiling_is_subset({"external_fs_write": {"allowed_dirs": ["/tmp/a"]}}, self.parent)
        self.assertTrue(ok)

    def test_narrower_subdir_is_allowed(self):
        ok, _ = B.ceiling_is_subset({"external_fs_write": {"allowed_dirs": ["/tmp/a/sub"]}}, self.parent)
        self.assertTrue(ok)

    def test_network_escalation_is_denied(self):
        ok, reason = B.ceiling_is_subset({"network_access": {"allowed": True}}, self.parent)
        self.assertFalse(ok)
        self.assertIn("network_access", reason)

    def test_dir_outside_parent_is_denied(self):
        ok, _ = B.ceiling_is_subset({"external_fs_write": {"allowed_dirs": ["/tmp/b"]}}, self.parent)
        self.assertFalse(ok)

    def test_unrecognized_category_fails_closed(self):
        ok, reason = B.ceiling_is_subset({"credential_access": {"allowed": True}}, self.parent)
        self.assertFalse(ok)
        self.assertIn("not present", reason)

    def test_omitted_allowed_hosts_against_host_restricted_parent_is_denied(self):
        """Regression: a parent restricted to specific hosts previously let
        a child omit allowed_hosts entirely (or pass it as null) and pass
        the subset check -- "child said nothing about hosts" was silently
        treated as "no widening", when omitting it actually means
        unrestricted, which IS wider than a host-restricted parent.
        Confirmed exploitable by direct testing before this fix."""
        parent = {"network_access": {"allowed": True, "allowed_hosts": ["example.invalid"]}}
        ok, reason = B.ceiling_is_subset({"network_access": {"allowed": True}}, parent)
        self.assertFalse(ok)
        self.assertIn("allowed_hosts", reason)

        ok2, reason2 = B.ceiling_is_subset({"network_access": {"allowed": True, "allowed_hosts": None}}, parent)
        self.assertFalse(ok2)
        self.assertIn("allowed_hosts", reason2)

    def test_real_host_subset_still_allowed(self):
        """Not a false positive from the fix above: a child that DOES name
        a real subset of the parent's allowed_hosts must still pass."""
        parent = {"network_access": {"allowed": True, "allowed_hosts": ["a.invalid", "b.invalid"]}}
        ok, _ = B.ceiling_is_subset({"network_access": {"allowed": True, "allowed_hosts": ["a.invalid"]}}, parent)
        self.assertTrue(ok)


class TestNestedChildWorker(SandboxTestCase):
    def setUp(self):
        super().setUp()
        self.approved_dir = os.path.join(self.tmp, "approved-out")
        os.makedirs(self.approved_dir, exist_ok=True)
        self.child_sub = os.path.join(self.approved_dir, "child-sub")
        os.makedirs(self.child_sub, exist_ok=True)
        self.child_workspace = os.path.join(self.tmp, "child-workspace")
        os.makedirs(self.child_workspace, exist_ok=True)
        self.broker_socket_dir = tempfile.mkdtemp(prefix="njl-bsock-")
        self.receipts_path = os.path.join(self.tmp, "receipts.jsonl")

    def tearDown(self):
        shutil.rmtree(self.broker_socket_dir, ignore_errors=True)
        super().tearDown()

    def _serve(self, ceiling):
        stop = threading.Event()
        t = threading.Thread(
            target=B.serve,
            args=(os.path.join(self.broker_socket_dir, "broker.sock"), ceiling, self.receipts_path, "nested-test", stop),
            kwargs={"project_dir": PROJECT_DIR, "repo_dir": REPO_DIR},
            daemon=True,
        )
        t.start()
        time.sleep(0.3)
        return stop

    def test_child_requesting_wider_ceiling_is_denied(self):
        stop = self._serve({"external_fs_write": {"allowed_dirs": [self.child_sub]}, "network_access": {"allowed": False}})
        try:
            script = (
                'using Palette; r = Palette.request_capability("spawn_child_worker", Dict('
                '"ceiling" => Dict("network_access" => Dict("allowed" => true)), '
                '"script" => "println(1)", '
                f'"child_workspace" => "{self.child_workspace}", '
                f'"child_project" => "{PROJECT_DIR}", '
                f'"child_repo" => "{REPO_DIR}")); println(r)'
            )
            r = self.run_script(script, broker_socket_dir=self.broker_socket_dir)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn('"approved" => false', r.stdout)
        finally:
            stop.set()

    def test_child_with_approved_network_cannot_reach_host_outside_allowlist(self):
        """Regression test for a real bug found and fixed after Experiment
        002 first shipped: an approved child ceiling with `network_access`
        used to get a raw, unshared network namespace (`--unshare-net`
        omitted) regardless of `allowed_hosts` -- proven exploitable by
        reaching an arbitrary local listener the ceiling never named, with
        zero broker mediation. Independent host-side evidence, not the
        worker's own self-report: a real local HTTP server, outside the
        sandbox, that only a real network connection could have reached."""

        class _Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"UNAUTHORIZED HOST REACHED")

            def log_message(self, *_args):
                pass

        httpd = http.server.HTTPServer(("127.0.0.1", 0), _Handler)
        port = httpd.server_address[1]
        server_thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        server_thread.start()

        stop = self._serve({"network_access": {"allowed": True, "allowed_hosts": ["example.invalid"]}})
        try:
            child_script = (
                "using Sockets\n"
                "try\n"
                f'    sock = connect("127.0.0.1", {port})\n'
                '    println(sock, "GET / HTTP/1.0\\r\\n\\r\\n")\n'
                "    resp = readline(sock)\n"
                '    println("REACHED: ", resp)\n'
                "catch e\n"
                '    println("BLOCKED: ", e)\n'
                "end\n"
            )
            spawn_script = (
                'using Palette; r = Palette.request_capability("spawn_child_worker", Dict('
                '"ceiling" => Dict("network_access" => Dict("allowed" => true, "allowed_hosts" => ["example.invalid"])), '
                f'"script" => {json.dumps(child_script)}, '
                f'"child_workspace" => "{self.child_workspace}", '
                f'"child_project" => "{PROJECT_DIR}", '
                f'"child_repo" => "{REPO_DIR}")); println(r)'
            )
            r = self.run_script(spawn_script, broker_socket_dir=self.broker_socket_dir)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn('"approved" => true', r.stdout)  # the spawn itself is a legitimate subset request
            self.assertIn("BLOCKED", r.stdout)
            self.assertNotIn("REACHED", r.stdout)
        finally:
            stop.set()
            httpd.shutdown()
            httpd.server_close()

    def test_child_workspace_param_cannot_pick_an_arbitrary_host_path(self):
        """Regression test for a real bug found by direct exploitation: a
        worker with a ceiling containing ONLY `spawn_child_worker` (no
        `external_fs_write` at all) could previously name any host
        directory as `child_workspace` in the request and have the broker
        bind-mount it writable into the child -- a write attacker-chosen
        outside any ceiling check. Independent host-side evidence: the
        target directory is checked directly, not through the worker's
        own self-report."""
        attacker_chosen_dir = tempfile.mkdtemp(prefix="njl-attacker-target-")
        stop = self._serve({"spawn_child_worker": {}})  # deliberately NO external_fs_write at all
        try:
            script = (
                'using Palette; r = Palette.request_capability("spawn_child_worker", Dict('
                '"ceiling" => Dict(), '
                '"script" => "write(joinpath(pwd(), \\"ESCAPED.txt\\"), \\"no external_fs_write needed\\")", '
                f'"child_workspace" => "{attacker_chosen_dir}")); println(r)'
            )
            r = self.run_script(script, broker_socket_dir=self.broker_socket_dir)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn('"approved" => true', r.stdout)  # the spawn itself is legitimate -- ceiling {} is a valid subset
            self.assertFalse(
                os.path.isfile(os.path.join(attacker_chosen_dir, "ESCAPED.txt")),
                "child_workspace param let the request pick a real host write target with no ceiling check",
            )
        finally:
            stop.set()

    def test_grandchild_nesting_actually_works(self):
        """Regression test for a real bug found by direct testing:
        ceiling_is_subset used `if not parent_cap` (Python truthiness) to
        check whether a category was present in the parent's ceiling --
        but `spawn_child_worker`'s own ceiling shape is `{}` throughout
        this codebase, and an empty dict is falsy. That made granting a
        CHILD the ability to spawn its own children unconditionally
        impossible even when the parent's ceiling explicitly named
        spawn_child_worker, and there was also no explicit
        ceiling_is_subset branch for the category at all. Both fixed;
        this proves a real two-level nesting end to end, not just the
        ceiling-logic unit case -- the grandchild reports its own real,
        independent PID namespace (pid=2: the only process under that
        namespace's own bwrap reaper, where a host pid would be large), the
        same independent evidence the one-level case already used."""
        stop = self._serve({"spawn_child_worker": {}})
        try:
            grandchild_script = 'println("grandchild pid=", getpid())'
            child_script = (
                'using Palette; r2 = Palette.request_capability("spawn_child_worker", Dict('
                '"ceiling" => Dict(), '
                f'"script" => {json.dumps(grandchild_script)})); println("child got: ", r2)'
            )
            top_script = (
                'using Palette; r1 = Palette.request_capability("spawn_child_worker", Dict('
                '"ceiling" => Dict("spawn_child_worker" => Dict()), '
                f'"script" => {json.dumps(child_script)})); println("top got: ", r1)'
            )
            r = self.run_script(top_script, broker_socket_dir=self.broker_socket_dir, timeout=120)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn('"approved" => true', r.stdout)
            self.assertIn("grandchild pid=2", r.stdout)
        finally:
            stop.set()


class TestPackageManagement(SandboxTestCase):
    """package_management: the broker decides whether a specific package is
    allowed and performs Pkg.add into the durable operator package store.
    Workers receive that store read-only and use a separate private writable
    depot for precompile caches. Parsers is already present in the prepared
    depot as a transitive dependency, so this regression runs without a live
    registry or package download.
    """

    def setUp(self):
        super().setUp()
        self._old_package_depot = os.environ.get("PALETTE_PACKAGE_DEPOT")
        self.package_store = tempfile.mkdtemp(prefix="palette-pkg-store-")
        os.environ["PALETTE_PACKAGE_DEPOT"] = self.package_store
        self._package_worker_evidence = []
        self._package_test_passed = False
        self.depot_dir = create_session_depot()

    def tearDown(self):
        if not self._package_test_passed and getattr(self, "package_store", None):
            archive = Path(REPO_DIR) / ".archive" / "maturity-20261006" / f"failed-package-store-{time.time_ns()}"
            archive.mkdir(parents=True, exist_ok=True)
            inventory = []
            for path in sorted(Path(self.package_store).rglob("*")):
                if path.is_symlink() or not path.is_file():
                    continue
                digest = hashlib.sha256()
                with path.open("rb") as source:
                    for block in iter(lambda: source.read(1024 * 1024), b""):
                        digest.update(block)
                inventory.append({"path": str(path.relative_to(self.package_store)),
                                  "bytes": path.stat().st_size, "sha256": digest.hexdigest()})
            Path(archive, "store-inventory.json").write_text(
                json.dumps(inventory, indent=2), encoding="utf-8")
            Path(archive, "store-bytes.json").write_text(json.dumps({
                "root": self.package_store,
                "total_bytes": sum(entry["bytes"] for entry in inventory),
                "file_count": len(inventory),
            }, indent=2), encoding="utf-8")
            Path(archive, "worker-output.json").write_text(
                json.dumps(self._package_worker_evidence, indent=2), encoding="utf-8")
            Path(archive, "failure.txt").write_text("Package authority test failed; see unittest output.\n",
                                                        encoding="utf-8")
            print(f"PRESERVED_PACKAGE_STORE: {archive}")
        shutil.rmtree(self.depot_dir, ignore_errors=True)
        shutil.rmtree(self.package_store, ignore_errors=True)
        if self._old_package_depot is None:
            os.environ.pop("PALETTE_PACKAGE_DEPOT", None)
        else:
            os.environ["PALETTE_PACKAGE_DEPOT"] = self._old_package_depot
        super().tearDown()

    def _run_package_worker(self, label, **kwargs):
        try:
            result = run_worker(**kwargs)
        except subprocess.TimeoutExpired as exc:
            self._package_worker_evidence.append({
                "label": label, "timeout": exc.timeout,
                "stdout": (exc.stdout or b"").decode(errors="replace") if isinstance(exc.stdout, bytes) else exc.stdout,
                "stderr": (exc.stderr or b"").decode(errors="replace") if isinstance(exc.stderr, bytes) else exc.stderr,
            })
            raise
        self._package_worker_evidence.append({
            "label": label, "returncode": result.returncode,
            "stdout": result.stdout, "stderr": result.stderr,
        })
        return result

    def _serve(self, ceiling):
        sock_dir = tempfile.mkdtemp(prefix="njl-pkgmgmt-bsock-")
        stop = threading.Event()
        server, _ = B.serve(
            os.path.join(sock_dir, "broker.sock"), ceiling,
            os.path.join(sock_dir, "receipts.jsonl"), "pkgmgmt-test",
            depot_dir=self.depot_dir,
        )
        return sock_dir, server

    def test_P02_package_store_rejects_protected_collision_before_mutation(self):
        root = Path(self.package_store).parent / f"{Path(self.package_store).name}-runtime-collision"
        previous = os.environ.get("PALETTE_RUNTIME_ROOT")
        previous_read_roots = os.environ.get("PALETTE_READ_ROOTS")
        self.addCleanup(lambda: os.environ.pop("PALETTE_RUNTIME_ROOT", None)
                        if previous is None else os.environ.__setitem__("PALETTE_RUNTIME_ROOT", previous))
        self.addCleanup(lambda: os.environ.pop("PALETTE_READ_ROOTS", None)
                        if previous_read_roots is None else os.environ.__setitem__("PALETTE_READ_ROOTS", previous_read_roots))
        os.environ["PALETTE_RUNTIME_ROOT"] = str(root)
        with self.assertRaisesRegex(RuntimeError, "overlaps protected host path"):
            package_store_paths(str(root))
        self.assertFalse(root.exists(), "rejected package store collision created a protected path")

        read_root = Path(self.package_store).parent / f"{Path(self.package_store).name}-read-root"
        read_root.mkdir(mode=0o755)
        self.addCleanup(shutil.rmtree, read_root, ignore_errors=True)
        os.chmod(read_root, 0o755)
        os.environ["PALETTE_READ_ROOTS"] = str(read_root)
        with self.assertRaisesRegex(RuntimeError, "overlaps protected host path"):
            package_store_paths(str(read_root))
        self.assertEqual(read_root.stat().st_mode & 0o777, 0o755,
                         "rejected read-root overlap changed the protected directory mode")
        self.assertFalse((read_root / "depot").exists(),
                         "rejected read-root overlap created a package depot")
        self._package_test_passed = True

    def test_P03_package_store_rejects_symlink_seed_before_chmod_or_copy(self):
        root = Path(self.package_store).parent / f"{Path(self.package_store).name}-symlink-seed"
        depot = root / "depot"
        outside = root.parent / f"{root.name}-outside"
        outside.mkdir(mode=0o755)
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        self.addCleanup(shutil.rmtree, outside, ignore_errors=True)
        (outside / "sentinel").write_text("untouched", encoding="utf-8")
        depot.mkdir(parents=True, mode=0o755)
        os.chmod(root, 0o755)
        os.chmod(depot, 0o755)
        (depot / "registries").symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(RuntimeError, "must not be a symlink"):
            package_store_paths(str(root))
        self.assertEqual((outside / "sentinel").read_text(encoding="utf-8"), "untouched")
        self.assertEqual(root.stat().st_mode & 0o777, 0o755, "rejected store was chmodded before validation")
        self.assertEqual(depot.stat().st_mode & 0o777, 0o755, "rejected depot was chmodded before validation")
        (depot / "registries").unlink()
        nested = depot / "packages" / "Fixture" / "slug" / "src"
        nested.mkdir(parents=True)
        (nested / "module.jl").symlink_to(outside / "sentinel")
        with self.assertRaisesRegex(RuntimeError, "symlinks"):
            package_store_paths(str(root))
        self.assertEqual((outside / "sentinel").read_text(encoding="utf-8"), "untouched")
        self.assertEqual(root.stat().st_mode & 0o777, 0o755, "nested-link rejection happened after chmod")
        self._package_test_passed = True

    def test_P01_pkg_build_script_cannot_write_outside_durable_store(self):
        """Pkg's real build phase runs with only its managed store writable."""
        store = Path(self.package_store)
        depot = store / "depot"
        environment = store / "environment"
        depot.mkdir(mode=0o700, exist_ok=True)
        environment.mkdir(mode=0o700, exist_ok=True)
        registries = depot / "registries"
        shutil.rmtree(registries, ignore_errors=True)
        package_name = "BuildProbe"
        package_uuid = "7b4f9f1c-2bc0-4ae6-85f8-0a1327452f61"
        registry_uuid = "36eb0b89-20d6-4ff4-9494-24ad49e82f57"
        repository = depot / "fixture-repository"
        (repository / "src").mkdir(parents=True)
        (repository / "deps").mkdir()
        gesso_source = store / "sources" / "gesso"
        (gesso_source / "src").mkdir(parents=True)
        gesso_uuid = "b63ad0dc-6b3b-4c45-a9c4-123dbbcc1940"
        (gesso_source / "Project.toml").write_text(
            f'name = "GessoFixture"\nuuid = "{gesso_uuid}"\nversion = "0.1.0"\n', encoding="utf-8")
        (gesso_source / "src" / "GessoFixture.jl").write_text(
            "module GessoFixture\nfixture_value() = 42\nend\n", encoding="utf-8")
        outside_marker = store.parent / f"{store.name}-outside-write"
        self.addCleanup(outside_marker.unlink, missing_ok=True)
        source_marker = gesso_source / "build-script-write"
        self.addCleanup(source_marker.unlink, missing_ok=True)
        store_marker = depot / "build-script-ran"
        project = repository / "Project.toml"
        project.write_text(
            f'name = "{package_name}"\nuuid = "{package_uuid}"\nversion = "0.1.0"\n',
            encoding="utf-8",
        )
        (repository / "src" / f"{package_name}.jl").write_text(
            f"module {package_name}\nend\n", encoding="utf-8")
        # The first write is authorized because it stays in the managed store.
        # The second targets a real host sibling that is intentionally absent
        # from the installer's filesystem namespace.
        (repository / "deps" / "build.jl").write_text(
            f'open({_julia_str(str(store_marker))}, "w") do io; write(io, "ran"); end\n'
            f'try; open({_julia_str(str(gesso_source / "build-script-write"))}, "w") do io; write(io, "escaped"); end; '
            'catch; println("SOURCE_WRITE_BLOCKED"); end\n'
            f'try; open({_julia_str(str(outside_marker))}, "w") do io; write(io, "escaped"); end; '
            'catch; println("OUTSIDE_WRITE_BLOCKED"); end\n',
            encoding="utf-8",
        )
        subprocess.run(["git", "init", "-q"], cwd=repository, check=True)
        subprocess.run(["git", "add", "."], cwd=repository, check=True)
        subprocess.run(["git", "-c", "user.name=Palette test", "-c", "user.email=test@example.invalid",
                        "commit", "-qm", "installer confinement fixture"], cwd=repository, check=True)
        tree_hash = subprocess.run(
            ["git", "rev-parse", "HEAD^{tree}"], cwd=repository, check=True,
            capture_output=True, text=True,
        ).stdout.strip()
        registry = registries / "LocalFixture"
        package_registry = registry / "B" / package_name
        package_registry.mkdir(parents=True)
        registry.joinpath("Registry.toml").write_text(
            f'name = "LocalFixture"\nuuid = "{registry_uuid}"\nrepo = "file://{registry}"\n\n'
            f'[packages]\n"{package_uuid}" = {{ name = "{package_name}", path = "B/{package_name}" }}\n',
            encoding="utf-8",
        )
        (package_registry / "Package.toml").write_text(
            f'name = "{package_name}"\nuuid = "{package_uuid}"\nrepo = "file://{repository}"\n',
            encoding="utf-8",
        )
        (package_registry / "Versions.toml").write_text(
            f'["0.1.0"]\ngit-tree-sha1 = "{tree_hash}"\n', encoding="utf-8")
        julia = resolve_real_julia_binary()
        # The package source/registry are private local fixtures. Disabling the
        # Pkg server in this one-shot script keeps the proof independent of any
        # external registry or package service; Pkg clones the fixture's file URL.
        script = (
            'ENV["JULIA_PKG_SERVER"] = ""; using Pkg; '
            f'Pkg.develop(path={_julia_str(str(gesso_source))}); '
            f'Pkg.add(Pkg.PackageSpec(name="{package_name}", version="0.1.0")); '
            'using GessoFixture; println("GESSO_VALUE=", GessoFixture.fixture_value())'
        )
        argv = build_package_installer_argv(
            julia_bin=julia, package_store_root=str(store), package_depot_dir=str(depot),
            package_environment_dir=str(environment), base_depot=default_depot(),
            offline=False, script=script,
        )
        result = subprocess.run(
            argv, env={"PATH": "/usr/bin:/bin"}, capture_output=True, text=True,
            errors="replace", timeout=180, stdin=subprocess.DEVNULL,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("GESSO_VALUE=42", result.stdout)
        build_logs = list((depot / "scratchspaces").glob("**/build.log"))
        self.assertTrue(
            any("SOURCE_WRITE_BLOCKED" in log.read_text(encoding="utf-8", errors="replace")
                for log in build_logs),
            f"Pkg build log did not record the protected developed-source write denial: {build_logs}",
        )
        self.assertTrue(store_marker.is_file(), "fixture build script did not run in the managed store")
        self.assertFalse(source_marker.exists(), "Pkg build script modified operator-owned developed source")
        self.assertFalse(outside_marker.exists(), "Pkg build script wrote outside its writable package store")
        # Independent negative control: run the exact build script directly
        # under the same private fixture project/depot, with no namespace.
        direct_home = tempfile.mkdtemp(prefix="palette-pkg-direct-home-")
        self.addCleanup(shutil.rmtree, direct_home, ignore_errors=True)
        direct_env = {
            "HOME": direct_home,
            "JULIA_PROJECT": str(environment),
            "JULIA_DEPOT_PATH": f"{depot}:{default_depot()}",
            "JULIA_PKG_PRECOMPILE_AUTO": "0",
            "JULIA_PKG_OFFLINE": "true",
            "JULIA_PKG_SERVER": "",
            "PATH": "/usr/bin:/bin",
        }
        direct = subprocess.run(
            [julia, "--startup-file=no", "-e", f"include({_julia_str(str(repository / 'deps' / 'build.jl'))})"],
            env=direct_env, stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace", timeout=60,
        )
        self.assertEqual(direct.returncode, 0, direct.stdout + direct.stderr)
        self.assertTrue(source_marker.is_file(), "unconfined build fixture did not demonstrate the source write")
        self.assertTrue(outside_marker.is_file(), "unconfined build fixture did not demonstrate the outside write")
        source_marker.unlink()
        outside_marker.unlink()
        self._package_test_passed = True

    def test_allowed_package_installs_and_becomes_usable(self):
        sock_dir, server = self._serve({"package_management": {"allowed_packages": ["Parsers"], "offline": True}})
        try:
            script = (
                'using Palette\n'
                'println("PARSERS_BEFORE: ", Base.find_package("Parsers") !== nothing)\n'
                'r = Palette.request_capability("package_management", Dict("name" => "Parsers"))\n'
                'println("approved: ", r["approved"])\n'
                'Core.eval(Main, :(using Parsers))\n'
                'println("PARSED: ", Parsers.parse(Int, "42"))\n'
                'store_write = try; write(joinpath(DEPOT_PATH[2], "worker-must-not-write"), "no"); true; catch; false; end\n'
                'println("PACKAGE_STORE_WRITE: ", store_write)\n'
            )
            r = self._run_package_worker("authorized-install-and-use",
                workspace_dir=self.workspace, project_dir=PROJECT_DIR, repo_dir=REPO_DIR,
                script=script, broker_socket_dir=sock_dir, network_enabled=False,
                depot_clone_dir=self.depot_dir, timeout=400,
            )
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
            self.assertIn("PARSERS_BEFORE: false", r.stdout)
            self.assertIn("approved: true", r.stdout)
            self.assertIn("PARSED: 42", r.stdout)
            self.assertIn("PACKAGE_STORE_WRITE: false", r.stdout)
        finally:
            server.shutdown()
            server.server_close()
            shutil.rmtree(sock_dir, ignore_errors=True)
        self.assertFalse(Path(self.package_store, "depot", "worker-must-not-write").exists())
        restarted = self._run_package_worker("fresh-process-restart",
            workspace_dir=self.workspace, project_dir=PROJECT_DIR, repo_dir=REPO_DIR,
            script='println("RESTART_LOAD_PATH: ", repr(LOAD_PATH)); '
                   'println("RESTART_DEPOT_PATH: ", repr(DEPOT_PATH)); '
                   'println("RESTART_ACTIVE_PROJECT: ", Base.active_project()); '
                   'println("PARSERS_AFTER_RESTART: ", Base.find_package("Parsers") !== nothing); '
                   'using Parsers; println("RESTART_PARSED: ", Parsers.parse(Int, "42"))',
            network_enabled=False, timeout=400,
        )
        self.assertEqual(restarted.returncode, 0, restarted.stdout + restarted.stderr)
        self.assertIn("PARSERS_AFTER_RESTART: true", restarted.stdout)
        self.assertIn("RESTART_PARSED: 42", restarted.stdout)
        self._package_test_passed = True

    def test_package_outside_allowlist_is_denied_without_installing(self):
        sock_dir, server = self._serve({"package_management": {"allowed_packages": ["Parsers"], "offline": True}})
        try:
            script = (
                'using Palette\n'
                'r = Palette.request_capability("package_management", Dict("name" => "DefinitelyNotAllowed"))\n'
                'println("approved: ", r["approved"])\n'
                'ok = Base.find_package("DefinitelyNotAllowed") !== nothing\n'
                'println("FORBIDDEN_USABLE: ", ok)\n'
            )
            r = self._run_package_worker("unauthorized-package-denial",
                workspace_dir=self.workspace, project_dir=PROJECT_DIR, repo_dir=REPO_DIR,
                script=script, broker_socket_dir=sock_dir, network_enabled=False,
                depot_clone_dir=self.depot_dir, timeout=180,
            )
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
            self.assertIn("approved: false", r.stdout)
            self.assertIn("FORBIDDEN_USABLE: false", r.stdout)
            self._package_test_passed = True
        finally:
            server.shutdown()
            server.server_close()
            shutil.rmtree(sock_dir, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
