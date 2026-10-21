//! The preparation record, ported from `prewarm_depot.py`: whether the depot
//! has been prewarmed for this project, Palette's sources and this Julia, and
//! the run of `palette-host prewarm` that makes it so.
//!
//! The record lives at `<depot>/.palette-preparation/<key>.json` (or in a
//! given state directory), keyed by the project, the repo and the depot. A
//! record whose identity no longer matches, or whose recorded caches changed,
//! reads as `stale`; an interrupted run reads as `failed`.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::{atomic_json, Fault};
use crate::{sandbox, util, PREWARM};

const SCHEMA: &str = "palette.preparation.v1";
const CACHES_LINE: &str = "palette_preparation_caches=";

fn file_digest(path: &Path) -> std::io::Result<String> {
    fs::read(path).map(|b| util::sha256_hex(&b))
}

/// Julia's error output without colour codes, cut in the middle: the cause
/// leads and the stack ends it.
pub(crate) fn error_summary(text: &str) -> String {
    const HEAD: usize = 1000;
    const TAIL: usize = 3000;
    // Drops `ESC [ [0-9;?]* letter`.
    let mut plain = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('\x1b') {
        plain.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let params = after.strip_prefix('[').map(|p| p.trim_start_matches(|c: char| c.is_ascii_digit() || c == ';' || c == '?'));
        match params.and_then(|p| p.chars().next().filter(char::is_ascii_alphabetic).map(|_| &p[1..])) {
            Some(tail) => rest = tail,
            None => {
                plain.push('\x1b');
                rest = after;
            }
        }
    }
    plain.push_str(rest);
    let chars: Vec<char> = plain.trim().chars().collect();
    if chars.len() <= HEAD + TAIL {
        return chars.into_iter().collect();
    }
    let head: String = chars[..HEAD].iter().collect();
    let tail: String = chars[chars.len() - TAIL..].iter().collect();
    format!("{head}\n[... {} characters left out ...]\n{tail}", chars.len() - HEAD - TAIL)
}

/// `path` made absolute against the working directory without resolving
/// symlinks or `..`, as Python's `Path.absolute()`.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()).components().collect()
}

/// Files under `dir` whose names end with `suffix`, recursing into real
/// directories only when `recursive`.
fn files_ending(dir: &Path, suffix: &str, recursive: bool, out: &mut BTreeSet<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if recursive && is_dir {
            files_ending(&path, suffix, true, out);
        } else if entry.file_name().to_string_lossy().ends_with(suffix) && path.is_file() {
            out.insert(path);
        }
    }
}

fn digests(paths: BTreeSet<PathBuf>) -> Result<Value, Fault> {
    let mut map = Map::new();
    for p in paths {
        let digest = file_digest(&p).map_err(|e| Fault::new("OSError", format!("{e}: {}", p.display())))?;
        map.insert(p.to_string_lossy().into_owned(), Value::String(digest));
    }
    Ok(Value::Object(map))
}

