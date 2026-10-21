//! Execution contexts: one call runs in exactly one of normal, traced,
//! reference, memoized or budgeted, chosen by the request's `context` or by the
//! session default (Palette.context!). The four-context demo is the promotion
//! gate: the same code gives the same data in normal, reference, traced and
//! memoized, and the X-Ray records differ.

use palette_conformance::{ok, Kernel, KernelCase, KernelOptions};
use serde_json::{json, Value};

const DEFINE: &str = "
f(xs) = sum(xs)
g(xs) = 2f(xs)
const REF_CALLS = Ref(0)
ref_f(xs::Vector{Int}) = (REF_CALLS[] += 1; foldl(+, xs))
Palette.reference!(f, Tuple{Vector{Int}}, ref_f)
nothing
";

/// Top-level forms the traced/memoized rewrite must leave meaning the same. Only
/// an outermost call (or an assignment's call right-hand side) is wrapped, so
/// traced dispatch lists sq four times and + once, in statement order.
const FORMS: &str = "
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
";

fn kernel(case: &KernelCase) -> Kernel {
    case.kernel(KernelOptions::default())
}

fn in_context(k: &mut Kernel, code: &str, context: &str) -> Value {
    k.turn_with(code, json!({"context": context}))
}

fn budgeted(k: &mut Kernel, code: &str, budget: Value) -> Value {
    k.turn_with(code, json!({"context": "budgeted", "budget": budget}))
}

fn xray(k: &mut Kernel, r: &Value) -> Value {
    ok(k.turn(&format!("Palette.xray({})", r["call"])))["data"].clone()
}

/// The record has `key`, and it is null.
fn null(record: &Value, key: &str) -> bool {
    record.get(key) == Some(&Value::Null)
}

fn error(r: &Value) -> &str {
    assert_eq!(r["success"], false, "{r}");
    r["error"].as_str().unwrap()
}

#[test]
fn four_contexts_same_data_different_records() {
    let case = KernelCase::new();
    let mut k = kernel(&case);
    ok(k.turn(DEFINE));
    let names = ["normal", "reference", "traced", "memoized"];
    let runs: Vec<Value> = names.iter().map(|c| ok(in_context(&mut k, "g([1, 2, 3])", c))).collect();
    let again = ok(in_context(&mut k, "g([1, 2, 3])", "memoized"));
    let ref_calls = ok(k.turn("REF_CALLS[]"))["data"].clone();
    let methods_after = ok(k.turn("length(methods(f))"))["data"].clone();
    let plain_after = ok(k.turn("g([1, 2, 3])"))["data"].clone();
    let ref_calls_after = ok(k.turn("REF_CALLS[]"))["data"].clone();
    let xr: Vec<Value> = runs.iter().map(|r| xray(&mut k, r)).collect();
    let xr_again = xray(&mut k, &again);
    k.close();
    let [normal, reference, traced, memoized] = [&xr[0], &xr[1], &xr[2], &xr[3]];
    for (name, r) in names.iter().zip(&runs) {
        assert_eq!(r["data"], 12, "{name}: the context does not change the result");
    }
    assert_eq!(again["data"], 12);
    assert!(runs[0]["context"].is_null(), "{}", runs[0]);
    for (name, r) in names.iter().zip(&runs).skip(1) {
        assert_eq!(r["context"], *name);
    }
    for (name, x) in names.iter().zip(&xr) {
        assert_eq!(x["context"], *name);
    }
    // reference: ref_f replaced f for the call, inside g too, and is gone afterwards
    assert_eq!(ref_calls, 1);
    let replaced: Vec<(&Value, &Value)> =
        reference["references"].as_array().unwrap().iter().map(|q| (&q["function"], &q["signature"])).collect();
    assert_eq!(replaced, [(&json!("f"), &json!("Tuple{Vector{Int64}}"))]);
    assert_eq!(reference["references"][0]["replaced_method"], false);
    assert_eq!(methods_after, 1);
    assert_eq!((plain_after, ref_calls_after), (json!(12), json!(1)));
    // traced: the outermost call's dispatched method
    let dispatch = traced["dispatch"].as_array().unwrap();
    assert_eq!(dispatch.len(), 1, "{traced}");
    let d = &dispatch[0];
    assert_eq!((&d["function"], &d["signature"]), (&json!("g"), &json!("Tuple{Vector{Int64}}")));
    assert!(d["method"].as_str().unwrap().contains("g(xs)"), "{d}");
    assert!(d["line"].is_i64(), "{d}");
    // memoized: miss, then hit
    assert_eq!((&memoized["memo"]["hits"], &memoized["memo"]["misses"]), (&json!(0), &json!(1)));
    assert_eq!((&xr_again["memo"]["hits"], &xr_again["memo"]["misses"]), (&json!(1), &json!(0)));
    // each record carries only its own context's field
    assert!(null(normal, "dispatch") && null(normal, "memo") && null(normal, "references"), "{normal}");
    assert!(null(traced, "memo"), "{traced}");
    assert!(null(memoized, "dispatch"), "{memoized}");
}

