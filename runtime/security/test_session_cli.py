#!/usr/bin/env python3
"""Real stdio CLI tests, including persistent state and scratch calls.

Julia subprocesses must use stdin=DEVNULL: inheriting this process's stdin
breaks later CLI requests, even when the first scratch request succeeds.
Run directly with python3 runtime/security/test_session_cli.py.
"""
from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from host_adapter import session_cmd  # noqa: E402

REPO_DIR = str(Path(__file__).resolve().parents[2])
CLI = str(Path(__file__).resolve().parent / "session_cli.py")
PROJECT_DIR = os.environ.get(
    "PALETTE_TEST_PROJECT_DIR",
    REPO_DIR,
)


def _skip_if_no_bwrap():
    if shutil.which("bwrap") is None:
        raise unittest.SkipTest("bubblewrap (bwrap) not found on PATH")


def _skip_if_no_project():
    if not Path(PROJECT_DIR).exists():
        raise unittest.SkipTest(f"no Julia dev project at {PROJECT_DIR}")


def _host_processes_with(marker: str) -> list[str]:
    """Command lines of every host process containing `marker`, read from the
    host's own /proc, which sees into every sandbox's PID namespace."""
    found = []
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            cmd = Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
        except OSError:
            continue
        if marker in cmd:
            found.append(cmd)
    return found


# Assembles the marker at runtime, so the child's own command line (which
# carries the turn source) never contains it; only descendants it spawns do.
def _descendant_code(marker: str, body: str) -> str:
    return f'mk = string("{marker[:6]}", "{marker[6:]}"); desc = "sleep 300; : $mk"; ' + body


# A daemon in the classic shape: fork, setsid, fork again, ignore TERM and
# HUP, close stdio, then exec the marked descendant.
_DAEMON_PY = (
    "import os, signal\n"
    "if os.fork(): os._exit(0)\n"
    "os.setsid()\n"
    "if os.fork(): os._exit(0)\n"
    "signal.signal(signal.SIGTERM, signal.SIG_IGN); signal.signal(signal.SIGHUP, signal.SIG_IGN)\n"
    "os.chdir('/')\n"
    "for f in (0, 1, 2): os.close(f)\n"
    "os.execvp('bash', ['bash', '-c', os.environ['D']])\n"
)
_ESCAPE_ATTEMPTS = (
    'run(`bash -c $desc`; wait=false); '
    'bash("(setsid nohup bash -c \\"trap \'\' TERM HUP INT; $desc\\" >/dev/null 2>&1 &)"); '
    'run(pipeline(`bash -c $desc`; stdout=stdout, stderr=stderr); wait=false); '
    f'write("d.py", {json.dumps(_DAEMON_PY)}); run(addenv(`python3 d.py`, "D" => desc)); '
)
_COUNT_INSIDE = (
    'sleep(1.0); count(p -> occursin(mk, try read("/proc/$p/cmdline", String) catch; "" end), '
    'filter(p -> all(isdigit, p), readdir("/proc")))'
)


