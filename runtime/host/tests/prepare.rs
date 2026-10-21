//! `palette-host prepare` end to end with a fake Julia and a fake prewarm
//! host, in a temporary directory: the real depot and package store are never
//! touched.

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Fixture {
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write(path: &Path, contents: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = tmp.join(format!("palette-prepare-it-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write(&root.join("toolchain/bin/julia"), "#!/bin/sh\necho 'julia version 1.12.6'\n", 0o755);
        write(&root.join("project/Project.toml"), "", 0o644);
        write(&root.join("project/src/inner/A.jl"), "a", 0o644);
        write(&root.join("repo/runtime/scripts/s.jl"), "s", 0o644);
        write(&root.join("depot/compiled/A.ji"), "ji", 0o644);
        fs::create_dir_all(root.join("home")).unwrap();
        Fixture { root }
    }

    fn host(&self, name: &str, script: &str) -> PathBuf {
        let host = self.root.join(name);
        write(&host, &format!("#!/bin/sh\n{script}\n"), 0o755);
        host
    }

    /// The printed summary, the record file it names and the exit code.
    fn prepare(&self, host: &Path, extra: &[&str]) -> (Value, Value, i32) {
        let at = |p: &str| self.root.join(p);
        let out = Command::new(env!("CARGO_BIN_EXE_palette-host"))
            .arg("prepare").arg("--project-dir").arg(at("project")).arg("--repo-dir").arg(at("repo"))
            .arg("--state-dir").arg(at("state")).arg("--host-bin").arg(host).args(extra)
            .env("HOME", at("home")).env("PALETTE_JULIA_BIN", at("toolchain/bin/julia"))
            .env("PALETTE_PACKAGE_DEPOT", at("packages")).env("JULIA_DEPOT_PATH", at("depot"))
            .env_remove("PALETTE_TASK_ENV")
            .output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let summary: Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("{e}: {stdout} {}", String::from_utf8_lossy(&out.stderr)));
        let record = fs::read_to_string(summary["record_file"].as_str().unwrap()).map_or(Value::Null, |t| serde_json::from_str(&t).unwrap());
        (summary, record, out.status.code().unwrap())
    }
}

#[test]
fn prepares_reuses_fails_and_times_out() {
    let f = Fixture::new();
    let cache = f.root.join("depot/compiled/A.ji");
    let host = f.host("prewarm-ok", &format!("echo \"$@\"; echo 'palette_preparation_caches=[\"{}\"]'; echo warn >&2", cache.display()));

    let (summary, record, code) = f.prepare(&host, &["--status"]);
    assert_eq!((summary["state"].as_str(), code), (Some("cold"), 0));
    assert!(record.is_null());
    let sources: Vec<_> = summary["identity"]["source"].as_object().unwrap().keys().cloned().collect();
    let expected = ["project/Project.toml", "project/src/inner/A.jl", "repo/runtime/scripts/s.jl"].map(|p| f.root.join(p).to_string_lossy().into_owned());
    assert_eq!(sources, expected);
    assert_eq!(summary["identity"]["runtime"]["version"], "julia version 1.12.6");

    let (summary, record, code) = f.prepare(&host, &[]);
    assert_eq!((summary["state"].as_str(), summary["previous_state"].as_str(), code), (Some("prepared"), Some("cold"), 0));
    assert!(summary.get("output").is_none() && summary.get("stderr").is_none());
    let project = f.root.join("project");
    assert!(record["output"].as_str().unwrap().starts_with(&format!("prewarm --project-dir {} --repo-dir {}", project.display(), f.root.join("repo").display())));
    assert_eq!((record["stderr"].as_str(), record["error"].is_null()), (Some("warn\n"), true));
    assert!(record["cache_files"][cache.to_str().unwrap()].is_string());
    let mode = fs::metadata(summary["record_file"].as_str().unwrap()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let (summary, _, code) = f.prepare(&f.host("prewarm-fails", "exit 3"), &[]);
    assert_eq!((summary["reused"].as_bool(), summary.get("previous_state"), code), (Some(true), None, 0));

    let (summary, _, code) = f.prepare(&f.root.join("prewarm-fails"), &["--force"]);
    assert_eq!((summary["state"].as_str(), summary["returncode"].as_i64(), summary["previous_state"].as_str(), code),
               (Some("failed"), Some(3), Some("prepared"), 1));

    fs::write(&cache, "changed").unwrap();
    let (summary, _, _) = f.prepare(&host, &["--force"]);
    assert_eq!(summary["state"], "prepared");
    fs::write(&cache, "changed again").unwrap();
    let (summary, _, _) = f.prepare(&host, &["--status"]);
    assert_eq!((summary["state"].as_str(), summary["reason"].as_str()), (Some("stale"), Some("prepared cache missing or changed")));

    fs::write(f.root.join("project/src/inner/A.jl"), "b").unwrap();
    let (summary, _, _) = f.prepare(&host, &["--status"]);
    assert_eq!((summary["state"].as_str(), summary["error"].is_null()), (Some("stale"), true));
    assert!(summary["reason"].as_str().unwrap().contains("changed"));

    let started = Instant::now();
    let (summary, record, code) = f.prepare(&f.host("prewarm-hangs", "echo 'Precompiling Stuck' >&2\nexec sleep 30"), &["--force", "--timeout-s", "1"]);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!((summary["state"].as_str(), summary["timed_out"].as_bool(), summary["returncode"].as_i64(), code),
               (Some("failed"), Some(true), Some(-9), 1));
    let error = summary["error"].as_str().unwrap();
    assert!(error.contains("did not finish in 1 s") && error.contains("Precompiling Stuck"), "{error}");
    assert!(record["stderr"].as_str().unwrap().starts_with("prewarm did not finish in 1 s; its output so far:\nPrecompiling Stuck"));

    let (summary, _, code) = f.prepare(&f.root.join("nope"), &["--force"]);
    assert!(summary["error"].as_str().unwrap().starts_with("FileNotFoundError: [Errno 2] No such file or directory: "), "{summary}");
    assert_eq!(code, 1);
}
