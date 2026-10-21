//! Process-level proof that the daemon outlives its disposable socket clients.

use palette_conformance::{descendants, host_bin, repo_dir, run, wait_timeout, TempDir};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Barrier};
use std::time::Duration;

fn julia_pid(root_pid: u32) -> u32 {
    descendants(root_pid)
        .into_iter()
        .find(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).replace('\0', " ").contains("runtime/scripts/session_loop.jl"))
        })
        .expect("Julia session process is absent from the daemon's workspace process tree")
}

fn request_frame(sock: &Path, name: &str, args: Value) -> Value {
    let mut conn = UnixStream::connect(sock).unwrap_or_else(|e| panic!("{}: {e}", sock.display()));
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}});
    writeln!(conn, "{msg}").unwrap();
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("frame {line:?}: {e}"))
}

fn payload(frame: &Value) -> Value {
    serde_json::from_str(frame["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

struct Daemon {
    tmp: TempDir,
    project: PathBuf,
    sock: PathBuf,
    env: Vec<(String, String)>,
    proc: Option<Child>,
}

impl Daemon {
    fn new() -> Daemon {
        let tmp = TempDir::new("palette-daemon-");
        let project = tmp.join("project");
        std::fs::create_dir(&project).unwrap();
        let path = |p: &Path| p.to_str().unwrap().to_string();
        let env = vec![
            ("PALETTE_REPO".into(), path(&repo_dir())),
            ("PALETTE_HOST".into(), path(&host_bin())),
            ("OPERATOR_WORKSPACE".into(), path(&project)),
            ("PALETTE_STATE_DIR".into(), path(&tmp.join("legacy"))),
            ("PALETTE_WORKSPACE_STATE_ROOT".into(), path(&tmp.join("worlds"))),
            ("PALETTE_SCRATCH_ROOT".into(), path(&tmp.join("scratch"))),
        ];
        let sock = tmp.join("run/palette/daemon.sock");
        let mut d = Daemon { tmp, project, sock, env, proc: None };
        d.start();
        d
    }

    fn mcp(&self, socket: &Path) -> Command {
        let mut c = Command::new(host_bin());
        c.arg("mcp").arg("--socket").arg(socket).envs(self.env.iter().cloned());
        c
    }

    fn start(&mut self) {
        let mut child =
            self.mcp(&self.sock).current_dir(&self.project).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
        // The first stderr line is the readiness line; the rest is drained so the daemon never blocks on it.
        let stderr = BufReader::new(child.stderr.take().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut lines = stderr.lines();
            let _ = tx.send(lines.next().and_then(Result::ok).unwrap_or_default());
            lines.for_each(drop);
        });
        self.proc = Some(child);
        let ready = rx.recv_timeout(Duration::from_secs(15)).expect("daemon readiness deadline expired");
        assert!(ready.contains("Palette daemon listening"), "{ready:?}");
        UnixStream::connect(&self.sock).unwrap();
    }

    fn pid(&self) -> u32 {
        self.proc.as_ref().unwrap().id()
    }

    fn running(&mut self) -> bool {
        self.proc.as_mut().is_some_and(|p| p.try_wait().unwrap().is_none())
    }

    fn frame(&self, name: &str, args: Value) -> Value {
        request_frame(&self.sock, name, args)
    }

    fn request(&self, name: &str, args: Value) -> Value {
        let frame = self.frame(name, args);
        assert!(frame.get("result").is_some(), "{frame}");
        assert_ne!(frame["result"]["isError"], true, "{}", frame["result"]);
        payload(&frame)
    }

    fn terminate(&mut self) -> Option<std::process::ExitStatus> {
        let mut child = self.proc.take()?;
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        wait_timeout(&mut child, Duration::from_secs(60)).or_else(|| {
            let _ = child.kill();
            child.wait().ok()
        })
    }

    fn stop(&mut self) {
        let status = self.terminate().expect("the daemon is running");
        assert_eq!(status.code(), Some(128 + 15), "{status}");
    }

    /// `palette-host mcp --socket <socket>` run to completion: it must refuse to start.
    fn refuses(&self, socket: &Path) {
        let out = run(&mut self.mcp(socket), "", Duration::from_secs(15));
        assert!(!out.status.success(), "the daemon started on {}", socket.display());
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.running() {
            self.terminate();
        }
    }
}

#[test]
fn disconnect_restart_and_revive() {
    let mut d = Daemon::new();
    let daemon_pid = d.pid();
    assert_eq!(d.sock.metadata().unwrap().permissions().mode() & 0o7777, 0o600);
    let mut connect = Command::new(host_bin());
    connect.args(["mcp", "connect"]).envs(d.env.iter().cloned()).env("PALETTE_SOCKET", &d.sock);
    let proxy = run(&mut connect, &format!("{}\n", json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})), Duration::from_secs(20));
    assert!(proxy.status.success(), "{}", String::from_utf8_lossy(&proxy.stderr));
    let listed: Value = serde_json::from_slice(&proxy.stdout).unwrap();
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 4);
    let created = d.request("palette_workspace", json!({"action": "create", "workspace_id": "durable", "scope": "open"}));
    assert_eq!(created["workspace_id"], "durable");
    let mutation = d.request("palette", json!({"workspace_id": "durable", "code": "daemon_value = 431"}));
    assert!(mutation["epoch"].as_str().is_some_and(|e| !e.is_empty()), "{mutation}");
    let workspace_status = d.request("palette_workspace", json!({"action": "status", "workspace_id": "durable"}));
    let adapter_pid = workspace_status["local_adapter_pid"].as_u64().unwrap() as u32;
    let kernel_pid = julia_pid(adapter_pid);
    assert_eq!(d.request("palette", json!({"workspace_id": "durable", "code": "daemon_value"}))["data"], 431);

    // request() closes its client socket; reconnect reaches the same daemon and world.
    assert!(d.running());
    assert_eq!(d.pid(), daemon_pid);
    let status = d.request("palette_workspace", json!({"action": "status", "workspace_id": "durable"}));
    assert_eq!(status["local_adapter_pid"], adapter_pid);
    let value = d.request("palette", json!({"workspace_id": "durable", "code": "daemon_value"}));
    assert_eq!(value["data"], 431);
    assert_eq!(value["epoch"], mutation["epoch"]);
    assert_eq!(julia_pid(adapter_pid), kernel_pid);
    d.stop();
    assert!(!d.sock.exists());
    d.start();
    assert_ne!(d.pid(), daemon_pid);
    assert_eq!(d.request("palette", json!({"workspace_id": "durable", "code": "daemon_value"}))["data"], 431);
}

