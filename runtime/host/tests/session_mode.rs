//! Default operator mode and explicit sandbox selection through the real CLI.

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::ops::{Deref, DerefMut};
use std::process::{Child, Command, Stdio};

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("palette-session-mode-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ManagedChild(Child);

impl Deref for ManagedChild {
    type Target = Child;
    fn deref(&self) -> &Child { &self.0 }
}

impl DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Child { &mut self.0 }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn julia() -> PathBuf {
    if let Some(path) = std::env::var_os("PALETTE_JULIA_BIN") {
        return PathBuf::from(path);
    }
    let out = Command::new("julia")
        .args(["--startup-file=no", "-e", "print(Sys.BINDIR)"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "julia binary lookup failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim()).join("julia")
}

fn prepare_project(julia: &Path, project: &Path, repo: &Path, fixture: Option<&Path>, depots: &str) {
    let setup = r#"
using Pkg, TOML
Pkg.offline(true)
project, repo = ARGS[1:2]
Pkg.activate(project; io=devnull)
Pkg.develop(Pkg.PackageSpec(path=repo); io=devnull)
if length(ARGS) > 2
    Pkg.develop(Pkg.PackageSpec(path=ARGS[3]); io=devnull)
end
project_file = joinpath(project, "Project.toml")
project_data = TOML.parsefile(project_file)
delete!(project_data["deps"], "Palette")
open(project_file, "w") do io
    TOML.print(io, project_data)
end
manifest_file = joinpath(project, "Manifest.toml")
manifest_data = TOML.parsefile(manifest_file)
delete!(manifest_data["deps"], "Palette")
delete!(manifest_data, "project_hash")
open(manifest_file, "w") do io
    TOML.print(io, manifest_data)
end
"#;
    let mut command = Command::new(julia);
    command.args(["--startup-file=no", "-e", setup])
        .arg(project)
        .arg(repo)
        .env("JULIA_DEPOT_PATH", depots)
        .env_remove("JULIA_PROJECT")
        .env_remove("JULIA_LOAD_PATH");
    if let Some(fixture) = fixture {
        command.arg(fixture);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "project preparation failed: {}", String::from_utf8_lossy(&output.stderr));
}

fn base_depots() -> String {
    std::env::var("JULIA_DEPOT_PATH").unwrap_or_else(|_| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()))
            .join(".julia")
            .to_string_lossy()
            .into_owned()
    })
}

