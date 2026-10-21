//! The harness the conformance suites drive a built `palette-host` through:
//! session kernels over a temporary workspace and state directory, the
//! kernel-free control actions, and processes run under a deadline.
//!
//! The host is PALETTE_HOST_BIN, or else this checkout's release build; the
//! prepared test project is PALETTE_TEST_PROJECT_DIR. `cargo xtask verify`
//! sets both.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn repo_dir() -> PathBuf {
    std::env::var_os("PALETTE_REPO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().to_path_buf())
}

pub fn host_bin() -> PathBuf {
    let host = std::env::var_os("PALETTE_HOST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_dir().join("runtime/host/target/release/palette-host"));
    assert!(host.is_file(), "build the release palette-host first (or set PALETTE_HOST_BIN): {} is missing", host.display());
    host
}

pub fn project_dir() -> PathBuf {
    std::env::var_os("PALETTE_TEST_PROJECT_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").expect("HOME is set")).join(".palette-trial/project")
    })
}

pub fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(name).is_file()))
}

/// A directory under the system temp dir, removed with everything in it on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> TempDir {
        TempDir::new_in(&std::env::temp_dir(), prefix)
    }

    pub fn new_in(parent: &Path, prefix: &str) -> TempDir {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let dir = parent.join(format!("{prefix}{}-{}-{nanos:x}", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
        // Private like mkdtemp: the host refuses sockets under a directory others can enter.
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, p: impl AsRef<Path>) -> PathBuf {
        self.0.join(p)
    }

    pub fn str(&self) -> &str {
        self.0.to_str().expect("temp paths are UTF-8")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Polls `child` until it exits or `timeout` passes.
pub fn wait_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("waiting on a child") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Runs `command` to completion with `input` on stdin, panicking if it outlives `timeout`.
pub fn run(command: &mut Command, input: &str, timeout: Duration) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("{command:?}: {e}"));
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_string();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let Some(status) = wait_timeout(&mut child, timeout) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("{command:?} did not finish within {timeout:?}");
    };
    writer.join().unwrap();
    Output { status, stdout: stdout.join().unwrap(), stderr: stderr.join().unwrap() }
}

/// A failed `palette-host control` action: a ValueError (a bad argument) or any other failure.
#[derive(Debug)]
pub struct ControlError {
    pub value_error: bool,
    pub message: String,
}

/// palette_control's kernel-free actions (checkpoints, restore, receipts) via `palette-host control`.
pub fn control(action: &str, directory: &Path, args: Value) -> Result<Value, ControlError> {
    let out = run(Command::new(host_bin()).args(["control", action]).arg(directory).arg(args.to_string()), "", Duration::from_secs(60));
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let message = stderr.trim();
        let message = message.strip_prefix("palette-host: ").unwrap_or(message);
        return Err(match message.strip_prefix("ValueError: ") {
            Some(m) => ControlError { value_error: true, message: m.into() },
            None => ControlError { value_error: false, message: message.into() },
        });
    }
    Ok(serde_json::from_slice(&out.stdout).expect("control prints JSON"))
}

/// How a kernel is started: the defaults are a 20 s turn timeout, no ceiling and no extra arguments.
pub struct KernelOptions {
    pub timeout: f64,
    pub env: Vec<(String, String)>,
    pub extra_args: Vec<String>,
    pub ceiling: Value,
}

impl Default for KernelOptions {
    fn default() -> Self {
        KernelOptions { timeout: 20.0, env: Vec::new(), extra_args: Vec::new(), ceiling: json!({}) }
    }
}

/// Environment pairs from `(&str, &str)` literals.
pub fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// One session bridge process: one kernel, over the given workspace and state directory.
pub struct Kernel {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    pub hello: Value,
    n: usize,
    _scratch: TempDir,
}

impl Kernel {
    pub fn start(workspace: &Path, state: &Path, options: KernelOptions) -> Kernel {
        let scratch = TempDir::new("palette-adapter-");
        let mut child = Command::new(host_bin())
            .arg("session")
            .arg("--scratch-root")
            .arg(scratch.join("root"))
            .arg("--repo-dir")
            .arg(repo_dir())
            .arg("--project-dir")
            .arg(project_dir())
            .args(["--ceiling", &options.ceiling.to_string()])
            .arg("--workspace-dir")
            .arg(workspace)
            .args(["--turn-timeout", &options.timeout.to_string()])
            .arg("--state-dir")
            .arg(state)
            .args(&options.extra_args)
            .envs(options.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("starting palette-host session");
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).expect("reading the session HELLO");
        let hello = serde_json::from_str(&line).unwrap_or_else(|e| panic!("session HELLO {line:?}: {e}"));
        Kernel { child, stdin, stdout, hello, n: 0, _scratch: scratch }
    }