/// What a preparation was made for: the sources it compiled, the workload,
/// the Julia that ran it, the task and package environments and the depot.
pub(crate) fn identity(project_dir: &Path, repo_dir: &Path, julia_bin: &str) -> Result<Value, Fault> {
    let (project, repo) = (absolute(project_dir), absolute(repo_dir));
    let mut sources = BTreeSet::new();
    files_ending(&repo.join("src"), ".jl", true, &mut sources);
    files_ending(&project.join("src"), ".jl", true, &mut sources);
    files_ending(&repo.join("runtime/scripts"), ".jl", false, &mut sources);
    files_ending(&project, "Project.toml", false, &mut sources);
    files_ending(&project, "Manifest.toml", false, &mut sources);

    let package_root = match std::env::var("PALETTE_PACKAGE_DEPOT") {
        Ok(p) if !p.is_empty() => util::expand_user(&p),
        _ => util::home().join(".palette/packages"),
    };
    let package_environment = util::resolve(&package_root).join("environment");
    let mut environment_files = BTreeSet::new();
    files_ending(&package_environment, "Project.toml", false, &mut environment_files);
    files_ending(&package_environment, "Manifest.toml", false, &mut environment_files);

    let out = Command::new(julia_bin).args(["--startup-file=no", "--version"]).stdin(Stdio::null()).output()
        .map_err(|e| Fault::new("OSError", format!("{e}: {julia_bin}")))?;
    if !out.status.success() {
        return Err(Fault::new("CalledProcessError", format!("{julia_bin} --startup-file=no --version failed ({})", out.status)));
    }
    let task_environment = match std::env::var("PALETTE_TASK_ENV") {
        Ok(p) if !p.is_empty() => Value::String(file_digest(Path::new(&p)).map_err(|e| Fault::new("OSError", format!("{e}: {p}")))?),
        _ => Value::Null,
    };
    Ok(json!({
        "project": project.to_string_lossy(),
        "repo": repo.to_string_lossy(),
        "source": digests(sources)?,
        "workload": util::sha256_hex(PREWARM.as_bytes()),
        "runtime": {"binary": julia_bin, "version": String::from_utf8_lossy(&out.stdout).trim(),
                    "machine": std::env::consts::ARCH},
        "task_environment": task_environment,
        "package_environment": {"path": package_environment.to_string_lossy(), "files": digests(environment_files)?},
        "depot": sandbox::default_depot(),
    }))
}

/// Where the record for `identity` lives. The key is the one Python wrote, so
/// records already on disk are found.
pub(crate) fn record_path(identity: &Value, state_dir: Option<&Path>) -> PathBuf {
    let field = |k: &str| util::py_json_str(identity[k].as_str().unwrap_or(""));
    let key = util::sha256_hex(format!("[{}, {}, {}]", field("project"), field("repo"), field("depot")).as_bytes());
    let dir = match state_dir {
        Some(dir) => dir.to_path_buf(),
        None => Path::new(identity["depot"].as_str().unwrap_or("")).join(".palette-preparation"),
    };
    dir.join(key + ".json")
}

/// Field 22 of /proc/<pid>/stat, the start time: a reused pid does not make
/// an interrupted preparation look alive.
fn process_start(pid: i64) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 2..].split_whitespace().nth(19).map(str::to_string)
}

fn alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    // EPERM means it exists under another user.
    pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

fn with(record: &Map<String, Value>, fields: Value) -> Value {
    let mut out = record.clone();
    out.extend(fields.as_object().cloned().unwrap_or_default());
    Value::Object(out)
}

/// The preparation state for `identity`: cold, preparing, prepared, failed or
/// stale.
pub(crate) fn status(identity: &Value, state_dir: Option<&Path>) -> Value {
    let failed = |error: String| json!({"state": "failed", "error": error, "identity": identity});
    let text = match fs::read_to_string(record_path(identity, state_dir)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return json!({"state": "cold", "identity": identity}),
        Err(e) => return failed(format!("preparation record unreadable: {e}")),
        Ok(text) => text,
    };
    let record = match serde_json::from_str(&text) {
        Err(e) => return failed(format!("preparation record unreadable: {e}")),
        Ok(Value::Object(record)) => record,
        Ok(_) => return failed("preparation record is not an object".into()),
    };
    if record.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
        return failed("unknown preparation record format".into());
    }
    let state = record.get("state").and_then(Value::as_str).unwrap_or("").to_string();
    let empty = Map::new();
    let caches = match record.get("cache_files") {
        None => Some(&empty),
        Some(v) => v.as_object(),
    };
    let (Some(caches), true) = (caches, ["preparing", "prepared", "failed"].contains(&state.as_str())) else {
        return failed("invalid preparation state or cache inventory".into());
    };
    if record.get("identity") != Some(identity) {
        // The old error belongs to sources that are gone; the next prepare replaces it.
        return with(&record, json!({"state": "stale", "error": null, "current_identity": identity,
                                    "reason": "the project, Palette's sources or Julia changed since this preparation"}));
    }
    if state == "prepared" && caches.iter().any(|(p, d)| file_digest(Path::new(p)).ok().as_deref() != d.as_str()) {
        return with(&record, json!({"state": "stale", "error": null, "reason": "prepared cache missing or changed"}));
    }
    if state == "preparing" {
        let pid = record.get("pid").and_then(Value::as_i64);
        match pid.filter(|&pid| alive(pid)) {
            None => return with(&record, json!({"state": "failed", "error": "preparation process disappeared"})),
            Some(pid) if process_start(pid).as_deref() != record.get("process_start").and_then(Value::as_str) => {
                return with(&record, json!({"state": "failed", "error": "preparation process identity changed"}));
            }
            Some(_) => {}
        }
    }
    let mut record = record;
    if state == "prepared" && record.get("returncode").and_then(Value::as_f64) == Some(0.0) {
        if !record.contains_key("stderr") {
            let error = record.get("error").and_then(Value::as_str).unwrap_or("").to_string();
            record.insert("stderr".into(), Value::String(error));
        }
        record.insert("error".into(), Value::Null);
    }
    Value::Object(record)
}

