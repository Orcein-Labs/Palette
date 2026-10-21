//! The operator verification gates: Julia, Rust and the real conformance
//! suites against the Rust host, whole or as shard I of N.

use crate::timings::{self, Shard};
use crate::Fail;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Prints a unittest module's case ids as `Class.method`. The suites stay
/// Python until they move into the host's own tests.
const LIST_TESTS: &str = r#"
import json, sys, unittest
def ids(suite):
    for test in suite:
        yield from ids(test) if isinstance(test, unittest.TestSuite) else [test.id().split(".", 1)[1]]
print(json.dumps(list(ids(unittest.defaultTestLoader.loadTestsFromName(sys.argv[1])))))
"#;

/// Suites that start no session host.
const SHARED_SUITES: [&str; 1] = ["runtime/security/test_workspace_router.py"];
/// Suites run against the host binary.
const HOST_SUITES: [&str; 4] = ["test_authority.py", "test_session.py", "test_session_cli.py", "test_revival.py"];
/// The ported suites: test targets of runtime/conformance, run against the host binary.
const CONFORMANCE_TARGETS: [&str; 7] = ["palette_daemon", "time_machine", "xray", "contexts", "host_bridge", "host_command", "tools"];
const CONFORMANCE_MANIFEST: &str = "runtime/conformance/Cargo.toml";

macro_rules! argv {
    ($($word:expr),* $(,)?) => { vec![$(OsString::from($word)),*] };
}

enum Suite {
    /// A unittest module; its cases are `Class.method`.
    Python(PathBuf),
    /// A conformance test target; its cases are the test functions.
    Rust(&'static str),
}

struct Run {
    suite: Suite,
    /// PALETTE_HOST_BIN for the suite, or unset for one that starts no host.
    host: Option<PathBuf>,
}

impl Run {
    /// The suite's name in case keys and messages.
    fn name(&self) -> &str {
        match &self.suite {
            Suite::Python(p) => stem(p),
            Suite::Rust(target) => target,
        }
    }
}

struct Gates {
    root: PathBuf,
    project: PathBuf,
}

impl Gates {
    fn command(&self, argv: &[OsString], host: Option<&Path>) -> Command {
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..])
            .current_dir(&self.root)
            .env("PALETTE_REPO_DIR", &self.root)
            .env("PALETTE_TEST_PROJECT_DIR", &self.project)
            .stdin(Stdio::null());
        match host {
            Some(h) => c.env("PALETTE_HOST_BIN", h),
            None => c.env_remove("PALETTE_HOST_BIN"),
        };
        c
    }

    fn run(&self, argv: Vec<OsString>, host: Option<&Path>) -> Result<(), Fail> {
        let shown = argv.iter().map(|w| w.to_string_lossy()).collect::<Vec<_>>().join(" ");
        println!("\nverification: {shown}");
        let status = self.command(&argv, host).status().map_err(|e| format!("{shown}: {e}"))?;
        if !status.success() {
            return Err(Fail::Failed(format!("verification failed ({status}): {shown}")));
        }
        Ok(())
    }

    /// `cargo test` of one conformance target, with the arguments for libtest.
    fn cargo_test(&self, target: &str, libtest: &[&str]) -> Vec<OsString> {
        let mut words = argv!["cargo", "test", "--release", "--locked", "--manifest-path", self.root.join(CONFORMANCE_MANIFEST), "--test", target, "--"];
        words.extend(libtest.iter().map(OsString::from));
        words
    }

    /// Every case in `runs`, as (run, case), in listing order.
    fn cases(&self, runs: &[Run]) -> Result<Vec<(usize, String)>, Fail> {
        let mut cases = Vec::new();
        for (i, run) in runs.iter().enumerate() {
            let name = run.name();
            let mut command = match &run.suite {
                Suite::Python(suite) => {
                    let mut c = self.command(&["python3".into(), "-c".into(), LIST_TESTS.into(), name.into()], run.host.as_deref());
                    c.current_dir(suite.parent().expect("suites live in a directory"));
                    c
                }
                Suite::Rust(target) => self.command(&self.cargo_test(target, &["--list", "--format", "terse"]), run.host.as_deref()),
            };
            let out = command.stderr(Stdio::inherit()).output().map_err(|e| format!("listing {name}: {e}"))?;
            if !out.status.success() {
                return Err(Fail::Failed(format!("listing the cases of {name} failed ({})", out.status)));
            }
            let ids: Vec<String> = match run.suite {
                Suite::Python(_) => serde_json::from_slice(&out.stdout).map_err(|e| format!("listing {name}: {e}"))?,
                Suite::Rust(_) => String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.strip_suffix(": test")).map(str::to_string).collect(),
            };
            cases.extend(ids.into_iter().map(|id| (i, id)));
        }
        Ok(cases)
    }

    fn run_suites(&self, shard: &Shard, runs: &[Run]) -> Result<(), Fail> {
        let mine: Option<Vec<(usize, String)>> = if shard.count == 1 {
            None
        } else {
            let cases = self.cases(runs)?;
            let keys: Vec<String> = cases.iter().map(|(i, id)| format!("{}:{id}", runs[*i].name())).collect();
            let dealt = shard.deal(&keys, &timings::load(&self.root.join(timings::WEIGHTS))?);
            Some(cases.into_iter().zip(dealt).filter_map(|(case, m)| m.then_some(case)).collect())
        };
        for (i, run) in runs.iter().enumerate() {
            let mut words = match &run.suite {
                Suite::Python(suite) => argv!["python3", suite, "-v"],
                Suite::Rust(target) => self.cargo_test(target, &["--test-threads=1"]),
            };
            if let Some(mine) = &mine {
                let tests: Vec<&String> = mine.iter().filter(|(r, _)| *r == i).map(|(_, id)| id).collect();
                if tests.is_empty() {
                    let shown = match &run.suite {
                        Suite::Python(suite) => file_name(suite),
                        Suite::Rust(target) => format!("conformance target {target}"),
                    };
                    println!("\nverification: {shown} has no cases in shard {}/{}", shard.index, shard.count);
                    continue;
                }
                if let Suite::Rust(_) = run.suite {
                    words.push("--exact".into());
                }
                words.extend(tests.into_iter().map(OsString::from));
            }
            self.run(words, run.host.as_deref())?;
        }
        Ok(())
    }
}

