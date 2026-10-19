#!/usr/bin/env python3
"""
Tests for execution contexts: one call runs in exactly one of normal, traced,
reference, memoized or budgeted, chosen by the request's `context` or by the
session default (Palette.context!). The four-context demo is the promotion
gate: the same code gives the same data in normal, reference, traced and
memoized, and the X-Ray records differ.

Run:
    PALETTE_HOST_BIN=runtime/host/target/release/palette-host \\
    PALETTE_TEST_PROJECT_DIR=~/.palette-trial/project python3 runtime/security/test_contexts.py
"""
from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_revival import Kernel, KernelTestCase  # noqa: E402

DEFINE = """
f(xs) = sum(xs)
g(xs) = 2f(xs)
const REF_CALLS = Ref(0)
ref_f(xs::Vector{Int}) = (REF_CALLS[] += 1; foldl(+, xs))
Palette.reference!(f, Tuple{Vector{Int}}, ref_f)
nothing
"""

# Top-level forms the traced/memoized rewrite must leave meaning the same. Only
# an outermost call (or an assignment's call right-hand side) is wrapped, so
# traced dispatch lists sq four times and + once, in statement order.
FORMS = """
a = sq(3)
k = sq(2; p=3)
pair = (5,)
s = sq(pair...)
begin
    t = sq(4)
end
b = t + 1
ys = sq.([1, 2])
zs = map([1, 2]) do x; x + 1 end
n = ccall(:labs, Clong, (Clong,), -3)
(a, k, s, b, ys, zs, n)
"""


