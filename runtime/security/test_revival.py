#!/usr/bin/env python3
"""
Tests for state revival (src/revival.jl): a kernel that replaces a stopped
one revives the last saved state, and says exactly what was restored,
rebuilt, stale, different or lost. A quick snapshot is saved after every
completed call; a slow one gives way to a waiting request, for at most five
calls.

The point of most tests here is the opposite of restoring: a revival that
looks continuous but is wrong is worse than reporting state lost, so each
adversarial case checks that the report does NOT claim what is not true.

Run:
    JULIA_DEPOT_PATH=~/.palette-trial/depot PALETTE_TEST_PROJECT_DIR=~/.palette-trial/project \\
        python3 runtime/security/test_revival.py
"""
from __future__ import annotations

import json
import time
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from host_adapter import session_cmd  # noqa: E402

CLI = str(Path(__file__).resolve().parent / "session_cli.py")
PROJECT_DIR = os.environ.get("PALETTE_TEST_PROJECT_DIR", str(Path.home() / ".palette-trial/project"))


class Kernel:
    """One session_cli process: one kernel, over the given workspace and state directory."""

    def __init__(self, workspace: str, state: str, timeout: float = 20, env: dict | None = None):
        self.proc = subprocess.Popen(
            [*session_cmd(), "--repo-dir", str(Path(__file__).resolve().parents[2]), "--project-dir", PROJECT_DIR, "--ceiling", "{}", "--workspace-dir", workspace,
             "--turn-timeout", str(timeout), "--state-dir", state],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1,
            env={**os.environ, **(env or {})},
        )
        self.hello = json.loads(self.proc.stdout.readline())
        self.n = 0

    def turn(self, code: str) -> dict:
        self.n += 1
        self.proc.stdin.write(json.dumps({"request_id": str(self.n), "code": code}) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        assert line, "no reply"
        return json.loads(line)

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=60)
        finally:
            self.proc.stdout.close()


def section(report: str, heading: str) -> str:
    m = re.search(rf"^  {re.escape(heading)}: (.*)$", report, re.M)
    return m.group(1) if m else ""


