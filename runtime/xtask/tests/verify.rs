//! `cargo xtask verify` end to end, with every tool it runs replaced by a stub
//! on PATH that logs its call: the gates' order, arguments, working directory
//! and host binary, without running a real gate.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Logs `name|cwd|PALETTE_HOST_BIN|args`. As python3, `-c` lists three cases
/// for the suite named by the code's argument, and fails a suite named in
/// FAIL_SUITE; as cargo, `--list` lists three cases of the `--test` target.
/// Any other stub named in FAIL_TOOL fails.
const STUB: &str = r#"#!/bin/sh
name=${0##*/}
if [ "$name" = python3 ] && [ "$1" = -c ]; then
  echo "$name|$PWD|${PALETTE_HOST_BIN-unset}|list $3" >> "$STUB_LOG"
  echo '["Case.test_one", "Case.test_two", "Case.test_three"]'
  exit 0
fi
case " $* " in *" --list "*)
  all="$*"
  target=${all#* --test }
  target=${target%% *}
  echo "$name|$PWD|${PALETTE_HOST_BIN-unset}|list $target" >> "$STUB_LOG"
  printf 'one: test\ntwo: test\nthree: test\n'
  exit 0
esac
echo "$name|$PWD|${PALETTE_HOST_BIN-unset}|$*" >> "$STUB_LOG"
[ "$name" = "$FAIL_TOOL" ] && exit 1
case "$1" in */"$FAIL_SUITE".py) exit 1 ;; esac
exit 0
"#;

const TOOLS: [&str; 6] = ["bwrap", "julia", "cargo", "node", "python3", "git"];