/// Prewarms the depot with `host_bin prewarm` and records the result, unless
/// a prepared record for the same identity exists and `force` is false. One
/// preparation runs at a time per record.
pub(crate) fn prepare(project_dir: &Path, repo_dir: &Path, state_dir: Option<&Path>, host_bin: &Path,
                      force: bool, timeout: Option<f64>) -> Result<Value, Fault> {
    let julia = sandbox::resolve_julia().map_err(|e| Fault::new("RuntimeError", e))?;
    let identity = identity(project_dir, repo_dir, &julia)?;
    let path = record_path(&identity, state_dir);
    let parent = path.parent().expect("a record path has a directory");
    let field = |k: &str| PathBuf::from(identity[k].as_str().unwrap_or(""));
    let mut protected = vec![field("project"), field("repo"), parent.to_path_buf()];
    protected.extend(state_dir.map(Path::to_path_buf));
    sandbox::package_store_paths(&protected, None).map_err(|e| Fault::new("RuntimeError", e))?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(Fault::io)?;

    let mut lock_name = path.clone().into_os_string();
    lock_name.push(".lock");
    let lock = OpenOptions::new().append(true).create(true).mode(0o600).open(&lock_name).map_err(Fault::io)?;
    // Released when `lock` closes.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(Fault::io(std::io::Error::last_os_error()));
    }
    let previous = status(&identity, state_dir);
    let previous_state = previous["state"].as_str().unwrap_or("").to_string();
    if previous_state == "prepared" && !force {
        let depot = Path::new(identity["depot"].as_str().unwrap_or(""));
        if !sandbox::has_depot_snapshot(depot) {
            if let Err(e) = sandbox::publish_depot_snapshot(depot) {
                eprintln!("palette-host: the depot snapshot was not published: {e}");
            }
        }
        let mut reused = previous.as_object().cloned().unwrap_or_default();
        // What came before this preparation no longer describes it.
        reused.remove("previous_state");
        reused.insert("reused".into(), Value::Bool(true));
        return Ok(Value::Object(reused));
    }

    let pid = std::process::id() as i64;
    let mut record = json!({"schema": SCHEMA, "state": "preparing", "identity": identity, "pid": pid,
                            "process_start": process_start(pid), "started_at": util::now_secs(),
                            "previous_state": previous_state});
    atomic_json(&path, &record).map_err(Fault::io)?;
    let started = Instant::now();
    let argv: Vec<String> = vec![host_bin.to_string_lossy().into_owned(), "prewarm".into(), "--project-dir".into(),
                                 project_dir.to_string_lossy().into_owned(), "--repo-dir".into(), repo_dir.to_string_lossy().into_owned()];
    let fields = record.as_object_mut().expect("a record is an object");
    let outcome = super::run_captured(&argv, None, timeout.map(Duration::from_secs_f64))
        .map_err(|e| format!("OSError: {e}: {}", argv[0]))
        .and_then(|(code, stdout, mut stderr, timed_out)| {
            if timed_out {
                // A hung prewarm otherwise ends as a cancelled CI step with no output at all.
                let so_far = if stderr.is_empty() { &stdout } else { &stderr };
                stderr = format!("prewarm did not finish in {} s; its output so far:\n{so_far}", util::py_g(timeout.unwrap_or(0.0)));
            }
            let error = (code != 0).then(|| error_summary(if stderr.is_empty() { &stdout } else { &stderr }));
            fields.extend(json!({"state": if code == 0 { "prepared" } else { "failed" }, "timed_out": timed_out,
                                 "returncode": code, "error": error}).as_object().cloned().unwrap_or_default());
            let caches = stdout.lines().find_map(|l| l.strip_prefix(CACHES_LINE)).unwrap_or("[]");
            fields.insert("output".into(), Value::String(stdout.clone()));
            fields.insert("stderr".into(), Value::String(stderr));
            let caches: Vec<String> = serde_json::from_str(caches).map_err(|e| format!("ValueError: {CACHES_LINE} {e}"))?;
            let mut inventory = Map::new();
            for p in caches {
                let digest = file_digest(Path::new(&p)).map_err(|e| format!("OSError: {e}: {p}"))?;
                inventory.insert(p, Value::String(digest));
            }
            fields.insert("cache_files".into(), Value::Object(inventory));
            fields.insert("cache_tracking".into(), json!("loaded_images"));
            Ok(())
        });
    if let Err(error) = outcome {
        fields.insert("state".into(), json!("failed"));
        fields.insert("error".into(), Value::String(error));
    }
    fields.insert("seconds".into(), json!(started.elapsed().as_secs_f64()));
    fields.insert("finished_at".into(), json!(util::now_secs()));
    // `palette-host prewarm` publishes its own snapshot.
    atomic_json(&path, &record).map_err(Fault::io)?;
    Ok(record)
}

