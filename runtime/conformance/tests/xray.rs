//! Execution X-Ray: Palette.xray(n) in the kernel, and the host receipts of
//! call n (`palette-host control receipts`, palette_control action=receipts).
//!
//! The join is the point: every capability request a call made appears in the
//! host's receipts for that call, and no receipt is attributed to a call that
//! did not make it.

use palette_conformance::{control, run, Kernel, KernelCase, KernelOptions, TempDir};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const DIGEST: &str = r#"Palette.fs_digest([joinpath(pwd(), "a.txt")])"#;
const DENIED: &str = r#"Palette.request_capability("network_access", Dict{String,Any}("host" => "example.com", "port" => 443))"#;

/// Values the X-Ray bookkeeping could disturb if it leaked into the call: top-level
/// loops and globals, exceptions, @timed (it nests with the compile timer X-Ray
/// holds open), task-local state, and a closure compiled inside the call.
const SAME_AS_PLAIN_JULIA: [&str; 7] = [
    "repr(sum(abs2, 1:10^5))",
    "acc = Ref(0); for i in 1:1000; acc[] += i; end; repr(acc[])",
    r#"repr(try error("boom") catch e; sprint(showerror, e) end)"#,
    "t = @timed sum(1:10); repr((t.value, t.compile_time >= 0))",
    "repr(task_local_storage(:palette_probe, 7) do; task_local_storage(:palette_probe) * 6 end)",
    r#"h = x -> (x, typeof(x)); repr(map(h, (1, 2.0f0, "s")))"#,
    "repr(Base.Math.sincos(0.5))",
];

struct XRay {
    case: KernelCase,
    receipts: TempDir,
}

impl XRay {
    fn new() -> XRay {
        let case = KernelCase::new();
        std::fs::write(case.ws.join("a.txt"), "x\n").unwrap();
        XRay { case, receipts: TempDir::new("palette-xray-receipts-") }
    }

    fn kernel(&self) -> Kernel {
        self.kernel_on(self.case.state())
    }

    fn kernel_on(&self, state: &Path) -> Kernel {
        Kernel::start(
            self.case.ws(),
            state,
            KernelOptions {
                extra_args: vec!["--receipts-dir".into(), self.receipts.str().into()],
                ceiling: json!({"fs_digest": {"allowed_paths": [self.case.ws.str()]}}),
                ..Default::default()
            },
        )
    }

    fn receipts(&self, call: &Value, epoch: &Value) -> Value {
        control("receipts", self.receipts.path(), json!({"call": call, "epoch": epoch})).unwrap()
    }
}

#[test]
fn every_request_of_a_call_joins_its_receipts() {
    let t = XRay::new();
    let mut k = t.kernel();
    let r1 = k.turn(&format!("{DIGEST}; {DENIED}; xs = zeros(10^6); sum(xs)"));
    let r2 = k.turn(DIGEST);
    let x1 = k.turn(&format!("Palette.xray({})", r1["call"]));
    let x2 = k.turn(&format!("Palette.xray({})", r2["call"]));
    let epoch = k.epoch().clone();
    k.close();
    assert_eq!(r1["success"], true, "{r1}");
    assert_eq!(r1["data"], json!(0.0), "instrumentation does not change the result");
    for (r, x, n) in [(&r1, &x1, 2), (&r2, &x2, 1)] {
        assert_eq!(x["success"], true, "{x}");
        let xray = &x["data"];
        let asked: Vec<&Value> = xray["capability_requests"].as_array().unwrap().iter().map(|q| &q["id"]).collect();
        assert_eq!(asked.len(), n, "{xray}");
        let found = t.receipts(&r["call"], &epoch);
        let receipted: Vec<&Value> = found["receipts"].as_array().unwrap().iter().map(|q| &q["request_id"]).collect();
        assert_eq!(receipted, asked, "100% join, both ways: {found}");
        assert_eq!(found["call"]["session_request"], xray["session_request"]);
    }
    let network = x1["data"]["capability_requests"].as_array().unwrap().iter().find(|q| q["category"] == "network_access").unwrap();
    assert_eq!(network["approved"], false, "a denied request is receipted too");
    let seconds = &x1["data"]["seconds"];
    for key in ["execution", "compile", "gc", "run"] {
        assert!(seconds[key].is_f64(), "{seconds}");
    }
    assert!(x1["data"]["allocated_bytes"].as_f64().unwrap() >= 8e6);
    assert!(x1["data"]["limits"].as_str().unwrap().contains("process-wide"));
}

#[test]
fn instrumentation_does_not_change_results() {
    let plain = run(
        Command::new("julia").args(["--startup-file=no", "-e", r"for c in ARGS; print(include_string(Main, c), '\0'); end"]).args(SAME_AS_PLAIN_JULIA),
        "",
        Duration::from_secs(300),
    );
    assert!(plain.status.success(), "{}", String::from_utf8_lossy(&plain.stderr));
    let stdout = String::from_utf8(plain.stdout).unwrap();
    let mut expected: Vec<&str> = stdout.split('\0').collect();
    expected.pop();
    assert_eq!(expected.len(), SAME_AS_PLAIN_JULIA.len(), "{stdout}");
    let t = XRay::new();
    let mut k = t.kernel();
    let got: Vec<Value> = SAME_AS_PLAIN_JULIA.iter().map(|c| k.turn(c)).collect();
    let traced: Vec<Value> = SAME_AS_PLAIN_JULIA.iter().map(|c| k.turn_with(c, json!({"context": "traced"}))).collect();
    k.close();
    for r in got.iter().chain(&traced) {
        assert_eq!(r["success"], true, "{r}");
    }
    let data = |rs: &[Value]| rs.iter().map(|r| r["data"].as_str().unwrap_or_default().to_string()).collect::<Vec<_>>();
    assert_eq!(data(&got), expected);
    assert_eq!(data(&traced), expected);
}

#[test]
fn call_record_carries_the_context() {
    let t = XRay::new();
    let mut k = t.kernel();
    let calls = [
        k.turn("1 + 1"),
        k.turn_with("1 + 1", json!({"context": "traced"})),
        k.turn("Palette.context!(:memoized)"),
        k.turn("1 + 1"),
        k.turn_with("1 + 1", json!({"context": "normal"})),
    ];
    let epoch = k.epoch().clone();
    k.close();
    let contexts: Vec<Value> = calls.iter().map(|r| t.receipts(&r["call"], &epoch)["call"]["context"].clone()).collect();
    assert_eq!(contexts, ["normal", "traced", "normal", "memoized", "normal"]);
}

#[test]
fn call_numbers_restart_per_kernel() {
    let t = XRay::new();
    let mut k = t.kernel();
    let first = k.turn(DIGEST);
    let e1 = k.epoch().clone();
    k.close();
    // A fresh state dir: a revived kernel would continue the numbering.
    let fresh = TempDir::new_in(t.case.state(), "fresh-");
    let mut k = t.kernel_on(fresh.path());
    let second = k.turn("nothing");
    let e2 = k.epoch().clone();
    k.close();
    assert_eq!(first["call"], second["call"]);
    assert_eq!(t.receipts(&first["call"], &e1)["receipts"].as_array().unwrap().len(), 1);
    assert_eq!(t.receipts(&second["call"], &e2)["receipts"], json!([]));
}

#[test]
fn unknown_call_is_an_error() {
    let t = XRay::new();
    let mut k = t.kernel();
    let r = k.turn("Palette.xray(99)");
    k.close();
    assert_eq!(r["success"], false);
    assert!(r["error"].as_str().unwrap().contains("99"), "{r}");
}