fn line(reader: &mut BufReader<std::process::ChildStdout>) -> Value {
    let mut text = String::new();
    assert_ne!(
        reader.read_line(&mut text).unwrap(),
        0,
        "session closed stdout early"
    );
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn operator_session(
    root: &Path,
    path: &str,
    project: &Path,
    inherited_project: Option<&Path>,
    depot_path: Option<&str>,
) -> ManagedChild {
    let repo = checkout();
    let julia = julia();
    let depots = depot_path.map(str::to_owned).unwrap_or_else(|| {
        format!(
            "{}:{base}",
            root.join("depot").display(),
            base = base_depots()
        )
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_palette-host"));
    command
        .args(["session", "--project-dir"])
        .arg(project)
        .args(["--repo-dir"])
        .arg(&repo)
        .args(["--scratch-root"])
        .arg(root.join("scratch"))
        .args(["--receipts-dir"])
        .arg(root.join("receipts"))
        .args(["--ceiling", "{}"])
        .env("PALETTE_JULIA_BIN", julia)
        .env("PATH", path)
        .env("JULIA_PROJECT", inherited_project.unwrap_or(project))
        .env("JULIA_DEPOT_PATH", depots)
        .env_remove("JULIA_LOAD_PATH")
        .env_remove("PALETTE_MODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    ManagedChild(command.spawn().unwrap())
}

fn bwrap_trap(root: &Path) -> (PathBuf, PathBuf) {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let called = root.join("bwrap-called");
    let trap = bin.join("bwrap");
    fs::write(
        &trap,
        format!("#!/bin/sh\n: > {}\nexit 91\n", called.display()),
    )
    .unwrap();
    fs::set_permissions(&trap, fs::Permissions::from_mode(0o755)).unwrap();
    (bin, called)
}

#[test]
fn case_operator_cli_001_defaults_to_host_mode_and_kills_its_child_group_on_close() {
    let tmp = Temp::new("close");
    let (bin, bwrap_called) = bwrap_trap(&tmp.0);
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let repo = checkout();
    let julia = julia();
    let project = tmp.0.join("project");
    let depots = format!("{}:{}", tmp.0.join("depot").display(), base_depots());
    prepare_project(&julia, &project, &repo, None, &depots);
    let mut child = operator_session(&tmp.0, &path, &project, None, Some(&depots));
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let hello = line(&mut stdout);
    assert_eq!(hello["kind"], "HELLO", "{hello}");
    assert_eq!(
        hello["mode"], "operator",
        "omitting --mode must select operator"
    );
    child.stdin.as_mut().unwrap().write_all(
        format!("{}\n", json!({"request_id":"1", "kind":"EPHEMERAL", "code":"1 + 1"})).as_bytes()
    ).unwrap();
    child.stdin.as_mut().unwrap().flush().unwrap();
    let ephemeral = line(&mut stdout);
    assert_eq!(ephemeral["success"], true, "{ephemeral}");
    assert_eq!(ephemeral["data"], 2);
    child.stdin.as_mut().unwrap().write_all(
        format!("{}\n", json!({"request_id":"2", "code":"helper = run(`sleep 300`; wait=false); getpid(helper)"})).as_bytes()
    ).unwrap();
    child.stdin.as_mut().unwrap().flush().unwrap();
    let result = line(&mut stdout);
    assert_eq!(result["success"], true, "{result}");
    let pid = result["data"].as_i64().unwrap() as libc::pid_t;
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int };
    assert!(
        pidfd >= 0,
        "pidfd_open({pid}): {}",
        std::io::Error::last_os_error()
    );
    drop(child.stdin.take());
    let status = child.wait().unwrap();
    assert!(status.success(), "host exited {status}");
    let mut poll = libc::pollfd {
        fd: pidfd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut poll, 1, 10_000) };
    unsafe { libc::close(pidfd) };
    assert_eq!(
        ready, 1,
        "operator helper {pid} outlived its kernel process group"
    );
    assert!(
        !bwrap_called.exists(),
        "default operator mode invoked bwrap"
    );
}

#[test]
fn case_operator_cli_002_candidate_checkout_precedes_an_unrelated_global_palette() {
    let tmp = Temp::new("load-path");
    let project = tmp.0.join("empty-project");
    fs::create_dir_all(&project).unwrap();
    let fixture = tmp.0.join("selected-fixture");
    fs::create_dir_all(fixture.join("src")).unwrap();
    let fixture_uuid = "56f4f6b2-1c18-4a62-975f-006dc71cdb10";
    fs::write(fixture.join("Project.toml"), format!(
        "name = \"LocalFixture\"\nuuid = \"{fixture_uuid}\"\nversion = \"0.1.0\"\n[deps]\n")).unwrap();
    fs::write(fixture.join("src/LocalFixture.jl"), "module LocalFixture; const ORIGIN = :selected; end\n").unwrap();
    let depot = tmp.0.join("depot");
    let unrelated = tmp.0.join("unrelated/Palette");
    let julia = julia();
    let version = Command::new(&julia)
        .args([
            "--startup-file=no",
            "-e",
            "print(\"v\", VERSION.major, \".\", VERSION.minor)",
        ])
        .output()
        .unwrap();
    assert!(version.status.success(), "Julia version query failed");
    let version = String::from_utf8(version.stdout).unwrap();
    let env_dir = depot.join("environments").join(&version);
    fs::create_dir_all(&env_dir).unwrap();
    fs::create_dir_all(unrelated.join("src")).unwrap();
    let palette_project = fs::read_to_string(checkout().join("Project.toml")).unwrap();
    let uuid = palette_project
        .lines()
        .find_map(|line| line.strip_prefix("uuid = "))
        .unwrap()
        .trim_matches('"');
    fs::write(
        unrelated.join("Project.toml"),
        format!("name = \"Palette\"\nuuid = \"{uuid}\"\nversion = \"0.1.0\"\n"),
    )
    .unwrap();
    fs::write(
        unrelated.join("src/Palette.jl"),
        "module Palette; const ORIGIN = :unrelated; end\n",
    )
    .unwrap();
    fs::write(env_dir.join("Project.toml"), format!(
        "name = \"GlobalEnvironment\"\nuuid = \"28e8f9c2-8a86-4d50-8c73-7d5a3119ad67\"\nversion = \"0.1.0\"\n[deps]\nPalette = \"{uuid}\"\n")).unwrap();
    fs::write(env_dir.join("Manifest.toml"), format!(
        "manifest_format = \"2.0\"\njulia_version = \"{}.0\"\n[[deps.Palette]]\npath = \"{}\"\nuuid = \"{uuid}\"\nversion = \"0.1.0\"\n",
        &version[1..], unrelated.display())).unwrap();
    let depots = format!("{}:{}", depot.display(), base_depots());
    let repo = checkout();
    prepare_project(&julia, &project, &repo, Some(&fixture), &depots);
    let inherited_project = tmp.0.join("inherited-project");
    fs::create_dir_all(&inherited_project).unwrap();
    fs::write(inherited_project.join("Project.toml"), "[deps]\n").unwrap();
    let probe = Command::new(&julia)
        .args([
            "--startup-file=no",
            "-e",
            "using Palette; print(pathof(Palette))",
        ])
        .env("JULIA_PROJECT", &project)
        .env("JULIA_DEPOT_PATH", &depots)
        .env_remove("JULIA_LOAD_PATH")
        .output()
        .unwrap();
    assert!(
        probe.status.success(),
        "global fixture failed: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    assert_eq!(
        PathBuf::from(String::from_utf8(probe.stdout).unwrap())
            .canonicalize()
            .unwrap(),
        unrelated.join("src/Palette.jl").canonicalize().unwrap(),
        "fixture must expose the unrelated package"
    );

    let (bin, _) = bwrap_trap(&tmp.0);
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let mut child = operator_session(&tmp.0, &path, &project, Some(&inherited_project), Some(&depots));
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    assert_eq!(line(&mut stdout)["mode"], "operator");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            format!(
                "{}\n",
                json!({"request_id":"1", "code":"using LocalFixture; string(pathof(Palette), \"\\n\", Base.active_project(), \"\\n\", LocalFixture.ORIGIN)"})
            )
            .as_bytes(),
        )
        .unwrap();
    child.stdin.as_mut().unwrap().flush().unwrap();
    let result = line(&mut stdout);
    assert_eq!(result["success"], true, "{result}");
    let mut data = result["data"].as_str().unwrap().lines();
    let loaded = PathBuf::from(data.next().unwrap())
        .canonicalize()
        .unwrap();
    assert_eq!(loaded, repo.join("src/Palette.jl").canonicalize().unwrap());
    assert_eq!(PathBuf::from(data.next().unwrap()), project.join("Project.toml"));
    assert_eq!(data.next(), Some("selected"));
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

#[test]
fn case_operator_cli_003_invalid_mode_fails_before_creating_scratch() {
    let tmp = Temp::new("invalid");
    let scratch = tmp.0.join("scratch");
    let output = Command::new(env!("CARGO_BIN_EXE_palette-host"))
        .args(["session", "--mode", "unknown", "--project-dir"])
        .arg(checkout())
        .args(["--scratch-root"])
        .arg(&scratch)
        .args(["--ceiling", "{}"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("mode must be"));
    assert!(!scratch.exists());
}

#[test]
fn case_operator_cli_004_session_mode_environment_is_resolved_before_startup() {
    let tmp = Temp::new("invalid-env");
    let scratch = tmp.0.join("scratch");
    let output = Command::new(env!("CARGO_BIN_EXE_palette-host"))
        .args(["session", "--project-dir"])
        .arg(checkout())
        .args(["--scratch-root"])
        .arg(&scratch)
        .args(["--ceiling", "{}"])
        .env("PALETTE_MODE", "unknown")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("mode must be"));
    assert!(!scratch.exists());
}

#[test]
fn case_operator_cli_005_project_can_contain_its_owned_scratch_directory() {
    let tmp = Temp::new("project-scratch");
    let repo = checkout();
    let julia = julia();
    let depots = format!("{}:{}", tmp.0.join("depot").display(), base_depots());
    prepare_project(&julia, &tmp.0, &repo, None, &depots);
    let (bin, bwrap_called) = bwrap_trap(&tmp.0);
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let mut child = operator_session(&tmp.0, &path, &tmp.0, None, Some(&depots));
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    assert_eq!(line(&mut stdout)["mode"], "operator");
    child.stdin.as_mut().unwrap().write_all(
        format!("{}\n", json!({"request_id":"1", "code":"pwd()"})).as_bytes()
    ).unwrap();
    child.stdin.as_mut().unwrap().flush().unwrap();
    let result = line(&mut stdout);
    assert_eq!(result["success"], true, "{result}");
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    assert!(!bwrap_called.exists(), "operator mode invoked bwrap");
}
