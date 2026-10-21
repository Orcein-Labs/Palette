//! `palette-host mcp child`: one workspace's Palette over MCP stdio, ported
//! from operator_mcp.py. It runs `palette-host session` for the workspace,
//! answers the kernel's `palette.runtime` host requests, and reports
//! preparation, restoration and call costs. The router starts one per live
//! workspace; a client may also talk to it directly.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::patch::{self, PatchBroker};
use super::registry::Registry;
use super::repair::RepairBroker;
use super::{preparation, receipts, time_machine, transport, Fault};
use crate::{cli, provisioning, sandbox, util};

const WARM_CALLS_KEPT: usize = 20;

/// `value.get(key)` that reads an absent key, or a non-object, as null.
pub(crate) fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    static NULL: Value = Value::Null;
    value.get(key).unwrap_or(&NULL)
}

/// `str(value or "")`.
pub(crate) fn text_or_empty(value: &Value) -> String {
    match value {
        v if !patch::truthy(v) => String::new(),
        Value::String(s) => s.clone(),
        v => util::py_repr(v),
    }
}

/// The AttributeError Python raises calling `.{name}` on a JSON value that
/// is not an object.
pub(crate) fn no_get(value: &Value) -> Fault {
    patch::no_attribute(value, "get")
}

/// A JSON-RPC frame, one line on stdout.
pub(crate) fn emit(frame: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{frame}");
    let _ = out.flush();
}

pub(crate) fn text_result(event: &Value, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": event.to_string()}], "isError": is_error})
}

/// Waits up to `timeout` for `child` to exit; its return code as Python's
/// subprocess gives it (minus the signal that ended it), or None.
pub(crate) fn wait_for(child: &mut Child, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(returncode(status)),
            Err(_) => return Some(-1),
            Ok(None) if Instant::now() >= deadline => return None,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

pub(crate) fn returncode(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| -status.signal().unwrap_or(0))
}

/// Closes a child's stdin, then gives it `grace` to exit before SIGTERM,
/// then 5 s before SIGKILL. True if it had to be signalled.
pub(crate) fn retire(child: &mut Child, grace: Duration) -> (i32, bool) {
    if let Some(code) = wait_for(child, grace) {
        return (code, false);
    }
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    if let Some(code) = wait_for(child, Duration::from_secs(5)) {
        return (code, true);
    }
    let _ = child.kill();
    (child.wait().map(returncode).unwrap_or(-9), true)
}

/// The same as `retire` for a process another thread owns, by pid; for the
/// signal handler, which exits right after.
pub(crate) fn retire_pid(pid: libc::pid_t, grace: Duration) {
    let gone = |within: Duration| {
        let deadline = Instant::now() + within;
        loop {
            let mut status = 0;
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if r != 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    if gone(grace) {
        return;
    }
    unsafe { libc::kill(pid, libc::SIGTERM) };
    if !gone(Duration::from_secs(5)) {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        gone(Duration::from_secs(5));
    }
}

/// Blocks SIGTERM, SIGHUP and SIGINT in every thread and runs `on_signal`
/// on one when one arrives, then exits 128 + its number, as the Python
/// servers' `SystemExit(128 + signum)` did. Call before starting threads.
pub(crate) fn exit_on_signal(on_signal: impl FnOnce() + Send + 'static) {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGHUP);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        std::thread::spawn(move || {
            let mut sig: libc::c_int = 0;
            if libc::sigwait(&set, &mut sig) == 0 {
                on_signal();
                std::process::exit(128 + sig);
            }
        });
    }
}

/// The parts of the live session the signal thread needs: closing stdin
/// asks the host to save its last snapshot and exit.
#[derive(Default)]
struct Live {
    stdin: Option<ChildStdin>,
    pid: Option<libc::pid_t>,
}

type Frames = Receiver<Result<Value, Fault>>;

/// Lines of `stdout` as JSON frames, then an `EOFError` once it ends.
fn pump(stdout: impl std::io::Read + Send + 'static) -> Frames {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&buf);
            match serde_json::from_str::<Value>(&line) {
                Ok(frame) => {
                    if tx.send(Ok(frame)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let head: String = line.chars().take(256).collect();
                    let _ = tx.send(Err(Fault::new("RuntimeError", format!(
                        "Invalid operator JSON frame ({} characters): {}", line.chars().count(), util::py_str(&head)))));
                    break;
                }
            }
        }
        let _ = tx.send(Err(Fault::new("EOFError", "operator exited")));
    });
    rx
}