struct Fixture {
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct Call {
    tool: String,
    cwd: String,
    host: String,
    args: String,
}

impl Fixture {
    fn new() -> Fixture {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("verify-{}-{}", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("project")).unwrap();
        for tool in TOOLS {
            let stub = dir.join("bin").join(tool);
            fs::write(&stub, STUB).unwrap();
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for file in ["Project.toml", "Manifest.toml"] {
            fs::write(dir.join("project").join(file), "").unwrap();
        }
        Fixture { dir }
    }

    fn verify(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_xtask"));
        c.arg("verify")
            .args(["--project-dir", self.dir.join("project").to_str().unwrap()])
            .args(args)
            .env("PATH", self.dir.join("bin"))
            .env("STUB_LOG", self.dir.join("calls.log"))
            // An inherited host must never reach a gate that should not see it.
            .env("PALETTE_HOST_BIN", "/inherited/palette-host")
            .env_remove("FAIL_TOOL")
            .env_remove("FAIL_SUITE");
        c.envs(env.iter().copied()).output().unwrap()
    }

    fn calls(&self) -> Vec<Call> {
        let log = fs::read_to_string(self.dir.join("calls.log")).unwrap_or_default();
        fs::remove_file(self.dir.join("calls.log")).ok();
        log.lines()
            .map(|l| {
                let mut f = l.splitn(4, '|').map(str::to_string);
                Call { tool: f.next().unwrap(), cwd: f.next().unwrap(), host: f.next().unwrap(), args: f.next().unwrap() }
            })
            .collect()
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().to_path_buf()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

const CONFORMANCE_TARGETS: [&str; 3] = ["palette_daemon", "time_machine", "xray"];

/// The suite a call ran (`python3 <dir>/<stem>.py -v ...`, or the conformance
/// `cargo test ... --test <target> -- ...`), if it ran one.
fn suite(call: &Call) -> Option<&str> {
    if call.tool == "cargo" {
        let (_, target) = call.args.split_once("/runtime/conformance/Cargo.toml --test ")?;
        return target.split(' ').next();
    }
    let script = call.args.split(' ').next()?;
    let stem = Path::new(script).file_stem()?.to_str()?;
    (call.tool == "python3" && stem.starts_with("test_")).then_some(stem)
}

/// The cases a suite call names: after `-v` for unittest, after `--exact` for libtest.
fn named_cases(call: &Call) -> Vec<&str> {
    let after = if call.tool == "cargo" { " --exact " } else { ".py -v " };
    call.args.split_once(after).map_or(Vec::new(), |(_, cases)| cases.split(' ').collect())
}

#[test]
fn runs_every_gate_then_each_suite_whole_against_the_host() {
    let f = Fixture::new();
    let out = f.verify(&[], &[]);
    assert!(out.status.success(), "{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
    let calls = f.calls();
    let root = root();
    let r = root.display();
    let host = format!("{r}/runtime/host/target/release/palette-host");
    for call in &calls {
        assert_eq!(call.cwd, r.to_string(), "{} ran outside the checkout", call.args);
    }
    let gates: Vec<String> = calls.iter().take(9).map(|c| format!("{} {}", c.tool, c.args)).collect();
    assert!(gates[0].starts_with(&format!("julia --startup-file=no --project={} -e using Palette", f.dir.join("project").display())));
    assert!(gates[0].ends_with(&format!(" {r}")));
    assert_eq!(
        gates[1..],
        [
            "git diff --check".to_string(),
            format!("node {r}/runtime/scripts/check-test-policy.mjs"),
            format!("julia --startup-file=no --project={r} {r}/test/runtests.jl"),
            format!("julia --startup-file=no --project={r} {r}/runtime/integrations/gesso/test_gesso.jl"),
            format!("cargo test --manifest-path {r}/runtime/host/Cargo.toml"),
            format!("cargo test --locked --manifest-path {r}/runtime/xtask/Cargo.toml"),
            format!("cargo build --release --manifest-path {r}/runtime/host/Cargo.toml"),
            format!("cargo run --release --quiet --manifest-path {r}/runtime/host/Cargo.toml --bin palette-host -- prepare --project-dir {} --repo-dir {r}", f.dir.join("project").display()),
        ]
    );
    assert!(calls.iter().take(9).all(|c| c.host == "unset"));
    let suites: Vec<(&str, &str)> = calls[9..].iter().map(|c| (suite(c).unwrap(), c.host.as_str())).collect();
    let hosted = ["test_authority", "test_session", "test_session_cli", "test_revival", "test_host_bridge", "test_contexts", "test_tools", "test_host_command"];
    let expected: Vec<(&str, &str)> = [("test_workspace_router", "unset")]
        .into_iter()
        .chain(hosted.iter().chain(&CONFORMANCE_TARGETS).map(|s| (*s, host.as_str())))
        .collect();
    assert_eq!(suites, expected);
    assert!(calls[9..18].iter().all(|c| c.args.ends_with(".py -v")), "a whole run names no cases");
    assert!(calls[9].args.starts_with(&format!("{r}/runtime/security/test_workspace_router.py")));
    assert_eq!(
        calls[18].args,
        format!("test --release --locked --manifest-path {r}/runtime/conformance/Cargo.toml --test palette_daemon -- --test-threads=1")
    );
    let text = stdout(&out);
    assert!(!text.contains("operator implementation:"));
    assert!(text.contains(&format!("\nverification: julia --startup-file=no --project={r} {r}/test/runtests.jl\n")));
    assert!(text.ends_with("\nAll operator verification gates passed (shard 1/1).\n"));
}

#[test]
fn shards_deal_each_listed_case_to_exactly_one_shard() {
    let f = Fixture::new();
    let mut ran: Vec<(String, String)> = Vec::new();
    let mut listed = BTreeSet::new();
    for index in 1..=4 {
        let shard = format!("{index}/4");
        let out = f.verify(&["--shard", &shard], &[]);
        let text = stdout(&out);
        assert!(out.status.success(), "{text}");
        assert!(text.ends_with(&format!("\nAll operator verification gates passed (shard {shard}).\n")));
        let calls = f.calls();
        assert_eq!(calls.iter().any(|c| c.tool == "git"), index == 1, "the shared gates run on shard 1 only");
        assert!(calls.iter().any(|c| c.args.starts_with("build --release")));
        for call in &calls {
            if let Some(stem) = call.args.strip_prefix("list ") {
                if call.tool == "cargo" {
                    assert!(CONFORMANCE_TARGETS.contains(&stem), "{stem} is not a conformance target");
                } else {
                    assert!(Path::new(&call.cwd).join(format!("{stem}.py")).is_file(), "{stem} was listed outside its directory");
                }
                assert_ne!(call.host, "/inherited/palette-host", "{stem} was listed with the inherited host");
                listed.insert(stem.to_string());
            } else if let Some(stem) = suite(call) {
                let cases = named_cases(call);
                assert!(!cases.is_empty(), "a sharded run names its cases");
                ran.extend(cases.into_iter().map(|c| (stem.to_string(), c.to_string())));
            }
        }
        let skipped = text.matches(&format!("has no cases in shard {shard}")).count();
        let suites_run = calls.iter().filter(|c| suite(c).is_some()).count();
        assert_eq!(skipped + suites_run, 12, "every suite runs or says it has no cases");
    }
    assert_eq!(listed.len(), 12);
    let cases = |s: &str| if CONFORMANCE_TARGETS.contains(&s) { ["one", "two", "three"] } else { ["Case.test_one", "Case.test_two", "Case.test_three"] };
    let mut expected: Vec<(String, String)> = listed.iter().flat_map(|s| cases(s).map(|c| (s.clone(), c.to_string()))).collect();
    expected.sort();
    ran.sort();
    assert_eq!(ran, expected, "each case runs on exactly one shard");
}

#[test]
fn a_failing_gate_stops_verification() {
    let f = Fixture::new();
    let out = f.verify(&[], &[("FAIL_TOOL", "node")]);
    assert_eq!(out.status.code(), Some(1));
    let calls = f.calls();
    assert_eq!(calls.last().unwrap().tool, "node");
    assert!(String::from_utf8_lossy(&out.stderr).contains("verification failed"));
    assert!(!stdout(&out).contains("All operator verification gates passed"));

    let out = f.verify(&[], &[("FAIL_SUITE", "test_session")]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(suite(f.calls().last().unwrap()), Some("test_session"));
}

#[test]
fn refuses_an_uninstantiated_project_or_a_missing_tool() {
    let f = Fixture::new();
    fs::remove_file(f.dir.join("project/Manifest.toml")).unwrap();
    let out = f.verify(&[], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("must be instantiated"));
    fs::write(f.dir.join("project/Manifest.toml"), "").unwrap();
    fs::remove_file(f.dir.join("bin/bwrap")).unwrap();
    let out = f.verify(&[], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("required verification binary missing: bwrap"));
    assert!(f.calls().is_empty());
}