class RevivalTest(unittest.TestCase):
    def setUp(self):
        if shutil.which("bwrap") is None or not Path(PROJECT_DIR).exists():
            self.fail("needs bwrap and the prepared test project")
        self.ws = tempfile.mkdtemp(prefix="palette-revival-ws-")
        self.state = tempfile.mkdtemp(prefix="palette-revival-state-")

    def tearDown(self):
        shutil.rmtree(self.ws, ignore_errors=True)
        shutil.rmtree(self.state, ignore_errors=True)

    def session(self, *codes: str, env: dict | None = None, timeout: float = 20, may_fail: bool = False) -> list[dict]:
        k = Kernel(self.ws, self.state, timeout=timeout, env=env)
        try:
            results = [k.turn(c) for c in codes]
        finally:
            k.close()
        if not may_fail:
            for r in results:
                self.assertTrue(r["success"], r)
        return results

    def revive(self, code: str = "nothing", env: dict | None = None) -> tuple[str, dict]:
        k = Kernel(self.ws, self.state, env=env)
        try:
            r = k.turn(code)
            return r["output"], r
        finally:
            k.close()

    def manifest(self) -> dict:
        return json.loads(Path(self.state, "manifest.json").read_text())

    def test_MT016_dynamic_include_and_five_dependents(self):
        Path(self.ws, "foundation.jl").write_text("module Foundation; export Token; struct Token; n::Int; end; end\n")
        self.session('path_="foundation.jl"; include(path_); using .Foundation; ' +
                     '; '.join(f'function dependent{i}(x::Foundation.Token); x.n+{i}; end' for i in range(5)))
        out, r = self.revive('([dependent0(Foundation.Token(7)), dependent4(Foundation.Token(7))], Palette.revival()["lost"])')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"][0], [7, 11])
        self.assertFalse(r["data"][1], out)

    def test_MT016_definition_graph_handles_restored_constants(self):
        self.session('const N=2; struct ParametricData; x::NTuple{N,Int}; end; retained=ParametricData((1,2)); next_value(x::ParametricData)=x.x[2]')
        m=self.manifest(); m["definitions"].reverse()
        Path(self.state,"manifest.json").write_text(json.dumps(m))
        out,r=self.revive('next_value(retained)')
        self.assertTrue(r["success"],r)
        self.assertEqual(r["data"],2)
        self.assertNotIn("not revived",out)

    def test_MT026_dynamic_include_result_and_scoped_block(self):
        Path(self.ws, "value.jl").write_text("81")
        self.session('path="value.jl"; result=include(path); unrelated=42', 'begin local x=3; scoped_value=x+1; end')
        Path(self.ws, "value.jl").write_text("82")
        _, r = self.revive('(result, unrelated, scoped_value, Palette.provenance(:result)["freshness"], Palette.provenance(:unrelated)["freshness"])')
        self.assertEqual(r["data"], [81, 42, 4, "source_files_changed", "no_observed_staleness"])

    def test_MT017_world_identity_after_revival(self):
        self.session("mt003_marker=[1,2,3]")
        out, r = self.revive('((@__MODULE__) === Main, isdefined(Main,:mt003_marker), :mt003_marker in names(Main; all=true), getglobal(Main,:mt003_marker) === mt003_marker)')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], [True]*4)

    def test_MT024_MT026_source_digest_and_precise_dependents(self):
        source = Path(self.ws, "source.jl"); source.write_text("source_value=101\n")
        self.session('include("source.jl"); derived_value=source_value+1; unrelated_integer=9; unrelated_string="independent"')
        stamp = source.stat(); source.write_text("source_value=202\n"); os.utime(source, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        out, r = self.revive('[Palette.provenance(n)["freshness"] for n in (:source_value,:derived_value,:unrelated_integer,:unrelated_string)]')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], ["source_files_changed"]*2+["no_observed_staleness"]*2)
        self.session('include("source.jl"); derived_value=source_value+1')
        out, r = self.revive('Palette.provenance(:derived_value)["freshness"]')
        self.assertEqual(r["data"], "no_observed_staleness")

    def test_MT028_tracked_artifact_authority_advances(self):
        import hashlib
        artifact=Path(self.ws,"artifact.txt"); authority=Path(self.ws,"authority.json")
        artifact.write_text("A"); authority.write_text(json.dumps({"generation":"A", "commit":"commit-A", "digest":hashlib.sha256(b"A").hexdigest()}))
        self.session('artifact=Palette.read_artifact("artifact.txt"; authority_path="authority.json"); derived_artifact=artifact.content*"-derived"; unrelated=42')
        artifact.write_text("B"); authority.write_text(json.dumps({"generation":"B", "commit":"commit-B", "digest":hashlib.sha256(b"B").hexdigest()}))
        out,r=self.revive('(derived_artifact,Palette.provenance(:derived_artifact)["freshness"],Palette.provenance(:unrelated)["freshness"])')
        self.assertTrue(r["success"],r)
        self.assertEqual(r["data"],["A-derived","artifact_superseded","no_observed_staleness"])

    # --- what should come back ------------------------------------------

    def test_parse_rejected_definitions_are_not_replayed(self):
        results = self.session("guard() = 1", "guard() = 2\nfunction incomplete(", may_fail=True)
        self.assertEqual([r["success"] for r in results], [True, False])
        out, result = self.revive("guard()")
        self.assertEqual(result["data"], 1)
        self.assertIn("guard (function, call 1)", section(out, "rebuilt from source"))

    def test_plain_data_and_containers_are_restored_exactly(self):
        self.session('n = 42; x = 1.5; s = "héllo"; sym = :k; c = \'z\'; nothing_ = nothing; m = missing; '
                     'bg = big(2)^100; v = [1, 2, 3]; mat = rand(3, 3); d = Dict("a" => [1, 2], "b" => Dict(:c => 3)); '
                     'st = Set([1, 2]); tup = (1, "two", 3.0); nt = (a = 1, b = [2]); rng = collect(1:5); sub = SubString("hello", 1, 3)')
        out, r = self.revive("(n, x, s, sym, c, nothing_ === nothing, ismissing(m), bg == big(2)^100, v, size(mat), "
                             "d[\"b\"][:c], st == Set([1, 2]), tup, nt.b, rng, sub)")
        self.assertTrue(r["success"], r)
        self.assertIn("end of call 1", out)
        exact = section(out, "restored exactly")
        for name in ("n", "x", "s", "sym", "d", "st", "tup", "nt", "bg", "mat", "sub"):
            self.assertIn(f"{name} (", exact)
        self.assertTrue(r["display"].startswith('(42, 1.5, "héllo", :k, \'z\', true, true, true, [1, 2, 3], (3, 3), 3, true, (1, "two", 3.0), [2], [1, 2, 3, 4, 5], "hel")'), r["display"])
        self.assertNotIn("not revived", out)

    def test_dataframes_and_parsed_structures(self):
        self.session('using DataFrames; df = DataFrame(a = 1:3, b = ["x", "y", "z"]); '
                     'ex = Meta.parse("f(x) = x + 1"); tree = Meta.parseall("a = 1\\nb = a + 2")')
        out, r = self.revive("(size(df), df.b, ex.head, length(tree.args), eval(Meta.parse(\"1 + 2\")))")
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], [[3, 2], ["x", "y", "z"], "=", 4, 3])
        self.assertIn("df (DataFrames.DataFrame, call 1)", section(out, "restored exactly"))

    def test_shared_references_stay_shared_and_cycles_survive(self):
        self.session('mutable struct Node; name::String; next::Union{Nothing, Node}; end\n'
                     'a = Node("a", nothing); b = Node("b", a); a.next = b; ring = [a, b]\n'
                     'v = [1, 2]; p = (v, v); w = v; holder = Dict(:v => v)')
        out, r = self.revive("(p[1] === p[2] === w === holder[:v], ring[1].next === ring[2], a.next.next === a, "
                             "ring[1] === a)")
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], [True, True, True, True])

    def test_functions_methods_types_and_macros_are_rebuilt_from_source(self):
        self.session('abstract type Shape end\n'
                     'struct Circle <: Shape; r::Float64; end\n'
                     'Base.@kwdef struct Opts; n::Int = 3; end\n'
                     '"doc for area"\narea(c::Circle) = pi * c.r^2\n'
                     'area(x::Number) = x\n'
                     'function scale(s::Shape, k); Circle(s.r * k); end\n'
                     'macro twice(ex) :($(esc(ex)) * 2) end\n'
                     '@enum Color red green\n'
                     'Base.show(io::IO, c::Circle) = print(io, "Circle(", c.r, ")")\n'
                     'const LIMIT = 10\n'
                     'c = Circle(2.0)')
        out, r = self.revive('(area(c), area(3), repr(scale(c, 2)), @twice(4), Opts().n, Int(green), LIMIT, '
                             'string(@doc area), c isa Shape)')
        self.assertTrue(r["success"], r)
        self.assertAlmostEqual(r["data"][0], 3.14159265 * 4, places=5)
        self.assertEqual(r["data"][1:7], [3, "Circle(4.0)", 8, 3, 1, 10])
        self.assertIn("doc for area", r["data"][7])
        self.assertTrue(r["data"][8])
        rebuilt = section(out, "rebuilt from source")
        for name in ("Circle (type", "Shape (type", "area (function", "scale (function", "Opts (type", "Color (type"):
            self.assertIn(name, rebuilt)
        self.assertIn("c (Circle, call 1)", section(out, "restored exactly"))
        self.assertIn("LIMIT (Int64, call 1)", section(out, "restored exactly"))
        # Definitions are never serialized.
        blobs = b"".join(p.read_bytes() for p in Path(self.state).glob("data-*.jls"))
        self.assertNotIn(b"area", blobs)

    def test_packages_load_path_env_and_cwd_come_back(self):
        pkg = Path(self.ws, "Wp", "src")
        pkg.mkdir(parents=True)
        Path(self.ws, "Wp", "Project.toml").write_text('name = "Wp"\nuuid = "0f5c2a5e-9a44-4c3b-8f2e-6c1d8e7b9a03"\n')
        Path(pkg, "Wp.jl").write_text("module Wp\nf() = 7\nend\n")
        self.session('pushfirst!(LOAD_PATH, joinpath(pwd(), "Wp")); using Wp; using Statistics: median; import JSON; '
                     'ENV["MY_SETTING"] = "on"; mkdir("sub"); cd("sub"); r = median([1, 2, 3])')
        out, r = self.revive('(Wp.f(), median([4, 5, 6]), JSON.json([1]), ENV["MY_SETTING"], basename(pwd()), r)')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], [7, 5.0, "[1]", "on", "sub", 2.0])

    def test_legacy_snapshot_retains_current_managed_package_environment(self):
        with tempfile.TemporaryDirectory(prefix="palette-revival-packages-") as store:
            env = {"PALETTE_PACKAGE_DEPOT": store}
            package_environment = str(Path(store, "environment"))
            self.session("retained_before_provisioning=42", env=env)
            manifest = self.manifest()
            manifest["load_path"] = [p for p in manifest["load_path"] if p != package_environment]
            manifest["env"]["PALETTE_PACKAGE_ENVIRONMENT"] = self.ws
            Path(self.state, "manifest.json").write_text(json.dumps(manifest))
            code = '(retained_before_provisioning, ' + json.dumps(package_environment) + ' in LOAD_PATH, ' + \
                   'ENV["PALETTE_PACKAGE_ENVIRONMENT"] == ' + json.dumps(package_environment) + ', Palette.revival()["lost"])'
            out, result = self.revive(code, env=env)
            self.assertTrue(result["success"], result)
            self.assertEqual(result["data"], [42, True, True, []], out)

    # --- what must be marked, not restored silently --------------------

    def test_value_computed_from_a_file_that_changed_is_marked_stale(self):
        Path(self.ws, "data.csv").write_text("a\n1\n2\n")
        # Staleness is tracked per call: a value set by the call that read the
        # file counts as computed from it. `unrelated` comes from its own call.
        self.session('using CSV, DataFrames; d = CSV.read("data.csv", DataFrame); total = sum(d.a)', "unrelated = 5")
        original = next(b["origin"] for b in self.manifest()["bindings"] if b["name"] == "total")
        Path(self.ws, "data.csv").write_text("a\n10\n20\n")
        out, r = self.revive("(value=total,provenance=Palette.provenance(:total))")
        self.assertTrue(r["success"], r)
        provenance = r["data"]["provenance"]
        self.assertEqual(provenance["source_digest"], original["source_digest"])
        self.assertEqual(len(provenance["source_digest"]), 64)
        self.assertEqual(provenance["origin_epoch"], original["epoch"])
        self.assertNotEqual(provenance["epoch"], provenance["origin_epoch"])
        self.assertEqual(provenance["freshness"], "source_files_changed")
        stale = section(out, "restored but stale (a file it was computed from changed)")
        self.assertIn("d (DataFrames.DataFrame, call 1): data.csv changed since", stale)
        self.assertIn("total (Int64, call 1): data.csv changed since", stale)
        self.assertIn("unrelated (", section(out, "restored exactly"))
        self.assertNotIn("total (", section(out, "restored exactly"))

    def test_definitions_from_a_changed_include_are_not_called_the_same(self):
        Path(self.ws, "lib.jl").write_text("helper(x) = 10x\nstruct Box; v::Int; end\nmodule Nested; struct B; v::Int; end; end\n")
        self.session('include("lib.jl"); b = Box(1); nested_box=Nested.B(1)')
        self.assertIn("Nested.B", self.manifest()["type_fingerprints"])
        Path(self.ws, "lib.jl").write_text("helper(x) = 20x\nstruct Box; v::Float64; end\nmodule Nested; struct B; v::Float64; end; end\n")
        out, r = self.revive("(helper(1), @isdefined(b), @isdefined(nested_box))")
        self.assertNotIn("helper (", section(out, "rebuilt from source"))
        self.assertIn("helper (function, call 1): rebuilt from a file that changed since", section(out, "not the same as before"))
        self.assertIn("Box (type, call 1): rebuilt, but its methods or fields differ", section(out, "not the same as before"))
        # A Box saved with an Int field is not poured into a Box with a Float64 field.
        self.assertIn("b (Box, call 1): type Box is defined differently now", section(out, "not revived"))
        self.assertIn("type Nested.B is defined differently now", section(out, "not revived"))
        self.assertEqual(r["data"], [20, False, False])

    def test_a_value_of_a_replaced_struct_definition_is_not_revived(self):
        self.session('struct P; x::Int; end\nold = P(1); '
                     'module Nested; struct R; x::Int; end; behavior(r::R)=r.x+1; end; '
                     'nested_old=Nested.R(1); nested_type=Nested.R',
                     'struct P; x::Int; y::Int; end\nnew = P(1, 2); '
                     'module Nested; struct R; x::Int; end; behavior(r::R)=r.x+100; end; nested_new=Nested.R(2)')
        out, r = self.revive("(@isdefined(old), new.y, @isdefined(nested_old), @isdefined(nested_type), "
                             "Nested.behavior(nested_new), Palette.provenance(:nested_new))")
        self.assertIn("old (P, an earlier definition, call 1): its type P is an earlier definition", section(out, "not revived"))
        self.assertIn("new (P, call 2)", section(out, "restored exactly"))
        self.assertIn("nested_old (", section(out, "not revived"))
        self.assertIn("nested_type (", section(out, "not revived"))
        self.assertIn("nested_new (", section(out, "restored exactly"))
        self.assertIsInstance(r["data"], list)
        self.assertEqual(r["data"][:5], [False, 2, False, False, 102])
        self.assertIsInstance(r["data"][5]["definition_at_observation"], dict)

    def test_a_method_the_failed_call_never_reached_is_reported_different(self):
        self.session('h(x) = 1\nerror("stop")\nh(x::Int) = 2', "1", may_fail=True)
        out, r = self.revive("length(methods(h))")
        self.assertIn("h (function, call 1): rebuilt, but its methods or fields differ", section(out, "not the same as before"))

    def test_data_referring_to_a_changed_definition_is_not_called_exact(self):
        Path(self.ws, "lib.jl").write_text("helper(x) = 10x\n")
        self.session('include("lib.jl"); ops = [helper, sin]', "y = 1")
        Path(self.ws, "lib.jl").write_text("helper(x) = 20x\n")
        out, r = self.revive("ops[1](1)")
        self.assertIn("ops (Vector{Function}, call 1): restored, but it refers to helper, which is not the same as before",
                      section(out, "not the same as before"))
        self.assertNotIn("ops (", section(out, "restored exactly"))
        self.assertIn("y (Int64, call 2)", section(out, "restored exactly"))

    def test_changed_workspace_package_is_not_called_rebuilt(self):
        pkg = Path(self.ws, "Wq", "src")
        pkg.mkdir(parents=True)
        Path(self.ws, "Wq", "Project.toml").write_text('name = "Wq"\nuuid = "0f5c2a5e-9a44-4c3b-8f2e-6c1d8e7b9a04"\n')
        Path(pkg, "Wq.jl").write_text("module Wq\nf() = 1\nend\n")
        self.session('pushfirst!(LOAD_PATH, joinpath(pwd(), "Wq")); using Wq; v = Wq.f()')
        Path(pkg, "Wq.jl").write_text("module Wq\nf() = 2\nend\n")
        out, r = self.revive("Wq.f()")
        self.assertIn("Wq (workspace package): loaded again from sources that changed since", section(out, "not the same as before"))
        self.assertEqual(r["data"], 2)

    def test_workspace_numeric_type_layout_is_checked_before_restore(self):
        pkg = Path(self.ws, "NumericDesk", "src"); pkg.mkdir(parents=True)
        Path(self.ws, "NumericDesk", "Project.toml").write_text(
            'name = "NumericDesk"\nuuid = "9c12dbf5-d18f-43b2-95bb-c00840bb6f60"\n')
        source = pkg / "NumericDesk.jl"
        source.write_text("module NumericDesk\nstruct N <: Number; value::Int; end\nf(n::N)=n.value+1\nend\n")
        self.session('pushfirst!(LOAD_PATH, joinpath(pwd(), "NumericDesk")); using NumericDesk; '
                     'retained=NumericDesk.N(3); callback=NumericDesk.f; nothing')
        source.write_text(source.read_text().replace("n.value+1", "n.value+2"))
        out, r = self.revive("(callback(retained), retained.value)")
        self.assertEqual(r["data"], [5, 3])
        self.assertIn("retained (", section(out, "not the same as before"))
        self.assertIn("callback (", section(out, "not the same as before"))
        self.assertNotIn("retained (", section(out, "restored exactly"))
        source.write_text("module NumericDesk\nstruct N <: Number; value::Float64; extra::Int; end\nf(n::N)=n.value\nend\n")
        out, r = self.revive("@isdefined(retained)")
        self.assertFalse(r["data"])
        self.assertIn("retained (", section(out, "not revived"))
        self.assertIn("type @workspace:NumericDesk.N is defined differently now", out)

    def test_a_foreign_method_that_cannot_be_rebuilt_is_reported(self):
        self.session('struct T1; x::Int; end\nBase.show(io::IO, t::T1) = print(io, "T1:", t.x)\nv = T1(1)')
        m = self.manifest()
        for e in m["definitions"]:
            if "Base.show" in e["code"]:
                e["code"] = e["code"].replace("t.x", "undefined_helper(t.x)").replace("Base.show(io::IO", "Base.show(io::IO, extra::UndefinedType")
        Path(self.state, "manifest.json").write_text(json.dumps(m))
        out, r = self.revive("1")
        self.assertIn("`Base.show(", section(out, "not revived"))
        self.assertIn("rebuilding it failed", section(out, "not revived"))

    def test_the_report_does_not_claim_runtime_settings_came_back(self):
        self.session("using Random; Random.seed!(1); a = 1")
        out, r = self.revive("a")
        self.assertIn("Other runtime state starts fresh: random number streams", out)

    # --- what cannot come back -------------------------------------------

    def test_runtime_objects_are_lost_with_their_recipe(self):
        self.session('t = @async sleep(1000); ch = Channel{Int}(1); lk = ReentrantLock(); io = open("f.txt", "w"); '
                     'p = run(`sleep 1000`; wait=false); ptr = pointer([1]); '
                     'buf = IOBuffer("abc"); cmd = `ls -l`\n'
                     'let; global letdef(x) = x; end')
        out, r = self.revive("(@isdefined(t), @isdefined(ptr), String(take!(buf)), cmd)")
        lost = section(out, "not revived")
        for name, why in (("t", "a Task cannot be revived"), ("ch", "a Channel"), ("lk", "a ReentrantLock"),
                          ("io", "an open IOStream"), ("p", "a Process"), ("ptr", "it holds a pointer"),
                          ("letdef", "not defined by top-level code")):
            self.assertRegex(lost, rf"(^|; ){name} \([^)]*\): [^;]*{re.escape(why)}", f"{name}: {lost}")
        self.assertIn("it came from `t = ", lost)
        self.assertEqual(r["display"], '(false, false, "abc", `ls -l`)')

    def test_closures_come_back_with_their_captured_state(self):
        self.session('helper(x) = 10x\n'
                     'make_adder(n) = x -> x + n\n'
                     'add3 = make_adder(3); bare = x -> 2x; uses_helper = x -> helper(x) + 1\n'
                     'counter = let c = Ref(0); () -> (c[] += 1) end; counter(); counter()\n'
                     'boxed = let k = 0; () -> (k += 1) end; boxed()\n'
                     'shared = [1, 2]; pushes = y -> push!(shared, y)\n'
                     'holds = Dict(:f => add3, :g => [bare, counter]); in_tuple = (1, add3)\n'
                     'struct Wrap; f::Function; end; wrapped = Wrap(add3)\n'
                     'composed = add3 ∘ bare; fixed = Base.Fix1(+, 5)\n'
                     'task_holder = let t = @async sleep(1000); () -> t end\n'
                     'struct Old; a::Int; end; over_old = let o = Old(1); () -> o.a end\n'
                     'struct Old; a::Int; b::Int; end')
        out, r = self.revive('(add3(1), bare(5), uses_helper(2), counter(), boxed(), (pushes(3); shared), '
                             'holds[:f] === add3, holds[:g][2] === counter, in_tuple[2](0), wrapped.f(1), composed(5), '
                             'fixed(1), @isdefined(task_holder), @isdefined(over_old))')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], [4, 10, 21, 3, 2, [1, 2, 3], True, True, 3, 4, 13, 6, False, False])
        exact = section(out, "restored exactly")
        for name in ("add3", "bare", "uses_helper", "counter", "boxed", "pushes", "holds", "in_tuple", "wrapped", "composed"):
            self.assertIn(f"{name} (", exact)
        lost = section(out, "not revived")
        self.assertRegex(lost, r"task_holder \([^)]*\): [^;]*a Task cannot be revived")
        self.assertRegex(lost, r"over_old \([^)]*\): [^;]*earlier definition")

    def test_a_large_report_puts_what_needs_attention_first_and_stays_short(self):
        # In the 12h run the report reached 23 KB with 170 bindings; the model
        # sees a long result cut in the middle.
        self.session("for i in 1:400; @eval $(Symbol(:binding_with_a_long_name_, i)) = [$i, $i]; end; "
                     "t = @async sleep(1000); f(x) = x")
        out, r = self.revive("1")
        self.assertTrue(r["success"], r)
        lines = [l for l in out.split("\n") if l.startswith("  ")]
        order = [next(i for i, l in enumerate(lines) if l.startswith(h)) for h in ("  not revived", "  rebuilt from source", "  restored exactly")]
        self.assertEqual(order, sorted(order), lines)
        exact = next(l for l in lines if l.startswith("  restored exactly"))
        self.assertLess(len(exact), 1600)
        self.assertIn("restored exactly (400):", exact)
        self.assertIn("more (varinfo() lists every binding)", exact)
        self.assertIn("t (Task", out)
        self.assertLess(len(out), 3000)

    def test_a_request_does_not_wait_for_a_slow_snapshot_until_it_is_five_calls_old(self):
        # In the 12h run a 98 MB state took 25 s to save after every call, and
        # the next call waited for it: the check for a waiting request never
        # saw one, because the stream does not read ahead.
        k = Kernel(self.ws, self.state, timeout=120)
        try:
            # Delay serialization itself; data volume is machine-speed dependent.
            self.assertTrue(k.turn("using Serialization; struct SlowState; value::Int; end; "
                       "function Serialization.serialize(s::Serialization.AbstractSerializer, x::SlowState); "
                       "sleep(0.1); invoke(Serialization.serialize, Tuple{Serialization.AbstractSerializer,Any}, s, x); end; "
                       "for i in 1:40; @eval $(Symbol(:b, i)) = SlowState($i); end")["success"])
            deadline = time.time() + 60   # the first snapshot always completes, and is timed
            while not Path(self.state, "manifest.json").exists() and time.time() < deadline:
                time.sleep(0.5)
            first = self.manifest()
            self.assertGreater(first["seconds"], 2.0, "the state must be slow to save for this test")
            k.turn("x = 1")
            started = time.time()
            self.assertTrue(k.turn("1 + 1")["success"])
            waited = time.time() - started
            for _ in range(6):
                self.assertTrue(k.turn("1 + 1")["success"])
            saved_meanwhile = self.manifest()["call"]
            deadline = time.monotonic() + 60
            while self.manifest()["call"] != 9 and time.monotonic() < deadline:
                time.sleep(0.05)
            m = self.manifest()
        finally:
            k.close()
        self.assertLess(waited, 0.6 * first["seconds"], f"waited {waited:.2f}s behind a {first['seconds']}s snapshot")
        self.assertGreaterEqual(saved_meanwhile, 6, "a snapshot five calls old must complete even with requests waiting")
        self.assertEqual(m["call"], 9, "once idle, the last call's state is saved")

    def test_calls_completed_after_the_saved_state_are_named(self):
        # A slow snapshot can give way and fall up to four calls behind; a
        # revival must not let the model believe those calls' work survived.
        self.session("a = 1", "b = 2")
        self.assertEqual(Path(self.state, "last_call").read_text(), "2")
        Path(self.state, "last_call").write_text("5")   # calls 3-5 completed, none saved
        out, r = self.revive("(a, b)")
        self.assertEqual(r["data"], [1, 2])
        self.assertIn("Calls 3–5 completed after this state was saved", out)
        self.session("c = 3")
        out, r = self.revive("c")
        self.assertNotIn("completed after this state was saved", out)

    def test_a_quick_snapshot_never_gives_way(self):
        # Revival's promise for an ordinary session: the state of the last
        # completed call, even when the next call arrives at once and kills
        # the kernel.
        k = Kernel(self.ws, self.state, timeout=4)
        k.turn("v = [1, 2, 3]")
        k.turn("v[1] = 7; w = 2")
        k.turn("while true; end")
        k.close()
        out, r = self.revive("(v, w)")
        self.assertEqual(r["data"], [[7, 2, 3], 2])
        self.assertIn("end of call 2", out)

    def test_background_processes_that_stopped_with_the_kernel_are_named(self):
        # A server started in the background died with the kernel, and the
        # model went on as if it were still up.
        self.session("srv = run(`sleep 300`; wait=false); x = 1")
        out, r = self.revive("x")
        self.assertEqual(r["data"], 1)
        self.assertIn("Background processes stopped with the previous kernel: `sleep 300`", out)

    def test_an_included_module_comes_back_and_a_failed_using_is_not_a_loss(self):
        # The 12h run: `using JSON5Lite` failed (not a registered package),
        # then `include`d the package's module file. Revival replayed the
        # failed `using`, reported it lost, and never rebuilt the module.
        pkg = Path(self.ws, "json5", "src"); pkg.mkdir(parents=True)
        (pkg / "JSON5Lite.jl").write_text("module JSON5Lite\nparse(s) = length(s)\nend\n")
        # One kernel: a failed call leaves no snapshot, the next success does.
        results = self.session('using JSON5Lite', 'before_error(x) = x + 1; using NoSuchPackage',
                               'struct T2; x::Int; end',
                               'Base.show(io::IO, t::T2) = print(io, "T2!"); error("after the method")',
                               'include("json5/src/JSON5Lite.jl"); n = JSON5Lite.parse("abc")', may_fail=True)
        self.assertEqual([r["success"] for r in results], [False, False, True, False, True])
        out, r = self.revive('(JSON5Lite.parse("ab"), before_error(1), n, repr(T2(1)))')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], [2, 2, 3, "T2!"])
        self.assertIn("JSON5Lite (module", section(out, "rebuilt from source"))
        self.assertIn("before_error (", section(out, "rebuilt from source"))
        self.assertNotIn("using JSON5Lite", out)
        self.assertNotIn("NoSuchPackage", out)

    def test_regexes_and_matches_are_restored(self):
        # In the 12h run a log-parsing regex was lost at every kernel death.
        self.session('LINE_RE = r"^(?<user>\\w+) (\\d+)$"im; m = match(LINE_RE, "Ann 42"); '
                     'rules = Dict(:ip => r"\\d+\\.\\d+", :word => r"\\w+"); bad = match(r"x", "y")')
        out, r = self.revive('(match(LINE_RE, "BOB 7")[:user], m[:user], m[2], match(rules[:ip], "at 10.2").match, '
                             'LINE_RE == r"^(?<user>\\w+) (\\d+)$"im, bad === nothing)')
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], ["BOB", "Ann", "42", "10.2", True, True])
        exact = section(out, "restored exactly")
        for name in ("LINE_RE", "m", "rules"):
            self.assertIn(f"{name} (", exact)

    def test_a_revived_closure_runs_the_code_it_was_made_with(self):
        # make_adder is redefined after add3 was made. The rebuilt make_adder
        # makes closures of the same generated name; add3 must not become one.
        self.session('make_adder(n) = x -> x + n; add3 = make_adder(3)',
                     'make_adder(n) = x -> x * n; times3 = make_adder(3)')
        out, r = self.revive("(add3(10), times3(10), make_adder(2)(10))")
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], [13, 30, 20])
        self.assertIn("add3 (", section(out, "restored exactly"))
        # A lambda that is data is not rebuilt; a const one is, first, and in a
        # new kernel it takes the generated name the data lambda had.
        self.session('bare = x -> 2x', 'const K = x -> x + 100')
        out, r = self.revive("(bare(5), K(5))")
        self.assertTrue(r["success"], r)
        self.assertEqual(r["data"], [10, 105])

    def test_package_object_holding_a_c_pointer_is_not_revived(self):
        self.session('using EzXML; doc = parsexml("<a><b/></a>"); names_ = [nodename(n) for n in eachelement(root(doc))]')
        out, r = self.revive("names_")
        self.assertIn("doc (", section(out, "not revived"))
        self.assertIn("pointer", section(out, "not revived"))
        self.assertEqual(r["data"], ["b"])

    # --- kernel death, restart, environment changes, damage, limits ----

    def test_kernel_death_mid_call_revives_the_last_completed_call(self):
        k = Kernel(self.ws, self.state, timeout=4)
        k.turn("v = [1, 2, 3]; count_ = 1")
        r = k.turn("v[1] = 100; count_ = 2; s = 0; while true; s += 1; end")
        self.assertTrue(r.get("session_dead") or r.get("kernel_exit") or not r["success"], r)
        k.close()
        out, r = self.revive("(v, count_, @isdefined(s))")
        self.assertIn("end of call 1", out)
        self.assertEqual(r["data"], [[1, 2, 3], 1, False])

    def test_normal_restart_continues_call_numbers_and_revives_again(self):
        self.session("a = 1", "b = 2")
        out, r = self.revive("c = 3")
        self.assertIn("end of call 2", out)
        self.assertEqual(r.get("call"), 3)
        out, r = self.revive("(a, b, c)")
        self.assertIn("end of call 3", out)
        self.assertIn("c (Int64, call 3)", section(out, "restored exactly"))
        self.assertEqual(r["data"], [1, 2, 3])

    def test_a_different_julia_or_package_environment_restores_no_data(self):
        self.session("f(x) = x + 1; v = [1, 2]")
        m = self.manifest()
        m["environment"]["julia"] = "1.11.0"
        Path(self.state, "manifest.json").write_text(json.dumps(m))
        out, r = self.revive("(f(1), @isdefined(v))")
        self.assertIn("its data could not be restored because it was saved by Julia 1.11.0", out)
        self.assertIn("f (function, call 1)", section(out, "rebuilt from source"))
        self.assertIn("v (Vector{Int64}, call 1): it was saved by Julia 1.11.0", section(out, "not revived"))
        self.assertEqual(r["data"], [2, False])
        m["environment"]["julia"] = self.manifest()["environment"]["julia"]
        m["environment"]["manifest"] = "0" * 64
        self.session("v = [1, 2]")
        m2 = self.manifest()
        m2["environment"]["manifest"] = "0" * 64
        Path(self.state, "manifest.json").write_text(json.dumps(m2))
        out, r = self.revive("@isdefined(v)")
        self.assertIn("the package environment changed since it was saved", out)
        self.assertFalse(r["data"])

    def test_damaged_or_missing_data_loses_only_its_own_group(self):
        self.session("a = [1, 2]; b = a; c = 3")
        m = self.manifest()
        group = next(g for g in m["data"] if "a" in g["names"])
        path = Path(self.state, group["file"])
        path.write_bytes(path.read_bytes()[:-3] + b"xyz")
        out, r = self.revive("(@isdefined(a), @isdefined(b), c)")
        self.assertIn("a (Vector{Int64}, call 1): its data file is damaged", section(out, "not revived"))
        self.assertIn("b (Vector{Int64}, call 1): its data file is damaged", section(out, "not revived"))
        self.assertEqual(r["data"], [False, False, 3])
        self.session("a = [1, 2]; c = 3")
        for g in self.manifest()["data"]:
            Path(self.state, g["file"]).unlink()
        out, r = self.revive("@isdefined(c)")
        self.assertIn("its data file is missing", out)
        self.assertFalse(r["data"])

    def test_unreadable_manifest_revives_nothing_and_says_so(self):
        self.session("a = 1")
        Path(self.state, "manifest.json").write_text('{"format": 1, "call": 1, "bin')
        out, r = self.revive("@isdefined(a)")
        self.assertIn("its snapshot could not be read", out)
        self.assertIn("Nothing was revived", out)
        self.assertFalse(r["data"])

    def test_leftover_temporary_files_are_ignored(self):
        self.session("a = 1")
        Path(self.state, "manifest.json.tmp").write_text("garbage")
        Path(self.state, "data-9-9-1.jls.tmp").write_bytes(b"garbage")
        out, r = self.revive("a")
        self.assertEqual(r["data"], 1)

    def test_state_of_another_workspace_is_not_revived(self):
        self.session("secret = 1")
        other = tempfile.mkdtemp(prefix="palette-revival-other-")
        try:
            k = Kernel(other, self.state)
            r = k.turn("@isdefined(secret)")
            k.close()
            self.assertFalse(k.hello["revival"])
            self.assertEqual(r["output"], "")
            self.assertFalse(r["data"])
        finally:
            shutil.rmtree(other, ignore_errors=True)

    def test_size_limits_skip_and_report(self):
        env = {"PALETTE_STATE_MAX_BINDING_BYTES": str(1_000_000), "PALETTE_STATE_MAX_BYTES": str(1_500_000)}
        self.session("huge = zeros(200_000); a = zeros(100_000); b = zeros(100_000); small = 1", env=env)
        out, r = self.revive("(@isdefined(huge), @isdefined(small))", env=env)
        lost = section(out, "not revived")
        self.assertIn("huge (Vector{Float64}, call 1): not saved: 1.526 MiB is over the 976.562 KiB limit per binding", lost)
        self.assertRegex(lost, r"(a|b) \(Vector\{Float64\}, call 1\): not saved: the snapshot reached its 1.431 MiB limit")
        self.assertEqual(r["data"], [False, True])


if __name__ == "__main__":
    unittest.main(verbosity=2)