/// The next frame before `deadline`, or Python's `queue.Empty`.
fn receive(frames: &Frames, deadline: Instant) -> Result<Value, Fault> {
    let wait = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    match frames.recv_timeout(wait) {
        Ok(item) => item,
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Err(Fault::new("Empty", "")),
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n % 2 == 1 { values[n / 2] } else { (values[n / 2 - 1] + values[n / 2]) / 2.0 }
}

pub(crate) fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// A tool reply's quiet form: changed bindings instead of the inventory,
/// and none of the empty or bookkeeping fields. `view` is the bindings
/// seen in this epoch so far, as (name, row) in first-seen order.
pub(crate) fn project_result(event: &Map<String, Value>, full: bool, ephemeral: bool,
                             view: &mut (Vec<(String, Value)>, Value)) -> Map<String, Value> {
    let mut result = event.clone();
    if let (Some(Value::Array(rows)), false) = (event.get("bindings"), ephemeral) {
        let mut current: Vec<(String, Value)> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for row in rows {
            let text = row.as_str().unwrap_or("");
            let name = text.split_once(" (").map_or(text, |(name, _)| name).to_string();
            match at.get(&name) {
                Some(&i) => current[i].1 = row.clone(),
                None => {
                    at.insert(name.clone(), current.len());
                    current.push((name, row.clone()));
                }
            }
        }
        let epoch = get(&Value::Object(event.clone()), "epoch").clone();
        let previous: HashMap<&str, &Value> = if view.1 == epoch {
            view.0.iter().map(|(name, row)| (name.as_str(), row)).collect()
        } else {
            HashMap::new()
        };
        let changed: Vec<Value> = current.iter().filter(|(name, row)| previous.get(name.as_str()) != Some(&row)).map(|(_, row)| row.clone()).collect();
        let absent: Vec<Value> = if view.1 == epoch {
            view.0.iter().filter(|(name, _)| !at.contains_key(name)).map(|(name, _)| json!(name)).collect()
        } else {
            Vec::new()
        };
        if !full {
            result.remove("bindings");
            if !changed.is_empty() || !absent.is_empty() {
                result.insert("binding_changes".into(), json!({"new_or_changed": changed, "no_longer_listed": absent}));
            }
            if rows.len() >= 200 {
                result.insert("binding_observation".into(), json!("Inventory lists at most 200 bindings; no longer listed does not prove deletion. Inspect varinfo() for the full world."));
            }
        }
        *view = (current, epoch);
    }
    if full {
        return result;
    }
    if result.get("data").is_some_and(|v| !v.is_null()) {
        result.remove("display");
    }
    if result.contains_key("costs") && result.get("success") != Some(&Value::Bool(false)) {
        result.remove("costs");
    }
    for key in ["display", "error", "output", "revival", "reloads", "retired_snapshot", "call", "costs", "context", "transport_shortened"] {
        if matches!(result.get(key), None | Some(Value::Null)) || result.get(key) == Some(&json!("")) {
            result.remove(key);
        }
    }
    if result.get("interrupted") == Some(&Value::Bool(false)) {
        result.remove("interrupted");
    }
    result.remove("request_id");
    result
}

pub(crate) const PALETTE_DESCRIPTION: &str = "Persistent Julia Palette through the Rust sandbox. Payload carries text verbatim. \
Scratch runs in a disposable process. Replies default to quiet; view=full retains response metadata. Inspect varinfo(), \
Api.provenance(:name), Api.changes(), Api.jobs(), and Api.costs() on demand. Offline preparation and existing state revival \
are wired; inspect restoration evidence after any epoch change. ";

pub(crate) const CONTROL_DESCRIPTION: &str = "Inspect preparation, prepare offline, restart, or go back to a checkpoint. \
restore=false starts a clean world and retires the previous snapshot without deleting workspace files; checkpoints are kept. \
prepare=false exercises the cold path. ";

pub(crate) fn palette_schema(limits: (f64, f64)) -> Value {
    json!({"type": "object", "properties": transport::palette_properties(limits), "additionalProperties": false})
}

pub(crate) fn control_schema(actions: &[&str]) -> Value {
    let mut properties = Map::new();
    properties.insert("action".into(), json!({"enum": actions}));
    properties.extend(transport::control_properties());
    if let Value::Object(query) = receipts::query_schema() {
        properties.extend(query);
    }
    json!({"type": "object", "properties": properties, "required": ["action"], "additionalProperties": false})
}

pub(crate) const CONTROL_ACTIONS: [&str; 6] = ["status", "prepare", "restart", "checkpoints", "restore", "receipts"];

/// The repair broker's client when a kernel asks: there is no MCP client to
/// ask for approval, so nothing ever answers.
struct NoClient;

impl patch::Client for NoClient {
    fn send(&mut self, _: &Value) -> std::io::Result<()> {
        Ok(())
    }
    fn recv(&mut self, _: Duration) -> Option<String> {
        None
    }
}

struct Adapter {
    repo: String,
    host: String,
    workspace: String,
    state_dir: PathBuf,
    receipts_dir: PathBuf,
    ceiling: Value,
    limits: (f64, f64),
    workspace_id: String,
    registry: Registry,
    repair: Arc<RepairBroker>,

    proc: Option<Child>,
    frames: Option<Frames>,
    live: Arc<Mutex<Live>>,
    hello: Option<Value>,
    session_notice: Option<Map<String, Value>>,
    preparation: Option<Value>,
    bindings: (Vec<(String, Value)>, Value),
    connection_failure: Option<Value>,
    /// This kernel's start-up, first call and warm calls, which
    /// palette_control status reports.
    kernel_costs: Option<Map<String, Value>>,
    refresh_pending: Option<Map<String, Value>>,
}

pub(crate) fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

pub(crate) fn current_exe() -> String {
    std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "palette-host".into())
}

/// The default `palette-host` and Palette checkout when the environment
/// names none: this binary and the checkout it runs from.
pub(crate) fn repo_and_host() -> Result<(String, String), String> {
    let repo = match env("PALETTE_REPO") {
        Some(r) => r,
        None => cli::default_repo_dir()?,
    };
    Ok((repo, env("PALETTE_HOST").unwrap_or_else(current_exe)))
}

pub(crate) fn default_state_dir(workspace: &str) -> PathBuf {
    util::home().join(".local/share/operator-surfaces/palette-state").join(util::sha256_hex(workspace.as_bytes()))
}

pub(crate) fn default_workspace_root() -> PathBuf {
    util::home().join(".local/share/operator-surfaces/palette-workspaces")
}

pub(crate) fn receipts_dir(state_dir: &Path) -> PathBuf {
    let mut name = state_dir.file_name().unwrap_or_default().to_os_string();
    name.push(".receipts");
    state_dir.with_file_name(name)
}

/// PALETTE_REPAIR_CONFIG, if set: `test_recipes`, `auto_apply_prefixes` and
/// `sandbox_source_paths`, nothing else.
pub(crate) fn repair_config() -> Result<Map<String, Value>, Fault> {
    let Some(path) = env("PALETTE_REPAIR_CONFIG").filter(|p| !p.is_empty()) else { return Ok(Map::new()) };
    let path = util::expand_user(&path);
    let bytes = fs::read(&path).map_err(|e| Fault::os(&e, &path))?;
    let text = std::str::from_utf8(&bytes).map_err(|e| Fault::new("UnicodeDecodeError", util::py_utf8_error(&bytes, &e)))?;
    match super::py_json_loads(text)? {
        Value::Object(config) if config.keys().all(|k| ["test_recipes", "auto_apply_prefixes", "sandbox_source_paths"].contains(&k.as_str())) => Ok(config),
        _ => Err(Fault::value("PALETTE_REPAIR_CONFIG has unsupported fields")),
    }
}

