//! The tool lifecycle and the code graph: a helper used in 3 successful calls
//! is promoted to a project tool, a fresh session lists it from the index and
//! loads it on its first call, X-Ray cites its evidence, a human approval
//! through the router's palette_patch makes it trusted, and a hash mismatch
//! drops it back to a project tool. The kernel cannot write the trusted
//! directory.

use palette_conformance::{host_bin, ok, read_json, wait_timeout, Kernel, KernelCase, KernelOptions, TempDir};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const DEFINE: &str = r#"
"Twice x, plus an offset."
function double_it(x; offset=0)
    return 2x + offset
end
"#;

struct Case {
    case: KernelCase,
    trusted: PathBuf,
    _trusted_root: TempDir,
    _states: Vec<TempDir>,
}

impl Case {
    fn new() -> Case {
        let root = TempDir::new("palette-trusted-");
        Case { case: KernelCase::new(), trusted: root.join("tools"), _trusted_root: root, _states: Vec::new() }
    }

    fn env(&self) -> Vec<(String, String)> {
        vec![("PALETTE_TRUSTED_TOOLS".into(), self.trusted.to_str().unwrap().into())]
    }

    fn kernel(&self) -> Kernel {
        self.kernel_on(self.case.state())
    }

    fn kernel_on(&self, state: &Path) -> Kernel {
        Kernel::start(self.case.ws(), state, KernelOptions { env: self.env(), ..Default::default() })
    }

    /// A kernel on a new, empty state directory: a fresh session over the same workspace.
    fn fresh_kernel(&mut self) -> (Kernel, PathBuf) {
        let state = TempDir::new("palette-tools-state-");
        let path = state.path().to_path_buf();
        self._states.push(state);
        (self.kernel_on(&path), path)
    }

    fn tools_dir(&self) -> PathBuf {
        self.case.ws().join(".palette/tools")
    }

    fn trusted_entries(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.trusted).unwrap().map(|e| e.unwrap().path()).collect()
    }

    fn promote_double_it(&self) -> Value {
        let mut k = self.kernel();
        ok(k.turn(DEFINE));
        for i in 0..2 {
            ok(k.turn(&format!("double_it({i})")));
        }
        fails(&k.turn("Palette.promote(:double_it)"), "has 2 successful call(s)");
        ok(k.turn("double_it(5; offset=1)"));
        let promoted = ok(k.turn("Palette.promote(:double_it)"))["data"].clone();
        k.close();
        promoted
    }

    /// Prepares the patch through `palette-host mcp`, then applies it and answers the elicitation.
    fn router_apply(&self, args: &Value, confirm: bool) -> Value {
        let root = TempDir::new("palette-tools-router-");
        std::fs::create_dir(root.join("project")).unwrap();
        let mut proc = Command::new(host_bin())
            .arg("mcp")
            .envs(self.env())
            .env("OPERATOR_WORKSPACE", root.join("project"))
            .env("PALETTE_STATE_DIR", root.join("state"))
            .env("PALETTE_WORKSPACE_STATE_ROOT", root.join("registry"))
            .env("PALETTE_PATCH_ROOTS", "{}")
            .env("PALETTE_RUNTIME_ROOT", root.join("runtime"))
            .env("PALETTE_TEAMLAB_DIR", root.join("teamlab"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("starting palette-host mcp");
        let mut stdin = proc.stdin.take().unwrap();
        let mut stdout = BufReader::new(proc.stdout.take().unwrap());
        let mut exchange = |value: Value| -> Value {
            writeln!(stdin, "{value}").and_then(|()| stdin.flush()).expect("writing to the router");
            let mut line = String::new();
            stdout.read_line(&mut line).expect("reading from the router");
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("router line {line:?}: {e}"))
        };
        let text = |frame: &Value| -> Value { serde_json::from_str(frame["result"]["content"][0]["text"].as_str().unwrap()).unwrap() };
        let tool = |id: u32, arguments: Value| {
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "palette_patch", "arguments": arguments}})
        };
        exchange(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"capabilities": {"elicitation": {"form": {}}}}}));
        let prepared = text(&exchange(tool(1, json!({"action": "prepare", "target": args["target"], "changes": args["changes"]}))));
        let prompt = exchange(tool(2, json!({"action": "apply", "request_id": prepared["request_id"]})));
        assert_eq!(prompt["method"], "elicitation/create", "{prompt}");
        let answer = if confirm { json!({"action": "accept", "content": {"confirm": true}}) } else { json!({"action": "decline"}) };
        let result = text(&exchange(json!({"jsonrpc": "2.0", "id": prompt["id"], "result": answer})));
        drop(stdin);
        if wait_timeout(&mut proc, Duration::from_secs(60)).is_none() {
            let _ = proc.kill();
            let _ = proc.wait();
            panic!("palette-host mcp did not exit within 60 s of its stdin closing");
        }
        result
    }
}

