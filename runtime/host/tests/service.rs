//! `palette-host service` end to end, against a fake systemctl, under an
//! isolated HOME, XDG_CONFIG_HOME and XDG_RUNTIME_DIR: the operator's real
//! units and user manager are never touched.

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Logs each call; `show` reports the caller as the unit's main process.
const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
echo "$*" >> "$CALL_LOG"
if [ "$2" = show ]; then
  printf 'ActiveState=active\nSubState=running\nMainPID=%s\nExecMainStartTimestamp=now\n' "$PPID"
fi
"#;

const SHOW: &str = "--no-page --property=ActiveState,SubState,MainPID,ExecMainStartTimestamp";

fn write(path: &Path, contents: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

struct Fixture {
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    fn new() -> Fixture {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let tmp = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = tmp.join(format!("palette-service-it-{}-{} with space", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&root);
        let f = Fixture { root };
        write(&f.bin().join("systemctl"), FAKE_SYSTEMCTL, 0o755);
        write(&f.repo().join("runtime/host/target/release/palette-host"), "#!/bin/sh\n", 0o755);
        fs::create_dir_all(f.runtime()).unwrap();
        fs::create_dir_all(f.root.join("home")).unwrap();
        f
    }

    fn host(&self) -> PathBuf {
        self.repo().join("runtime/host/target/release/palette-host")
    }

    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn runtime(&self) -> PathBuf {
        self.root.join("runtime dir")
    }

    fn units(&self) -> PathBuf {
        self.root.join("config/systemd/user")
    }

    fn log(&self) -> PathBuf {
        self.root.join("calls.log")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_palette-host"));
        c.arg("service")
            .args(args)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_RUNTIME_DIR", self.runtime())
            .env("CALL_LOG", self.log())
            .current_dir(&self.root);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "service {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn err(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert_eq!(out.status.code(), Some(2), "service {args:?} should fail");
        String::from_utf8(out.stderr).unwrap()
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.log()).unwrap_or_default().lines().map(str::to_string).collect()
    }

    fn install_args(&self) -> Vec<String> {
        let workspace = self.root.join("workspace's name");
        vec!["install".into(), "--repo-dir".into(), self.repo().to_string_lossy().into(), "--workspace-dir".into(), workspace.to_string_lossy().into()]
    }
}

fn args(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

#[test]
fn install_updates_only_owned_units_and_runs_fake_systemctl() {
    let f = Fixture::new();
    let other = f.units().join("unrelated.service");
    write(&other, "operator content\n", 0o644);
    let install = f.install_args();
    let first: Value = serde_json::from_str(&f.ok(&args(&install))).unwrap();
    let sock = f.runtime().join("palette/daemon.sock");
    let daemon = f.units().join("palette.service");
    assert_eq!(first["installed"], serde_json::json!([daemon.to_string_lossy()]));
    assert_eq!(first["socket"], sock.to_string_lossy().as_ref());
    assert!(first["tunnel_unit"].is_null());
    f.ok(&args(&install));
    assert_eq!(fs::read_to_string(&other).unwrap(), "operator content\n");
    assert!(!f.units().join("palette-tunnel.service").exists());
    assert_eq!(mode(&daemon), 0o600);
    let unit = fs::read_to_string(&daemon).unwrap();
    assert!(unit.contains(&format!("ExecStart=\"{}\" \"mcp\" \"--socket\" ", f.host().display())), "{unit}");
    assert!(unit.contains("workspace's name"));

    for unit in ["palette.service", "palette-tunnel.service"] {
        for action in ["start", "stop", "restart"] {
            f.ok(&[action, "--unit", unit]);
        }
    }
    let child = f.command(&["status"]).stdout(Stdio::piped()).spawn().unwrap();
    let pid = child.id();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["unit"], "palette.service");
    assert_eq!(status["ActiveState"], "active");
    assert_eq!(status["MainPID"], pid.to_string());
    assert!(status["process_start_ticks"].as_str().is_some_and(|t| t.parse::<u64>().is_ok()));
    assert_eq!(status["socket"], sock.to_string_lossy().as_ref());
    let tunnel: Value = serde_json::from_str(&f.ok(&["status", "--unit", "palette-tunnel.service"])).unwrap();
    assert_eq!(tunnel["unit"], "palette-tunnel.service");
    assert!(tunnel["process_start_ticks"].is_null() && tunnel["socket"].is_null());

    let calls = f.calls();
    assert_eq!(calls[0], "--user daemon-reload");
    assert_eq!(calls[1], "--user daemon-reload");
    for unit in ["palette.service", "palette-tunnel.service"] {
        for action in ["start", "stop", "restart"] {
            assert!(calls.contains(&format!("--user {action} {unit}")), "{action} {unit} missing from {calls:?}");
        }
    }
    assert_eq!(calls[calls.len() - 2], format!("--user show palette.service {SHOW}"));
    assert_eq!(calls[calls.len() - 1], format!("--user show palette-tunnel.service {SHOW}"));
}

#[test]
fn install_writes_an_absolute_host_for_a_relative_repo_dir() {
    let f = Fixture::new();
    let mut install = f.install_args();
    install[2] = "repo".into();
    let out = f.command(&args(&install)).current_dir(&f.root).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let unit = fs::read_to_string(f.units().join("palette.service")).unwrap();
    assert!(unit.contains(&format!("ExecStart=\"{}\" \"mcp\" ", f.host().display())), "{unit}");
}

#[test]
fn install_writes_tunnel_unit_after_checking_both() {
    let f = Fixture::new();
    let client = f.root.join("tunnel client");
    write(&client, "#!/bin/sh\n", 0o700);
    let profiles = f.root.join("profiles");
    write(&profiles.join("chat.yaml"), "", 0o644);
    let mut install = f.install_args();
    let sock = f.root.join("custom.sock");
    install.extend(["--socket".into(), sock.to_string_lossy().into(), "--tunnel-client".into(), client.to_string_lossy().into()]);

    // Half a tunnel, or a profile that is not there, writes nothing.
    assert!(f.err(&args(&install)).contains("must be supplied together"));
    let mut missing = install.clone();
    missing.extend(["--tunnel-profile".into(), "absent".into(), "--tunnel-profile-dir".into(), profiles.to_string_lossy().into()]);
    assert!(f.err(&args(&missing)).contains("profile must exist"));
    assert!(!f.units().exists());
    assert!(f.calls().is_empty());

    install.extend(["--tunnel-profile".into(), "chat".into(), "--tunnel-profile-dir".into(), profiles.to_string_lossy().into()]);
    let out: Value = serde_json::from_str(&f.ok(&args(&install))).unwrap();
    let tunnel = f.units().join("palette-tunnel.service");
    assert_eq!(out["tunnel_unit"], tunnel.to_string_lossy().as_ref());
    assert_eq!(out["socket"], sock.to_string_lossy().as_ref());
    assert_eq!(mode(&tunnel), 0o600);
    assert_eq!(mode(&f.units()), 0o700);
    let unit = fs::read_to_string(&tunnel).unwrap();
    assert!(unit.contains(&format!("Environment=PALETTE_SOCKET=\"{}\"\n", sock.display())));
    assert!(unit.contains("\"run\" \"--profile\" \"chat\" \"--profile-dir\""));
    assert!(fs::read_to_string(f.units().join("palette.service")).unwrap().contains(&format!("Environment=PALETTE_SOCKET=\"{}\"", sock.display())));
}

#[test]
fn bad_invocations_fail_without_touching_units() {
    let f = Fixture::new();
    assert!(f.err(&[]).contains("usage: palette-host service"));
    assert!(f.err(&["enable"]).contains("usage: palette-host service"));
    assert!(f.err(&["start", "--unit", "other.service"]).contains("--unit must be"));
    assert!(f.err(&["status", "--bogus", "x"]).contains("unknown option --bogus"));
    assert!(f.err(&["install", "--repo-dir", &f.repo().to_string_lossy()]).contains("--workspace-dir is required"));
    let out = f.command(&["status"]).env_remove("XDG_RUNTIME_DIR").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("XDG_RUNTIME_DIR is required"));
    let out = f.command(&["start"]).env("PATH", "/nonexistent").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("systemctl is not available"));
    assert!(f.ok(&["--help"]).contains("--tunnel-profile-dir"));
    assert!(!f.units().exists());
    assert!(f.calls().iter().all(|c| !c.contains("daemon-reload")));
}