/// The patch broker over `storage` with the repair config's prefixes and
/// sandbox sources.
pub(crate) fn patch_broker(storage: &Path, config: &Map<String, Value>, protected: &[PathBuf], registry_root: &Path)
    -> Result<PatchBroker, Fault> {
    let roots = patch::patch_roots_from_env(env)?;
    let options = patch::Options {
        auto_apply_prefixes: config.get("auto_apply_prefixes").cloned().unwrap_or_else(|| json!({})),
        protected_paths: protected.to_vec(),
        registry_root: Some(registry_root.to_string_lossy().into_owned()),
        sandbox_source_paths: config.get("sandbox_source_paths").cloned().unwrap_or_else(|| json!({})),
        approval: env("PALETTE_PATCH_APPROVAL"),
    };
    PatchBroker::new(storage, &roots, options)
}

impl Adapter {
    fn new() -> Result<Adapter, String> {
        let (repo, host) = repo_and_host()?;
        let workspace = env("OPERATOR_WORKSPACE").ok_or("KeyError: 'OPERATOR_WORKSPACE'")?;
        let workspace = util::absolute(Path::new(&workspace)).to_string_lossy().into_owned();
        let state_dir = util::absolute(&env("PALETTE_STATE_DIR").map(PathBuf::from).unwrap_or_else(|| default_state_dir(&workspace)));
        let receipts_dir = receipts_dir(&state_dir);
        let ceiling = provisioning::with_teamlab(provisioning::operator_ceiling(env)?, env)?;
        let limits = transport::turn_limits(env)?;
        let runtime_root = util::resolve(&util::expand_user(&env("PALETTE_RUNTIME_ROOT").unwrap_or_else(|| format!("{repo}/runtime/capabilities"))));
        let registry = Registry::new(&runtime_root, &ceiling, Path::new(&workspace), &state_dir, Some(Path::new(&repo)), Some(Path::new(&host)))
            .map_err(|e| e.to_string())?;
        let patch_root = util::resolve(&env("PALETTE_WORKSPACE_STATE_ROOT").map(|p| util::expand_user(&p)).unwrap_or_else(default_workspace_root));
        let mut protected = provisioning::protected_host_paths(&ceiling, true, false, env)?;
        protected.extend([state_dir.clone(), receipts_dir.clone(), patch_root.clone()]);
        let repair = (|| {
            let config = repair_config()?;
            let patches = patch_broker(&patch_root.join("patches"), &config, &protected, &runtime_root)?;
            let recipes = match config.get("test_recipes") {
                None => Map::new(),
                Some(Value::Object(recipes)) => recipes.clone(),
                Some(other) => return Err(patch::no_attribute(other, "items")),
            };
            let prefixes = config.get("auto_apply_prefixes").cloned().unwrap_or_else(|| json!({}));
            RepairBroker::new(patches, &recipes, &prefixes, &protected)
        })().map_err(|e| format!("RuntimeError: Invalid trusted repair configuration: {}", e.message))?;
        Ok(Adapter {
            repo,
            host,
            workspace,
            state_dir,
            receipts_dir,
            ceiling,
            limits,
            workspace_id: env("PALETTE_WORKSPACE_ID").unwrap_or_else(|| "default".into()),
            registry,
            repair: Arc::new(repair),
            proc: None,
            frames: None,
            live: Arc::new(Mutex::new(Live::default())),
            hello: None,
            session_notice: None,
            preparation: None,
            bindings: (Vec::new(), Value::Null),
            connection_failure: None,
            kernel_costs: None,
            refresh_pending: None,
        })
    }

    fn epoch(&self) -> Value {
        self.hello.as_ref().map(|h| get(h, "epoch").clone()).unwrap_or(Value::Null)
    }