class ContextsTest(KernelTestCase):
    def kernel(self):
        return Kernel(self.ws, self.state)

    def ok(self, r):
        self.assertTrue(r["success"], r)
        return r

    def xray(self, k, r):
        return self.ok(k.turn(f'Palette.xray({r["call"]})'))["data"]

    def test_four_contexts_same_data_different_records(self):
        k = self.kernel()
        try:
            self.ok(k.turn(DEFINE))
            runs = {name: self.ok(k.turn("g([1, 2, 3])", context=name))
                    for name in ("normal", "reference", "traced", "memoized")}
            again = self.ok(k.turn("g([1, 2, 3])", context="memoized"))
            ref_calls = self.ok(k.turn("REF_CALLS[]"))["data"]
            methods_after = self.ok(k.turn("length(methods(f))"))["data"]
            plain_after = self.ok(k.turn("g([1, 2, 3])"))["data"]
            ref_calls_after = self.ok(k.turn("REF_CALLS[]"))["data"]
            xr = {name: self.xray(k, r) for name, r in runs.items()}
            xr_again = self.xray(k, again)
        finally:
            k.close()
        for name, r in runs.items():
            self.assertEqual(r["data"], 12, name)   # the context does not change the result
        self.assertEqual(again["data"], 12)
        self.assertIsNone(runs["normal"].get("context"))
        for name in ("reference", "traced", "memoized"):
            self.assertEqual(runs[name]["context"], name)
        for name, x in xr.items():
            self.assertEqual(x["context"], name)
        # reference: ref_f replaced f for the call, inside g too, and is gone afterwards
        self.assertEqual(ref_calls, 1)
        self.assertEqual([(q["function"], q["signature"]) for q in xr["reference"]["references"]],
                         [("f", "Tuple{Vector{Int64}}")])
        self.assertFalse(xr["reference"]["references"][0]["replaced_method"])
        self.assertEqual(methods_after, 1)
        self.assertEqual((plain_after, ref_calls_after), (12, 1))
        # traced: the outermost call's dispatched method
        (d,) = xr["traced"]["dispatch"]
        self.assertEqual((d["function"], d["signature"]), ("g", "Tuple{Vector{Int64}}"))
        self.assertIn("g(xs)", d["method"])
        self.assertIsInstance(d["line"], int)
        # memoized: miss, then hit
        self.assertEqual((xr["memoized"]["memo"]["hits"], xr["memoized"]["memo"]["misses"]), (0, 1))
        self.assertEqual((xr_again["memo"]["hits"], xr_again["memo"]["misses"]), (1, 0))
        # each record carries only its own context's field
        self.assertIsNone(xr["normal"]["dispatch"])
        self.assertIsNone(xr["normal"]["memo"])
        self.assertIsNone(xr["normal"]["references"])
        self.assertIsNone(xr["traced"]["memo"])
        self.assertIsNone(xr["memoized"]["dispatch"])

    def test_redefinition_between_memoized_calls_is_a_miss(self):
        k = self.kernel()
        try:
            self.ok(k.turn("f(xs) = sum(xs); g(xs) = 2f(xs); nothing"))
            first = self.ok(k.turn("g([1, 2, 3])", context="memoized"))
            self.ok(k.turn("f(xs) = sum(xs) + 1; nothing"))
            second = self.ok(k.turn("g([1, 2, 3])", context="memoized"))
            x2 = self.xray(k, second)
        finally:
            k.close()
        self.assertEqual((first["data"], second["data"]), (12, 14))
        self.assertEqual((x2["memo"]["hits"], x2["memo"]["misses"]), (0, 1))

    def test_budgets_interrupt_and_name_the_budget(self):
        k = self.kernel()
        try:
            by_bytes = k.turn("for i in 1:10^6; zeros(10^4); yield(); end", context="budgeted",
                              budget={"bytes": 10**8})
            by_time = k.turn("sleep(30)", context="budgeted", budget={"seconds": 1})
            within = self.ok(k.turn("1 + 1", context="budgeted", budget={"seconds": 5}))
            alive = self.ok(k.turn("2 + 2"))
            xb, xt = self.xray(k, by_bytes), self.xray(k, by_time)
        finally:
            k.close()
        self.assertFalse(by_bytes["success"], by_bytes)
        self.assertIn("budget of 100000000 bytes", by_bytes["error"])
        self.assertEqual(xb["budget_exceeded"], "bytes")
        self.assertFalse(by_time["success"], by_time)
        self.assertIn("budget of 1s", by_time["error"])
        self.assertEqual(xt["budget_exceeded"], "seconds")
        self.assertEqual((within["data"], alive["data"]), (2, 4))

    def test_session_default_is_reported_until_cleared(self):
        k = self.kernel()
        try:
            set_ = self.ok(k.turn("Palette.context!(:traced)"))
            during = self.ok(k.turn("1 + 1"))
            override = self.ok(k.turn("1 + 1", context="normal"))
            clear = self.ok(k.turn("Palette.context!(nothing)"))
            after = self.ok(k.turn("1 + 1"))
            x_during, x_override = self.xray(k, during), self.xray(k, override)
        finally:
            k.close()
        self.assertEqual(set_["context"],
                         "normal; session default from the next call: traced; Palette.context!(nothing) clears it")
        self.assertEqual(during["context"], "traced (session default)")
        self.assertEqual(override["context"],
                         "normal; session default from the next call: traced; Palette.context!(nothing) clears it")
        self.assertEqual(clear["context"], "traced (session default); the session default is cleared from the next call")
        self.assertIsNone(after.get("context"))
        self.assertEqual((x_during["context"], x_override["context"]), ("traced", "normal"))

    def test_top_level_forms_keep_their_meaning(self):
        k = self.kernel()
        try:
            self.ok(k.turn("sq(x; p=2) = x^p; nothing"))
            runs = {c: self.ok(k.turn(FORMS, context=c)) for c in ("normal", "traced", "memoized")}
            xrays = {c: self.xray(k, r) for c, r in runs.items()}
        finally:
            k.close()
        self.assertEqual(runs["normal"]["data"], [9, 8, 25, 17, [1, 4], [2, 3], 3])
        for c in ("traced", "memoized"):
            self.assertEqual(runs[c]["data"], runs["normal"]["data"], c)
        self.assertEqual([d["function"] for d in xrays["traced"]["dispatch"]], ["sq", "sq", "sq", "sq", "+"])

    def test_exactly_one_context_per_call(self):
        k = self.kernel()
        try:
            self.ok(k.turn("Palette.context!(:memoized)"))
            override = self.ok(k.turn("1 + 1", context="traced"))
            two = k.turn("1", context=["traced", "memoized"])
            x = self.xray(k, override)
        finally:
            k.close()
        self.assertEqual(override["costs"]["context"], "traced")
        self.assertEqual((x["context"], x["memo"], x["references"]), ("traced", None, None))   # the default did not also apply
        self.assertIsInstance(x["dispatch"], list)
        self.assertFalse(two["success"])
        self.assertIn("must be a string", two["error"])

    def test_session_default_does_not_survive_a_restart(self):
        k = self.kernel()
        try:
            self.ok(k.turn("Palette.context!(:traced)"))
            self.assertEqual(self.ok(k.turn("1"))["context"], "traced (session default)")
        finally:
            k.close()
        k = self.kernel()
        try:
            r = self.ok(k.turn("1"))
        finally:
            k.close()
        self.assertIsNone(r.get("context"))
        self.assertEqual(r["costs"]["context"], "normal")

    def test_bad_requests_are_errors(self):
        k = self.kernel()
        try:
            unknown = k.turn("1", context="bogus")
            stray_budget = k.turn("1", budget={"seconds": 1})
            wrong_ctx = k.turn("1", context="traced", budget={"seconds": 1})
            empty_budget = k.turn("1", context="budgeted", budget={})
            bad_values = [k.turn("1", context="budgeted", budget=b)
                          for b in ({"seconds": 0}, {"seconds": -1}, {"bytes": 0}, {"bytes": 1.5}, {"minutes": 1})]
            core = k.turn("Palette.reference!(sum, Tuple{Vector{Int}}, xs -> 0)")
            alive = self.ok(k.turn("3"))
        finally:
            k.close()
        self.assertFalse(unknown["success"])
        self.assertIn("normal, traced, reference, memoized, budgeted", unknown["error"])
        self.assertFalse(stray_budget["success"])
        self.assertIn("budgeted", stray_budget["error"])
        self.assertFalse(wrong_ctx["success"])
        self.assertIn("applies only to the budgeted context", wrong_ctx["error"])
        self.assertFalse(empty_budget["success"])
        self.assertIn("seconds and/or bytes", empty_budget["error"])
        for r, needle in zip(bad_values, ("seconds must be a positive number", "seconds must be a positive number",
                                          "bytes must be a positive integer", "bytes must be a positive integer",
                                          "unknown budget field(s) minutes")):
            self.assertFalse(r["success"])
            self.assertIn(needle, r["error"])
        self.assertFalse(core["success"])
        self.assertIn("cannot be replaced", core["error"])
        self.assertEqual(alive["data"], 3)


if __name__ == "__main__":
    unittest.main()
