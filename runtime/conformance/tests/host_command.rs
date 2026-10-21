//! host_command: operator-named host commands, run outside the sandbox. Every
//! case runs against `palette-host broker` over its socket.

use palette_conformance::{ceiling_is_subset, Broker, TempDir};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Whether `pid` has exited (a zombie counts), waiting on its pidfd: the kernel
/// signals the descriptor when the process exits.
fn pid_dead(pid: &str) -> bool {
    let pid: libc::pid_t = pid.trim().parse().unwrap_or_else(|e| panic!("pid {pid:?}: {e}"));
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as libc::c_int;
    if fd < 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let ready = unsafe { libc::poll(&mut p, 1, 5000) } > 0;
    unsafe { libc::close(fd) };
    ready
}

/// Blocks until a writer opens the FIFO and closes it: a deterministic "the
/// command has reached this point".
fn read_fifo(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap().trim().to_string()
}

/// Blocks until the command opens the FIFO to read, then releases it.
fn write_fifo(path: &Path) {
    std::fs::write(path, "go\n").unwrap();
}

fn flock() -> Option<&'static str> {
    ["/usr/bin/flock", "/bin/flock"].into_iter().find(|f| Path::new(f).is_file())
}

struct Case {
    b: Broker,
    tmp: PathBuf,
    _dir: TempDir,
}

impl Case {
    fn new() -> Case {
        let dir = TempDir::new("palette-hostcmd-");
        let tmp = dir.path().canonicalize().unwrap();
        for fifo in ["go", "started", "long-ready"] {
            let path = std::ffi::CString::new(tmp.join(fifo).to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0, "{}", std::io::Error::last_os_error());
        }
        let d = tmp.to_str().unwrap();
        let mut commands = json!({
            "echo": {"argv": ["/bin/echo", "fixed"], "cwd": d, "extra_args": true},
            "pwd": {"argv": ["/bin/pwd"], "cwd": d},
            "relative": {"argv": ["sh", "-c", "true"], "cwd": d},
            "hang": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $! > helper.pid; wait"], "cwd": d, "timeout_s": 0.5},
            "spawn": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $!"], "cwd": d, "timeout_s": 30},
            "job": {"argv": ["/bin/sh", "-c", "echo $$ > started; read line < go; echo done"], "cwd": d},
            "long": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $! > long-ready; wait"], "cwd": d},
            "ticks": {"argv": ["/bin/sh", "-c", "echo one; echo $$ > started; read line < go; echo two; echo err >&2"], "cwd": d},
            "loud": {"argv": ["/bin/sh", "-c", "head -c 40000 /dev/zero | tr '\\0' x; echo"], "cwd": d},
            "sleep": {"argv": ["/bin/sleep", "300"], "cwd": d, "timeout_s": 600},
            "quick": {"argv": ["/bin/true"], "cwd": d},
        });
        if let Some(flock) = flock() {
            let lock = tmp.join("gpu.lock");
            let lock = lock.to_str().unwrap();
            // A nested non-blocking flock on the same file exits 7 only if the
            // command already holds the lock.
            let probe = json!([flock, "--nonblock", "--conflict-exit-code", "7", lock, "/bin/true"]);
            let extra = json!({
                "locked": {"argv": probe, "cwd": d, "lock": lock, "description": "probe the lock"},
                "unlocked": {"argv": probe, "cwd": d},
                "lock-relative": {"argv": ["/bin/true"], "cwd": d, "lock": "gpu.lock"},
                "lock-nodir": {"argv": ["/bin/true"], "cwd": d, "lock": tmp.join("missing/gpu.lock")},
                "lock-isdir": {"argv": ["/bin/true"], "cwd": d, "lock": d},
            });
            commands.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        }
        let b = Broker::start(&tmp, &json!({"host_command": {"commands": commands}}));
        Case { b, tmp, _dir: dir }
    }

    fn call(&self, params: Value) -> Value {
        self.b.request("host_command", params)
    }

    fn result(&self, params: Value) -> Value {
        self.call(params)["result"].clone()
    }

    fn reason(&self, params: Value) -> String {
        self.call(params)["reason"].as_str().unwrap_or("APPROVED").to_string()
    }
}

#[test]
fn runs_a_named_command_with_fixed_argv_and_cwd() {
    let c = Case::new();
    let r = c.call(json!({"name": "echo", "args": ["one", "two words"]}));
    assert_eq!(r["approved"], true, "{r}");
    assert_eq!(r["result"]["stdout"], "fixed one two words\n");
    assert_eq!(r["result"]["returncode"], 0);
    assert_eq!(c.result(json!({"name": "pwd"}))["stdout"].as_str().unwrap().trim(), c.tmp.to_str().unwrap());
}