#[test]
fn redefinition_between_memoized_calls_is_a_miss() {
    let case = KernelCase::new();
    let mut k = kernel(&case);
    ok(k.turn("f(xs) = sum(xs); g(xs) = 2f(xs); nothing"));
    let first = ok(in_context(&mut k, "g([1, 2, 3])", "memoized"));
    ok(k.turn("f(xs) = sum(xs) + 1; nothing"));
    let second = ok(in_context(&mut k, "g([1, 2, 3])", "memoized"));
    let x2 = xray(&mut k, &second);
    k.close();
    assert_eq!((&first["data"], &second["data"]), (&json!(12), &json!(14)));
    assert_eq!((&x2["memo"]["hits"], &x2["memo"]["misses"]), (&json!(0), &json!(1)));
}

#[test]
fn budgets_interrupt_and_name_the_budget() {
    let case = KernelCase::new();
    let mut k = kernel(&case);
    let by_bytes = budgeted(&mut k, "for i in 1:10^6; zeros(10^4); yield(); end", json!({"bytes": 100_000_000}));
    let by_time = budgeted(&mut k, "sleep(30)", json!({"seconds": 1}));
    let within = ok(budgeted(&mut k, "1 + 1", json!({"seconds": 5})));
    let alive = ok(k.turn("2 + 2"));
    let (xb, xt) = (xray(&mut k, &by_bytes), xray(&mut k, &by_time));
    k.close();
    assert!(error(&by_bytes).contains("budget of 100000000 bytes"), "{by_bytes}");
    assert_eq!(xb["budget_exceeded"], "bytes");
    assert!(error(&by_time).contains("budget of 1s"), "{by_time}");
    assert_eq!(xt["budget_exceeded"], "seconds");
    assert_eq!((&within["data"], &alive["data"]), (&json!(2), &json!(4)));
}

#[test]
fn session_default_is_reported_until_cleared() {
    let case = KernelCase::new();
    let mut k = kernel(&case);
    let set = ok(k.turn("Palette.context!(:traced)"));
    let during = ok(k.turn("1 + 1"));
    let overridden = ok(in_context(&mut k, "1 + 1", "normal"));
    let clear = ok(k.turn("Palette.context!(nothing)"));
    let after = ok(k.turn("1 + 1"));
    let (x_during, x_override) = (xray(&mut k, &during), xray(&mut k, &overridden));
    k.close();
    let pending = "normal; session default from the next call: traced; Palette.context!(nothing) clears it";
    assert_eq!(set["context"], pending);
    assert_eq!(during["context"], "traced (session default)");
    assert_eq!(overridden["context"], pending);
    assert_eq!(clear["context"], "traced (session default); the session default is cleared from the next call");
    assert!(after["context"].is_null(), "{after}");
    assert_eq!((&x_during["context"], &x_override["context"]), (&json!("traced"), &json!("normal")));
}

