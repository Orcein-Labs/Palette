//! Process-level proof that the daemon outlives its disposable socket clients.

use palette_conformance::{descendants, host_bin, repo_dir, run, wait_timeout, TempDir};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
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

fn julia_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("PALETTE_JULIA_BIN").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    let shim = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("julia"))
        .find(|path| path.is_file())
        .expect("Julia shim is on PATH");
    let output = Command::new(shim)
        .args(["--startup-file=no", "-e", "print(Sys.BINDIR)"])
        .output()
        .expect("run Julia shim to find its versioned executable");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    PathBuf::from(format!("{}/julia", String::from_utf8_lossy(&output.stdout)))
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

struct ProxyProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: mpsc::Receiver<String>,
}

impl ProxyProcess {
    fn start(socket: &Path) -> ProxyProcess {
        let mut child = Command::new(host_bin())
            .args(["mcp", "connect"])
            .env("PALETTE_SOCKET", socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        ProxyProcess { stdin: child.stdin.take(), child, frames }
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{message}").unwrap();
        self.stdin.as_mut().unwrap().flush().unwrap();
    }

    fn frame(&self) -> Value {
        self.frame_for("MCP proxy reply", Duration::from_secs(20))
    }

    fn frame_for(&self, phase: &str, timeout: Duration) -> Value {
        let line = self.frames.recv_timeout(timeout)
            .unwrap_or_else(|error| panic!("waiting for {phase} after {timeout:?}: {error}"));
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("frame {line:?}: {e}"))
    }

    fn finish(&mut self) -> std::process::ExitStatus {
        self.stdin.take();
        wait_timeout(&mut self.child, Duration::from_secs(15)).expect("MCP proxy did not exit after stdin EOF")
    }
}

struct PidFd(libc::c_int);

impl PidFd {
    fn open(pid: u32) -> Option<PidFd> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) as libc::c_int };
        if fd >= 0 {
            Some(PidFd(fd))
        } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            None
        } else {
            panic!("pidfd_open({pid}): {}", std::io::Error::last_os_error());
        }
    }

    fn wait_exit(&self, timeout: Duration) -> bool {
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd { fd: self.0, events: libc::POLLIN, revents: 0 };
        loop {
            let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
            if result > 0 {
                return descriptor.revents != 0;
            }
            if result == 0 {
                return false;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                panic!("polling pidfd: {}", std::io::Error::last_os_error());
            }
        }
    }
}

impl Drop for PidFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

