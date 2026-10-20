//! `palette-host session`: the persistent stdio bridge an agent host (Node)
//! spawns once per agent session. Newline-delimited JSON:
//!
//!   HELLO     {"kind": "HELLO", "epoch", "session_id", "revival"}   (once, at startup)
//!   Request   {"request_id", "code", "ephemeral"?, "ceiling"?, "payload"?, "map"?, "digest"?,
//!              "context"?, "budget"?}
//!   Response  {"request_id", "success", "data", "display", "output", "error", "epoch",
//!              "interrupted", "call", "bindings", "context", "session_dead"?}
//!
//! Mid-call host requests (kernel -> host, while a call runs or after it):
//!   Event     {"event": "host_request", "id", "data": {"type", ...payload}}
//!   Reply     {"host_reply": "<id>", "reply": {"status": "ok", "result"} | {"status": "error", "error"}}
//! The kernel reaches these only through its broker, for the request types its
//! ceiling names. Stdin is read on its own thread so a reply lands mid-call; a
//! reply for an unknown or finished request is dropped, and stdin closing fails
//! every waiting request. This process exits when stdin closes, when the kernel
//! dies, or on SIGTERM/SIGHUP/SIGINT (after tearing the session down).

use crate::session::{PaletteSession, SessionError, SessionOptions, Turn};
use crate::util;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The longest --max-turn-timeout this host takes (7 days); the Python host
/// and launchers share it as source_transport.LONGEST_TURN_TIMEOUT.
pub(crate) const LONGEST_TURN_TIMEOUT: f64 = 604800.0;

static OUT: Mutex<()> = Mutex::new(());