    pub fn epoch(&self) -> &Value {
        &self.hello["epoch"]
    }

    /// One call.
    pub fn turn(&mut self, code: &str) -> Value {
        self.turn_with(code, json!({}))
    }

    /// One call with further request fields (context, budget, timeout_s, ...).
    pub fn turn_with(&mut self, code: &str, fields: Value) -> Value {
        self.n += 1;
        let mut request = json!({"request_id": self.n.to_string(), "code": code});
        if let Value::Object(extra) = fields {
            request.as_object_mut().unwrap().extend(extra);
        }
        let stdin = self.stdin.as_mut().expect("the kernel is open");
        writeln!(stdin, "{request}").and_then(|()| stdin.flush()).expect("writing a turn");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("reading a reply");
        assert!(!line.is_empty(), "no reply");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("reply {line:?}: {e}"))
    }

    pub fn close(&mut self) {
        if self.stdin.take().is_some() && wait_timeout(&mut self.child, Duration::from_secs(60)).is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
            panic!("the session bridge did not exit within 60 s of its stdin closing");
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        if self.stdin.take().is_some() && wait_timeout(&mut self.child, Duration::from_secs(60)).is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The text of the `  heading: ...` line of a revival report, or "".
pub fn section<'a>(report: &'a str, heading: &str) -> &'a str {
    let prefix = format!("  {heading}: ");
    report.lines().find_map(|l| l.strip_prefix(&prefix)).unwrap_or("")
}

/// A temporary workspace and state dir, and kernels started on them.
pub struct KernelCase {
    pub ws: TempDir,
    pub state: TempDir,
}

pub struct SessionOptions {
    pub env: Vec<(String, String)>,
    pub timeout: f64,
    pub may_fail: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions { env: Vec::new(), timeout: 20.0, may_fail: false }
    }
}

impl KernelCase {
    pub fn new() -> KernelCase {
        assert!(on_path("bwrap") && project_dir().exists(), "needs bwrap and the prepared test project");
        KernelCase { ws: TempDir::new("palette-revival-ws-"), state: TempDir::new("palette-revival-state-") }
    }

    pub fn ws(&self) -> &Path {
        self.ws.path()
    }

    pub fn state(&self) -> &Path {
        self.state.path()
    }

    pub fn kernel(&self, options: KernelOptions) -> Kernel {
        Kernel::start(self.ws(), self.state(), options)
    }

    /// One kernel running `codes` in turn; every call must succeed unless `may_fail`.
    pub fn session_with(&self, codes: &[&str], options: SessionOptions) -> Vec<Value> {
        let mut k = self.kernel(KernelOptions { timeout: options.timeout, env: options.env, ..Default::default() });
        let results: Vec<Value> = codes.iter().map(|c| k.turn(c)).collect();
        k.close();
        if !options.may_fail {
            for r in &results {
                assert_eq!(r["success"], true, "{r}");
            }
        }
        results
    }

    pub fn session(&self, codes: &[&str]) -> Vec<Value> {
        self.session_with(codes, SessionOptions::default())
    }

    /// A fresh kernel's (output, reply) for one call.
    pub fn revive_with(&self, code: &str, env: Vec<(String, String)>) -> (String, Value) {
        let mut k = self.kernel(KernelOptions { env, ..Default::default() });
        let r = k.turn(code);
        k.close();
        (r["output"].as_str().unwrap_or_default().to_string(), r)
    }

    pub fn revive(&self, code: &str) -> (String, Value) {
        self.revive_with(code, Vec::new())
    }

    pub fn manifest(&self) -> Value {
        read_json(&self.state.join("manifest.json"))
    }
}

impl Default for KernelCase {
    fn default() -> Self {
        KernelCase::new()
    }
}

pub fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every descendant of `pid`, from /proc's per-task children lists.
pub fn descendants(pid: u32) -> Vec<u32> {
    let mut result: Vec<u32> = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .map(|s| s.split_whitespace().filter_map(|c| c.parse().ok()).collect())
        .unwrap_or_default();
    for child in result.clone() {
        result.extend(descendants(child));
    }
    result
}