fn accept_signal(listener: &UnixListener, timeout: Duration) -> UnixStream {
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    let mut descriptor = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
    assert_eq!(result, 1, "Julia did not reach the effect barrier within {timeout:?}");
    listener.accept().unwrap().0
}

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn fake_response(stream: &mut UnixStream, id: &Value, result: Value) {
    writeln!(stream, "{}", json!({"jsonrpc": "2.0", "id": id, "result": result})).unwrap();
    stream.flush().unwrap();
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

    fn kill_abruptly(&mut self) {
        let mut child = self.proc.take().expect("the daemon is running");
        child.kill().unwrap();
        child.wait().unwrap();
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
fn default_operator_daemon_starts_a_real_julia_session_without_bwrap_on_path() {
    let mut d = Daemon::new();
    d.stop();
    let bin = d.tmp.join("without-bwrap");
    std::fs::create_dir(&bin).unwrap();
    let julia = julia_binary();
    std::os::unix::fs::symlink(&julia, bin.join("julia")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/cp", bin.join("cp")).unwrap();
    d.env.push(("PATH".into(), bin.to_string_lossy().into_owned()));
    d.env.push(("PALETTE_JULIA_BIN".into(), julia.to_string_lossy().into_owned()));
    assert!(!bin.join("bwrap").exists());
    d.start();
    d.request("palette_workspace", json!({"action": "create", "workspace_id": "operator-no-bwrap", "scope": "open"}));
    let result = d.request("palette", json!({"workspace_id": "operator-no-bwrap", "code": "40 + 2"}));
    assert_eq!(result["mode"], "operator");
    assert_eq!(result["data"], 42);
    d.stop();
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

#[test]
fn proxy_forwards_idle_notifications_and_cancellation_during_a_call() {
    let tmp = TempDir::new("palette-proxy-full-duplex-");
    let socket = tmp.join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (cancel_tx, cancel_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        writeln!(stream, "{}", json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"message": "idle"}})).unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0, "proxy closed before the test call");
            let frame: Value = serde_json::from_str(&line).unwrap();
            match frame["method"].as_str() {
                Some("initialize") => fake_response(&mut stream, &frame["id"], json!({"protocolVersion": "2025-06-18", "capabilities": {}})),
                Some("notifications/initialized") => {}
                Some("tools/call") => {
                    assert_eq!(frame["id"], 2);
                    writeln!(stream, "{}", json!({"jsonrpc": "2.0", "id": "approval-1", "method": "elicitation/create",
                        "params": {"message": "approve this patch?"}})).unwrap();
                    stream.flush().unwrap();
                    line.clear();
                    assert!(reader.read_line(&mut line).unwrap() > 0, "proxy did not return the server request response");
                    let response: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(response, json!({"jsonrpc": "2.0", "id": "approval-1", "result": {"action": "accept"}}));
                    line.clear();
                    assert!(reader.read_line(&mut line).unwrap() > 0, "proxy did not forward cancellation");
                    let cancelled: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(cancelled["method"], "notifications/cancelled");
                    assert_eq!(cancelled["params"]["requestId"], 2);
                    cancel_tx.send(()).unwrap();
                    fake_response(&mut stream, &frame["id"], json!({"isError": false}));
                    line.clear();
                    assert!(reader.read_line(&mut line).unwrap() > 0, "proxy lost its connection after server request response");
                    let next: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(next["id"], 3);
                    fake_response(&mut stream, &next["id"], json!({"still_connected": true}));
                    return;
                }
                other => panic!("unexpected client frame: {other:?} {frame}"),
            }
        }
    });

    let mut proxy = ProxyProcess::start(&socket);
    let idle = proxy.frame();
    assert_eq!(idle["method"], "notifications/message");
    proxy.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}}));
    assert_eq!(proxy.frame()["id"], 1);
    proxy.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    proxy.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "palette", "arguments": {"code": "sleep(5)"}}}));
    let server_request = proxy.frame();
    assert_eq!(server_request["id"], "approval-1");
    assert_eq!(server_request["method"], "elicitation/create");
    proxy.send(&json!({"jsonrpc": "2.0", "id": "approval-1", "result": {"action": "accept"}}));
    proxy.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 2}}));
    let response = proxy.frame();
    assert_eq!(response["id"], 2);
    assert_eq!(cancel_rx.recv_timeout(Duration::from_secs(5)), Ok(()));
    proxy.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "palette", "arguments": {"code": "1"}}}));
    let next = proxy.frame();
    assert_eq!(next["id"], 3);
    assert_eq!(next["result"]["still_connected"], true);
    assert!(proxy.finish().success());
    server.join().unwrap();
}

#[test]
fn proxy_drains_an_inflight_response_after_client_eof() {
    let tmp = TempDir::new("palette-proxy-eof-");
    let socket = tmp.join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0, "proxy closed before the in-flight call");
            let frame: Value = serde_json::from_str(&line).unwrap();
            match frame["method"].as_str() {
                Some("initialize") => fake_response(&mut stream, &frame["id"], json!({"protocolVersion": "2025-06-18", "capabilities": {}})),
                Some("notifications/initialized") => {}
                Some("tools/call") => {
                    writeln!(stream, "{}", json!({"jsonrpc": "2.0", "id": "approval-after-eof", "method": "elicitation/create",
                        "params": {"message": "approve?"}})).unwrap();
                    stream.flush().unwrap();
                    line.clear();
                    assert!(reader.read_line(&mut line).unwrap() > 0, "proxy did not reject a server request after client EOF");
                    let answer: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(answer["id"], "approval-after-eof");
                    assert_eq!(answer["error"]["message"], "MCP client input is closed");
                    fake_response(&mut stream, &frame["id"], json!({"completed_after_eof": true}));
                    return;
                }
                other => panic!("unexpected client frame: {other:?} {frame}"),
            }
        }
    });

    let mut proxy = ProxyProcess::start(&socket);
    proxy.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}}));
    assert_eq!(proxy.frame()["id"], 1);
    proxy.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    proxy.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "palette", "arguments": {"code": "sleep(20)"}}}));
    let request = proxy.frame();
    assert_eq!(request["method"], "elicitation/create");
    assert_eq!(request["id"], "approval-after-eof");
    drop(proxy.stdin.take());
    let response = proxy.frame();
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["completed_after_eof"], true);
    assert!(proxy.finish().success());
    server.join().unwrap();
}

