//! One persistent sandboxed worker (runtime/scripts/session_loop.jl) with its own
//! broker, ceiling and depot clone, kept alive across calls. Its epoch comes
//! from the worker's own first line and dies with it: a dead worker is
//! reported, never silently replaced, because a new process is a different
//! session.

use crate::broker::{self, Broker, BrokerConfig, BrokerServer};
use crate::sandbox::{self, SandboxSpec};
use crate::util;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How much longer than the call limit the host waits for a reply: the
/// worker's own interrupt grace plus slack. Only compute that never yields
/// gets here.
const HARD_TIMEOUT_MARGIN_S: f64 = 12.0;

#[derive(Debug)]
pub enum SessionError {
    /// The worker is gone; nothing after this can be served.
    Dead(String),
    Protocol(String),
}

pub struct SessionOptions {
    pub project_dir: String,
    pub repo_dir: String,
    pub ceiling: Value,
    pub network_enabled: bool,
    pub receipts_dir: Option<String>,
    /// This session's own scratch root, created here and deleted on close. Default ~/.palette-sessions/<id>.
    pub scratch_root: Option<String>,
    pub turn_timeout: f64,
    pub task_workspace_dir: Option<String>,
    pub startup_timeout: f64,
    pub state_dir: Option<String>,
}

pub struct Turn<'a> {
    pub code: &'a str,
    pub ephemeral: bool,
    pub ephemeral_ceiling: Option<Value>,
    pub payload: Option<Value>,
    pub include_map: bool,
    pub digest: bool,
}

pub struct PaletteSession {
    pub session_id: String,
    pub epoch: String,
    pub capabilities: Value,
    pub broker: Arc<Broker>,
    turn_timeout: f64,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Option<String>>,
    stderr_tail: Arc<Mutex<String>>,
    next_request_id: u64,
    server: Option<BrokerServer>,
    cleanser: PaletteCleanser,
    closed: bool,
    /// Set from outside (a signal) to give up waiting on the worker.
    pub stop: Arc<AtomicBool>,
}

/// Scrubs a session's per-run residue (depot clone, tmp root, socket dir)
/// between courses without touching the caller's workspace or durable state.
#[derive(Default)]
struct PaletteCleanser {
    depot_clone: Option<PathBuf>,
    tmp_root: Option<PathBuf>,
    short_sock_dir: Option<PathBuf>,
}

impl PaletteCleanser {
    fn cleanse(&mut self) {
        for p in [self.depot_clone.take(), self.tmp_root.take(), self.short_sock_dir.take()].into_iter().flatten() {
            let _ = fs::remove_dir_all(p);
        }
    }
}

enum Wait {
    Stopped,
    TimedOut,
}

fn dead(msg: impl Into<String>) -> SessionError {
    SessionError::Dead(msg.into())
}

impl PaletteSession {
    pub fn start(o: SessionOptions, stop: Arc<AtomicBool>) -> Result<PaletteSession, SessionError> {
        let session_id = format!("palette-{}", util::hex(16));
        let mut cleanser = PaletteCleanser::default();
        match Self::start_inner(&o, &session_id, &mut cleanser, stop) {
            Ok(s) => Ok(s),
            Err(e) => {
                // A failure anywhere in startup must not leak the depot clone,
                // the broker's socket or the scratch directories.
                cleanser.cleanse();
                Err(e)
            }
        }
    }

