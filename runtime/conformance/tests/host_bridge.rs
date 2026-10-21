//! Mid-call host requests: the kernel asks the agent host to act (an RLM child,
//! for instance) while a call runs, through its broker, and only for the request
//! types its ceiling names. These tests play the host on the session bridge's
//! stdio.

use palette_conformance::{ceiling_is_subset, Broker, Kernel, KernelCase, KernelOptions, TempDir};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const RLM_TYPES: [&str; 3] = ["rlm.run", "rlm.collect", "rlm.list_subagents"];

fn child() -> Value {
    json!({"rlm_child_id": "c1", "name": "w", "session_dir": "/s/c1", "model": "openai/luna"})
}

fn ok(result: Value) -> Value {
    json!({"status": "ok", "result": result})
}

fn rlm_host(data: &Value) -> Option<Value> {
    Some(match data["type"].as_str().unwrap() {
        "rlm.run" => {
            let mut c = child();
            c["name"] = data["kwargs"]["name"].clone();
            ok(c)
        }
        "rlm.collect" => ok(json!({"results": [{"rlm_child_id": "c1", "status": "done", "settled": true, "answer_preview": "42"}]})),
        other => json!({"status": "error", "error": format!("no handler for {other}")}),
    })
}

/// The session bridge with a given ceiling, driven the way the agent host drives it.
struct Host {
    k: Kernel,
    n: usize,
    _case: KernelCase,
}

impl Host {
    fn new(ceiling: Value) -> Host {
        Host::with_timeout(ceiling, 20.0)
    }

    fn with_timeout(ceiling: Value, timeout: f64) -> Host {
        let case = KernelCase::new();
        let k = case.kernel(KernelOptions { timeout, ceiling, ..Default::default() });
        assert_eq!(k.hello["kind"], "HELLO", "{}", k.hello);
        Host { k, n: 0, _case: case }
    }

    fn rlm() -> Host {
        Host::new(json!({"host_request": {"allowed_types": RLM_TYPES}}))
    }

    fn read(&mut self, timeout: u64) -> Option<Value> {
        self.k.read(Duration::from_secs(timeout))
    }

    /// Runs one call, answering each host request with `answer(data)` (None: no
    /// reply). Returns the call's response and the requests seen.
    fn call_with(&mut self, code: &str, mut answer: impl FnMut(&Value) -> Option<Value>, extra: Value) -> (Value, Vec<Value>) {
        self.n += 1;
        let rid = format!("r{}", self.n);
        let mut request = json!({"request_id": rid, "code": code});
        request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        self.k.send(&request);
        let mut seen = Vec::new();
        loop {
            let msg = self.read(60).expect("no response");
            if msg["event"] == "host_request" {
                seen.push(msg["data"].clone());
                if let Some(reply) = answer(&msg["data"]) {
                    self.k.send(&json!({"host_reply": msg["id"], "reply": reply}));
                }
                continue;
            }
            assert_eq!(msg["request_id"], rid, "{msg}");
            return (msg, seen);
        }
    }

    fn call(&mut self, code: &str, answer: impl FnMut(&Value) -> Option<Value>) -> (Value, Vec<Value>) {
        self.call_with(code, answer, json!({}))
    }
}

fn no_host(_: &Value) -> Option<Value> {
    None
}

fn fails_with(r: &Value, text: &str) {
    assert_eq!(r["success"], false, "{r}");
    assert!(r.to_string().contains(text), "{text}: {r}");
}