#[test]
fn daemon_reconnect_does_not_replay_an_uncertain_effect() {
    let mut d = Daemon::new();
    d.request("palette_workspace", json!({"action": "create", "workspace_id": "proxy-effect", "scope": "open"}));
    d.request("palette", json!({"workspace_id": "proxy-effect", "code": "nothing"}));
    let adapter = d.request("palette_workspace", json!({"action": "status", "workspace_id": "proxy-effect"}))["local_adapter_pid"]
        .as_u64().unwrap() as u32;
    let kernel = julia_pid(adapter);
    let effect = d.project.join("effect-count.txt");
    let effect_text = effect.to_string_lossy().into_owned();
    let append_effect = format!("open({effect_text:?}, \"a\") do io; println(io, \"effect\"); flush(io); end");
    let ready_path = d.project.join("effect-ready.sock");
    let ready_path_text = ready_path.to_string_lossy().into_owned();
    let listener = UnixListener::bind(&ready_path).unwrap();
    let owned_processes: Vec<(u32, PidFd)> = std::iter::once(adapter).chain(descendants(adapter)).filter_map(|pid| {
        PidFd::open(pid).map(|fd| (pid, fd))
    }).collect();
    assert!(owned_processes.iter().any(|(pid, _)| *pid == kernel), "Julia worker did not have a live pidfd");
    let mut proxy = ProxyProcess::start(&d.sock);
    proxy.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}}));
    assert_eq!(proxy.frame()["id"], 1);
    proxy.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let code = format!("using Sockets; {append_effect}; gate = Sockets.connect({ready_path_text:?}); write(gate, \"effect-written\\n\"); flush(gate); read(gate, UInt8); close(gate); 17");
    proxy.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "palette",
        "arguments": {"workspace_id": "proxy-effect", "code": code}}}));
    let mut gate = accept_signal(&listener, Duration::from_secs(300));
    gate.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut ready = String::new();
    BufReader::new(gate.try_clone().unwrap()).read_line(&mut ready).unwrap();
    assert_eq!(ready, "effect-written\n");
    assert_eq!(std::fs::read_to_string(&effect).unwrap(), "effect\n");

    d.kill_abruptly();
    let lost = proxy.frame_for("uncertain call response after daemon kill", Duration::from_secs(20));
    assert_eq!(lost["id"], 2);
    assert_eq!(lost["error"]["data"], json!({"outcome": "unknown", "replayed": false}));
    let _ = gate.write_all(b"continue\n");
    let _ = gate.flush();
    for (pid, handle) in &owned_processes {
        assert!(handle.wait_exit(Duration::from_secs(60)), "owned process {pid} outlived daemon shutdown");
    }
    d.start();

    proxy.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "palette",
        "arguments": {"workspace_id": "proxy-effect", "code": format!("length(readlines({effect_text:?}))")}}}));
    let resumed = proxy.frame_for("resumed Julia call after daemon reconnect", Duration::from_secs(300));
    assert_eq!(resumed["id"], 3);
    assert!(resumed.get("result").is_some(), "reconnected call failed: {resumed}");
    assert_ne!(resumed["result"]["isError"], true, "reconnected call failed: {resumed}");
    assert_eq!(payload(&resumed)["data"], 1, "lost call must not be replayed");

    // Negative control: a new explicit call produces a second observable effect.
    proxy.send(&json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "palette",
        "arguments": {"workspace_id": "proxy-effect", "code": format!("{append_effect}; length(readlines({effect_text:?}))")}}}));
    let deliberate = proxy.frame_for("explicit negative-control effect", Duration::from_secs(300));
    assert_eq!(deliberate["id"], 4);
    assert_eq!(payload(&deliberate)["data"], 2);
    assert!(proxy.finish().success());
    d.stop();
}