    fn start_inner(o: &SessionOptions, session_id: &str, cleanser: &mut PaletteCleanser, stop: Arc<AtomicBool>)
        -> Result<PaletteSession, SessionError> {
        let err = |e: String| dead(e);
        let tmp_root = o.scratch_root.as_deref().map(PathBuf::from).unwrap_or_else(|| util::home().join(".palette-sessions").join(session_id));
        let task_workspace = match o.task_workspace_dir.as_deref() {
            Some(t) => {
                let tw = fs::canonicalize(t).map_err(|e| err(format!("task workspace {t}: {e}")))?;
                if !tw.is_dir() {
                    return Err(err(format!("task workspace is not a directory: {}", tw.display())));
                }
                Some(tw)
            }
            None => None,
        };
        let state_dir = o.state_dir.as_deref().map(|p| fs::canonicalize(p).map_err(|e| err(e.to_string()))).transpose()?;
        let mut package_protection = vec![
            PathBuf::from(&o.project_dir), PathBuf::from(&o.repo_dir), tmp_root.clone(),
        ];
        for path in [o.state_dir.as_deref(), o.task_workspace_dir.as_deref(), o.receipts_dir.as_deref()].into_iter().flatten() {
            package_protection.push(PathBuf::from(path));
        }
        let package_store = sandbox::package_store_paths(&package_protection, Some(&o.ceiling)).map_err(err)?;
        if let Some(state) = state_dir.as_deref() {
            if !state.is_dir() {
                return Err(err(format!("state directory is not a directory: {}", state.display())));
            }
        }
        let mut protected_layout = Vec::new();
        if let Some(tw) = task_workspace.as_deref() { protected_layout.push(("task workspace", tw)); }
        if let Some(state) = state_dir.as_deref() { protected_layout.push(("persistent state directory", state)); }
        protected_layout.push(("persistent package store", package_store.root.as_path()));
        // Resolve the future scratch path before creating it so rejecting a
        // caller-owned workspace or durable state cannot transfer it to recursive cleanup.
        let planned_root = util::resolve(&tmp_root);
        if !protected_layout.is_empty() {
            sandbox::check_writable_mounts_disjoint(
                &[("session scratch root", planned_root.as_path())],
                &protected_layout,
            ).map_err(err)?;
        }
        let tmp_root_existed = fs::symlink_metadata(&tmp_root).is_ok();
        fs::create_dir_all(&tmp_root).map_err(|e| err(e.to_string()))?;
        if !protected_layout.is_empty() {
            let root = match fs::canonicalize(&tmp_root) {
                Ok(root) => root,
                Err(e) => {
                    if !tmp_root_existed && util::resolve(&tmp_root) == planned_root {
                        let _ = fs::remove_dir(&planned_root);
                    }
                    return Err(err(e.to_string()));
                }
            };
            if let Err(e) = sandbox::check_writable_mounts_disjoint(
                &[("session scratch root", root.as_path())],
                &protected_layout,
            ) {
                // Only remove a new, still-empty directory at the path we
                // resolved before creation; never recurse into caller data.
                if !tmp_root_existed && root == util::resolve(&tmp_root) {
                    let _ = fs::remove_dir(&root);
                }
                return Err(err(e));
            }
        }
        cleanser.tmp_root = Some(tmp_root.clone());
        let workspace = match task_workspace {
            Some(tw) => tw,
            None => {
                let w = tmp_root.join("workspace");
                fs::create_dir_all(&w).map_err(|e| err(e.to_string()))?;
                w
            }
        };
        let workspace = workspace.to_string_lossy().into_owned();

        let depot_clone = sandbox::create_session_depot(None).map_err(err)?;
        cleanser.depot_clone = Some(depot_clone.clone());

        // A Unix socket path is limited to 108 bytes; under a long HOME the
        // socket gets a short private directory of its own.
        let broker_root = tmp_root.join("broker");
        fs::create_dir_all(&broker_root).map_err(|e| err(e.to_string()))?;
        let sock_dir = if broker_root.join("broker.sock").as_os_str().len() > 100 {
            let d = util::mkdtemp("nj-").map_err(|e| err(e.to_string()))?;
            cleanser.short_sock_dir = Some(d.clone());
            d
        } else {
            broker_root.clone()
        };
        let receipts_root = o.receipts_dir.as_deref().map(PathBuf::from).unwrap_or(broker_root.clone());
        fs::create_dir_all(&receipts_root).map_err(|e| err(e.to_string()))?;
        let private_broker = fs::canonicalize(&broker_root).map_err(|e| err(e.to_string()))?;
        let private_receipts = fs::canonicalize(&receipts_root).map_err(|e| err(e.to_string()))?;
        let writable_workspace = fs::canonicalize(&workspace).map_err(|e| err(e.to_string()))?;
        let writable_state = state_dir;
        let mut writable = vec![("workspace", writable_workspace.as_path())];
        if let Some(state) = writable_state.as_deref() { writable.push(("state directory", state)); }
        sandbox::check_writable_mounts_disjoint(&writable,
            &[("private broker", private_broker.as_path()), ("broker receipts", private_receipts.as_path()),
              ("persistent package store", package_store.root.as_path())]).map_err(err)?;
        let receipts = receipts_root.join("receipts.jsonl");
        let broker = Arc::new(Broker::new(BrokerConfig {
            ceiling: o.ceiling.clone(),
            receipt_log_path: receipts,
            session_id: session_id.to_string(),
            package_depot_dir: Some(package_store.depot.to_string_lossy().into_owned()),
            package_environment_dir: Some(package_store.environment.to_string_lossy().into_owned()),
            project_dir: Some(o.project_dir.clone()),
            repo_dir: Some(o.repo_dir.clone()),
            // Leaves the child's own shutdown and the reply time to land inside the call.
            child_timeout: Duration::from_secs_f64((o.turn_timeout - 10.0).max(5.0)),
        }).map_err(err)?);
        let server = broker::serve(&sock_dir.join("broker.sock"), broker.clone()).map_err(err)?;

        let julia = sandbox::resolve_julia().map_err(err)?;
        let depot = sandbox::default_depot();
        let sock_dir_s = sock_dir.to_string_lossy().into_owned();
        let depot_clone_s = depot_clone.to_string_lossy().into_owned();
        let mut argv = sandbox::build_bwrap_argv(&SandboxSpec {
            workspace_dir: &workspace,
            broker_socket_dir: Some(&sock_dir_s),
            project_dir: &o.project_dir,
            repo_dir: &o.repo_dir,
            julia_bin: &julia,
            julia_depot: &depot,
            network_enabled: o.network_enabled,
            depot_clone_dir: Some(&depot_clone_s),
            package_store_root: Some(package_store.root.to_str().ok_or_else(|| err("package store path is not valid UTF-8".into()))?),
            package_depot_dir: Some(package_store.depot.to_str().ok_or_else(|| err("package depot path is not valid UTF-8".into()))?),
            package_environment_dir: Some(package_store.environment.to_str().ok_or_else(|| err("package environment path is not valid UTF-8".into()))?),
            state_dir: o.state_dir.as_deref(),
        }).map_err(err)?;
        let mut setenv = |k: &str, v: String| argv.extend(["--setenv".to_string(), k.to_string(), v]);
        setenv("PALETTE_PROVENANCE_DIR", "/run/palette/home/.palette".into());
        setenv("PALETTE_TURN_TIMEOUT", util::fmt_g(o.turn_timeout));
        for name in ["PALETTE_STATE_MAX_BINDING_BYTES", "PALETTE_STATE_MAX_BYTES"] {
            if let Ok(v) = std::env::var(name) {
                setenv(name, v);
            }
        }
        let loop_script = Path::new(&o.repo_dir).join("runtime/scripts").join("session_loop.jl");
        argv.extend(["--".into(), julia, "--startup-file=no".into(), loop_script.to_string_lossy().into_owned()]);

        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| err(format!("starting the worker: {e}")))?;
        let stdin = child.stdin.take();
        // Nothing else reads the worker's stderr; left undrained, 64 KiB of
        // warnings fills the pipe and blocks the worker mid-call.
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        {
            let tail = stderr_tail.clone();
            let mut e = child.stderr.take().unwrap();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = e.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut t = tail.lock().unwrap();
                    t.push_str(&String::from_utf8_lossy(&buf[..n]));
                    let keep = util::tail(&t, 8000);
                    *t = keep;
                }
            });
        }
        // Worker lines arrive on their own thread so every wait has a deadline.
        // Output is arbitrary bytes; invalid UTF-8 is replaced, never fatal.
        let (tx, lines) = mpsc::channel();
        {
            let out = child.stdout.take().unwrap();
            std::thread::spawn(move || {
                let mut r = BufReader::new(out);
                loop {
                    let mut buf = Vec::new();
                    match r.read_until(b'\n', &mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(None);
                            break;
                        }
                        Ok(_) => {
                            if tx.send(Some(String::from_utf8_lossy(&buf).into_owned())).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
        let mut s = PaletteSession {
            session_id: session_id.to_string(),
            epoch: String::new(),
            capabilities: json!({}),
            broker,
            turn_timeout: o.turn_timeout,
            child,
            stdin,
            lines,
            stderr_tail,
            next_request_id: 0,
            server: Some(server),
            cleanser: std::mem::take(cleanser),
            closed: false,
            stop,
        };
        let hello = match s.read_line(Duration::from_secs_f64(o.startup_timeout)) {
            Ok(Some(l)) => l,
            Ok(None) => {
                let _ = s.child.wait();
                std::thread::sleep(Duration::from_millis(200));
                let tail = util::tail(&s.stderr_tail.lock().unwrap(), 2000);
                s.teardown();
                return Err(dead(format!("worker exited before HELLO: {tail}")));
            }
            Err(_) => {
                let _ = s.child.kill();
                let _ = s.child.wait();
                s.teardown();
                return Err(dead(format!("worker did not produce HELLO within {}s", util::fmt_g(o.startup_timeout))));
            }
        };
        let parsed: Value = serde_json::from_str(hello.trim()).unwrap_or(Value::Null);
        match (parsed.get("kind").and_then(Value::as_str), parsed.get("epoch").and_then(Value::as_str)) {
            (Some("HELLO"), Some(epoch)) => s.epoch = epoch.to_string(),
            _ => {
                s.teardown();
                return Err(SessionError::Protocol(format!("malformed HELLO: {:?}", hello.trim_end())));
            }
        }
        s.capabilities = parsed.get("capabilities").cloned().unwrap_or_else(|| json!({}));
        Ok(s)
    }

    /// One line from the worker within `timeout`: Ok(None) on EOF,
    /// Err(Stopped) when asked to stop, Err(TimedOut) at the deadline.
    fn read_line(&self, timeout: Duration) -> Result<Option<String>, Wait> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Err(Wait::Stopped);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Wait::TimedOut);
            }
            match self.lines.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(l) => return Ok(l),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(None),
            }
        }
    }

    pub fn turn(&mut self, t: Turn) -> Result<Value, SessionError> {
        if self.closed {
            return Err(dead(format!("session {} was explicitly closed", self.session_id)));
        }
        if let Ok(Some(st)) = self.child.try_wait() {
            return Err(dead(format!("session {} worker process exited (code {}) since epoch {} was issued",
                                    self.session_id, st.code().unwrap_or(-1), self.epoch)));
        }
        self.next_request_id += 1;
        let request_id = self.next_request_id.to_string();
        let mut req = json!({"request_id": request_id, "kind": if t.ephemeral { "EPHEMERAL" } else { "EXECUTE" },
                             "code": t.code, "timeout_s": self.turn_timeout});
        if t.ephemeral {
            if let Some(c) = t.ephemeral_ceiling {
                req["ceiling"] = c;
            }
        }
        if let Some(p) = t.payload {
            req["payload"] = p;
        }
        if t.include_map {
            req["map"] = json!(true);
        }
        if !t.digest {
            req["digest"] = json!(false);
        }
        let write = self.stdin.as_mut().ok_or_else(|| dead("stdin closed")).and_then(|w| {
            writeln!(w, "{req}").and_then(|_| w.flush()).map_err(|e| dead(format!("session {} pipe broke: {e}", self.session_id)))
        });
        write?;
        let line = match self.read_line(Duration::from_secs_f64(self.turn_timeout + HARD_TIMEOUT_MARGIN_S)) {
            Ok(l) => l,
            // The host is shutting down: the teardown that follows lets the
            // kernel save its state, as it would at any other exit.
            Err(Wait::Stopped) => return Err(dead("the session host is shutting down")),
            Err(Wait::TimedOut) => {
                // Killed, not left pending: its eventual answer would be read
                // as the next call's.
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(dead(format!(
                    "The call exceeded its {}s limit and did not respond to the interrupt: compute that never yields cannot be interrupted, so the kernel was stopped. Every binding is gone; files written to the workspace remain.",
                    util::fmt_g(self.turn_timeout))));
            }
        };
        let Some(line) = line else {
            let st = self.child.wait().ok();
            std::thread::sleep(Duration::from_millis(200));
            let tail = util::tail(&self.stderr_tail.lock().unwrap(), 2000).trim().to_string();
            let code = st.and_then(|s| s.code()).map(|c| c.to_string()).unwrap_or_else(|| "None".into());
            return Err(dead(format!("session {} kernel process exited during the turn (exit code {code}){}",
                                    self.session_id, if tail.is_empty() { String::new() } else { format!(": {tail}") })));
        };
        let resp: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => {
                // The channel is out of sync; nothing after this can be trusted.
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(dead(format!("session {} sent a non-JSON protocol line: {:?}", self.session_id, util::head(&line, 200))));
            }
        };
        if resp.get("epoch").and_then(Value::as_str) != Some(self.epoch.as_str()) {
            return Err(dead(format!("epoch mismatch: session issued for '{}', response carries {}", self.epoch, resp.get("epoch").unwrap_or(&Value::Null))));
        }
        if resp.get("request_id").and_then(Value::as_str) != Some(request_id.as_str()) {
            return Err(SessionError::Protocol(format!("response request_id {} != sent '{request_id}'", resp.get("request_id").unwrap_or(&Value::Null))));
        }
        if resp.get("kernel_exit").and_then(Value::as_bool).unwrap_or(false) {
            // The worker answered and then exited on purpose; waiting here
            // means the next call cannot race that exit.
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
            return Err(dead(resp.get("error").and_then(Value::as_str).filter(|e| !e.is_empty()).unwrap_or("the kernel stopped").to_string()));
        }
        Ok(resp)
    }

    fn teardown(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            // EOF lets the kernel finish saving the last call's state, which
            // for a new data type means compiling its serializer.
            drop(self.stdin.take());
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        drop(self.stdin.take());
        if let Some(s) = self.server.take() {
            s.shutdown();
        }
        self.cleanser.cleanse();
    }

    pub fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.teardown();
        }
    }
}

impl Drop for PaletteSession {
    fn drop(&mut self) {
        self.close();
    }
}