fn fails(r: &Value, text: &str) {
    assert_eq!(r["success"], false, "{r}");
    assert!(r.to_string().contains(text), "{text}: {r}");
}

fn xray(k: &mut Kernel, r: &Value) -> Value {
    ok(k.turn(&format!("Palette.xray({})", r["call"])))["data"].clone()
}

#[test]
fn promote_list_load_on_first_call_and_cite_evidence() {
    let mut c = Case::new();
    let promoted = c.promote_double_it();
    assert_eq!((&promoted["stage"], &promoted["evidence_count"]), (&json!("project"), &json!(3)));
    let meta = read_json(&c.tools_dir().join("double_it.json"));
    assert_eq!(meta["doc"], "Twice x, plus an offset.");
    assert_eq!(meta["evidence"].as_array().unwrap().len(), 3);
    assert!(meta["signatures"].as_array().unwrap().contains(&json!("double_it(x; offset = 0)")), "{meta}");

    let (mut k, state) = c.fresh_kernel();
    let listed = ok(k.turn("Palette.tools()"))["data"].clone();
    let hidden = ok(k.turn(r#"any(r -> r[1] == "double_it", varinfo().rows)"#));
    let first = ok(k.turn("double_it(21)"));
    let second = ok(k.turn("double_it(1; offset=2)"));
    let (x1, x2) = (xray(&mut k, &first), xray(&mut k, &second));
    k.close();
    let entry = &listed["double_it"];
    assert_eq!((&entry["stage"], &entry["verified"], &entry["evidence_count"]), (&json!("project"), &json!(true), &json!(3)));
    assert_eq!(hidden["data"], false, "a tool stub showed up as a session binding");
    assert_eq!((&first["data"], &second["data"]), (&json!(42), &json!(4)));
    let tools = x1["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "{x1}");
    let t1 = &tools[0];
    assert_eq!(
        (&t1["name"], &t1["stage"], &t1["loaded"], &t1["evidence_count"]),
        (&json!("double_it"), &json!("project"), &json!(true), &json!(3))
    );
    assert_eq!(t1["source_sha256"], meta["source_sha256"]);
    assert_eq!(x2["tools"][0]["loaded"], false);

    // The stub is not a binding: a revived session neither restores nor loses it.
    let mut k = c.kernel_on(&state);
    let lost = ok(k.turn(r#"Palette.revival()["lost"]"#))["data"].clone();
    let again = ok(k.turn("double_it(2)"))["data"].clone();
    k.close();
    let none_lost = match &lost {
        Value::Null | Value::Bool(false) => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    };
    assert!(none_lost, "{lost}");
    assert_eq!(again, 4);
}

#[test]
fn a_tool_must_stand_alone() {
    let c = Case::new();
    let mut k = c.kernel();
    ok(k.turn("scale = 3; helper(x) = scale * x; inner(x) = x + 1; outer(x) = inner(x) * 2; nothing"));
    for _ in 0..3 {
        ok(k.turn("helper(1) + outer(1) + inner(1)"));
    }
    fails(&k.turn("Palette.promote(:helper)"), "uses session bindings scale");
    fails(&k.turn("Palette.promote(:outer)"), "uses session bindings inner");
    ok(k.turn("Palette.promote(:inner)"));
    fails(&k.turn("Palette.promote(:nope)"), "only those can be promoted");
    k.close();
    let mut names: Vec<String> =
        std::fs::read_dir(c.tools_dir()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["inner.jl", "inner.json"]);
}

#[test]
fn a_redefinition_resets_the_uses() {
    let c = Case::new();
    let mut k = c.kernel();
    ok(k.turn("h(x) = x + 1"));
    for _ in 0..3 {
        ok(k.turn("h(1)"));
    }
    ok(k.turn("h(x) = x + 2"));
    ok(k.turn("h(1)"));
    fails(&k.turn("Palette.promote(:h)"), "has 1 successful call(s)");
    k.close();
}

#[test]
fn a_sidecar_cannot_promote_itself_and_the_kernel_cannot_write_trusted() {
    let mut c = Case::new();
    c.promote_double_it();
    let sidecar = c.tools_dir().join("double_it.json");
    let mut meta = read_json(&sidecar);
    meta["stage"] = json!("trusted");
    std::fs::write(&sidecar, meta.to_string()).unwrap();
    let (mut k, _) = c.fresh_kernel();
    let entry = ok(k.turn("Palette.tools()"))["data"]["double_it"].clone();
    fails(&k.turn(r#"write(joinpath(ENV["PALETTE_TRUSTED_TOOLS"], "x.jl"), "x() = 1")"#), "Read-only file system");
    k.close();
    assert_eq!(entry["stage"], "project");
    assert!(entry["reason"].as_str().unwrap().contains("outside the trusted directory"), "{entry}");
    assert_eq!(c.trusted_entries(), Vec::<PathBuf>::new());
}

#[test]
fn a_human_approval_makes_it_trusted_and_drift_demotes_it() {
    let mut c = Case::new();
    c.promote_double_it();
    let mut k = c.kernel();
    let request = ok(k.turn("Palette.trust(:double_it)"))["data"].clone();
    k.close();
    assert_eq!(request["tool"], "palette_patch");
    let args = &request["arguments"];
    assert_eq!(args["target"], "trusted_tools");
    // Declined, then confirmed: the human answers the router's elicitation.
    assert_eq!(c.router_apply(args, false)["status"], "not_approved");
    assert_eq!(c.trusted_entries(), Vec::<PathBuf>::new());
    let receipt = c.router_apply(args, true);
    assert_eq!((&receipt["status"], &receipt["granted_by"]), (&json!("applied"), &json!("mcp_client_confirmed_user")), "{receipt}");

    let (mut k, _) = c.fresh_kernel();
    let trusted = ok(k.turn("Palette.tools()"))["data"]["double_it"].clone();
    let used = ok(k.turn("double_it(4)"));
    let stage = xray(&mut k, &used)["tools"][0]["stage"].clone();
    k.close();
    assert_eq!((&trusted["stage"], &trusted["verified"]), (&json!("trusted"), &json!(true)));
    assert_eq!((&used["data"], &stage), (&json!(8), &json!("trusted")));

    std::fs::write(c.trusted.join("double_it.jl"), "double_it(x; offset=0) = 3x\n").unwrap();
    let (mut k, _) = c.fresh_kernel();
    let demoted = ok(k.turn("Palette.tools()"))["data"]["double_it"].clone();
    let result = ok(k.turn("double_it(4)"))["data"].clone();
    k.close();
    assert_eq!((&demoted["stage"], &demoted["verified"]), (&json!("project"), &json!(true)));
    assert_eq!(result, 8, "the changed trusted copy ran");
}

#[test]
fn a_changed_project_tool_is_not_loaded() {
    let mut c = Case::new();
    c.promote_double_it();
    let (mut k, _) = c.fresh_kernel();
    ok(k.turn("double_it(1)"));
    std::fs::write(c.tools_dir().join("double_it.jl"), "double_it(x; offset=0) = 3x\n").unwrap();
    fails(&k.turn("double_it(1)"), "changed since it was promoted");
    let entry = ok(k.turn("Palette.tools()"))["data"]["double_it"].clone();
    k.close();
    assert_eq!(entry["verified"], false);
}

#[test]
fn codegraph_refreshes_one_file_and_is_cited() {
    let c = Case::new();
    let ws = c.case.ws();
    std::fs::write(ws.join("lib.jl"), "g(x) = x\nf(x) = g(x)\n").unwrap();
    std::fs::write(ws.join("other.py"), "def p():\n    pass\n").unwrap();
    let mut k = c.kernel();
    let first = ok(k.turn(r#"[c[2].caller for c in Palette.callers("g")]"#));
    let unchanged = ok(k.turn(r#"length(Palette.definitions("p"))"#));
    std::fs::write(ws.join("lib.jl"), "g(x) = x\nf(x) = g(x)\nk(x) = g(x)\n").unwrap();
    let changed = ok(k.turn(r#"sort([c[2].caller for c in Palette.callers("g")])"#));
    let records: Vec<Value> = [&first, &unchanged, &changed].iter().map(|r| xray(&mut k, r)["codegraph"].clone()).collect();
    k.close();
    assert_eq!((&first["data"], &unchanged["data"], &changed["data"]), (&json!(["f"]), &json!(1), &json!(["f", "k"])));
    let one = |r: &Value| -> Value {
        let a = r.as_array().unwrap();
        assert_eq!(a.len(), 1, "{r}");
        a[0].clone()
    };
    let (c1, c2, c3) = (one(&records[0]), one(&records[1]), one(&records[2]));
    assert_eq!(c1["files"], 2);
    let mut reparsed: Vec<&str> = c1["reparsed_paths"].as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
    reparsed.sort();
    assert_eq!(reparsed, ["lib.jl", "other.py"]);
    assert_eq!(c2["reparsed_paths"], json!([]));
    assert_eq!(c3["reparsed_paths"], json!(["lib.jl"]));
    assert_eq!(c1["digest"], c2["digest"]);
    assert_ne!(c2["digest"], c3["digest"]);
}