#[test]
fn top_level_forms_keep_their_meaning() {
    let case = KernelCase::new();
    let mut k = kernel(&case);
    ok(k.turn("sq(x; p=2) = x^p; nothing"));
    let runs: Vec<Value> = ["normal", "traced", "memoized"].iter().map(|c| ok(in_context(&mut k, FORMS, c))).collect();
    let traced = xray(&mut k, &runs[1]);
    k.close();
    assert_eq!(runs[0]["data"], json!([9, 8, 25, 17, [1, 4], [2, 3], 3]));
    for r in &runs[1..] {
        assert_eq!(r["data"], runs[0]["data"], "{r}");
    }
    let functions: Vec<&Value> = traced["dispatch"].as_array().unwrap().iter().map(|d| &d["function"]).collect();
    assert_eq!(functions, ["sq", "sq", "sq", "sq", "+"]);
}

#[test]
fn exactly_one_context_per_call() {
    let case = KernelCase::new();
    let mut k = kernel(&case);
    ok(k.turn("Palette.context!(:memoized)"));
    let overridden = ok(in_context(&mut k, "1 + 1", "traced"));
    let two = k.turn_with("1", json!({"context": ["traced", "memoized"]}));
    let x = xray(&mut k, &overridden);
    k.close();
    assert_eq!(overridden["costs"]["context"], "traced");
    // the default did not also apply
    assert_eq!(x["context"], "traced");
    assert!(null(&x, "memo") && null(&x, "references"), "{x}");
    assert!(x["dispatch"].is_array(), "{x}");
    assert!(error(&two).contains("must be a string"), "{two}");
}

#[test]
fn session_default_does_not_survive_a_restart() {
    let case = KernelCase::new();
    let mut k = kernel(&case);
    ok(k.turn("Palette.context!(:traced)"));
    assert_eq!(ok(k.turn("1"))["context"], "traced (session default)");
    k.close();
    let mut k = kernel(&case);
    let r = ok(k.turn("1"));
    k.close();
    assert!(r["context"].is_null(), "{r}");
    assert_eq!(r["costs"]["context"], "normal");
}

#[test]
fn bad_requests_are_errors() {
    let case = KernelCase::new();
    let mut k = kernel(&case);
    let unknown = in_context(&mut k, "1", "bogus");
    let stray_budget = k.turn_with("1", json!({"budget": {"seconds": 1}}));
    let wrong_ctx = k.turn_with("1", json!({"context": "traced", "budget": {"seconds": 1}}));
    let empty_budget = budgeted(&mut k, "1", json!({}));
    let bad_values: Vec<Value> = [json!({"seconds": 0}), json!({"seconds": -1}), json!({"bytes": 0}), json!({"bytes": 1.5}), json!({"minutes": 1})]
        .into_iter()
        .map(|b| budgeted(&mut k, "1", b))
        .collect();
    let core = k.turn("Palette.reference!(sum, Tuple{Vector{Int}}, xs -> 0)");
    let alive = ok(k.turn("3"));
    k.close();
    assert!(error(&unknown).contains("normal, traced, reference, memoized, budgeted"), "{unknown}");
    assert!(error(&stray_budget).contains("budgeted"), "{stray_budget}");
    assert!(error(&wrong_ctx).contains("applies only to the budgeted context"), "{wrong_ctx}");
    assert!(error(&empty_budget).contains("seconds and/or bytes"), "{empty_budget}");
    let needles = [
        "seconds must be a positive number",
        "seconds must be a positive number",
        "bytes must be a positive integer",
        "bytes must be a positive integer",
        "unknown budget field(s) minutes",
    ];
    for (r, needle) in bad_values.iter().zip(needles) {
        assert!(error(r).contains(needle), "{needle}: {r}");
    }
    assert!(error(&core).contains("cannot be replaced"), "{core}");
    assert_eq!(alive["data"], 3);
}