/// Responses (main thread) and host-request events (broker threads) share
/// stdout; one lock keeps every line whole.
fn respond(v: &Value) {
    let _g = OUT.lock().unwrap();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

struct Bridge {
    /// Set for each call from its limit.
    timeout: Mutex<Duration>,
    pending: Mutex<HashMap<String, Sender<Value>>>,
    closed: AtomicBool,
}

impl Bridge {
    fn call(&self, rtype: &str, payload: &Value) -> Value {
        let rid = util::hex(32);
        let (tx, rx) = mpsc::channel();
        {
            let mut p = self.pending.lock().unwrap();
            if self.closed.load(Ordering::SeqCst) {
                return json!({"status": "error", "error": "the agent host has closed its connection"});
            }
            p.insert(rid.clone(), tx);
        }
        // The type goes last, so a payload key named "type" cannot reroute the request.
        let mut data = payload.as_object().cloned().unwrap_or_default();
        data.insert("type".into(), json!(rtype));
        respond(&json!({"event": "host_request", "id": rid, "data": data}));
        let timeout = *self.timeout.lock().unwrap();
        let reply = rx.recv_timeout(timeout).unwrap_or_else(|_| {
            json!({"status": "error", "error": format!("the agent host did not answer '{rtype}' within {}s", util::fmt_g(timeout.as_secs_f64()))})
        });
        self.pending.lock().unwrap().remove(&rid);
        reply
    }

    fn resolve(&self, rid: &Value, reply: Option<&Value>) {
        let Some(rid) = rid.as_str() else { return };
        let Some(tx) = self.pending.lock().unwrap().remove(rid) else { return }; // late, unknown or timed out
        let ok = reply.and_then(|r| r.get("status")).and_then(Value::as_str).is_some_and(|s| s == "ok" || s == "error");
        let _ = tx.send(if ok { reply.unwrap().clone() } else { json!({"status": "error", "error": "the agent host sent a malformed reply"}) });
    }

    fn close(&self) {
        let mut p = self.pending.lock().unwrap();
        self.closed.store(true, Ordering::SeqCst);
        for (_, tx) in p.drain() {
            let _ = tx.send(json!({"status": "error", "error": "the agent host has closed its connection"}));
        }
    }
}

enum Incoming {
    Request(Value),
    Malformed(String),
    End,
}

struct Args {
    project_dir: String,
    repo_dir: Option<String>,
    ceiling: String,
    network: bool,
    turn_timeout: f64,
    max_turn_timeout: Option<f64>,
    startup_timeout: f64,
    workspace_dir: Option<String>,
    state_dir: Option<String>,
    receipts_dir: Option<String>,
    scratch_root: Option<String>,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut a = Args { project_dir: String::new(), repo_dir: None, ceiling: "{}".into(), network: false,
                       turn_timeout: 60.0, max_turn_timeout: None, startup_timeout: 180.0, workspace_dir: None, state_dir: None, receipts_dir: None, scratch_root: None };
    let mut i = 0;
    let val = |i: &mut usize| -> Result<String, String> {
        *i += 1;
        argv.get(*i).cloned().ok_or_else(|| format!("{} needs a value", argv[*i - 1]))
    };
    while i < argv.len() {
        match argv[i].as_str() {
            "--project-dir" => a.project_dir = val(&mut i)?,
            "--repo-dir" => a.repo_dir = Some(val(&mut i)?),
            "--ceiling" => a.ceiling = val(&mut i)?,
            "--network" => a.network = true,
            "--turn-timeout" => a.turn_timeout = val(&mut i)?.parse().map_err(|e| format!("--turn-timeout: {e}"))?,
            "--max-turn-timeout" => a.max_turn_timeout = Some(val(&mut i)?.parse().map_err(|e| format!("--max-turn-timeout: {e}"))?),
            "--startup-timeout" => a.startup_timeout = val(&mut i)?.parse().map_err(|e| format!("--startup-timeout: {e}"))?,
            "--workspace-dir" => a.workspace_dir = Some(val(&mut i)?),
            "--state-dir" => a.state_dir = Some(val(&mut i)?),
            "--receipts-dir" => a.receipts_dir = Some(val(&mut i)?),
            "--scratch-root" => a.scratch_root = Some(val(&mut i)?),
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    if a.project_dir.is_empty() {
        return Err("--project-dir is required".into());
    }
    let max_turn_timeout = a.max_turn_timeout.unwrap_or(a.turn_timeout);
    if !(a.turn_timeout > 0.0 && a.turn_timeout <= max_turn_timeout && max_turn_timeout <= LONGEST_TURN_TIMEOUT) {
        return Err(format!("--turn-timeout must be positive and no more than --max-turn-timeout, \
                            which is at most {LONGEST_TURN_TIMEOUT} (7 days)"));
    }
    if a.repo_dir.is_none() {
        a.repo_dir = Some(default_repo_dir()?);
    }
    Ok(a)
}

/// The Palette checkout when --repo-dir is absent: PALETTE_REPO_DIR, or the
/// checkout this binary runs from. Never the checkout it was built in, which
/// may not exist on this machine or may be a different version.
pub fn default_repo_dir() -> Result<String, String> {
    if let Ok(r) = std::env::var("PALETTE_REPO_DIR") {
        if !r.is_empty() {
            return Ok(r);
        }
    }
    crate::util::installed_checkout().map(|dir| dir.to_string_lossy().into_owned()).ok_or_else(|| {
        "no Palette checkout found: this binary is not inside one, so pass --repo-dir or set PALETTE_REPO_DIR".into()
    })
}

/// Text, or an object of named texts; anything else is dropped.
fn payload(v: Option<&Value>) -> Option<Value> {
    match v? {
        Value::String(s) => Some(json!(s)),
        Value::Object(m) if m.values().all(Value::is_string) => Some(Value::Object(m.clone())),
        _ => None,
    }
}

/// Blocks SIGTERM, SIGHUP and SIGINT in every thread and waits for them on one,
/// so Ctrl-C tears the session down like any other stop request, and a
/// signal never lands mid-write; returns the flag it sets and the signal seen.
fn signal_thread(wake: Sender<Incoming>) -> (Arc<AtomicBool>, Arc<AtomicI32>) {
    let stop = Arc::new(AtomicBool::new(false));
    let signum = Arc::new(AtomicI32::new(0));
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGHUP);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        let (stop2, signum2) = (stop.clone(), signum.clone());
        std::thread::spawn(move || {
            let mut sig: libc::c_int = 0;
            if libc::sigwait(&set, &mut sig) == 0 {
                signum2.store(sig, Ordering::SeqCst);
                stop2.store(true, Ordering::SeqCst);
                let _ = wake.send(Incoming::End);
            }
        });
    }
    (stop, signum)
}

pub fn main(argv: &[String]) -> i32 {
    let args = match parse_args(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("palette-host session: {e}");
            return 2;
        }
    };
    let (tx, rx) = mpsc::channel::<Incoming>();
    let (stop, signum) = signal_thread(tx.clone());

    let ceiling_file = crate::sandbox::ceiling_file(&args.ceiling);
    let ceiling: Value = {
        let text = match &ceiling_file { Some(path) => std::fs::read_to_string(path).unwrap_or_default(), None => args.ceiling.clone() };
        match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                respond(&json!({"kind": "ERROR", "error": format!("failed to start PaletteSession: bad ceiling: {e}")}));
                return 1;
            }
        }
    };
    // The kernel revives only a state saved in this same workspace, and
    // reports an unreadable one itself.
    let mut revival = false;
    let mut state_dir = None;
    if let Some(sd) = &args.state_dir {
        let _ = std::fs::create_dir_all(sd);
        let abs = std::fs::canonicalize(sd).map(|p| p.to_string_lossy().into_owned()).unwrap_or(sd.clone());
        let manifest = Path::new(sd).join("manifest.json");
        let workspace = args.workspace_dir.as_ref().and_then(|w| std::fs::canonicalize(w).ok()).map(|p| p.to_string_lossy().into_owned());
        if manifest.is_file() && workspace.is_some() {
            let saved = std::fs::read_to_string(&manifest).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
            // An unreadable manifest counts as this workspace's: the kernel reports it.
            revival = match saved {
                Some(v) => v.get("workspace").and_then(Value::as_str) == workspace.as_deref(),
                None => true,
            };
        }
        state_dir = Some(abs);
    }

    let turn_timeout = args.turn_timeout;
    let max_turn_timeout = args.max_turn_timeout.unwrap_or(turn_timeout);
    let session = PaletteSession::start(SessionOptions {
        project_dir: args.project_dir.clone(),
        repo_dir: args.repo_dir.clone().unwrap_or_default(),
        ceiling,
        ceiling_file,
        network_enabled: args.network,
        receipts_dir: args.receipts_dir.clone(),
        scratch_root: args.scratch_root.clone(),
        turn_timeout,
        max_turn_timeout,
        task_workspace_dir: args.workspace_dir.clone(),
        startup_timeout: args.startup_timeout,
        state_dir,
    }, stop.clone());
    let mut session = match session {
        Ok(s) => s,
        Err(SessionError::Dead(e)) | Err(SessionError::Protocol(e)) => {
            respond(&json!({"kind": "ERROR", "error": format!("failed to start PaletteSession: {e}")}));
            return 1;
        }
    };

    let bridge = Arc::new(Bridge {
        timeout: Mutex::new(bridge_timeout(turn_timeout)),
        pending: Mutex::new(HashMap::new()),
        closed: AtomicBool::new(false),
    });
    {
        let b = bridge.clone();
        session.broker.set_host_bridge(Arc::new(move |t: &str, p: &Value| b.call(t, p)));
    }
    {
        let (b, tx) = (bridge.clone(), tx.clone());
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut lock = stdin.lock();
            loop {
                let mut buf = Vec::new();
                match lock.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(msg) if msg.get("host_reply").is_some() => b.resolve(&msg["host_reply"], msg.get("reply")),
                    Ok(msg) => {
                        let _ = tx.send(Incoming::Request(msg));
                    }
                    Err(e) => {
                        let _ = tx.send(Incoming::Malformed(e.to_string()));
                    }
                }
            }
            b.close();
            let _ = tx.send(Incoming::End);
        });
    }

    respond(&json!({"kind": "HELLO", "epoch": session.epoch, "session_id": session.session_id, "revival": revival,
                   "capabilities": session.capabilities}));

    loop {
        let req = match rx.recv() {
            Ok(Incoming::Request(r)) => r,
            Ok(Incoming::Malformed(e)) => {
                respond(&json!({"request_id": null, "success": false, "data": null, "error": format!("malformed request: {e}")}));
                continue;
            }
            Ok(Incoming::End) | Err(_) => break,
        };
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if !req.is_object() {
            respond(&json!({"request_id": null, "success": false, "data": null, "error": "request must be a JSON object"}));
            continue;
        }
        let request_id = req.get("request_id").cloned().unwrap_or(Value::Null);
        let Some(code) = req.get("code").and_then(Value::as_str) else {
            respond(&json!({"request_id": request_id, "success": false, "data": null, "error": "request 'code' must be a string"}));
            continue;
        };
        let limit = match req.get("timeout_s").filter(|v| !v.is_null()) {
            None => turn_timeout,
            Some(v) => match v.as_f64().filter(|&t| t > 0.0 && t <= max_turn_timeout) {
                Some(t) => t,
                None => {
                    respond(&json!({"request_id": request_id, "success": false, "data": null,
                                    "error": format!("request 'timeout_s' must be a number in (0, {}], this host's --max-turn-timeout",
                                                     util::fmt_g(max_turn_timeout))}));
                    continue;
                }
            },
        };
        let ephemeral = req.get("ephemeral").is_some_and(truthy);
        if ephemeral && req.get("timeout_s").is_some_and(|v| !v.is_null()) {
            respond(&json!({"request_id": request_id, "success": false, "data": null,
                            "error": "request 'timeout_s' does not apply to an ephemeral call: its broker child \
                                      is bounded by the session's --turn-timeout"}));
            continue;
        }
        let context_error = [("context", "a string", Value::is_string as fn(&Value) -> bool), ("budget", "an object", Value::is_object)]
            .into_iter()
            .find_map(|(field, kind, is_kind)| match req.get(field).filter(|v| !v.is_null()) {
                Some(v) if !is_kind(v) => Some(format!("request '{field}' must be {kind}")),
                Some(_) if ephemeral => Some(format!("request '{field}' does not apply to an ephemeral call: \
                                                      a scratch worker always runs in the normal context")),
                _ => None,
            });
        let caller_error = match req.get("caller").filter(|v| !v.is_null()) {
            Some(Value::String(a)) if crate::teamlab::valid_agent(a) => None,
            Some(_) => Some("request 'caller' must be an agent name: 1-32 of [a-z0-9._-], starting with a letter or digit".to_string()),
            None => None,
        };
        if let Some(error) = context_error.or(caller_error) {
            respond(&json!({"request_id": request_id, "success": false, "data": null, "error": error}));
            continue;
        }
        *bridge.timeout.lock().unwrap() = bridge_timeout(limit);
        let result = session.turn(Turn {
            code,
            ephemeral,
            ephemeral_ceiling: req.get("ceiling").filter(|c| !c.is_null()).cloned(),
            payload: payload(req.get("payload")),
            include_map: req.get("map") == Some(&Value::Bool(true)),
            digest: req.get("digest") != Some(&Value::Bool(false)),
            timeout_s: Some(limit),
            context: req.get("context").filter(|v| !v.is_null()).cloned(),
            budget: req.get("budget").filter(|v| !v.is_null()).cloned(),
            caller: req.get("caller").and_then(Value::as_str).map(str::to_string),
        });
        if stop.load(Ordering::SeqCst) {
            break; // shutting down: no reply, as for any process killed mid-call
        }
        match result {
            Ok(r) => respond(&json!({
                "request_id": request_id,
                "success": r.get("success").cloned().unwrap_or(json!(false)),
                "data": r.get("data").cloned().unwrap_or(Value::Null),
                "display": r.get("display").cloned().unwrap_or(Value::Null),
                "output": r.get("output").cloned().unwrap_or(Value::Null),
                "error": r.get("error").cloned().unwrap_or(Value::Null),
                "epoch": r.get("epoch").cloned().unwrap_or(Value::Null),
                "interrupted": r.get("interrupted").is_some_and(truthy),
                "call": r.get("call").cloned().unwrap_or(Value::Null),
                "bindings": r.get("bindings").cloned().unwrap_or(Value::Null),
                "revival": r.get("revival").cloned().unwrap_or(Value::Null),
                "reloads": r.get("reloads").cloned().unwrap_or(Value::Null),
                "costs": r.get("costs").cloned().unwrap_or(Value::Null),
                "context": r.get("context").cloned().unwrap_or(Value::Null),
                "transport_shortened": r.get("transport_shortened").cloned().unwrap_or(Value::Null),
            })),
            Err(SessionError::Dead(e)) => {
                // `session_dead` lets the host stop sending calls now, rather
                // than racing this process's exit.
                respond(&json!({"request_id": request_id, "success": false, "data": null, "error": e, "session_dead": true}));
                break;
            }
            Err(SessionError::Protocol(e)) => {
                respond(&json!({"request_id": request_id, "success": false, "data": null, "error": e}));
            }
        }
    }
    bridge.close();
    session.close();
    let sig = signum.load(Ordering::SeqCst);
    if sig != 0 { 128 + sig } else { 0 }
}

/// A host request must be answered or given up on before the call itself
/// times out, so the kernel gets an error rather than being killed.
fn bridge_timeout(limit: f64) -> Duration {
    Duration::from_secs_f64((limit - 5.0).max(1.0))
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