#[test]
fn refusals() {
    let c = Case::new();
    let cases = [
        (json!({"name": "rm"}), "not in this session's ceiling"),
        (json!({"name": "pwd", "args": ["-P"]}), "takes no arguments"),
        (json!({"name": "pwd", "args": "-P"}), "list of strings"),
        (json!({"name": "relative"}), "absolute path"),
        (json!({"name": "pwd", "action": "sudo"}), "run, start, poll, cancel, list or commands"),
        // A request may shorten a command's timeout, never extend it.
        (json!({"name": "pwd", "timeout_s": 61}), "(0, 60]"),
        (json!({"name": "pwd", "timeout_s": 0}), "timeout_s"),
        (json!({"name": "pwd", "timeout_s": "5"}), "timeout_s"),
        (json!({"name": "pwd", "timeout_s": true}), "timeout_s"),
        (json!({"action": "cancel"}), "cancel requires a string 'job'"),
    ];
    for (params, needle) in cases {
        let reason = c.reason(params.clone());
        assert!(reason.contains(needle), "{params}: {reason}");
    }
    assert_eq!(c.reason(json!({"name": "pwd", "timeout_s": 5})), "APPROVED");
}

#[test]
fn lock_runs_the_command_holding_the_lock() {
    if flock().is_none() {
        eprintln!("skipped: flock(1) is not installed");
        return;
    }
    let c = Case::new();
    let lock = c.tmp.join("gpu.lock");
    let lock = lock.to_str().unwrap();
    let r = c.result(json!({"name": "locked"}));
    assert_eq!(r["returncode"], 7, "{r}");
    assert_eq!(r["lock"], lock);
    let r = c.result(json!({"name": "unlocked"}));
    assert_eq!(r["returncode"], 0, "{r}");
    assert!(r.get("lock").is_none(), "{r}");
    for name in ["lock-relative", "lock-nodir", "lock-isdir"] {
        assert!(c.reason(json!({"name": name})).contains("'lock' must be"), "{name}");
    }
    let r = c.result(json!({"name": "locked", "action": "start"}));
    assert_eq!(r["lock"], lock, "{r}");
}

#[test]
fn commands_lists_what_the_ceiling_allows() {
    let c = Case::new();
    let r = c.call(json!({"action": "commands"}));
    assert_eq!(r["approved"], true, "{r}");
    let commands = r["result"]["commands"].as_array().unwrap();
    let by = |name: &str| commands.iter().find(|c| c["name"] == name).unwrap_or_else(|| panic!("{name} is not listed"));
    assert_eq!(by("echo")["extra_args"], true);
    assert_eq!(by("pwd")["extra_args"], false);
    assert_eq!(by("pwd")["timeout_s"].as_f64(), Some(60.0));
    assert_eq!(by("pwd")["argv"], json!(["/bin/pwd"]));
    assert_eq!(by("sleep")["timeout_s"].as_f64(), Some(600.0));
    assert!(by("pwd").get("description").is_none());
    if flock().is_some() {
        assert_eq!(by("locked")["description"], "probe the lock");
        assert_eq!(by("locked")["lock"], c.tmp.join("gpu.lock").to_str().unwrap());
    }
}

