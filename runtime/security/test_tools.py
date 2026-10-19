#!/usr/bin/env python3
"""
Tests for the tool lifecycle and the code graph: a helper used in 3
successful calls is promoted to a project tool, a fresh session lists it from
the index and loads it on its first call, X-Ray cites its evidence, a human
approval through the patch broker makes it trusted, and a hash mismatch drops
it back to a project tool. The kernel cannot write the trusted directory.

Run:
    PALETTE_HOST_BIN=runtime/host/target/release/palette-host \\
    PALETTE_TEST_PROJECT_DIR=~/.palette-trial/project python3 runtime/security/test_tools.py
"""
from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from patch_broker import PatchBroker, TRUSTED_TOOLS_TARGET, patch_roots_from_env  # noqa: E402
from test_revival import Kernel, KernelTestCase  # noqa: E402

DEFINE = '''
"Twice x, plus an offset."
function double_it(x; offset=0)
    return 2x + offset
end
'''


class ToolsTest(KernelTestCase):
    def setUp(self):
        super().setUp()
        self.trusted = Path(tempfile.mkdtemp(prefix="palette-trusted-")) / "tools"
        self.addCleanup(shutil.rmtree, self.trusted.parent, True)
        self.env = {"PALETTE_TRUSTED_TOOLS": str(self.trusted)}

    def kernel(self, state=None):
        return Kernel(self.ws, state or self.state, env=self.env)

    def fresh_state(self):
        state = tempfile.mkdtemp(prefix="palette-tools-state-")
        self.addCleanup(shutil.rmtree, state, True)
        return state

    def ok(self, r):
        self.assertTrue(r["success"], r)
        return r

    def fails(self, r, text):
        self.assertFalse(r["success"], r)
        self.assertIn(text, json.dumps(r))
        return r

    def promote_double_it(self):
        k = self.kernel()
        try:
            self.ok(k.turn(DEFINE))
            for i in range(2):
                self.ok(k.turn(f"double_it({i})"))
            self.fails(k.turn("Palette.promote(:double_it)"), "has 2 successful call(s)")
            self.ok(k.turn("double_it(5; offset=1)"))
            return self.ok(k.turn("Palette.promote(:double_it)"))["data"]
        finally:
            k.close()

    def tools_dir(self):
        return Path(self.ws, ".palette", "tools")

    def test_promote_list_load_on_first_call_and_cite_evidence(self):
        promoted = self.promote_double_it()
        self.assertEqual((promoted["stage"], promoted["evidence_count"]), ("project", 3))
        meta = json.loads((self.tools_dir() / "double_it.json").read_text())
        self.assertEqual(meta["doc"], "Twice x, plus an offset.")
        self.assertEqual(len(meta["evidence"]), 3)
        self.assertIn("double_it(x; offset = 0)", meta["signatures"])

        state = self.fresh_state()
        k = self.kernel(state)
        try:
            listed = self.ok(k.turn("Palette.tools()"))["data"]
            hidden = self.ok(k.turn('any(r -> r[1] == "double_it", varinfo().rows)'))
            first = self.ok(k.turn("double_it(21)"))
            second = self.ok(k.turn("double_it(1; offset=2)"))
            x1 = self.ok(k.turn(f'Palette.xray({first["call"]})'))["data"]
            x2 = self.ok(k.turn(f'Palette.xray({second["call"]})'))["data"]
        finally:
            k.close()
        entry = listed["double_it"]
        self.assertEqual((entry["stage"], entry["verified"], entry["evidence_count"]), ("project", True, 3))
        self.assertFalse(hidden["data"], "a tool stub showed up as a session binding")
        self.assertEqual((first["data"], second["data"]), (42, 4))
        (t1,) = x1["tools"]
        self.assertEqual((t1["name"], t1["stage"], t1["loaded"], t1["evidence_count"]), ("double_it", "project", True, 3))
        self.assertEqual(t1["source_sha256"], meta["source_sha256"])
        self.assertFalse(x2["tools"][0]["loaded"])

        # The stub is not a binding: a revived session neither restores nor loses it.
        k = self.kernel(state)
        try:
            lost = self.ok(k.turn('Palette.revival()["lost"]'))["data"]
            again = self.ok(k.turn("double_it(2)"))["data"]
        finally:
            k.close()
        self.assertFalse(lost)
        self.assertEqual(again, 4)

    def test_a_tool_must_stand_alone(self):
        k = self.kernel()
        try:
            self.ok(k.turn("scale = 3; helper(x) = scale * x; inner(x) = x + 1; outer(x) = inner(x) * 2; nothing"))
            for _ in range(3):
                self.ok(k.turn("helper(1) + outer(1) + inner(1)"))
            self.fails(k.turn("Palette.promote(:helper)"), "uses session bindings scale")
            self.fails(k.turn("Palette.promote(:outer)"), "uses session bindings inner")
            self.ok(k.turn("Palette.promote(:inner)"))
            self.fails(k.turn("Palette.promote(:nope)"), "only those can be promoted")
        finally:
            k.close()
        self.assertEqual(sorted(p.name for p in self.tools_dir().iterdir()), ["inner.jl", "inner.json"])

    def test_a_redefinition_resets_the_uses(self):
        k = self.kernel()
        try:
            self.ok(k.turn("h(x) = x + 1"))
            for _ in range(3):
                self.ok(k.turn("h(1)"))
            self.ok(k.turn("h(x) = x + 2"))
            self.ok(k.turn("h(1)"))
            self.fails(k.turn("Palette.promote(:h)"), "has 1 successful call(s)")
        finally:
            k.close()

    def test_a_sidecar_cannot_promote_itself_and_the_kernel_cannot_write_trusted(self):
        self.promote_double_it()
        sidecar = self.tools_dir() / "double_it.json"
        meta = json.loads(sidecar.read_text())
        meta["stage"] = "trusted"
        sidecar.write_text(json.dumps(meta))
        k = self.kernel(self.fresh_state())
        try:
            entry = self.ok(k.turn("Palette.tools()"))["data"]["double_it"]
            self.fails(k.turn('write(joinpath(ENV["PALETTE_TRUSTED_TOOLS"], "x.jl"), "x() = 1")'), "Read-only file system")
        finally:
            k.close()
        self.assertEqual(entry["stage"], "project")
        self.assertIn("outside the trusted directory", entry["reason"])
        self.assertEqual(list(self.trusted.iterdir()), [])

    def test_a_human_approval_makes_it_trusted_and_drift_demotes_it(self):
        self.promote_double_it()
        k = self.kernel()
        try:
            request = self.ok(k.turn("Palette.trust(:double_it)"))["data"]
        finally:
            k.close()
        self.assertEqual(request["tool"], "palette_patch")
        store = Path(tempfile.mkdtemp(prefix="palette-tools-grants-"))
        self.addCleanup(shutil.rmtree, store, True)
        broker = PatchBroker(store, patch_roots_from_env({"PALETTE_PATCH_ROOTS": "{}", **self.env}))
        args = request["arguments"]
        self.assertEqual(args["target"], TRUSTED_TOOLS_TARGET)
        prepared = broker.prepare({"target": args["target"], "changes": args["changes"]}, "world-a")
        with self.assertRaises(PermissionError):
            broker.apply(prepared["request_id"], "world-a")
        approved = subprocess.run([sys.executable, str(Path(__file__).with_name("patch_broker.py")), "--storage", str(store),
                                   "approve", prepared["request_id"], "--digest", prepared["patch_sha256"]],
                                  capture_output=True, text=True)
        self.assertEqual(approved.returncode, 0, approved.stderr)
        self.assertEqual(broker.apply(prepared["request_id"], "world-a")["status"], "applied")

        k = self.kernel(self.fresh_state())
        try:
            trusted = self.ok(k.turn("Palette.tools()"))["data"]["double_it"]
            used = self.ok(k.turn("double_it(4)"))
            stage = self.ok(k.turn(f'Palette.xray({used["call"]})'))["data"]["tools"][0]["stage"]
        finally:
            k.close()
        self.assertEqual((trusted["stage"], trusted["verified"]), ("trusted", True))
        self.assertEqual((used["data"], stage), (8, "trusted"))

        (self.trusted / "double_it.jl").write_text("double_it(x; offset=0) = 3x\n")
        k = self.kernel(self.fresh_state())
        try:
            demoted = self.ok(k.turn("Palette.tools()"))["data"]["double_it"]
            result = self.ok(k.turn("double_it(4)"))["data"]
        finally:
            k.close()
        self.assertEqual((demoted["stage"], demoted["verified"]), ("project", True))
        self.assertEqual(result, 8, "the changed trusted copy ran")

    def test_a_changed_project_tool_is_not_loaded(self):
        self.promote_double_it()
        k = self.kernel(self.fresh_state())
        try:
            self.ok(k.turn("double_it(1)"))
            (self.tools_dir() / "double_it.jl").write_text("double_it(x; offset=0) = 3x\n")
            self.fails(k.turn("double_it(1)"), "changed since it was promoted")
            entry = self.ok(k.turn("Palette.tools()"))["data"]["double_it"]
        finally:
            k.close()
        self.assertFalse(entry["verified"])

    def test_codegraph_refreshes_one_file_and_is_cited(self):
        Path(self.ws, "lib.jl").write_text("g(x) = x\nf(x) = g(x)\n")
        Path(self.ws, "other.py").write_text("def p():\n    pass\n")
        k = self.kernel()
        try:
            first = self.ok(k.turn('[c[2].caller for c in Palette.callers("g")]'))
            unchanged = self.ok(k.turn('length(Palette.definitions("p"))'))
            Path(self.ws, "lib.jl").write_text("g(x) = x\nf(x) = g(x)\nk(x) = g(x)\n")
            changed = self.ok(k.turn('sort([c[2].caller for c in Palette.callers("g")])'))
            records = [self.ok(k.turn(f'Palette.xray({r["call"]})'))["data"]["codegraph"] for r in (first, unchanged, changed)]
        finally:
            k.close()
        self.assertEqual((first["data"], unchanged["data"], changed["data"]), (["f"], 1, ["f", "k"]))
        (c1,), (c2,), (c3,) = records
        self.assertEqual(c1["files"], 2)
        self.assertEqual(sorted(c1["reparsed_paths"]), ["lib.jl", "other.py"])
        self.assertEqual(c2["reparsed_paths"], [])
        self.assertEqual(c3["reparsed_paths"], ["lib.jl"])
        self.assertEqual(c1["digest"], c2["digest"])
        self.assertNotEqual(c2["digest"], c3["digest"])


if __name__ == "__main__":
    unittest.main()