class TestSessionCli(unittest.TestCase):
    def test_D05_private_layout_rejection_preserves_caller_files(self):
        with tempfile.TemporaryDirectory(prefix="palette-private-layout-") as tmp:
            base = Path(tmp)
            for mode in ("child", "equal", "parent", "receipts", "state"):
                with self.subTest(mode=mode):
                    scratch = base / mode / ("long-scratch-" + "x" * 80)
                    workspace = scratch / "broker" if mode == "child" else scratch if mode == "equal" else scratch.parent if mode == "parent" else base / mode / "workspace"
                    workspace.mkdir(parents=True)
                    marker = workspace / "caller-owned.txt"
                    marker.write_text("preserve")
                    command = [*session_cmd(str(scratch)), "--project-dir", PROJECT_DIR,
                        "--ceiling", "{}", "--workspace-dir", str(workspace)]
                    if mode == "receipts":
                        command += ["--receipts-dir", str(workspace)]
                    if mode == "state":
                        state = scratch / "state"
                        state.mkdir(parents=True)
                        state_marker = state / "durable-state.txt"
                        state_marker.write_text("preserve")
                        command += ["--state-dir", str(state)]
                    result = subprocess.run(command, input="", capture_output=True, text=True, timeout=60)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("overlap", result.stdout + result.stderr)
                    self.assertEqual(marker.read_text(), "preserve")
                    if mode == "state":
                        self.assertEqual(state_marker.read_text(), "preserve")

    def setUp(self):
        _skip_if_no_bwrap()
        _skip_if_no_project()
        self.task_workspace = tempfile.mkdtemp(prefix="palette-cli-task-")
        Path(self.task_workspace, "marker.txt").write_text("from-host")
        self.proc = subprocess.Popen(
            [*session_cmd(), "--project-dir", PROJECT_DIR, "--ceiling", "{}",
             "--workspace-dir", self.task_workspace, "--turn-timeout", "20"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
        )
        self.addCleanup(self._close_session)
        hello_line = self.proc.stdout.readline()
        self.assertTrue(hello_line, "no HELLO line -- process died before startup")
        self.hello = json.loads(hello_line)
        self.assertEqual(self.hello.get("kind"), "HELLO", self.hello)

    def _close_session(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        for pipe in (self.proc.stdin, self.proc.stdout, self.proc.stderr):
            try:
                pipe.close()
            except Exception:
                pass
        shutil.rmtree(self.task_workspace, ignore_errors=True)

    def _turn(self, code: str, request_id: str = "1", **extra) -> dict:
        req = {"request_id": request_id, "code": code, **extra}
        self.proc.stdin.write(json.dumps(req) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        self.assertTrue(line, f"no response line for request {req!r} -- stdin pipe likely broken")
        return json.loads(line)

    def test_hello_has_a_real_epoch(self):
        self.assertIsInstance(self.hello.get("epoch"), str)
        self.assertTrue(self.hello["epoch"])
        self.assertTrue(all(self.hello["capabilities"].get(name) for name in
            ("revival_observation_v1", "reload_observation_v1", "phase_costs_v1")))
        # A required-field consumer still needs only the established result.
        result = self._turn("legacy_value=42; (legacy_value, sort!(collect(keys(Palette.Api.costs()))))")
        self.assertEqual((result["success"], result["data"]), (True, [42, [1]]))
        self.assertEqual(self._turn("legacy_value+1")["data"], 43)
        # Run a real worker with the older wire shape, not a simulated reply.
        with tempfile.TemporaryDirectory(prefix="palette-legacy-wire-") as root:
            repo = Path(root, "repo")
            repo.mkdir()
            for name in ("src", "runtime/scripts"):
                shutil.copytree(Path(REPO_DIR, name), repo / name)
            for name in ("Project.toml", "Manifest.toml"):
                shutil.copyfile(Path(REPO_DIR, name), repo / name)
            loop = repo / "runtime/scripts/session_loop.jl"
            source = loop.read_text()
            marker = "function respond(resp::Dict{String,Any})\n"
            self.assertEqual(source.count(marker), 1)
            loop.write_text(source.replace(marker, marker +
                '    for key in ("schema_revision", "capabilities", "revival", "reloads", "costs"); pop!(resp, key, nothing); end\n'))
            process = subprocess.Popen([*session_cmd(), "--repo-dir", str(repo), "--project-dir", str(repo),
                "--ceiling", "{}", "--workspace-dir", self.task_workspace],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
            try:
                hello = json.loads(process.stdout.readline())
                self.assertEqual(hello["kind"], "HELLO", hello)
                self.assertEqual(hello["capabilities"], {})
                for code, expected in (("v=42", 42), ("v+1", 43)):
                    process.stdin.write(json.dumps({"request_id": "legacy", "code": code}) + "\n")
                    process.stdin.flush()
                    result = json.loads(process.stdout.readline())
                    self.assertEqual((result["success"], result["data"]), (True, expected), result)
                    self.assertIsNone(result["costs"])
                    self.assertIsNone(result["revival"])
            finally:
                process.stdin.close()
                try:process.wait(timeout=10)
                finally:
                    if process.poll() is None:process.kill(); process.wait()
                    process.stdout.close()

    def test_state_persists_across_turns(self):
        r1 = self._turn("x = 42")
        self.assertTrue(r1["success"])
        r2 = self._turn("x + 1")
        self.assertTrue(r2["success"])
        self.assertEqual(r2["data"], 43)

    def test_ephemeral_turn_does_not_break_the_stdin_pipe_for_later_turns(self):
        """The regression this file exists for. An ephemeral turn routes
        through spawn_child_worker, which calls launch_worker.run_worker,
        which calls resolve_real_julia_binary -- both subprocess.run
        `julia` invocations that used to inherit (and silently break)
        this CLI process's own stdin. If the fix regresses, this test
        hangs or fails on the turn AFTER the ephemeral one, not the
        ephemeral one itself."""
        r1 = self._turn("x = 42")
        self.assertTrue(r1["success"])

        r2 = self._turn("helper(y) = y * 10; helper(3)", ephemeral=True)
        self.assertTrue(r2["success"], r2)
        self.assertEqual(r2["data"], 30)

        # The real test: does the pipe still work at all, and is the
        # persistent mind's state (from turn 1) still there?
        r3 = self._turn("x")
        self.assertTrue(r3["success"], r3)
        self.assertEqual(r3["data"], 42)

        r4 = self._turn("@isdefined(helper)")
        self.assertTrue(r4["success"], r4)
        self.assertFalse(r4["data"])

    def test_malformed_request_gets_an_error_response_not_a_crash(self):
        self.proc.stdin.write("not valid json\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        self.assertTrue(line)
        resp = json.loads(line)
        self.assertFalse(resp["success"])
        self.assertIn("malformed", resp["error"])

        # process must still be alive and usable after a malformed request
        r = self._turn("1 + 1")
        self.assertTrue(r["success"])
        self.assertEqual(r["data"], 2)

    def test_printed_output_is_returned_and_does_not_corrupt_the_protocol(self):
        """The worker's stdout used to be the protocol channel, so one
        `println` in turn code was parsed as a response and killed the
        session. Output from subprocesses and from tasks still running after
        the turn must not reach the channel either."""
        r1 = self._turn('println("hello"); @warn "careful"; run(`echo from-child`); @async (sleep(0.2); println("late")); 7')
        self.assertTrue(r1["success"], r1)
        self.assertEqual(r1["data"], 7)
        for text in ("hello", "careful", "from-child"):
            self.assertIn(text, r1["output"])
        time.sleep(0.5)
        r2 = self._turn("8", request_id="2")
        self.assertEqual(r2["request_id"], "2")
        self.assertEqual(r2["data"], 8)

    def test_worker_works_in_the_task_workspace_and_writes_reach_the_host(self):
        r1 = self._turn('read("marker.txt", String)')
        self.assertEqual(r1["data"], "from-host")
        self._turn('write("from-julia.txt", "written")', request_id="2")
        # Checked from outside the sandbox, not from the worker's own view.
        self.assertEqual(Path(self.task_workspace, "from-julia.txt").read_text(), "written")

    def test_task_workspace_survives_session_close(self):
        self._turn("1")
        self.proc.stdin.close()
        self.proc.wait(timeout=60)
        self.assertTrue(Path(self.task_workspace, "marker.txt").is_file())

    def test_turn_code_reading_stdin_does_not_consume_the_next_request(self):
        """Turn code shared fd 0 with the request channel: a `readline()` took
        the next request, and the host waited for a reply until timeout."""
        r1 = self._turn('(readline(), read(`cat`, String))')
        self.assertTrue(r1["success"], r1)
        self.assertEqual(r1["data"], ["", ""])
        r2 = self._turn("2 + 2", request_id="2")
        self.assertEqual(r2["request_id"], "2")
        self.assertEqual(r2["data"], 4)

    def test_ephemeral_provenance_stays_out_of_the_task_workspace(self):
        r = self._turn("1 + 1", ephemeral=True)
        self.assertTrue(r["success"], r)
        self.assertEqual(sorted(os.listdir(self.task_workspace)), ["marker.txt"])

    def test_hung_ephemeral_child_is_killed_before_the_session_is(self):
        """The broker gave a child 90s while the session gave the turn less,
        so a hung ephemeral turn killed the persistent kernel with it."""
        self._turn("x = 5")
        r = self._turn("sleep(600)", request_id="2", ephemeral=True)
        self.assertFalse(r["success"])
        self.assertNotIn("session_dead", r)
        self.assertIn("time limit", r["error"])
        r3 = self._turn("x", request_id="3")
        self.assertEqual(r3["data"], 5)

    def test_stdlib_loads_without_precompiling(self):
        """Fails until runtime/security/prewarm_depot.py has run for this depot: a
        cold `using Pkg` took ~80s per session, longer than a turn."""
        r = self._turn("using Test, Pkg, SparseArrays; 1")
        self.assertTrue(r["success"], r)
        self.assertNotIn("Precompiling", r["output"], "run runtime/security/prewarm_depot.py for this depot")

    def test_bash_runs_shell_syntax_and_returns_its_output_and_status(self):
        """bash() used to return only the exit code, so a command's output
        could be read but not kept."""
        r = self._turn('r = bash("ls *.txt | wc -l; echo to-stderr >&2; exit 3")')
        self.assertTrue(r["success"], r)
        self.assertIn("exitcode=3", r["display"])
        self.assertEqual(r["output"].split(), ["1", "to-stderr"])
        r = self._turn("(r.exitcode, strip(r.stdout), strip(r.stderr), success(r))")
        self.assertEqual(r["data"], [3, "1", "to-stderr", False])

    def test_sh_literal_passes_dollar_to_the_shell(self):
        """In bash("...") a `$` is Julia interpolation, so `$?` and `$VAR`
        were parse errors."""
        r = self._turn('sh"x=5; echo $((x*2)); false; echo status=$?".stdout')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], "10\nstatus=1\n")
        r = self._turn('bash("echo $?")')
        self.assertFalse(r["success"])
        self.assertIn('sh"..."', r["error"])

    def test_payload_reaches_the_kernel_without_julia_quoting(self):
        """Source files embedded in Julia string literals failed to parse
        on triple quotes, `$` and backslashes."""
        text = 'def f():\n    """Doc with "quotes", $dollar and a \\\\ backslash."""\n    return 1\n'
        r = self._turn('write("f.py", PAYLOAD); read("f.py", String) == PAYLOAD', payload=text)
        self.assertTrue(r["success"], r)
        self.assertIs(r["data"], True)
        self.assertEqual(Path(self.task_workspace, "f.py").read_text(), text)
        r = self._turn("PAYLOAD === nothing")
        self.assertIs(r["data"], True, "a payload must not leak into the next call")
        r = self._turn('write("scratch.py", PAYLOAD); PAYLOAD == read("scratch.py", String)', payload=text, ephemeral=True)
        self.assertTrue(r["success"], r)
        self.assertIs(r["data"], True)
        self.assertGreaterEqual(r["costs"]["child_import_seconds"], 0)
        self.assertGreaterEqual(r["costs"]["child_broker_roundtrip_seconds"], r["costs"]["child_import_seconds"])
        self.assertNotIn("[background jobs", r["output"], "precompile jobs must not appear in a fresh scratch process")
        self.assertIs(self._turn("PAYLOAD === nothing")["data"], True)
        self.assertEqual(Path(self.task_workspace, "f.py").read_text(), text)
        self.assertFalse(Path(self.task_workspace, "scratch.py").exists())

    def test_a_docstring_that_ends_an_enclosing_string_is_named(self):
        """Code with a docstring, embedded in a triple-quoted string, failed in
        every endurance run as `invalid keyword argument name "last::Bool"`:
        the docstring's quotes ended the string and its signature ran as code."""
        code = ('block = """\n\n"""\n    move_to_end!(d, key; last::Bool=true)\n\nMove `key` to the end.\n"""\n'
                'function move_to_end!(d, key; last::Bool=true)\n    d\nend\n"""\nlength(block)')
        r = self._turn(code)
        self.assertFalse(r["success"], r)
        self.assertIn("ParseError", r["error"])
        self.assertFalse(self._turn("isdefined(@__MODULE__, :block)")["data"])
        self.assertIn("write(path, PAYLOAD)", r["error"])
        # A docstring kept whole in its own literal is fine, and an unrelated
        # error in the same shape of code gets no such hint.
        r = self._turn('doc = """\n    f(x)\n\nDoc.\n"""\nerror("unrelated")')
        self.assertFalse(r["success"], r)
        self.assertNotIn("docstring", r["error"])

    def test_an_undefined_name_interpolated_into_file_text_is_named(self):
        """Julia source for a file, written as a string literal: its `$K`
        ran in the kernel and failed as an undefined name, 13 times in the
        endurance runs, with no word about interpolation."""
        r = self._turn('write("f.jl", "function f(d::Dict{K,V}) where {K,V}\\n    error(\\"bad key type $K\\")\\nend\\n")')
        self.assertFalse(r["success"], r)
        self.assertIn("`K` is interpolated into a string", r["error"])
        self.assertIn("write(path, PAYLOAD)", r["error"])
        r = self._turn("undefined_thing + 1")
        self.assertFalse(r["success"], r)
        self.assertNotIn("interpolated", r["error"])

    def test_named_payload_texts_carry_an_edit(self):
        """An edit needs two texts, the old and the new, and a single payload
        held one, so models embedded both in Julia literals and met quoting
        errors (2 or more texts in 90% of the pilot's inline writes)."""
        old = 'x = "$a"\n"""\n    f(x)\n"""\n'
        new = 'x = "\\$b"  # \\ and """ kept\n'
        Path(self.task_workspace, "e.jl").write_text("head\n" + old + "tail\n")
        r = self._turn('p = "e.jl"; s = read(p, String); occursin(PAYLOAD["old"], s) || error("no"); '
                       'write(p, replace(s, PAYLOAD["old"] => PAYLOAD["new"])); sort(collect(keys(PAYLOAD)))',
                       payload={"old": old, "new": new})
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], ["new", "old"])
        self.assertEqual(Path(self.task_workspace, "e.jl").read_text(), "head\n" + new + "tail\n")
        r = self._turn("PAYLOAD === nothing", payload={"bad": 1})
        self.assertIs(r["data"], True, "a payload part that is not text is dropped, not bound")

    def test_the_workspace_map_opens_a_result_when_the_host_asks(self):
        """After a compaction a model rebuilt its picture of the repo from
        scratch; the host asks for the map at the first call and after each
        compaction, and it opens that call's result."""
        Path(self.task_workspace, "src").mkdir(exist_ok=True)
        Path(self.task_workspace, "src", "lib.rs").write_text("pub fn f() {}\n")
        Path(self.task_workspace, "Makefile").write_text("build:\n\tcargo build\ntest: build\n\tcargo test\n")
        r = self._turn("1 + 1", map=True)
        self.assertTrue(r["success"], r)
        self.assertTrue(r["output"].startswith("[workspace map]"), r["output"][:200])
        self.assertIn("Rust 1", r["output"])
        self.assertIn("Makefile: Makefile targets: build, test", r["output"])
        self.assertEqual(r["data"], 2)
        r = self._turn("2 + 2", request_id="2")
        self.assertNotIn("[workspace map]", r["output"])

    def test_the_digest_can_be_turned_off_for_an_ab(self):
        code = 'print(repeat("   Compiling dep v0.1.0\\n", 300)); println("error[E0308]: mismatched types\\n --> src/lib.rs:3:5")'
        r = self._turn(code)
        self.assertTrue(r["output"].startswith("[digest of"), r["output"][:80])
        r = self._turn(code, request_id="2", digest=False)
        self.assertFalse(r["output"].startswith("[digest of"), r["output"][:80])
        self.assertIn("src/lib.rs:3:5", r["output"])

    def test_stress_runs_a_flaky_command_many_times_in_the_sandbox(self):
        """A failure that shows up one run in many needs many runs; the report
        names the first failing run, which STRESS_RUN repeats."""
        r = self._turn('Palette.stress(PAYLOAD; n=60, jobs=6)', payload="[ $((STRESS_RUN % 20)) -ne 0 ] || { echo race at $STRESS_RUN; exit 2; }")
        self.assertTrue(r["success"], r)
        self.assertIn("60 runs in", r["display"])
        self.assertIn("3 failed (exit codes: 0×57, 2×3)", r["display"])
        self.assertIn("first failure: run 20 (STRESS_RUN=20) exited 2", r["display"])

    def test_background_jobs_are_named_when_they_change(self):
        """A model that started a server or a long build had no word of it
        again until it thought to look, and a failed task was reported as
        `ans` when the model had named it."""
        self._turn('gate=Channel{Nothing}(0); ready=Channel{Nothing}(1); '
                   'job = @async (put!(ready,nothing); take!(gate); 42); take!(ready); '
                   'srv = run(`sleep 300`; wait=false); nothing')
        r = self._turn("1", request_id="2")
        self.assertIn("[background jobs: running: task `job`, `sleep 300` (pid", r["output"])
        r = self._turn("1", request_id="3")
        self.assertNotIn("[background jobs", r["output"], "an unchanged set is not repeated")
        self.assertEqual(self._turn('put!(gate,nothing); fetch(job)')["data"], 42)
        r = self._turn("kill(srv); wait(srv); 1", request_id="4")
        self.assertIn("finished: task `job` (fetch(job) returns its value)", r["output"])
        self.assertIn("running: `sleep 300`", r["output"])
        r = self._turn('wait(srv); bad = @async error("boom"); wait(bad;throw=false); 1', request_id="5")
        self.assertIn("nothing is running any more", r["output"])
        r = self._turn("1", request_id="6")
        self.assertIn("[background: task `bad` failed: boom]", r["output"])
        named = {t["name"]: t for t in self._turn('Palette.jobs()')["data"]["tasks"]}
        self.assertEqual((named["job"]["state"], named["bad"]["state"]), ("finished", "failed"))
        self.assertEqual(named["bad"]["failure_type"], "ErrorException")
        self.assertEqual(named["job"]["first_observed_call"], 1)
        self.assertEqual(named["job"]["observed_epoch"], self.hello["epoch"])
        self.assertEqual(len(named["job"]["source_digest"]), 64)
        self.assertIsNone(named["job"]["creation_call"])

    def test_G_task_callback_dispatch_stays_ordinary_unless_explicitly_late_bound(self):
        self._turn('callback(x)=x+1; ready=Channel{Nothing}(2); gate=Channel{Nothing}(0); newest_gate=Channel{Nothing}(0); '
                   'old=@async (put!(ready,nothing);take!(gate);callback(1)); '
                   'newest=@async (put!(ready,nothing);take!(newest_gate);Base.invokelatest(callback,1)); '
                   'take!(ready);take!(ready);nothing')
        self.assertEqual(self._turn('callback(x)=x+100;put!(gate,nothing);put!(newest_gate,nothing);(fetch(old),fetch(newest))')["data"], [2, 101])
        observed = self._turn('Palette.taskinfo(old)')["data"]
        self.assertEqual(observed["state"], "finished")
        self.assertEqual(observed["first_observed_call"], 1)
        self.assertIsInstance(observed["world_at_first_observation"], str)

    def test_J_changes_since_ack_are_bounded_and_cover_existing_observations(self):
        file = Path(self.task_workspace, "observer.txt")
        file.write_text("old")
        self._turn('n=1; f(x)=x+1; input=read("observer.txt",String); ready=Channel{Nothing}(1); gate=Channel{Nothing}(0); '
                   'job=@async (put!(ready,nothing);take!(gate));take!(ready);nothing')
        baseline = self._turn('Palette.changes(acknowledge=true)')["data"]
        self.assertEqual(baseline["baseline"], "not_acknowledged")
        self.assertEqual(baseline["observed_through_call"], 1)
        self._turn('n=2; f(x)=x+9;put!(gate,nothing);fetch(job);nothing')
        file.write_text("new contents")
        delta = self._turn('Palette.changes()')["data"]
        self.assertIn("n", delta["bindings"]["changed"])
        self.assertIn("f", delta["definitions"]["changed"])
        self.assertIn(str(file), delta["files"]["changed"])
        self.assertIn("job", delta["jobs"]["changed"])
        self.assertEqual(delta["acknowledged_through_call"], 1)
        self._turn('Palette.changes(acknowledge=true)')
        empty = self._turn('Palette.changes()')["data"]
        for kind in ("bindings", "definitions", "files", "jobs"):
            self.assertEqual(empty[kind]["counts"], {"new": 0, "changed": 0, "removed": 0})
        self._turn('for i in 1:300; Core.eval(@__MODULE__,Expr(:(=),Symbol("Z",i),i));end;nothing')
        grown = self._turn('Palette.changes()')["data"]["bindings"]
        self.assertEqual(grown["counts"]["new"], 300)
        self.assertEqual(len(grown["new"]), 20)

    def test_display_does_not_corrupt_the_protocol(self):
        """display() wrote to the stdout captured at startup, which is the
        protocol pipe, and killed the session."""
        r = self._turn("display([1 2; 3 4]); 5")
        self.assertTrue(r["success"], r)
        self.assertIn("2×2 Matrix{Int64}", r["output"])
        self.assertEqual(self._turn("ans + 1")["data"], 6)

    def test_include_resolves_against_the_workspace(self):
        """include("m.jl") looked next to session_loop.jl."""
        r = self._turn('write("m.jl", "g(x) = 2x"); include("m.jl"); g(21)')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], 42)

    def test_edited_workspace_package_is_reloaded(self):
        """`using` a loaded package does nothing, so the kernel ran a
        workspace package's old code after the model edited it, and in-kernel
        tests checked code that no longer existed."""
        pkg = Path(self.task_workspace, "Wp")
        (pkg / "src").mkdir(parents=True)
        (pkg / "Project.toml").write_text('name = "Wp"\nuuid = "0f5c2a5e-9a44-4c3b-8f2e-6c1d8e7b9a01"\n')
        (pkg / "src" / "Wp.jl").write_text('module Wp\nexport f\n"f doc"\nf(x) = x + 1\ninclude("g.jl")\nend\n')
        (pkg / "src" / "g.jl").write_text("g(x) = 10x\n")
        r = self._turn('pushfirst!(LOAD_PATH, joinpath(pwd(), "Wp")); using Wp; (f(1), Wp.g(1))')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], [2, 10])

        # Edited in a turn of its own: reloaded when the next turn starts.
        r = self._turn('write("Wp/src/g.jl", "g(x) = 20x\\n"); 1')
        self.assertTrue(r["success"], r)
        r = self._turn("Wp.g(1)")
        self.assertEqual(r["data"], 20, r)
        self.assertIn("[reloaded Wp from the workspace: Wp/src/g.jl changed]", r["output"])
        self.assertNotIn("Replacing docs", r["output"])

        # Edited and then included in one call: include reloads first.
        Path(self.task_workspace, "t.jl").write_text("h_result = (f(1), Wp.h())\n")
        r = self._turn('write("Wp/src/Wp.jl", replace(read("Wp/src/Wp.jl", String), "x + 1" => "x + 2", '
                       '"include(\\"g.jl\\")" => "include(\\"g.jl\\")\\nh() = :new")); include("t.jl")')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], [3, "new"])

        # The root's redefinitions can land before an included file fails.
        r = self._turn('write("Wp/src/g.jl", "g(x) = (\\n"); 1')
        r = self._turn("Wp.g(1)")
        self.assertEqual(r["data"], 20, r)
        self.assertIn("reload_failed_partial_change", r["output"])

    def test_palette_handle_cannot_reset_the_kernel(self):
        """Palette.reset_kernel_state() used to erase every binding silently."""
        self._turn("keep = 1")
        r = self._turn("Palette.reset_kernel_state()")
        self.assertFalse(r["success"])
        self.assertEqual(self._turn("keep")["data"], 1)

    def test_kernel_helpers_describe_the_session(self):
        r = self._turn("struct Pt; x::Int; end; v = zeros(10); Pt(3)")
        self.assertEqual(r["display"], "Pt(3)", "types print without the session module's internal name")
        r = self._turn("varinfo()")
        self.assertTrue(r["success"], r)
        self.assertIn("v", r["display"].split())
        self.assertNotIn("PAYLOAD", r["display"])
        r = self._turn("kernelinfo()")
        self.assertIn("time limit 20s", r["output"])
        self.assertIn("network    none", r["output"])

    def test_unassigned_results_are_not_retained(self):
        """Every call's value used to stay referenced from the receipt log."""
        for i in range(3):
            self.assertTrue(self._turn(f"zeros(UInt8, 200 * 2^20); {i}")["success"])
        r = self._turn("GC.gc(); GC.gc(); Base.gc_live_bytes() / 2^20")
        self.assertLess(r["data"], 200, "three unbound 200 MiB results were kept alive")

    def test_shell_syntax_in_backticks_points_to_bash(self):
        r = self._turn("run(`ls *.txt 2>&1`)")
        self.assertFalse(r["success"])
        self.assertIn('bash("...")', r["error"])

    def test_missing_package_says_there_is_no_network(self):
        r = self._turn("using NoSuchPackageAnywhere")
        self.assertFalse(r["success"])
        self.assertIn("no network", r["error"])
        self.assertIn("Loadable: the Julia standard library", r["error"])
        self.assertIn("not_found_in_inspected_locations", r["error"])
        self.assertNotIn("Pkg.add", r["error"])

    def test_H_local_package_visibility_is_distinct_from_installation(self):
        pkg = Path(self.task_workspace, "HiddenLocal")
        (pkg / "src").mkdir(parents=True)
        (pkg / "Project.toml").write_text('name="HiddenLocal"\nuuid="cd794494-0a18-4444-9115-b9720d2bed07"\n')
        (pkg / "src/HiddenLocal.jl").write_text('module HiddenLocal; answer()=42; end\n')
        self._turn('environment_before=(copy(LOAD_PATH),read(Base.active_project(),String)); nothing')
        result = self._turn('using HiddenLocal')
        self.assertFalse(result["success"])
        self.assertIn("local_not_visible", result["error"])
        self.assertIn("LOAD_PATH", result["error"])
        self.assertNotIn("Pkg.add", result["error"])
        observed = self._turn('Palette.environment("HiddenLocal")')["data"]
        self.assertEqual(observed["local_paths"], [str(pkg)])
        self.assertTrue(observed["offline"])
        self.assertTrue(self._turn('environment_before==(LOAD_PATH,read(Base.active_project(),String))')["data"])
        self.assertEqual(self._turn('pushfirst!(LOAD_PATH,pwd()); using HiddenLocal; HiddenLocal.answer()')["data"], 42)
        self.assertEqual(self._turn('Palette.environment("HiddenLocal")')["data"]["status"], "visible")
        dependency = self._turn('Palette.environment("JSON")')["data"]
        self.assertTrue(dependency["declared_dependency"])
        self.assertEqual(dependency["status"], "visible")

    def test_pkg_add_read_only_failure_says_there_is_no_network(self):
        """A package install can fail after Julia reaches the read-only
        project/depot boundary instead of producing the usual resolver text.
        Keep that failure actionable without relabeling unrelated writes."""
        r = self._turn('error("Pkg.add failed: Read-only file system")')
        self.assertFalse(r["success"])
        self.assertIn("no network", r["error"])
        self.assertIn("Loadable: the Julia standard library", r["error"])

        unrelated = self._turn('error("write failed: Read-only file system")', request_id="2")
        self.assertFalse(unrelated["success"])
        self.assertNotIn("no network", unrelated["error"])

    def test_errors_name_the_failing_line_and_function(self):
        self._turn("function f(x)\n    return g(x)\nend")
        r = self._turn("a = 1\nb = f(a)", request_id="2")
        self.assertFalse(r["success"])
        self.assertIn("not defined in `Main`", r["error"])
        self.assertIn("f(x::Int64) at an earlier call, line 2", r["error"])
        self.assertIn("top-level code at this call, line 2", r["error"])

    def test_invalid_utf8_output_does_not_kill_the_session(self):
        """One invalid byte in turn output used to crash the bridge with a
        UnicodeDecodeError, ending the session."""
        r = self._turn('print(String(UInt8[0x61, 0xff, 0x62])); String(UInt8[0xfe])')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["output"], "a�b")
        r2 = self._turn("2 + 2", request_id="2")
        self.assertEqual(r2["data"], 4)

    def test_printing_many_lines_is_fast_and_ordered(self):
        """Each println through a libuv pipe cost ~50us, so 10^6 lines outran
        the turn limit; child output also has to stay in call order."""
        r = self._turn('print("a "); run(`echo b`); println("c"); for i in 1:10^6; println(i); end; 1')
        self.assertTrue(r["success"], r)
        self.assertTrue(r["output"].startswith("a b\nc\n1\n2\n"), r["output"][:40])
        self.assertIn("output truncated", r["output"])

    def test_redirecting_stdout_inside_a_turn_restores_the_capture(self):
        """Base could not restore stdout to the turn's writer: the devnull
        idiom threw, and a redirect to a file kept fd 1 on that file, so the
        rest of the turn's output was lost."""
        r = self._turn('redirect_stdout(devnull) do; println("hidden"); run(`echo hidden`); end; '
                       'open("log.txt", "w") do io; redirect_stdout(io) do; println("logged"); end; end; '
                       'println("visible"); run(`echo child`); read("log.txt", String)')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["output"], "visible\nchild\n")
        self.assertIn("logged", r["display"])

    def test_what_each_call_printed_is_kept(self):
        """The host elides the middle of long results; the kernel keeps the
        whole text, failed calls included, under the call's number."""
        r1 = self._turn('for i in 1:3000; println("row ", i); end', request_id="1")
        r2 = self._turn('println("before the error"); error("boom")', request_id="2")
        self.assertEqual((r1["call"], r2["call"]), (1, 2))
        r = self._turn('(count("\\n", Palette.output(1)), Palette.output(2))', request_id="3")
        self.assertEqual(r["data"], [3000, "before the error\n"], r)
        r = self._turn("Palette.output(3)", request_id="4")
        self.assertFalse(r["success"])
        self.assertIn("call 3 printed nothing", r["error"])

    def test_testset_value_is_shown_as_its_counts(self):
        """A passing test run returned its DefaultTestSet, displayed as every
        nested testset down to each one's RNG state: 14,000 to 25,000
        characters per run, all of it elided by the host."""
        r = self._turn('using Test; @testset "outer" begin; @test 1 == 1; '
                       '@testset "inner" begin; @test true; @test_broken false; end; end')
        self.assertTrue(r["success"], r)
        self.assertRegex(r["display"], r'^Test\.DefaultTestSet "outer": 2 passed, 0 failed, 0 errored, 1 broken \(.*s\)$')
        r = self._turn("(ans.description, length(ans.results))")
        self.assertEqual(r["data"], ["outer", 1])

    def test_background_output_and_failures_reach_the_next_call(self):
        """Output a task or process wrote after its call ended, and a bound
        task's failure, reached nobody."""
        r = self._turn('t_err = @async (sleep(0.5); error("background boom")); '
                       '@async (sleep(0.5); println("printed after the call")); '
                       'run(pipeline(`bash -c "sleep 0.5; echo from a process"`; stdout=stdout); wait=false); :started')
        self.assertTrue(r["success"], r)
        time.sleep(2)
        r = self._turn("1")
        self.assertTrue(r["output"].startswith("[background output since the last call]\n"), r["output"])
        self.assertIn("printed after the call", r["output"])
        self.assertIn("from a process", r["output"])
        self.assertIn("[background: task `t_err` failed: background boom]", r["output"])
        r = self._turn("2")
        self.assertEqual(r["output"], "", "each event is reported once")

    def test_background_job_in_sh_does_not_hold_the_call(self):
        """`sh"job &"` handed the job the output pipe, and reading it to its
        end waited for the job until the call's time limit killed it."""
        r = self._turn('t = time(); r = sh"(sleep 2; echo late) & echo started"; (round(time() - t), r.stdout)')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], [0, "started\n"])
        time.sleep(3)
        r = self._turn("1")
        self.assertIn("[background output since the last call]\nlate\n", r["output"])

    def test_reported_background_output_does_not_accumulate_in_tmp(self):
        """Reported job output must be removed without counting mounted depots."""
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
            gate = str(Path(self.task_workspace, "output-gate.sock"))
            listener.bind(gate)
            listener.listen()
            listener.settimeout(20)
            r = self._turn('using Sockets; tmp_before=parse(Int, split(read(`du -sb /tmp`, String))[1]); '
                           't=@async begin c=connect(PAYLOAD); read(c, 1); '
                           'for i in 1:3000; println(repeat("y", 1000)); end; '
                           'flush(stdout); write(c, "x"); close(c); end; :started', payload=gate)
            self.assertTrue(r["success"], r)
            with listener.accept()[0] as connection:
                connection.settimeout(20)
                connection.sendall(b"x")
                self.assertEqual(connection.recv(1), b"x")
            r = self._turn('1')
            self.assertIn("[background output since the last call]", r["output"])
            r = self._turn('(filesize(joinpath(tempdir(), "palette-background-output.log")), '
                           'parse(Int, split(read(`du -sb /tmp`, String))[1])-tmp_before)')
            sink, growth = r["data"]
            self.assertLess(sink, 100_000, r)
            self.assertLess(growth, 1_500_000, r)

    def test_file_changed_since_a_call_used_it_is_reported(self):
        """A binding computed from a workspace file kept the old contents
        after the file changed outside the call, with nothing to say so."""
        r = self._turn('write("data.csv", "a\\n1\\n"); d = read("data.csv", String); write("h.jl", "h() = 1"); include("h.jl")')
        self.assertTrue(r["success"], r)
        # Changed by a call that does not name the files: reported once.
        r = self._turn('for f in readdir(); endswith(f, ".csv") || endswith(f, ".jl") || continue; '
                       'open(io -> write(io, "\\n"), f, "a"); end')
        self.assertEqual(r["output"], "", r)
        r = self._turn("1")
        self.assertIn("[changed on disk since the call that used it: data.csv (call 1), h.jl (call 1).", r["output"])
        self.assertEqual(self._turn("2")["output"], "")
        # A call that names the file it changes is not told about its own change.
        self._turn('write("data.csv", "a\\n2\\n")')
        self.assertEqual(self._turn("3")["output"], "")

    def test_each_reply_lists_the_bindings_and_the_call_that_set_them(self):
        """When the kernel dies the host can only say which bindings were
        lost if it was told what they were."""
        self._turn("x = [1, 2]; y = 3; f(z) = z")
        r = self._turn("x = [5]; nothing")
        self.assertEqual(r["bindings"], ["f (function, call 1)", "x (Vector{Int64}, call 2)", "y (Int64, call 1)"])

    def test_ephemeral_result_is_displayed_like_a_persistent_one(self):
        """The child used to send struct internals as its result, and a
        value with no JSON form (NaN) crashed it after the code succeeded."""
        r = self._turn("[NaN, 1.0]", ephemeral=True)
        self.assertTrue(r["success"], r)
        self.assertEqual(r["display"], "2-element Vector{Float64}:\n NaN\n   1.0")
        r2 = self._turn("x -> x", request_id="2", ephemeral=True)
        self.assertIn("generic function", r2["display"])

    def test_B03_nested_reflection_and_cycles_survive_both_workers(self):
        cases = [('PAYLOAD', dict),
                 ('(reflection=methods(Palette.output),)', str),
                 ('a = Any[]; push!(a, a); a', str),
                 ('(a=[[1, 2], [3, 4]], label="λ")', dict),
                 ('Symbol(repeat("q", 2_000_000))', str),
                 ('using JSON; json_calls=Ref(0); struct EncoderNumber <: Number end; '
                  'Base.isfinite(::EncoderNumber)=true; JSON.lower(::EncoderNumber)=(json_calls[]+=1;1); EncoderNumber()', str),
                 ('struct FloodDisplay end; Base.show(io::IO,::MIME"text/plain",::FloodDisplay)='
                  '(for i in 1:1000000; print(io,"λ"); end); FloodDisplay()', str)]
        for ephemeral in (False, True):
            for code, expected in cases:
                with self.subTest(ephemeral=ephemeral, code=code):
                    r = self._turn(code, ephemeral=ephemeral, payload={"quoted": '"λ\''})
                    self.assertTrue(r["success"], r)
                    self.assertIsInstance(r["data"], expected, r)
                    self.assertLessEqual(len(json.dumps(r["data"], ensure_ascii=False).encode()), 6 * (256 * 1024 + 64))
            self.assertEqual(self._turn("40 + 2")["data"], 42)
        self.assertEqual(self._turn("json_calls[]")["data"], 0)

    def test_F_projection_deadline_reports_the_phase_and_preserves_world(self):
        result = self._turn('struct WaitingDisplay end; Base.show(io::IO,::MIME"text/plain",::WaitingDisplay)='
                            'wait(Channel{Nothing}(0)); retained=WaitingDisplay(); retained', request_id="projection")
        self.assertFalse(result["success"])
        self.assertTrue(result["interrupted"])
        self.assertEqual(result["costs"]["failed_phase"], "projection")
        self.assertIn("projection", result["error"])
        self.assertTrue(self._turn("isdefined(@__MODULE__,:retained)")["data"])
        observed = self._turn('Palette.costs(1)')["data"]
        self.assertGreaterEqual(observed["execution_seconds"], 0)
        self.assertIsNone(observed["projection_seconds"])
        self.assertGreaterEqual(observed["serialization_seconds"], 0)
        self.assertGreaterEqual(observed["worker_pipe_seconds"], 0)
        self.assertGreater(observed["worker_response_bytes"], 0)

    def test_B02_reload_reports_observed_changes_and_identity_risk(self):
        pkg = Path(self.task_workspace, "DeskKit")
        (pkg / "src").mkdir(parents=True)
        (pkg / "Project.toml").write_text('name = "DeskKit"\nuuid = "d6481d07-6a73-43c7-921b-e9e4d583ae53"\n')
        source = pkg / "src/DeskKit.jl"
        prefix = 'module DeskKit\nstruct Rule; weight::Int; end\n'
        source.write_text(prefix + 'apply(r::Rule,x)=r.weight*x\nobsolete()=:live\nend\n')
        self.assertEqual(self._turn('pushfirst!(LOAD_PATH,pwd()); using DeskKit; retained=DeskKit.Rule(3); DeskKit.apply(retained,7)')["data"], 21)
        source.write_text(prefix + 'apply(r::Rule,x)=r.weight*x+3\nobsolete()=:live\nend\n')
        r = self._turn('(value=DeskKit.apply(retained,7),reload=last(Palette.reloads()))')["data"]
        self.assertEqual((r["value"], r["reload"]["status"]), (24, "reload_succeeded"))
        source.write_text(prefix + 'apply(r::Rule,x)=r.weight*x+100\nobsolete()=:live\nerror("partial")\nend\n')
        r = self._turn('(value=DeskKit.apply(retained,7),reload=last(Palette.reloads()))')["data"]
        self.assertEqual((r["value"], r["reload"]["status"]), (121, "reload_failed_partial_change"))
        self.assertTrue(r["reload"]["methods_observed_changed"])
        self.assertNotEqual(r["reload"]["source_digest_before"], r["reload"]["source_digest_after"])
        source.write_text('module DeskKit\napply(r,x)=999\nfunction broken(\nend\n')
        r = self._turn('(value=DeskKit.apply(retained,7),reload=last(Palette.reloads()))')["data"]
        self.assertEqual((r["value"], r["reload"]["status"]), (121, "reload_failed_no_observed_change"))
        source.write_text(prefix + 'apply(r::Rule,x)=r.weight*x+3\nend\n')
        r = self._turn('last(Palette.reloads())')["data"]
        self.assertIn("obsolete", r["surviving_deleted_definitions"])
        self.assertEqual(self._turn('DeskKit.obsolete()')["data"], "live")
        origin = self._turn('Palette.provenance(:retained)')["data"]
        self.assertEqual(origin["origin_epoch"], self.hello["epoch"])
        self.assertEqual(len(origin["source_digest"]), 64)
        self.assertEqual(origin["freshness"], "definition_sources_changed")
        self.assertNotEqual(origin["definition_at_observation"]["source_digest"], origin["definition_current"]["source_digest"])
        source.write_text('module DeskKit\nstruct Rule; weight::Float64; end\napply(r::Rule,x)=r.weight*x\nend\n')
        r = self._turn('(compatible=retained isa DeskKit.Rule,reload=last(Palette.reloads()))')["data"]
        self.assertFalse(r["compatible"])
        self.assertEqual(r["reload"]["status"], "reload_requires_clean_process")
        self.assertIn("Rule", r["reload"]["identity_risks"])
        self.assertEqual(self._turn('Palette.provenance(:retained)')["data"]["freshness"], "earlier_type_definition")
        self._turn('module Replaced; struct R end; f(x::R)=1; end; old_object=Replaced.R()')
        r = self._turn('module Replaced; struct R end; f(x::R)=2; end; Replaced.f(old_object)')
        self.assertFalse(r["success"])
        self.assertIn("MethodError", r["error"])
        self.assertEqual(self._turn('Palette.provenance(:old_object)')["data"]["freshness"], "earlier_type_definition")
        previous = 0
        for generation in range(65):
            source.write_text('module DeskKit\nstruct Rule; weight::Float64; end\n'
                              f'apply(r::Rule,x)=r.weight*x+{generation}\nend\n')
            event = self._turn("nothing")
            sequence = event["reloads"][-1]["sequence"]
            self.assertGreater(sequence, previous)
            previous = sequence
        history = self._turn('let r=Palette.reloads(); (length(r),last(r)["sequence"]); end')["data"]
        self.assertEqual(history, [64, previous])

    def test_C_preparation_is_keyed_visible_and_offline(self):
        with tempfile.TemporaryDirectory(prefix="palette-preparation-test-") as root:
            repo = Path(root, "repo")
            repo.mkdir()
            for name in ("src", "runtime/scripts"):
                shutil.copytree(Path(REPO_DIR, name), repo / name)
            shutil.copyfile(Path(REPO_DIR, "Project.toml"), repo / "Project.toml")
            shutil.copyfile(Path(PROJECT_DIR, "Manifest.toml"), repo / "Manifest.toml")
            command = [sys.executable, str(Path(REPO_DIR, "runtime/security/prewarm_depot.py")),
                       "--project-dir", str(repo), "--repo-dir", str(repo), "--state-dir", str(Path(root, "state"))]
            def invoke(*flags):
                result = subprocess.run([*command, *flags], stdin=subprocess.DEVNULL, capture_output=True, text=True)
                return result.returncode, json.loads(result.stdout)
            self.assertEqual(invoke("--status")[1]["state"], "cold")
            code, record = invoke()
            self.assertEqual((code, record["state"]), (0, "prepared"), record)
            self.assertIsNone(record.get("error"), record)
            self.assertGreater(record["seconds"], 0)
            marker = next(Path(root, "state").glob("*.json"))
            marker.write_text(json.dumps({**record, "cache_files": []}))
            self.assertEqual(invoke("--status")[1]["state"], "failed")
            marker.write_text(json.dumps({**record, "state": "invented"}))
            self.assertEqual(invoke("--status")[1]["state"], "failed")
            marker.write_text(json.dumps(record))
            self.assertTrue(invoke()[1]["reused"])
            source = repo / "src/Palette.jl"
            source.write_text(source.read_text() + "\n# Preparation identity control\n")
            self.assertEqual(invoke("--status")[1]["state"], "stale")
            self.assertEqual(invoke()[1]["state"], "prepared")
            (repo / "Project.toml").write_text("this is invalid TOML!")
            code, record = invoke()
            self.assertEqual((code, record["state"]), (1, "failed"), record)
            self.assertEqual(invoke("--status")[1]["state"], "failed")
            marker = next(Path(root, "state").glob("*.json"))
            marker.write_text("[]")
            self.assertEqual(invoke("--status")[1]["state"], "failed")
            marker.write_text(json.dumps({**record, "state": "preparing", "pid": os.getpid(), "process_start": "wrong identity"}))
            self.assertEqual(invoke("--status")[1]["state"], "failed")

    def test_D_mcp_restarts_revive_existing_state_and_clean_restart_loses_it(self):
        with tempfile.TemporaryDirectory(prefix="palette-mcp-state-") as state_root:
            state = str(Path(state_root, "state"))
            plugin = Path(state_root, "plugin")
            subprocess.run([sys.executable, str(Path(REPO_DIR, "runtime/security/install_operator_plugin.py")),
                "--plugin-dir", str(plugin), "--workspace-dir", self.task_workspace], stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, check=True)
            connection = json.loads((plugin / ".mcp.json").read_text())["mcpServers"]["palette"]
            # This test targets MCP adapter restart/revival semantics directly;
            # service ownership and the socket transport are covered separately.
            connection["command"] = sys.executable
            connection["args"] = [str(Path(REPO_DIR, "runtime/security/serve_palette.py"))]
            env = {**os.environ, **connection["env"], "PALETTE_STATE_DIR": state,
                   "PALETTE_SCRATCH_ROOT": str(Path(state_root, "scratch"))}
            adapter = subprocess.Popen([connection["command"], *connection["args"]],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=env)
            def call(name, arguments, *, success=True):
                request = {"jsonrpc": "2.0", "id": "test", "method": "tools/call",
                           "params": {"name": name, "arguments": arguments}}
                adapter.stdin.write(json.dumps(request) + "\n"); adapter.stdin.flush()
                reply = json.loads(adapter.stdout.readline())["result"]
                self.assertEqual(reply["isError"], not success, reply)
                return json.loads(reply["content"][0]["text"])
            try:
                first = call("palette", {"code": 'n=42; matrix=reshape(collect(1.0:9.0),3,3); f(x)=x+1; '
                    'pending=@async wait(Channel{Nothing}(0)); write("durable.txt","durable"); nothing'})
                self.assertEqual(first["session"]["preparation"]["state"], "prepared")
                self.assertEqual(first["revival"]["state"], "fresh_empty_world")
                revived = call("palette_control", {"action": "restart"})
                self.assertNotEqual(revived["epoch"], first["epoch"])
                self.assertIn("restored exactly", revived["output"])
                self.assertIn("pending (Task", revived["output"])
                self.assertEqual(revived["revival"]["state"], "partially_restored")
                self.assertEqual(revived["session"]["restoration"], "partially_restored")
                self.assertIn("lost", revived["revival"]["states"])
                self.assertGreaterEqual(revived["revival"]["counts"]["restored"], 2)
                witness = call("palette", {"code": '(n,f(1),sum(matrix),!isdefined(@__MODULE__,:pending),read("durable.txt",String))'})
                self.assertEqual(witness["data"], [42, 2, 45.0, True, "durable"])
                self.assertNotIn("bindings", witness)
                self.assertNotIn("display", witness)
                self.assertNotIn("binding_changes", witness)
                inventory = call("palette", {"code": "nothing", "view": "full"})
                self.assertTrue(any(row.startswith("matrix (") for row in inventory["bindings"]))
                changed = call("palette", {"code": "n=43"})
                self.assertTrue(any(row.startswith("n (") for row in changed["binding_changes"]["new_or_changed"]))
                saved = call("palette_control", {"action": "restart"})
                death = call("palette", {"code": 'n=999;write("after-death.txt","file survived");exit(19)'}, success=False)
                self.assertTrue(death["session_dead"])
                recovered = call("palette", {"code": '(n,read("after-death.txt",String))'})
                self.assertEqual(recovered["data"], [43, "file survived"])
                self.assertNotEqual(recovered["epoch"], saved["epoch"])
                self.assertEqual(recovered["session"]["restoration"], "restored")
                self.assertIn("previous_connection_failure", recovered["session"])
                self.assertIn("unknown", recovered["session"]["recovery_warning"])
                clean = call("palette_control", {"action": "restart", "restore": False})
                self.assertNotEqual(clean["epoch"], revived["epoch"])
                self.assertEqual(clean["revival"]["state"], "fresh_empty_world")
                self.assertTrue(Path(clean["retired_snapshot"], "manifest.json").is_file())
                witness = call("palette", {"code": '(isdefined(@__MODULE__,:n),isdefined(@__MODULE__,:f),read("durable.txt",String))'})
                self.assertEqual(witness["data"], [False, False, "durable"])
                call("palette", {"code": "guard()=1"})
                call("palette", {"code": "guard()=2\nfunction incomplete("}, success=False)
                failed = call("palette", {"code": 'partial=123; error("after effect")'}, success=False)
                self.assertFalse(failed["success"])
                after_failure = call("palette_control", {"action": "restart"})
                self.assertEqual(after_failure["revival"]["state"], "restored")
                self.assertEqual(call("palette", {"code": "partial"})["data"], 123)
                self.assertEqual(call("palette", {"code": "guard()"})["data"], 1)
                for damage in ("corrupt", "incompatible"):
                    call("palette", {"code": "partial=123"})
                    adapter.stdin.close(); adapter.wait(timeout=40); adapter.stdout.close()
                    manifest = Path(state, "manifest.json")
                    snapshot = json.loads(manifest.read_text())
                    manifest.write_text("{" if damage == "corrupt" else json.dumps({**snapshot, "format": -1}))
                    adapter = subprocess.Popen([connection["command"], *connection["args"]],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=env)
                    damaged = call("palette", {"code": '(isdefined(@__MODULE__,:partial),read("durable.txt",String))'})
                    self.assertEqual(damaged["data"], [False, "durable"])
                    self.assertIn("lost", damaged["revival"]["states"])
                    self.assertEqual(damaged["session"]["restoration"], "uncertain" if damage == "corrupt" else "lost")
            finally:
                adapter.stdin.close()
                try:
                    adapter.wait(timeout=40)
                finally:
                    if adapter.poll() is None:
                        adapter.kill(); adapter.wait()
                    adapter.stdout.close()

    def test_top_level_loops_use_soft_scope_like_the_repl(self):
        """Turn code ran with script scope: assigning a global inside a
        top-level loop warned and created a new local instead."""
        r = self._turn("best = 0\nfor t in [3, 31, 18]\n    if t > best\n        best = t\n    end\nend\nbest")
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], 31)
        self.assertNotIn("Warning", r["output"])

    def test_session_packages_stay_loadable_after_activating_another_project(self):
        """A model's routine `Pkg.activate(".")` hid every session package."""
        r = self._turn('using Pkg; Pkg.activate("."); using JSON; JSON.json([1])')
        self.assertTrue(r["success"], r)
        self.assertEqual(sorted(os.listdir(self.task_workspace)), ["marker.txt"])

    def test_kernel_exit_reports_its_exit_code(self):
        r = self._turn("exit(3)")
        self.assertFalse(r["success"])
        self.assertIs(r.get("session_dead"), True)
        self.assertIn("exit code 3", r["error"])

    def test_nothing_an_ephemeral_turn_starts_outlives_it(self):
        """The child has its own PID namespace; when it exits, bwrap's reaper
        (PID 1) exits and the kernel kills everything left in that namespace, however it was
        detached. Four escape shapes are started and seen alive inside the
        child, then none may remain on the host."""
        marker = "NJL" + uuid.uuid4().hex[:10]
        r = self._turn(_descendant_code(marker, _ESCAPE_ATTEMPTS + _COUNT_INSIDE), ephemeral=True)
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], 4, "an escape attempt never started, so this test would prove nothing")
        time.sleep(1.0)
        self.assertEqual(_host_processes_with(marker), [])

    def test_descendants_of_a_timed_out_ephemeral_child_die_with_it(self):
        marker = "NJL" + uuid.uuid4().hex[:10]
        code = _descendant_code(marker, _ESCAPE_ATTEMPTS + "sleep(600)")
        self.proc.stdin.write(json.dumps({"request_id": "1", "code": code, "ephemeral": True}) + "\n")
        self.proc.stdin.flush()
        seen = 0
        deadline = time.time() + 30
        while time.time() < deadline and seen < 4:
            seen = len(_host_processes_with(marker))
            time.sleep(0.2)
        self.assertEqual(seen, 4, "the escape attempts never all started")
        r = json.loads(self.proc.stdout.readline())
        self.assertIn("time limit", r["error"])
        time.sleep(1.0)
        self.assertEqual(_host_processes_with(marker), [])

    def test_persistent_kernel_descendants_do_persist_until_the_session_ends(self):
        """Control for the two tests above: the same scan does find a live
        descendant, and the persistent kernel keeps its own."""
        marker = "NJL" + uuid.uuid4().hex[:10]
        r = self._turn(_descendant_code(marker, "run(`bash -c $desc`; wait=false); " + _COUNT_INSIDE))
        self.assertEqual(r["data"], 1)
        self._turn("1", request_id="2")
        self.assertEqual(len(_host_processes_with(marker)), 1)
        self.proc.stdin.close()
        self.proc.wait(timeout=60)
        time.sleep(1.0)
        self.assertEqual(_host_processes_with(marker), [])

    def test_task_tools_reach_the_kernel_and_ephemeral_children_read_only(self):
        """A benchmark runner exposes the task image's tools (interpreter,
        bun, ...) to every contestant as one read-only directory; NP2's worker
        used to drop it, so a task's own runtime was unreachable."""
        tools = tempfile.mkdtemp(prefix="palette-task-tools-")
        Path(tools, "bin").mkdir()
        tool = Path(tools, "bin", "task-tool")
        tool.write_text("#!/bin/sh\necho task-tool-ran\n")
        tool.chmod(0o755)
        proc = subprocess.Popen(
            [*session_cmd(), "--project-dir", PROJECT_DIR, "--ceiling", "{}",
             "--workspace-dir", self.task_workspace, "--turn-timeout", "20"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, bufsize=1, env={**os.environ, "PALETTE_TASK_TOOLS": tools},
        )
        try:
            self.assertEqual(json.loads(proc.stdout.readline())["kind"], "HELLO")
            code = 'readchomp(`task-tool`), (try; touch(joinpath(ENV["PATH"][1:findfirst(\':\', ENV["PATH"])-1], "x")); "writable"; catch; "read-only"; end)'
            for i, ephemeral in enumerate((False, True)):
                proc.stdin.write(json.dumps({"request_id": str(i), "code": code, "ephemeral": ephemeral}) + "\n")
                proc.stdin.flush()
                r = json.loads(proc.stdout.readline())
                self.assertTrue(r["success"], r)
                self.assertEqual(r["data"], ["task-tool-ran", "read-only"], f"ephemeral={ephemeral}")
        finally:
            proc.stdin.close()
            proc.wait(timeout=60)
            proc.stdout.close()
            shutil.rmtree(tools, ignore_errors=True)

    def test_task_environment_and_a_named_user_reach_the_kernel(self):
        """A task's toolchain needs its activation (OCaml looked for its library
        at the build machine's path without OCAMLLIB, tsc without CONDA_PREFIX),
        and initdb and whoami failed with no user for uid 1000. The other
        contestants get both from the host."""
        env_file = Path(tempfile.mkdtemp(prefix="palette-task-env-"), "task-env")
        env_file.write_text("# activation\nOCAMLLIB=/opt/ocaml/lib\nGREETING=a b=c\nPATH=/evil\nHOME=/evil\n\n")
        proc = subprocess.Popen(
            [*session_cmd(), "--project-dir", PROJECT_DIR, "--ceiling", "{}",
             "--workspace-dir", self.task_workspace, "--turn-timeout", "20"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, bufsize=1, env={**os.environ, "PALETTE_TASK_ENV": str(env_file)},
        )
        try:
            self.assertEqual(json.loads(proc.stdout.readline())["kind"], "HELLO")
            code = ('(ENV["OCAMLLIB"], ENV["GREETING"], ENV["HOME"], occursin("/evil", ENV["PATH"]), '
                    'strip(read(`whoami`, String)), ENV["USER"])')
            for i, ephemeral in enumerate((False, True)):
                proc.stdin.write(json.dumps({"request_id": str(i), "code": code, "ephemeral": ephemeral}) + "\n")
                proc.stdin.flush()
                r = json.loads(proc.stdout.readline())
                self.assertTrue(r["success"], r)
                self.assertEqual(r["data"], ["/opt/ocaml/lib", "a b=c", "/run/palette/home", False, "palette", "palette"],
                                 f"ephemeral={ephemeral}")
        finally:
            proc.stdin.close()
            proc.wait(timeout=60)
            proc.stdout.close()
            shutil.rmtree(env_file.parent, ignore_errors=True)

    def test_workspace_package_loads_ahead_of_the_kernels_copy(self):
        """The kernel's environment has OrderedCollections (DataFrames needs
        it). A model working on OrderedCollections appended the workspace to
        LOAD_PATH, and its whole test suite passed against the kernel's copy."""
        ws = tempfile.mkdtemp(prefix="palette-cli-pkg-")
        Path(ws, "src").mkdir()
        Path(ws, "Project.toml").write_text(
            'name = "OrderedCollections"\nuuid = "bac558e1-5e72-5ebc-8fee-abe8a469f55d"\nversion = "2.0.1"\n')
        Path(ws, "src", "OrderedCollections.jl").write_text("module OrderedCollections\nconst WORKSPACE_COPY = true\nend\n")
        proc = subprocess.Popen(
            [*session_cmd(), "--project-dir", PROJECT_DIR, "--ceiling", "{}",
             "--workspace-dir", ws, "--turn-timeout", "60"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
        try:
            self.assertEqual(json.loads(proc.stdout.readline())["kind"], "HELLO")
            code = ('push!(LOAD_PATH, pwd()); using OrderedCollections; '
                    '(isdefined(OrderedCollections, :WORKSPACE_COPY), startswith(pathof(OrderedCollections), pwd()))')
            proc.stdin.write(json.dumps({"request_id": "1", "code": code}) + "\n")
            proc.stdin.flush()
            r = json.loads(proc.stdout.readline())
            self.assertEqual(r["data"], [True, True], r)
        finally:
            proc.stdin.close()
            proc.wait(timeout=60)
            proc.stdout.close()
            shutil.rmtree(ws, ignore_errors=True)

    def test_quote_that_ends_sh_early_is_explained(self):
        """A `"` inside sh"..." made the next word a string-macro suffix, and
        Julia reported only a MethodError for @sh_str."""
        r = self._turn('sh"printf %s \\"quoted\\""')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["output"], "quoted")
        r = self._turn('sh"echo "hello')
        self.assertFalse(r["success"])
        self.assertIn("a quote inside sh", r["error"])
        self.assertIn("`hello`", r["error"])
        self.assertIn("bash(PAYLOAD)", r["error"])

    def test_timeout_interrupts_waiting_work_and_keeps_the_kernel(self):
        """A timeout used to kill the kernel even when the call was only
        waiting. Processes an earlier call started keep running; processes the
        interrupted call started are stopped."""
        self._turn("keep = 99; bg = run(`sleep 300`; wait=false); nothing")
        r = self._turn('println("before"); run(`sleep 300`)')
        self.assertFalse(r["success"])
        self.assertIs(r.get("interrupted"), True)
        self.assertNotIn("session_dead", r)
        self.assertIn("before", r["output"])
        self.assertIn("every binding are intact", r["error"])
        r = self._turn('(keep, process_running(bg), strip(sh"pgrep -c -x sleep".stdout))')
        self.assertEqual(r["data"], [99, True, "1"])

    def test_finished_background_processes_are_reaped(self):
        """With julia as PID 1 an orphaned process that finished stayed a
        zombie, and `pgrep` kept reporting it as running."""
        r = self._turn('sh"(sleep 0.5 &); true"; sleep(2); strip(sh"ps -eo stat= | grep -c ^Z || true".stdout)')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], "0")

    def test_compute_that_never_yields_stops_the_kernel(self):
        r = self._turn("s = 0.0; i = 0; while true; s += sin(i); i += 1; end")
        self.assertFalse(r["success"])
        self.assertIs(r.get("session_dead"), True)
        self.assertIn("never yields", r["error"])

    def test_turn_that_swallows_the_interrupt_stops_the_kernel(self):
        r = self._turn("while true; try sleep(0.5) catch end; end")
        self.assertFalse(r["success"])
        self.assertIs(r.get("session_dead"), True)
        self.assertIn("kept running after being interrupted", r["error"])


if __name__ == "__main__":
    unittest.main()