#[test]
fn timeout_kills_the_whole_group() {
    let c = Case::new();
    let started = Instant::now();
    let r = c.result(json!({"name": "hang"}));
    assert_eq!(r["timed_out"], true, "{r}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(pid_dead(&std::fs::read_to_string(c.tmp.join("helper.pid")).unwrap()));
}

#[test]
fn helpers_do_not_outlive_a_finished_command() {
    let c = Case::new();
    let started = Instant::now();
    let r = c.result(json!({"name": "spawn"}));
    assert_eq!(r["returncode"], 0, "{r}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(pid_dead(r["stdout"].as_str().unwrap()));
}

#[test]
fn start_poll_and_session_end() {
    let mut c = Case::new();
    let job = c.result(json!({"name": "job", "action": "start"}))["job"].clone();
    // the job is running, blocked on `go`
    let shell = read_fifo(&c.tmp.join("started"));
    assert_eq!(c.result(json!({"action": "poll", "job": job}))["running"], true);
    write_fifo(&c.tmp.join("go"));
    assert!(pid_dead(&shell));
    let r = c.call(json!({"action": "poll", "job": job}));
    assert_eq!(r["result"]["stdout"], "done\n", "{r}");
    assert!(c.reason(json!({"action": "poll", "job": job})).contains("no running"));
    c.call(json!({"name": "long", "action": "start"}));
    let helper = read_fifo(&c.tmp.join("long-ready"));
    c.b.close();
    assert!(pid_dead(&helper), "a job outlived its session");
}

#[test]
fn poll_streams_output_since_the_last_poll() {
    let c = Case::new();
    let job = c.result(json!({"name": "ticks", "action": "start"}))["job"].clone();
    // "one" is written; the job waits on `go`
    read_fifo(&c.tmp.join("started"));
    let (mut seen, deadline) = (String::new(), Instant::now() + Duration::from_secs(10));
    while seen != "one\n" && Instant::now() < deadline {
        let r = c.result(json!({"action": "poll", "job": job}));
        assert_eq!(r["running"], true, "{r}");
        assert_eq!(r["name"], "ticks");
        seen += r["stdout"].as_str().unwrap();
    }
    assert_eq!(seen, "one\n");
    assert_eq!(c.result(json!({"action": "poll", "job": job}))["stdout"], "");
    write_fifo(&c.tmp.join("go"));
    let (mut rest, mut err, deadline) = (String::new(), String::new(), Instant::now() + Duration::from_secs(10));
    let mut r;
    loop {
        r = c.result(json!({"action": "poll", "job": job}));
        rest += r["stdout"].as_str().unwrap();
        err += r["stderr"].as_str().unwrap();
        if r["running"] == false || Instant::now() >= deadline {
            break;
        }
    }
    assert_eq!(r["running"], false, "{r}");
    assert_eq!(rest, "two\n", "the final result repeated output a poll had returned");
    assert_eq!(err, "err\n");
    assert_eq!(r["cancelled"], false);
}

#[test]
fn cancel_kills_the_job_group_and_list_names_jobs() {
    let c = Case::new();
    let job = c.result(json!({"name": "long", "action": "start"}))["job"].clone();
    let helper = read_fifo(&c.tmp.join("long-ready"));
    let listed = c.result(json!({"action": "list"}))["jobs"].clone();
    let listed: Vec<(&Value, &Value, &Value)> = listed.as_array().unwrap().iter().map(|j| (&j["job"], &j["name"], &j["running"])).collect();
    assert_eq!(listed, [(&job, &json!("long"), &json!(true))]);
    let r = c.result(json!({"action": "cancel", "job": job}));
    assert_eq!(r["cancelled"], true, "{r}");
    assert_eq!(r["running"], false);
    assert_eq!(r["timed_out"], false);
    assert!(pid_dead(&helper), "a cancelled job's helper survived");
    assert_eq!(c.result(json!({"action": "list"}))["jobs"], json!([]));
    assert!(c.reason(json!({"action": "cancel", "job": job})).contains("no running"));
    // Cancelling a job that already finished reports it finished, not cancelled.
    let job = c.result(json!({"name": "quick", "action": "start"}))["job"].clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    // each broker round trip paces the loop
    while c.result(json!({"action": "list"}))["jobs"][0]["running"] == true && Instant::now() < deadline {}
    let r = c.result(json!({"action": "cancel", "job": job}));
    assert_eq!((&r["cancelled"], &r["returncode"]), (&json!(false), &json!(0)), "{r}");
}

#[test]
fn reports_output_the_tail_dropped() {
    let c = Case::new();
    let r = c.result(json!({"name": "loud"}));
    assert_eq!(r["stdout"].as_str().unwrap().len(), 16 * 1024);
    assert_eq!(r["stdout_skipped_bytes"], 40001 - 16 * 1024);
    assert!(r.get("stderr_skipped_bytes").is_none(), "{r}");
}

#[test]
fn request_timeout_shortens_the_ceiling_timeout() {
    let c = Case::new();
    let started = Instant::now();
    let r = c.result(json!({"name": "sleep", "timeout_s": 0.5}));
    assert_eq!(r["timed_out"], true, "{r}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn child_keeps_only_parent_commands_unchanged() {
    let parent = json!({"host_command": {"commands": {"render": {"argv": ["/bin/true"], "cwd": "/tmp"}}}});
    let child = |name: &str, argv: &str| json!({"host_command": {"commands": {name: {"argv": [argv], "cwd": "/tmp"}}}});
    assert!(ceiling_is_subset(&json!({"host_command": {"commands": {}}}), &parent).0);
    assert!(ceiling_is_subset(&parent, &parent).0);
    assert!(!ceiling_is_subset(&child("render", "/bin/sh"), &parent).0);
    assert!(!ceiling_is_subset(&child("sh", "/bin/sh"), &parent).0);
    assert!(!ceiling_is_subset(&parent, &json!({})).0);
}