    fn send(&self, message: &Value) -> Result<(), Fault> {
        let mut live = self.live.lock().unwrap();
        let stdin = live.stdin.as_mut().ok_or_else(|| Fault::value("I/O operation on closed file."))?;
        let line = message.to_string() + "\n";
        stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).map_err(|e| patch::bare_os(&e))
    }

    fn receive(&self, deadline: Instant) -> Result<Value, Fault> {
        match &self.frames {
            Some(frames) => receive(frames, deadline),
            None => Err(Fault::new("Empty", "")),
        }
    }

    fn stop(&mut self) -> Value {
        let Some(mut child) = self.proc.take() else {
            return json!({"stopped": false, "returncode": null, "timed_out": false});
        };
        self.live.lock().unwrap().stdin = None;
        // The Rust owner gives its last snapshot up to 30s before killing it.
        let (code, timed_out) = retire(&mut child, Duration::from_secs(35));
        self.live.lock().unwrap().pid = None;
        self.frames = None;
        json!({"stopped": true, "returncode": code, "timed_out": timed_out})
    }

    fn alive(&mut self) -> bool {
        self.proc.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    fn prepare_session(&mut self) -> Result<Value, Fault> {
        eprintln!("{}", r#"{"phase": "preparation", "state": "preparing"}"#);
        let started = Instant::now();
        let record = preparation::prepare(Path::new(&self.repo), Path::new(&self.repo), None, Path::new(&self.host), false, None)?;
        let mut prep = Map::new();
        for key in ["state", "reused", "previous_state"] {
            if let Some(v) = record.get(key) {
                prep.insert(key.into(), v.clone());
            }
        }
        prep.insert("seconds_this_start".into(), json!(started.elapsed().as_secs_f64()));
        let compile = if patch::truthy(get(&record, "reused")) { json!(0.0) } else { get(&record, "seconds").clone() };
        prep.insert("compile_prepare_seconds".into(), compile);
        prep.insert("recorded_preparation_seconds".into(), get(&record, "seconds").clone());
        let identity = record.get("identity").ok_or_else(|| Fault::new("KeyError", "'identity'"))?;
        prep.insert("identity".into(), json!(util::sha256_hex(util::py_dumps(identity, None).as_bytes())));
        let prep = Value::Object(prep);
        self.preparation = Some(prep.clone());
        if get(&record, "state") != "prepared" {
            return Err(Fault::new("PreparationFailed", format!(
                "Palette could not prepare its Julia environment, so no call ran. \
                 palette_control action=prepare retries it once the cause is fixed. \
                 The preparation reported:\n{}", text_or_empty(get(&record, "error")))));
        }
        Ok(prep)
    }

    fn preparation_status(&self) -> Result<Value, Fault> {
        let julia = sandbox::resolve_julia().map_err(|e| Fault::new("RuntimeError", e))?;
        let identity = preparation::identity(Path::new(&self.repo), Path::new(&self.repo), &julia)?;
        Ok(preparation::status(&identity, None))
    }

    fn start(&mut self, prepared: bool) -> Result<(), Fault> {
        if self.proc.is_some() {
            if !self.alive() {
                self.stop();
                return Err(Fault::new("RuntimeError", "operator died; restoration has not been established"));
            }
            return Ok(());
        }
        if prepared {
            self.prepare_session()?;
        } else {
            let state = get(&self.preparation_status()?, "state").clone();
            self.preparation = Some(json!({"state": state, "automatic_preparation": false}));
        }
        for dir in [Path::new(&self.workspace), self.state_dir.as_path()] {
            fs::create_dir_all(dir).map_err(|e| Fault::os(&e, dir))?;
        }
        let mut command = vec![
            "session".to_string(), "--project-dir".into(), self.repo.clone(), "--repo-dir".into(), self.repo.clone(),
            "--workspace-dir".into(), self.workspace.clone(), "--state-dir".into(), self.state_dir.to_string_lossy().into_owned(),
            "--receipts-dir".into(), self.receipts_dir.to_string_lossy().into_owned(),
            "--ceiling".into(), util::py_dumps(&self.ceiling, None),
            "--turn-timeout".into(), transport::seconds_text(self.limits.0),
            "--max-turn-timeout".into(), transport::seconds_text(self.limits.1),
        ];
        if let Some(scratch) = env("PALETTE_SCRATCH_ROOT").filter(|s| !s.is_empty()) {
            command.extend(["--scratch-root".into(), scratch]);
        }
        let started = Instant::now();
        let mut child = Command::new(&self.host).args(&command).current_dir(&self.workspace)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit())
            .spawn().map_err(|e| Fault::os(&e, Path::new(&self.host)))?;
        self.frames = Some(pump(child.stdout.take().expect("piped stdout")));
        *self.live.lock().unwrap() = Live { stdin: child.stdin.take(), pid: Some(child.id() as libc::pid_t) };
        self.proc = Some(child);
        let hello = self.receive(started + Duration::from_secs(180)).and_then(|hello| {
            if get(&hello, "kind") != "HELLO" {
                return Err(Fault::new("RuntimeError", format!("operator startup failed: {}", util::py_dumps(&hello, None))));
            }
            Ok(hello)
        });
        let hello = match hello {
            Ok(hello) => hello,
            Err(e) => {
                self.stop();
                return Err(e);
            }
        };
        let startup = started.elapsed().as_secs_f64();
        let epoch = get(&hello, "epoch").clone();
        let revival = hello.get("revival").cloned().unwrap_or(Value::Bool(false));
        self.kernel_costs = Some(json!({"epoch": epoch, "host_startup_seconds": round3(startup), "first_call": null,
                                        "warm_calls": 0, "recent_warm_calls": []}).as_object().unwrap().clone());
        let mut notice = json!({"epoch": epoch, "host_startup_seconds": startup, "preparation": self.preparation,
                                "snapshot_present": revival,
                                "restoration": if patch::truthy(&revival) { "uncertain" } else { "fresh_empty_world" }});
        if let Some(failure) = &self.connection_failure {
            notice["previous_connection_failure"] = failure.clone();
            notice["recovery_warning"] = json!("Restoration describes the saved snapshot. Effects after that snapshot are not restored; their precise contents are unknown.");
        }
        self.session_notice = notice.as_object().cloned();
        self.hello = Some(hello);
        Ok(())
    }

    fn execute(&mut self, mut args: Map<String, Value>) -> Result<Map<String, Value>, Fault> {
        let full = match args.remove("view").unwrap_or_else(|| json!("quiet")).as_str() {
            Some("quiet") => false,
            Some("full") => true,
            _ => return Err(Fault::value("view must be quiet or full")),
        };
        let rid = util::hex(32);
        let ephemeral = args.get("ephemeral") == Some(&Value::Bool(true));
        let started = Instant::now();
        match self.receive_result(&rid, &args) {
            Ok(event) => {
                if !ephemeral {
                    self.note_call_cost(&event, started.elapsed().as_secs_f64());
                }
                if event.get("session_dead") == Some(&Value::Bool(true)) && !ephemeral {
                    self.connection_failure = Some(json!({"epoch": self.epoch(), "error": event.get("error"), "call": event.get("call")}));
                    // The CLI exits after its terminal result. Retire its pipes before
                    // the next independent call starts a session from the saved state.
                    self.stop();
                }
                Ok(project_result(&event, full, ephemeral, &mut self.bindings))
            }
            Err(e) => {
                self.connection_failure = Some(json!({"epoch": self.epoch(), "error": e.message, "completion": "unknown"}));
                self.stop();
                Err(e)
            }
        }
    }

    fn note_call_cost(&mut self, event: &Map<String, Value>, seconds: f64) {
        let (Some(kernel), Some(Value::Object(costs))) = (self.kernel_costs.as_mut(), event.get("costs")) else { return };
        let costs = Value::Object(costs.clone());
        let row = json!({"call": event.get("call"), "round_trip_seconds": round3(seconds),
                         "execution_seconds": get(&costs, "execution_seconds"), "compile_seconds": get(&costs, "compile_seconds")});
        if patch::truthy(get(&costs, "first_call_in_kernel")) {
            kernel.insert("first_call".into(), row);
            return;
        }
        let warm = kernel.get("warm_calls").and_then(Value::as_u64).unwrap_or(0) + 1;
        kernel.insert("warm_calls".into(), json!(warm));
        let mut recent = kernel.get("recent_warm_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        recent.push(row);
        let skip = recent.len().saturating_sub(WARM_CALLS_KEPT);
        kernel.insert("recent_warm_calls".into(), Value::Array(recent.split_off(skip)));
    }

    fn cost_status(&self) -> Value {
        let Some(kernel) = &self.kernel_costs else { return Value::Null };
        let mut status = Map::new();
        for key in ["epoch", "host_startup_seconds", "first_call", "warm_calls"] {
            status.insert(key.into(), kernel.get(key).cloned().unwrap_or(Value::Null));
        }
        let recent = kernel.get("recent_warm_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(last) = recent.last() {
            let mut medians = Map::new();
            for key in ["round_trip_seconds", "execution_seconds", "compile_seconds"] {
                let mut values: Vec<f64> = recent.iter().filter_map(|row| get(row, key).as_f64()).collect();
                if !values.is_empty() {
                    medians.insert(key.into(), json!(round3(median(&mut values))));
                }
            }
            status.insert("recent_warm_median".into(), Value::Object(medians));
            status.insert("last_warm_call".into(), last.clone());
        }
        status.insert("limits".into(), json!(format!(
            "Round trips are measured by this server, from request to result. The median covers the last {WARM_CALLS_KEPT} warm calls. \
             compile_seconds counts compilation by any task in the kernel during the call.")));
        Value::Object(status)
    }

    /// A kernel's `palette.runtime` request during a call.
    fn host_service(&mut self, data: &Value, ephemeral: bool, id: &Value) -> Result<Value, Fault> {
        let action = get(data, "action");
        let empty = json!({});
        let payload = match data.get("payload").unwrap_or(&empty) {
            Value::Object(p) => p,
            _ => return Err(Fault::value("palette.runtime payload must be an object")),
        };
        if action == "refresh" && ephemeral {
            return Err(Fault::permission("runtime refresh is unavailable to disposable workers"));
        }
        let mut result = if action == "repair" {
            self.repair.call(payload, &self.workspace_id, &Map::new(), &mut NoClient)?
        } else {
            let epoch = self.epoch();
            self.registry.handle(action, payload, &epoch)?
        };
        if action == "refresh" {
            let Value::Object(fields) = &mut result else {
                return Err(Fault::new("TypeError", format!("'{}' object does not support item assignment", util::py_type_name(&result))));
            };
            fields.insert("deferred".into(), json!(true));
            fields.insert("snapshot".into(), json!("pending_current_turn_completion"));
            let previous = fields.get("previous_generation").cloned().ok_or_else(|| Fault::new("KeyError", "'previous_generation'"))?;
            self.refresh_pending = json!({"request_id": id, "requested": result.clone(), "previous_epoch": self.epoch(),
                                          "previous_generation": previous}).as_object().cloned();
        }
        Ok(result)
    }

    fn receive_result(&mut self, rid: &str, args: &Map<String, Value>) -> Result<Map<String, Value>, Fault> {
        let mut request = Map::new();
        request.insert("request_id".into(), json!(rid));
        request.extend(args.clone());
        self.send(&Value::Object(request))?;
        // The host stops a call that overruns its limit by 12 s; the rest is
        // for host requests answered during the call.
        let deadline = Instant::now() + Duration::from_secs_f64(transport::call_limit(args, self.limits) + 30.0);
        let ephemeral = args.get("ephemeral") == Some(&Value::Bool(true));
        loop {
            let event = self.receive(deadline)?;
            if get(&event, "event") == "host_request" {
                let empty = json!({});
                let data = event.get("data").unwrap_or(&empty);
                let id = get(&event, "id").clone();
                let reply = if data.is_object() && get(data, "type") == "palette.runtime" {
                    match self.host_service(data, ephemeral, &id) {
                        Ok(result) => json!({"status": "ok", "result": result}),
                        Err(e) => json!({"status": "error", "error": e.to_string()}),
                    }
                } else {
                    json!({"status": "error", "error": "This Palette operator has no host service for that request type."})
                };
                self.send(&json!({"host_reply": id, "reply": reply}))?;
                continue;
            }
            let Value::Object(mut event) = event else { continue };
            if event.get("request_id") != Some(&json!(rid)) {
                continue;
            }
            let observed = self.hello.as_ref().is_some_and(|h| patch::truthy(get(get(h, "capabilities"), "revival_observation_v1")));
            if let (true, Some(Value::Object(revival))) = (observed, event.get("revival")) {
                if let Some(notice) = &mut self.session_notice {
                    notice.insert("restoration".into(), revival.get("state").cloned().unwrap_or(Value::Null));
                }
            }
            if let Some(notice) = self.session_notice.take() {
                event.insert("session".into(), Value::Object(notice));
                self.connection_failure = None;
            }
            if self.refresh_pending.is_some() {
                event = self.complete_runtime_refresh(event)?;
            }
            return Ok(event);
        }
    }

    fn snapshot_evidence(&self, call: i64) -> Result<Map<String, Value>, Fault> {
        let manifest_path = self.state_dir.join("manifest.json");
        let last_call_path = self.state_dir.join("last_call");
        let mut result = json!({"manifest_present": manifest_path.is_file(), "manifest_call": null,
                                "last_completed_call": null, "workspace_matches": false}).as_object().unwrap().clone();
        if !manifest_path.is_file() {
            return Ok(result);
        }
        let read_text = |path: &Path| -> Result<(Vec<u8>, String), Fault> {
            let bytes = fs::read(path).map_err(|e| Fault::os(&e, path))?;
            let text = std::str::from_utf8(&bytes).map_err(|e| Fault::new("UnicodeDecodeError", util::py_utf8_error(&bytes, &e)))?.to_string();
            Ok((bytes, text))
        };
        let attempt = (|| -> Result<Result<(), Fault>, Fault> {
            let (bytes, text) = match read_text(&manifest_path) {
                Ok(read) => read,
                Err(e) => return Ok(Err(e)),
            };
            let manifest = match super::py_json_loads(&text) {
                Ok(m) => m,
                Err(e) => return Ok(Err(e)),
            };
            // Not an object: the AttributeError escapes, as it did in Python.
            let Value::Object(manifest) = manifest else { return Err(no_get(&manifest)) };
            let saved_call = manifest.get("call").cloned().unwrap_or(Value::Null);
            result.insert("manifest_call".into(), saved_call.clone());
            let workspace = util::resolve(Path::new(&self.workspace)).to_string_lossy().into_owned();
            result.insert("workspace_matches".into(), json!(manifest.get("workspace") == Some(&json!(workspace))));
            result.insert("manifest_sha256".into(), json!(util::sha256_hex(&bytes)));
            let last = if last_call_path.is_file() {
                match read_text(&last_call_path) {
                    Ok((_, t)) => t.trim().to_string(),
                    Err(e) => return Ok(Err(e)),
                }
            } else {
                String::new()
            };
            let last = (!last.is_empty() && last.bytes().all(|b| b.is_ascii_digit())).then(|| last.parse::<i64>().ok()).flatten();
            result.insert("last_completed_call".into(), json!(last));
            let saved = saved_call.as_i64().or_else(|| saved_call.as_u64().map(|_| i64::MAX));
            let covers = saved.is_some_and(|s| s >= call) && last.is_none_or(|l| l >= call);
            result.insert("covers_completed_turn".into(), json!(covers));
            Ok(Ok(()))
        })()?;
        if let Err(e) = attempt {
            result.insert("read_error".into(), json!(e.to_string()));
            result.insert("covers_completed_turn".into(), json!(false));
        }
        Ok(result)
    }

    fn revive_after_refresh(&mut self, pending: &Map<String, Value>, stopped: &Value, snapshot: &Map<String, Value>,
                            call: &Value, before_epoch: &Value) -> Result<(Map<String, Value>, bool), Fault> {
        let committed = get(stopped, "returncode") == 0 && get(stopped, "timed_out") != true
            && snapshot.get("covers_completed_turn") == Some(&json!(true)) && snapshot.get("workspace_matches") == Some(&json!(true));
        let commit = if committed { Some(self.registry.commit_refresh()?) } else { None };
        if commit.is_none() {
            self.registry.cancel_refresh();
        }
        self.start(false)?;
        let revived = self.execute(json!({"code": "nothing", "view": "full"}).as_object().unwrap().clone())?;
        let revival = revived.get("revival").cloned().unwrap_or(Value::Null);
        let after_epoch = self.epoch();
        let saved_call = if revival.is_object() { get(&revival, "saved_call").clone() } else { Value::Null };
        let observed = match (saved_call.as_i64(), call.as_i64()) {
            (Some(saved), Some(call)) => saved >= call,
            _ => false,
        };
        let requested = pending.get("requested").cloned().unwrap_or(Value::Null);
        let key = |v: &Value| v.get("capability_generation").cloned().ok_or_else(|| Fault::new("KeyError", "'capability_generation'"));
        let success = match &commit {
            Some(commit) if committed => {
                key(commit)? == key(&requested)? && !after_epoch.is_null() && after_epoch != *before_epoch && observed
            }
            _ => false,
        };
        let mut refresh = requested.as_object().cloned().unwrap_or_default();
        if let Some(Value::Object(commit)) = commit {
            refresh.extend(commit);
        }
        let mut snap = snapshot.clone();
        snap.insert("stop_returncode".into(), get(stopped, "returncode").clone());
        snap.insert("stop_timed_out".into(), get(stopped, "timed_out").clone());
        for (key, value) in [("deferred", json!(false)), ("success", json!(success)), ("snapshot", Value::Object(snap)),
                             ("previous_epoch", before_epoch.clone()), ("epoch", after_epoch), ("revival", revival),
                             ("observed_saved_call", saved_call)] {
            refresh.insert(key.into(), value);
        }
        Ok((refresh, success))
    }

    fn complete_runtime_refresh(&mut self, mut turn: Map<String, Value>) -> Result<Map<String, Value>, Fault> {
        let pending = self.refresh_pending.take().unwrap_or_default();
        let before_epoch = pending.get("previous_epoch").cloned().unwrap_or(Value::Null);
        let call = turn.get("call").cloned().unwrap_or(Value::Null);
        let stopped = self.stop();
        let snapshot = match call.as_i64() {
            Some(call) => self.snapshot_evidence(call)?,
            None => json!({"covers_completed_turn": false, "manifest_present": false}).as_object().unwrap().clone(),
        };
        let previous_error = text_or_empty(turn.get("error").unwrap_or(&Value::Null));
        match self.revive_after_refresh(&pending, &stopped, &snapshot, &call, &before_epoch) {
            Ok((mut refresh, success)) => {
                if !success {
                    refresh.insert("error".into(), json!("snapshot commit or revival of the completed turn was not established"));
                    turn.insert("success".into(), json!(false));
                    let error = previous_error + " Runtime refresh did not establish snapshot and revival; inspect runtime_refresh evidence.";
                    turn.insert("error".into(), json!(error.trim()));
                }
                turn.insert("runtime_refresh".into(), Value::Object(refresh));
                Ok(turn)
            }
            Err(e) => {
                self.registry.cancel_refresh();
                self.connection_failure = Some(json!({"epoch": before_epoch, "error": e.message, "completion": "runtime_refresh_uncertain"}));
                turn.insert("success".into(), json!(false));
                let mut refresh = get(&Value::Object(pending), "requested").as_object().cloned().unwrap_or_default();
                let mut snap = snapshot;
                snap.insert("stop_returncode".into(), get(&stopped, "returncode").clone());
                snap.insert("stop_timed_out".into(), get(&stopped, "timed_out").clone());
                for (key, value) in [("deferred", json!(false)), ("success", json!(false)), ("snapshot", Value::Object(snap)),
                                     ("previous_epoch", before_epoch), ("epoch", self.epoch()), ("error", json!(e.to_string()))] {
                    refresh.insert(key.into(), value);
                }
                turn.insert("runtime_refresh".into(), Value::Object(refresh));
                let error = previous_error + " Runtime refresh recovery is uncertain; inspect runtime_refresh evidence.";
                turn.insert("error".into(), json!(error.trim()));
                self.stop();
                Ok(turn)
            }
        }
    }

    fn control(&mut self, args: &Value) -> Result<Value, Fault> {
        let Value::Object(args) = args else {
            return Err(Fault::new("TypeError", format!("argument of type '{}' is not iterable", util::py_type_name(args))));
        };
        if ["restore", "prepare", "fork"].iter().any(|k| args.get(*k).is_some_and(|v| !v.is_boolean())) {
            return Err(Fault::value("restore, prepare and fork must be booleans"));
        }
        let action = args.get("action").ok_or_else(|| Fault::new("KeyError", "'action'"))?;
        match action.as_str() {
            Some("status") => {
                let record = self.preparation_status()?;
                let mut preparation = Map::new();
                for key in ["state", "seconds", "error", "reason"] {
                    if let Some(v) = record.get(key).filter(|v| !v.is_null()) {
                        preparation.insert(key.into(), v.clone());
                    }
                }
                let connected = self.alive();
                Ok(json!({"connected": connected, "epoch": self.epoch(), "state_dir": self.state_dir.to_string_lossy(),
                          "preparation": preparation, "costs": self.cost_status()}))
            }
            Some("prepare") => self.prepare_session(),
            Some("checkpoints") => Ok(time_machine::list_checkpoints(&self.state_dir)),
            Some("receipts") => {
                // Call numbers restart in every kernel: call n means this kernel's unless an epoch is named.
                let epoch = self.hello.as_ref().map(|_| self.epoch());
                receipts::query(&self.receipts_dir, args, epoch.as_ref())
            }
            Some(action @ ("restart" | "restore")) => {
                if action == "restore" {
                    // The next kernel saves the world it leaves, then revives the checkpoint.
                    time_machine::request_restore(&self.state_dir, args.get("checkpoint"), args.get("fork"))?;
                }
                let previous_epoch = self.epoch();
                self.stop();
                let mut retired = Value::Null;
                if action == "restart" && args.get("restore") == Some(&json!(false)) && self.state_dir.exists() {
                    retired = json!(time_machine::retire_world(&self.state_dir)?.to_string_lossy());
                }
                self.start(args.get("prepare") != Some(&json!(false)))?;
                let mut result = self.execute(json!({"code": "nothing"}).as_object().unwrap().clone())?;
                result.insert("previous_epoch".into(), previous_epoch);
                result.insert("retired_snapshot".into(), retired);
                Ok(Value::Object(result))
            }
            _ => Err(Fault::value("unknown control action")),
        }
    }

    fn tool_failure(&self, e: &Fault) -> Value {
        if e.kind == "PreparationFailed" {
            return text_result(&json!({"success": false, "error": e.message}), true);
        }
        let mut event = json!({"success": false,
            "error": format!("{e}. Check connection status and restoration evidence; work may not have completed.")});
        if let Some(failure) = &self.connection_failure {
            event["connection_failure"] = failure.clone();
        }
        text_result(&event, true)
    }

    fn tool_call(&mut self, params: &Value, caller: Option<&str>) -> Result<Value, Fault> {
        let empty = json!({});
        let args = params.get("arguments").unwrap_or(&empty);
        let name = params.get("name").ok_or_else(|| Fault::new("KeyError", "'name'"))?;
        let event = match name.as_str() {
            Some("palette_control") => self.control(args)?,
            Some("palette") => {
                let mut args = transport::normalize_palette_call(args).map_err(|e| Fault::new("SourceTransportError", e))?;
                if let (Some(caller), false) = (caller, args.contains_key("caller")) {
                    args.insert("caller".into(), json!(caller));
                }
                self.start(true)?;
                Value::Object(self.execute(args)?)
            }
            _ => return Err(Fault::value("expected a Palette tool and valid arguments")),
        };
        let is_error = event.get("success") == Some(&json!(false));
        Ok(text_result(&event, is_error))
    }

    fn tools(&self) -> Value {
        json!({"tools": [
            {"name": "palette", "description": format!("{PALETTE_DESCRIPTION}{}", transport::PALETTE_GUIDE),
             "inputSchema": palette_schema(self.limits)},
            {"name": "palette_control", "description": format!("{CONTROL_DESCRIPTION}{}", transport::CONTROL_GUIDE),
             "inputSchema": control_schema(&CONTROL_ACTIONS)},
        ]})
    }

    fn handle(&mut self, raw: &str, caller: &mut Option<String>) -> Result<Option<Value>, (Value, Fault)> {
        let msg = super::py_json_loads(raw).map_err(|e| (Value::Null, e))?;
        if !msg.is_object() {
            return Err((Value::Null, no_get(&msg)));
        }
        let mid = get(&msg, "id").clone();
        let fail = |e: Fault| (mid.clone(), e);
        let method = match msg.get("method") {
            None => "",
            Some(Value::String(m)) => m.as_str(),
            Some(other) => return Err(fail(patch::no_attribute(other, "startswith"))),
        };
        if method.starts_with("notifications/") {
            return Ok(None);
        }
        let result = match method {
            "initialize" => {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                if !params.is_object() {
                    return Err(fail(no_get(&params)));
                }
                let info = params.get("clientInfo").filter(|v| patch::truthy(v)).cloned().unwrap_or_else(|| json!({}));
                if !info.is_object() {
                    return Err(fail(no_get(&info)));
                }
                *caller = provisioning::agent_name(info.get("name"));
                let version = provisioning::palette_version(Path::new(&self.repo)).map_err(|e| fail(Fault::new("OSError", e)))?;
                json!({"protocolVersion": params.get("protocolVersion").cloned().unwrap_or_else(|| json!("2024-11-05")),
                       "capabilities": {"tools": {}}, "serverInfo": {"name": "palette", "version": version}})
            }
            "ping" => json!({}),
            "tools/list" => self.tools(),
            "tools/call" => {
                let params = msg.get("params").ok_or_else(|| fail(Fault::new("KeyError", "'params'")))?;
                if !params.is_object() {
                    return Err(fail(no_get(params)));
                }
                self.tool_call(params, caller.as_deref()).unwrap_or_else(|e| self.tool_failure(&e))
            }
            _ => return Ok(Some(json!({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Unknown method"}}))),
        };
        Ok(Some(json!({"jsonrpc": "2.0", "id": mid, "result": result})))
    }
}

/// `palette-host mcp child`. Returns the exit code.
pub(crate) fn main() -> i32 {
    let mut adapter = match Adapter::new() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("palette-host mcp child: {e}");
            return 1;
        }
    };
    let live = Arc::clone(&adapter.live);
    exit_on_signal(move || {
        let (stdin, pid) = {
            let mut live = live.lock().unwrap();
            (live.stdin.take(), live.pid)
        };
        drop(stdin);
        if let Some(pid) = pid {
            retire_pid(pid, Duration::from_secs(35));
        }
    });
    // The router names the agent on each call; a client that talks to this
    // adapter directly is named from its own initialize.
    let mut caller = None;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let raw = String::from_utf8_lossy(&buf);
        match adapter.handle(&raw, &mut caller) {
            Ok(Some(frame)) => emit(&frame),
            Ok(None) => {}
            Err((mid, e)) => emit(&json!({"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": e.message}})),
        }
    }
    adapter.stop();
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn quiet_replies_carry_binding_changes_and_drop_empty_fields() {
        let mut view = (Vec::new(), Value::Null);
        let first = obj(json!({"epoch": "e1", "bindings": ["x (Int64)", "y (String)"], "data": 1, "display": "1",
                               "error": null, "output": "", "costs": {"a": 1}, "interrupted": false, "request_id": "r"}));
        let quiet = project_result(&first, false, false, &mut view);
        assert_eq!(Value::Object(quiet), json!({"epoch": "e1", "data": 1,
            "binding_changes": {"new_or_changed": ["x (Int64)", "y (String)"], "no_longer_listed": []}}));
        let second = obj(json!({"epoch": "e1", "bindings": ["x (Float64)"], "success": false, "costs": {"a": 1}}));
        let quiet = project_result(&second, false, false, &mut view);
        assert_eq!(Value::Object(quiet), json!({"epoch": "e1", "success": false, "costs": {"a": 1},
            "binding_changes": {"new_or_changed": ["x (Float64)"], "no_longer_listed": ["y"]}}));
        // A new epoch starts the view over; full keeps everything.
        let third = obj(json!({"epoch": "e2", "bindings": ["x (Float64)"], "request_id": "r"}));
        assert_eq!(project_result(&third, true, false, &mut view), third);
        assert_eq!(view.1, json!("e2"));
        let unchanged = project_result(&third, false, false, &mut view);
        assert_eq!(Value::Object(unchanged), json!({"epoch": "e2"}));
    }

    #[test]
    fn a_full_inventory_says_it_may_be_cut() {
        let rows: Vec<Value> = (0..200).map(|i| json!(format!("v{i} (Int64)"))).collect();
        let mut view = (Vec::new(), Value::Null);
        let quiet = project_result(&obj(json!({"epoch": "e", "bindings": rows})), false, false, &mut view);
        assert!(quiet["binding_observation"].as_str().unwrap().starts_with("Inventory lists at most 200"));
        // Ephemeral calls leave the view alone.
        let mut fresh = (Vec::new(), Value::Null);
        let scratch = project_result(&obj(json!({"epoch": "e", "bindings": ["a (Int64)"]})), false, true, &mut fresh);
        assert_eq!(scratch.get("bindings"), Some(&json!(["a (Int64)"])));
        assert!(fresh.0.is_empty());
    }

    #[test]
    fn control_schema_lists_receipt_query_fields() {
        let schema = control_schema(&CONTROL_ACTIONS);
        assert_eq!(schema["properties"]["action"]["enum"], json!(CONTROL_ACTIONS));
        assert!(schema["properties"].get("checkpoint").is_some());
        assert!(schema["properties"].get("limit").is_some());
        assert_eq!(schema["required"], json!(["action"]));
    }

    #[test]
    fn medians_and_rounding_follow_python() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), 2.5);
        assert_eq!(round3(1.23456), 1.235);
        assert_eq!(text_or_empty(&Value::Null), "");
        assert_eq!(text_or_empty(&json!("x")), "x");
    }

    #[test]
    fn receipts_dir_sits_beside_the_state_dir() {
        assert_eq!(receipts_dir(Path::new("/a/state")), PathBuf::from("/a/state.receipts"));
    }
}