const USAGE: &str = "usage: palette-host prepare --project-dir P [--repo-dir R] [--state-dir S] [--host-bin H] [--status] [--force] [--timeout-s T]";

/// `palette-host prepare`: prewarms the depot once per identity and prints the
/// record, without Julia's output (the record file keeps it), naming the
/// record file. Exits 1 when the preparation failed.
pub(crate) fn main(flags: &std::collections::HashMap<String, String>) -> Result<i32, String> {
    let project = PathBuf::from(flags.get("project-dir").ok_or(USAGE)?);
    let repo = PathBuf::from(flags.get("repo-dir").cloned().map_or_else(crate::cli::default_repo_dir, Ok)?);
    let state_dir = flags.get("state-dir").map(PathBuf::from);
    let timeout = match flags.get("timeout-s") {
        Some(t) => Some(t.parse::<f64>().ok().filter(|t| t.is_finite() && *t > 0.0).ok_or("--timeout-s must be a positive number")?),
        None => None,
    };
    let record = if flags.contains_key("status") {
        let julia = sandbox::resolve_julia()?;
        status(&identity(&project, &repo, &julia).map_err(|e| e.to_string())?, state_dir.as_deref())
    } else {
        let host = match flags.get("host-bin") {
            Some(h) => PathBuf::from(h),
            None => std::env::current_exe().map_err(|e| e.to_string())?,
        };
        prepare(&project, &repo, state_dir.as_deref(), &host, flags.contains_key("force"), timeout).map_err(|e| e.to_string())?
    };
    let mut summary = record.as_object().cloned().unwrap_or_default();
    summary.remove("output");
    summary.remove("stderr");
    summary.insert("record_file".into(), json!(record_path(&record["identity"], state_dir.as_deref())));
    println!("{}", Value::Object(summary));
    Ok(if record["state"] == "failed" { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn error_summaries_drop_colour_and_keep_both_ends() {
        assert_eq!(error_summary("\x1b[91m\x1b[1mERROR:\x1b[22m\x1b[39m ZMQ failed\n"), "ERROR: ZMQ failed");
        assert_eq!(error_summary("a\x1b]b\x1b"), "a\x1b]b\x1b");
        let text = format!("ERROR: cause\n{}\nZMQ [end]", "é".repeat(50_000));
        let summary = error_summary(&text);
        assert!(summary.starts_with("ERROR: cause") && summary.ends_with("ZMQ [end]"));
        assert!(summary.contains("[... 46023 characters left out ...]"));
        assert_eq!(summary.chars().count(), 4000 + "\n[... 46023 characters left out ...]\n".len());
    }

    #[test]
    fn record_keys_match_the_python_ones() {
        // hashlib.sha256(json.dumps(["/p", "/r é", "/d\"\n"]).encode()).hexdigest()
        let identity = json!({"project": "/p", "repo": "/r é", "depot": "/d\"\n"});
        assert_eq!(util::py_json_str("/r é😀\u{7f}"), "\"/r \\u00e9\\ud83d\\ude00\\u007f\"");
        assert_eq!(record_path(&identity, Some(Path::new("/s"))),
                   Path::new("/s/e867693d64c661e80e711d5167eab68536a50b4360d1761d8e282da977f17ecf.json"));
        assert_eq!(record_path(&json!({"project": "/p", "repo": "/r", "depot": "/d"}), None).parent().unwrap(), Path::new("/d/.palette-preparation"));
    }

    fn write_record(root: &Path, identity: &Value, fields: Value) {
        let mut record = json!({"schema": SCHEMA, "state": "failed", "identity": identity, "error": "old failure", "cache_files": {}});
        record.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
        atomic_json(&record_path(identity, Some(root)), &record).unwrap();
    }

    #[test]
    fn status_reads_cold_stale_failed_and_prepared() {
        let t = Tmp(util::mkdtemp("palette-preparation-").unwrap());
        let root = t.0.as_path();
        let identity = json!({"project": "/p", "repo": "/r", "depot": root, "source": {"a.jl": "1"}});
        assert_eq!(status(&identity, Some(root)), json!({"state": "cold", "identity": identity}));

        write_record(root, &identity, json!({}));
        let changed = json!({"project": "/p", "repo": "/r", "depot": root, "source": {"a.jl": "2"}});
        let stale = status(&changed, Some(root));
        assert_eq!((stale["state"].as_str(), stale["error"].is_null(), stale["current_identity"] == changed), (Some("stale"), true, true));
        assert!(stale["reason"].as_str().unwrap().contains("changed"));
        assert_eq!(status(&identity, Some(root))["error"], "old failure");

        let cache = root.join("x.ji");
        fs::write(&cache, "cache").unwrap();
        write_record(root, &identity, json!({"state": "prepared", "returncode": 0, "error": "warnings",
                                             "cache_files": {cache.to_str().unwrap(): util::sha256_hex(b"cache")}}));
        let prepared = status(&identity, Some(root));
        assert_eq!((prepared["state"].as_str(), &prepared["error"], &prepared["stderr"]), (Some("prepared"), &Value::Null, &json!("warnings")));
        fs::write(&cache, "changed").unwrap();
        let stale = status(&identity, Some(root));
        assert_eq!((stale["state"].as_str(), stale["reason"].as_str()), (Some("stale"), Some("prepared cache missing or changed")));

        write_record(root, &identity, json!({"state": "preparing", "pid": std::process::id(), "process_start": "0"}));
        assert_eq!(status(&identity, Some(root))["error"], "preparation process identity changed");
        let me = std::process::id() as i64;
        write_record(root, &identity, json!({"state": "preparing", "pid": me, "process_start": process_start(me)}));
        assert_eq!(status(&identity, Some(root))["state"], "preparing");
        write_record(root, &identity, json!({"state": "preparing", "pid": "1"}));
        assert_eq!(status(&identity, Some(root))["error"], "preparation process disappeared");

        let path = record_path(&identity, Some(root));
        for (text, error) in [("{", "preparation record unreadable"), ("[]", "preparation record is not an object"),
                              ("{\"schema\":\"v0\"}", "unknown preparation record format"),
                              ("{\"schema\":\"palette.preparation.v1\",\"state\":\"done\"}", "invalid preparation state or cache inventory"),
                              ("{\"schema\":\"palette.preparation.v1\",\"state\":\"failed\",\"cache_files\":[]}", "invalid preparation state or cache inventory")] {
            fs::write(&path, text).unwrap();
            let s = status(&identity, Some(root));
            assert_eq!(s["state"], "failed");
            assert!(s["error"].as_str().unwrap().starts_with(error), "{text}: {s}");
        }
    }
}