#[test]
fn spawn_and_collect_in_one_call() {
    let mut h = Host::rlm();
    let (r, seen) = h.call(
        "h = Palette.rlm.spawn(\"add 40 and 2\"; name = \"w\"); \
         r = only(Palette.rlm.collect(h; timeout_ms = 1000)); (h.rlm_child_id, r.status, r.answer_preview)",
        rlm_host,
    );
    assert_eq!(r["success"], true, "{r}");
    assert!(r["display"].as_str().unwrap().contains(r#"("c1", "done", "42")"#), "{r}");
    let types: Vec<&Value> = seen.iter().map(|d| &d["type"]).collect();
    assert_eq!(types, ["rlm.run", "rlm.collect"]);
    assert_eq!(seen[0]["prompt"], "add 40 and 2");
    assert_eq!(seen[1]["targets"], json!(["c1"]));
    assert_eq!(seen[1]["timeout_ms"], 1000);
}

#[test]
fn spawn_now_collect_in_a_later_call_and_from_a_background_task() {
    let mut h = Host::rlm();
    let (r, _) = h.call("h = Palette.rlm.spawn(\"x\"; name = \"w\"); h.name", rlm_host);
    assert_eq!(r["success"], true, "{r}");
    let (r, _) = h.call("only(Palette.rlm.collect(h)).answer_preview", rlm_host);
    assert!(r["display"].as_str().unwrap().contains("\"42\""), "{r}");
    // A background collect whose request arrives after its call returned: the
    // host answers between calls, and the next call fetches it.
    let (r, _) = h.call("gate = Channel{Nothing}(1); job = @async (take!(gate); only(Palette.rlm.collect(h)).status); :started", no_host);
    assert_eq!(r["success"], true, "{r}");
    h.k.send(&json!({"request_id": "release", "code": "put!(gate, nothing); :released"}));
    let messages = [h.read(30), h.read(30)];
    assert!(messages.iter().all(Option::is_some), "{messages:?}");
    let messages: Vec<Value> = messages.into_iter().flatten().collect();
    let msg = messages.iter().find(|m| m["event"] == "host_request").expect("a host request").clone();
    let released = messages.iter().find(|m| m["request_id"] == "release").expect("the release reply");
    assert_eq!(released["success"], true, "{released}");
    h.k.send(&json!({"host_reply": msg["id"], "reply": rlm_host(&msg["data"])}));
    let (r, _) = h.call("fetch(job)", no_host);
    assert!(r["display"].as_str().unwrap().contains("\"done\""), "{r}");
}

#[test]
fn denied_without_the_permission_and_nothing_reaches_the_host() {
    let mut h = Host::new(json!({}));
    let (r, seen) = h.call("Palette.rlm.spawn(\"x\"; name = \"w\")", rlm_host);
    fails_with(&r, "host_request not in this session's ceiling");
    assert!(seen.is_empty(), "{seen:?}");
}

#[test]
fn a_type_outside_allowed_types_is_denied() {
    let mut h = Host::new(json!({"host_request": {"allowed_types": ["rlm.collect"]}}));
    let (r, seen) = h.call("Palette.rlm.spawn(\"x\"; name = \"w\")", rlm_host);
    fails_with(&r, "'rlm.run' is not in this session's allowed_types");
    assert!(seen.is_empty(), "{seen:?}");
    let (r, seen) = h.call(
        "Palette.host_request(\"rlm.collect\", Dict(\"type\" => \"rlm.run\", \"targets\" => []))",
        |_| Some(ok(json!({"results": []}))),
    );
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(seen[0]["type"], "rlm.collect", "a payload key must not reroute the request");
}

#[test]
fn the_hosts_error_and_a_malformed_reply_reach_julia_truthfully() {
    let mut h = Host::rlm();
    let (r, _) = h.call("Palette.rlm.spawn(\"x\"; name = \"w\")", |_| Some(json!({"status": "error", "error": "name 'w' is taken"})));
    fails_with(&r, "name 'w' is taken");
    let (r, _) = h.call("Palette.rlm.spawn(\"x\"; name = \"w\")", |_| Some(json!({"surprise": true})));
    fails_with(&r, "malformed reply");
}

#[test]
fn an_unanswered_request_times_out_and_a_late_reply_is_dropped() {
    let mut h = Host::with_timeout(json!({"host_request": {"allowed_types": RLM_TYPES}}), 8.0);
    let (r, seen) = h.call("Palette.rlm.spawn(\"x\"; name = \"w\")", no_host);
    assert_eq!(seen.len(), 1, "{seen:?}");
    fails_with(&r, "did not answer 'rlm.run' within 3s");
    assert_ne!(r["session_dead"], true, "the kernel must survive an unanswered request");
    h.k.send(&json!({"host_reply": "no-such-request", "reply": ok(child())}));
    let (r, _) = h.call("1 + 1", no_host);
    assert_eq!(r["success"], true, "{r}");
    assert!(r["display"].as_str().unwrap().contains('2'), "{r}");
}

#[test]
fn the_host_closing_its_end_fails_a_waiting_request_and_the_bridge_exits() {
    let mut h = Host::rlm();
    h.k.send(&json!({"request_id": "r1", "code": "Palette.rlm.spawn(\"x\"; name = \"w\")"}));
    let msg = h.read(60).expect("a host request");
    assert_eq!(msg["event"], "host_request", "{msg}");
    let started = Instant::now();
    // close() fails the test unless the bridge exits within 60 s of its stdin closing
    h.k.close();
    assert!(started.elapsed() < Duration::from_secs(60));
}

#[test]
fn ephemeral_code_cannot_reach_the_host() {
    let mut h = Host::rlm();
    let (r, seen) = h.call_with(
        "Palette.rlm.spawn(\"x\"; name = \"w\")",
        rlm_host,
        json!({"ephemeral": true, "ceiling": {"host_request": {"allowed_types": ["rlm.run"]}}}),
    );
    fails_with(&r, "no agent host is attached");
    assert!(seen.is_empty(), "an ephemeral worker must not reach the agent host");
}

// The broker's host_request category on its own: a standalone broker has no
// agent host attached, so these check the policy and the receipts.

#[test]
fn policy_and_receipts_without_a_host() {
    let d = TempDir::new("njl-hr-");
    let mut b = Broker::start(d.path(), &json!({"host_request": {"allowed_types": ["rlm.run"]}}));
    let r = b.request("host_request", json!({"type": "rlm.run", "payload": {}}));
    assert_eq!(r["approved"], false, "{r}");
    assert!(r["reason"].as_str().unwrap().contains("no agent host is attached"), "{r}");
    let r = b.request("host_request", json!({"type": "rlm.delete_subagent", "payload": {}}));
    assert!(r["reason"].as_str().unwrap().contains("'rlm.delete_subagent' is not in this session's allowed_types"), "{r}");
    let r = b.request("host_request", json!({"type": "rlm.run", "payload": [1]}));
    assert!(r["reason"].as_str().unwrap().contains("'payload' must be an object"), "{r}");
    // A request cannot attach a host of its own.
    let r = b.request("host_request", json!({"type": "rlm.run", "payload": {}, "host_bridge": "x"}));
    assert_eq!(r["approved"], false, "{r}");
    b.close();
    let receipts: Vec<Value> =
        std::fs::read_to_string(&b.receipts).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(receipts.len(), 4, "every request is receipted");
    assert!(receipts.iter().all(|r| r["category"] == "host_request" && r["approved"] == false), "{receipts:?}");
}

#[test]
fn without_the_category_nothing_is_allowed() {
    let d = TempDir::new("njl-hr-");
    let b = Broker::start(d.path(), &json!({}));
    let r = b.request("host_request", json!({"type": "rlm.run", "payload": {}}));
    assert!(r["reason"].as_str().unwrap().contains("host_request not in this session's ceiling"), "{r}");
}

#[test]
fn child_types_must_be_an_explicit_subset() {
    let parent = json!({"host_request": {"allowed_types": ["rlm.run", "rlm.collect"]}});
    let types = |t: &str| json!({"host_request": {"allowed_types": [t]}});
    assert!(ceiling_is_subset(&types("rlm.collect"), &parent).0);
    assert!(!ceiling_is_subset(&types("rlm.delete_subagent"), &parent).0);
    assert!(!ceiling_is_subset(&json!({"host_request": {}}), &parent).0);
    assert!(!ceiling_is_subset(&types("rlm.run"), &json!({})).0);
}