fn stem(p: &Path) -> &str {
    p.file_stem().and_then(|s| s.to_str()).unwrap_or_default()
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .filter(|d| !d.as_os_str().is_empty())
            .any(|d| d.join(name).metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
    })
}

struct Args {
    project: PathBuf,
    shard: Shard,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut flags = crate::flags(args, &["--project-dir", "--shard"])?;
    let project = flags.remove("--project-dir").ok_or("--project-dir is required")?;
    let project = std::path::absolute(&project).map_err(|e| format!("{project}: {e}"))?;
    Ok(Args { project, shard: Shard::parse(flags.get("--shard").map_or("1/1", String::as_str))? })
}

pub fn main(args: &[String]) -> Result<(), Fail> {
    let Args { project, shard } = parse(args).map_err(Fail::Usage)?;
    if !project.join("Project.toml").is_file() || !project.join("Manifest.toml").is_file() {
        return Err(Fail::Usage("the test project must be instantiated before verification".into()));
    }
    for binary in ["bwrap", "julia", "cargo", "node", "python3"] {
        if !on_path(binary) {
            return Err(Fail::Usage(format!("required verification binary missing: {binary}")));
        }
    }
    let root = crate::root();
    let gates = Gates { root: root.clone(), project: project.clone() };
    let host_manifest = root.join("runtime/host/Cargo.toml");
    // This is a test dependency check, not an automatic operator installation.
    gates.run(
        argv![
            "julia",
            "--startup-file=no",
            format!("--project={}", project.display()),
            "-e",
            "using Palette, JSON, CSV, DataFrames, EzXML, IJulia; \
             realpath(dirname(dirname(pathof(Palette)))) == realpath(ARGS[1]) || \
             error(\"Test environment points to a different Palette source tree\")",
            &root,
        ],
        None,
    )?;
    if shard.index == 1 {
        gates.run(argv!["git", "diff", "--check"], None)?;
        gates.run(argv!["node", root.join("runtime/scripts/check-test-policy.mjs")], None)?;
        gates.run(argv!["julia", "--startup-file=no", format!("--project={}", root.display()), root.join("test/runtests.jl")], None)?;
        gates.run(argv!["julia", "--startup-file=no", format!("--project={}", root.display()), root.join("runtime/integrations/gesso/test_gesso.jl")], None)?;
        gates.run(argv!["cargo", "test", "--manifest-path", &host_manifest], None)?;
        gates.run(argv!["cargo", "test", "--locked", "--manifest-path", root.join("runtime/xtask/Cargo.toml")], None)?;
    }
    gates.run(argv!["cargo", "build", "--release", "--manifest-path", &host_manifest], None)?;
    // Through cargo, which has just built it, so the stubbed tests run no real host.
    gates.run(
        argv!["cargo", "run", "--release", "--quiet", "--manifest-path", &host_manifest, "--bin", "palette-host", "--",
              "prepare", "--project-dir", &project, "--repo-dir", &root],
        None,
    )?;
    let host = root.join("runtime/host/target/release/palette-host");
    let mut runs: Vec<Run> = SHARED_SUITES.iter().map(|s| Run { suite: Suite::Python(root.join(s)), host: None }).collect();
    runs.extend(HOST_SUITES.iter().map(|s| Run { suite: Suite::Python(root.join("runtime/security").join(s)), host: Some(host.clone()) }));
    runs.extend(CONFORMANCE_TARGETS.iter().map(|t| Run { suite: Suite::Rust(t), host: Some(host.clone()) }));
    gates.run_suites(&shard, &runs)?;
    println!("\nAll operator verification gates passed (shard {}/{}).", shard.index, shard.count);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_flags_with_and_without_equals() {
        let a = parse(&strings(&["--project-dir", "env", "--shard=3/4"])).unwrap();
        assert!(a.project.is_absolute() && a.project.ends_with("env"));
        assert_eq!((a.shard.index, a.shard.count), (3, 4));
        let d = parse(&strings(&["--project-dir=/e"])).unwrap();
        assert_eq!((d.shard.index, d.shard.count), (1, 1));
        for bad in [&["--shard", "1/4"][..], &["--project-dir"], &["--project-dir", "e", "--implementation", "rust"], &["--project-dir", "e", "--shard", "5/4"], &["--project-dir", "e", "x"]] {
            assert!(parse(&strings(bad)).is_err(), "{bad:?} was accepted");
        }
    }
}