#[test]
fn concurrent_starts_reserve_live_capacity() {
    let mut d = Daemon::new();
    d.stop();
    d.env.push(("PALETTE_MAX_LIVE_WORKSPACES".into(), "1".into()));
    d.start();
    for id in ["race-a", "race-b"] {
        d.request("palette_workspace", json!({"action": "create", "workspace_id": id, "scope": "open"}));
    }
    let barrier = Arc::new(Barrier::new(3));
    let (tx, rx) = mpsc::channel();
    for id in ["race-a", "race-b"] {
        let (barrier, tx, sock) = (barrier.clone(), tx.clone(), d.sock.clone());
        std::thread::spawn(move || {
            barrier.wait();
            let _ = tx.send((id, request_frame(&sock, "palette", json!({"workspace_id": id, "code": "42"}))));
        });
    }
    barrier.wait();
    let outcomes: Vec<(&str, Value)> =
        (0..2).map(|_| rx.recv_timeout(Duration::from_secs(240)).expect("concurrent workspace start hung")).collect();
    let failed = |f: &Value| f.get("result").is_none() || f["result"]["isError"] == true;
    let successful = outcomes.iter().filter(|(_, f)| !failed(f)).count();
    assert_eq!(successful, 1, "{outcomes:?}");
    let rejected = &outcomes.iter().find(|(_, f)| f["result"]["isError"] == true).expect("one start is rejected").1;
    let denied = payload(rejected);
    assert!(denied["error"].as_str().unwrap().contains("Live workspace limit (1)"), "{denied}");
}

#[test]
fn rejects_unsafe_existing_path() {
    // A non-socket file or symlink is never unlinked/replaced by startup.
    let mut d = Daemon::new();
    d.stop();
    std::fs::write(&d.sock, "owned").unwrap();
    d.refuses(&d.sock);
    assert_eq!(std::fs::read_to_string(&d.sock).unwrap(), "owned");
    std::fs::remove_file(&d.sock).unwrap();
    let target = d.tmp.join("target");
    std::fs::write(&target, "owned").unwrap();
    std::os::unix::fs::symlink(&target, &d.sock).unwrap();
    d.refuses(&d.sock);
    assert!(d.sock.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "owned");
    std::fs::remove_file(&d.sock).unwrap();
    let shared = d.tmp.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
    let private_child = shared.join("private");
    std::fs::create_dir(&private_child).unwrap();
    std::fs::set_permissions(&private_child, std::fs::Permissions::from_mode(0o700)).unwrap();
    d.refuses(&private_child.join("daemon.sock"));
    assert!(!private_child.join("daemon.sock").exists());
}
