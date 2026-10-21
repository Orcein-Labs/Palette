//! Tested repairs with a disposable, zero-authority verification worker,
//! ported from `repair_broker.py`.
//!
//! The model may suggest file contents, but the operator supplies patch
//! roots, ordinary path classes and fixed test recipes. Candidate code runs
//! only in a Bubblewrap sandbox with networking disabled, immutable candidate
//! source and private temporary storage.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::patch::{self, field, index, no_attribute, py_iter, truthy, unhashable, Client, PatchBroker, AUTO_APPLY_CLASSES};
use super::{py_json_loads, Fault};
use crate::{provisioning, util};

const MAX_FILES: usize = 16;
const MAX_TREE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TREE_FILES: u64 = 20000;
const MAX_TEST_OUTPUT: usize = 8192;
const TEST_TIMEOUT: Duration = Duration::from_secs(180);
#[cfg(test)]
const WAIT_TIMEOUT: Duration = Duration::from_secs(180 + 30);
const SKIPPED_DIRS: [&str; 4] = [".git", ".codex", "__pycache__", ".julia"];

/// sha256 of `json.dumps(changes, ensure_ascii=False, sort_keys=True,
/// separators=(",", ":"))`: serde's compact form, keys already sorted.
fn canonical_changes(changes: Option<&Value>) -> String {
    util::sha256_hex(changes.unwrap_or(&Value::Null).to_string().as_bytes())
}

fn canonical_recipe(recipe: &Map<String, Value>) -> String {
    util::sha256_hex(util::py_dumps_compact(&Value::Object(recipe.clone())).as_bytes())
}

/// HMAC-SHA256 (RFC 2104).
fn hmac_sha256(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut block = [0u8; 64];
    block[..32].copy_from_slice(key);
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let mut inner = Sha256::new();
    inner.update(pad(0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(pad(0x5c));
    outer.update(inner.finalize());
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `value.get(key)` on a JSON object, with Python's error for anything else.
fn get<'a>(value: &'a Value, key: &str) -> Result<Option<&'a Value>, Fault> {
    match value {
        Value::Object(m) => Ok(m.get(key)),
        _ => Err(no_attribute(value, "get")),
    }
}

/// A recipe the operator configured: its id, fixed argv, and the host
/// directories the worker may read.
pub(crate) fn normalize_recipe(recipe: &Value) -> Result<Map<String, Value>, Fault> {
    let shape = recipe.as_object().filter(|r| {
        r.keys().all(|k| matches!(k.as_str(), "id" | "argv" | "read_roots" | "julia_depots"))
            && r.contains_key("id") && r.contains_key("argv")
    });
    let Some(recipe) = shape else {
        return Err(Fault::value("Each host test recipe needs id, argv and optional read_roots/julia_depots"));
    };
    let id = recipe["id"].as_str().filter(|id| !id.is_empty() && id.chars().count() <= 128);
    let Some(id) = id else { return Err(Fault::value("Invalid trusted test recipe id")) };
    let argv = recipe["argv"].as_array().filter(|a| {
        !a.is_empty() && a.len() <= 64 && a.iter().all(|item| item.as_str().is_some_and(|s| !s.contains('\0')))
    });
    let Some(argv) = argv else { return Err(Fault::value("Trusted test recipe argv must be a fixed nonempty string list")) };
    let mut roots = Map::new();
    for name in ["read_roots", "julia_depots"] {
        let values = recipe.get(name).cloned().unwrap_or_else(|| json!([]));
        let values = values.as_array().filter(|v| {
            v.len() <= 16 && v.iter().all(|p| p.as_str().is_some_and(|p| p.starts_with('/')))
        });
        let Some(values) = values else {
            return Err(Fault::value(format!("{name} must contain bounded absolute host directories")));
        };
        let mut paths = Vec::new();
        for value in values {
            paths.push(util::resolve_strict(Path::new(value.as_str().expect("checked above"))).map_err(|(e, p)| Fault::os(&e, &p))?);
        }
        if paths.iter().any(|p| !p.is_dir()) {
            return Err(Fault::value(format!("{name} must contain directories")));
        }
        let mut unique: Vec<Value> = Vec::new();
        for path in paths {
            let path = Value::String(path.to_string_lossy().into_owned());
            if !unique.contains(&path) {
                unique.push(path);
            }
        }
        roots.insert(name.into(), Value::Array(unique));
    }
    if roots["julia_depots"].as_array().unwrap().iter().any(|d| !roots["read_roots"].as_array().unwrap().contains(d)) {
        return Err(Fault::value("Julia depots must be explicitly included in read_roots"));
    }
    let mut out = Map::new();
    out.insert("id".into(), id.into());
    out.insert("argv".into(), Value::Array(argv.clone()));
    out.extend(roots);
    Ok(out)
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
}

/// Copies regular files only; symlinked or special-file trees fail closed.
/// Directories that cannot be listed are skipped, as `os.walk` skips them.
fn copy_stage(source: &Path, destination: &Path, classify: &dyn Fn(&str) -> Result<String, Fault>) -> Result<u64, Fault> {
    let (mut total, mut count) = (0, 0);
    copy_dir(source, destination, "", classify, &mut total, &mut count)?;
    Ok(total)
}

fn copy_dir(base: &Path, dst_dir: &Path, relative: &str, classify: &dyn Fn(&str) -> Result<String, Fault>,
            total: &mut u64, count: &mut u64) -> Result<(), Fault> {
    let Ok(entries) = fs::read_dir(base) else { return Ok(()) };
    let mut names: Vec<String> = entries.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    let (mut dirs, files): (Vec<String>, Vec<String>) = names.into_iter().partition(|name| base.join(name).is_dir());
    dirs.retain(|name| !SKIPPED_DIRS.contains(&name.as_str()));
    fs::DirBuilder::new().recursive(true).create(dst_dir).map_err(|e| Fault::os(&e, dst_dir))?;
    for name in &dirs {
        let path = base.join(name);
        if path.is_symlink() || !path.is_dir() {
            return Err(Fault::value("Repair test trees cannot contain symlinked directories"));
        }
        match fs::create_dir(dst_dir.join(name)) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(Fault::os(&e, &dst_dir.join(name))),
            _ => {}
        }
    }
    let join = |name: &str| if relative.is_empty() { name.to_string() } else { format!("{relative}/{name}") };
    for name in &files {
        let src = base.join(name);
        let metadata = fs::symlink_metadata(&src).map_err(|e| Fault::os(&e, &src))?;
        if !metadata.file_type().is_file() {
            return Err(Fault::value("Repair test trees can contain regular files only"));
        }
        *total += metadata.len();
        *count += 1;
        if *total > MAX_TREE_BYTES || *count > MAX_TREE_FILES {
            return Err(Fault::value("Repair test source exceeds the staged tree limit"));
        }
        if classify(&join(name))? == "forbidden" {
            continue;
        }
        let target = dst_dir.join(name);
        fs::copy(&src, &target).map_err(|e| Fault::os(&e, &src))?;
        // Verification runs with the stage mounted read-only. Keep the host's
        // private copy writable so read-only deployed sources can still
        // receive a proposed candidate before that mount is made.
        fs::set_permissions(&target, fs::Permissions::from_mode((metadata.mode() & 0o777) | 0o200))
            .map_err(|e| Fault::os(&e, &target))?;
    }
    for name in &dirs {
        copy_dir(&base.join(name), &dst_dir.join(name), &join(name), classify, total, count)?;
    }
    Ok(())
}

fn open_dir(path: &Path) -> Result<fs::File, Fault> {
    let c = patch::c_name(&path.to_string_lossy())?;
    util::open_at(libc::AT_FDCWD, &c, libc::O_RDONLY | libc::O_DIRECTORY, 0).map(fs::File::from).map_err(|e| Fault::os(&e, path))
}

fn write_candidate(stage: &Path, changes: &Value) -> Result<(), Fault> {
    let root = open_dir(stage)?;
    for change in py_iter(changes)? {
        let path = index(&change, "path")?;
        let (dir, name) = patch::parent_fd(root.as_raw_fd(), path)?;
        let snapshot = patch::file_snapshot(dir.as_raw_fd(), &name)?;
        if patch::sha_value(&snapshot) != *index(&change, "before_sha256")? {
            return Err(Fault::value(format!("Staged source differs from proposed before hash: {}", path.as_str().unwrap_or_default())));
        }
        let mode = snapshot.map_or(0, |(_, _, mode)| mode);
        let mode = if mode == 0 { 0o644 } else { mode };
        let fd = util::open_at(dir.as_raw_fd(), &patch::c_name(&name)?, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, mode)
            .map_err(|e| Fault::os(&e, Path::new(&name)))?;
        let content = index(&change, "content")?;
        let content = content.as_str().ok_or_else(|| no_attribute(content, "encode"))?;
        fs::File::from(fd).write_all(content.as_bytes()).map_err(|e| patch::bare_os(&e))?;
    }
    Ok(())
}

/// Runs `argv` with no environment and stdout and stderr merged into one
/// pipe. The exit code (negative for a signal), or None if `timeout` passed
/// first, with the output that arrived.
fn run_merged(argv: &[String], timeout: Duration) -> io::Result<(Option<i32>, Vec<u8>)> {
    use std::os::unix::process::ExitStatusExt;
    let (mut reader, writer) = io::pipe()?;
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).env_clear().stdin(Stdio::null()).stdout(writer.try_clone()?).stderr(writer);
    let mut child = command.spawn()?;
    // The command holds the parent's copy of the write end until dropped.
    drop(command);
    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&output);
    let pump = std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(n @ 1..) = reader.read(&mut chunk) {
            sink.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });
    let deadline = Instant::now() + timeout;
    let mut status = None;
    let code = loop {
        if status.is_none() {
            status = child.try_wait()?;
        }
        if let Some(status) = status.filter(|_| pump.is_finished()) {
            break Some(status.code().unwrap_or_else(|| -status.signal().unwrap_or(0)));
        }
        if Instant::now() >= deadline {
            if status.is_none() {
                let _ = child.kill();
                child.wait()?;
            }
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let bytes = output.lock().unwrap().clone();
    Ok((code, bytes))
}

fn run_isolated_test(stage: &Path, recipe: &Map<String, Value>) -> Result<Map<String, Value>, Fault> {
    let Some(bwrap) = util::which("bwrap") else {
        return Err(Fault::new("RuntimeError", "Repair testing requires Bubblewrap; no unsandboxed fallback is available"));
    };
    let stage = util::resolve_strict(stage).map_err(|(e, p)| Fault::os(&e, &p))?.to_string_lossy().into_owned();
    let mut argv: Vec<String> = [
        "--unshare-user", "--uid", "65534", "--gid", "65534",
        "--disable-userns", "--unshare-net", "--unshare-pid", "--unshare-ipc",
        "--cap-drop", "ALL", "--die-with-parent", "--new-session",
        "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
        "--ro-bind", "/usr", "/usr",
    ].map(String::from).to_vec();
    argv.insert(0, bwrap.to_string_lossy().into_owned());
    for host_path in ["/lib", "/lib64", "/bin", "/etc/ld.so.cache"] {
        if fs::symlink_metadata(host_path).is_ok() {
            argv.extend(["--ro-bind-try", host_path, host_path].map(String::from));
        }
    }
    for root in strings(&recipe["read_roots"]) {
        argv.extend(["--ro-bind".into(), root.clone(), root]);
    }
    let depots = std::iter::once("/tmp/julia-depot".to_string()).chain(strings(&recipe["julia_depots"])).collect::<Vec<_>>().join(":");
    // Candidate and verification files are immutable during execution. Any
    // test scratch must live under the worker's private /tmp.
    argv.extend(["--ro-bind", &stage, &stage,
        "--clearenv", "--setenv", "PATH", "/usr/local/bin:/usr/bin:/bin",
        "--setenv", "HOME", "/tmp", "--setenv", "TMPDIR", "/tmp",
        "--setenv", "PYTHONDONTWRITEBYTECODE", "1", "--setenv", "LANG", "C.UTF-8",
        "--setenv", "JULIA_PKG_OFFLINE", "true", "--setenv", "JULIA_DEPOT_PATH", &depots,
        "--chdir", &stage, "--"].map(String::from));
    argv.extend(strings(&recipe["argv"]));
    let started = Instant::now();
    let (code, bytes) = run_merged(&argv, TEST_TIMEOUT).map_err(|e| Fault::os(&e, &bwrap))?;
    let duration = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
    let text = String::from_utf8_lossy(&bytes);
    let result = match code {
        Some(code) => {
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            json!({"status": if code == 0 { "passed" } else { "failed" }, "exit_code": code,
                   "output": util::tail(&text, MAX_TEST_OUTPUT), "duration_seconds": duration})
        }
        None => json!({"status": "failed", "exit_code": null,
                       "output": util::tail(&text, MAX_TEST_OUTPUT) + "\n[test timed out]", "duration_seconds": duration}),
    };
    let Value::Object(result) = result else { unreachable!() };
    Ok(result)
}

/// One background verification: its result once it finishes.
#[derive(Default)]
struct Job {
    result: Mutex<Option<Result<Value, Fault>>>,
    done: Condvar,
}

impl Job {
    fn finish(&self, result: Result<Value, Fault>) {
        *self.result.lock().unwrap() = Some(result);
        self.done.notify_all();
    }

    fn running(&self) -> bool {
        self.result.lock().unwrap().is_none()
    }
}

/// Host-side repair workflow. All authority inputs are constructor-owned.
pub(crate) struct RepairBroker {
    pub patches: PatchBroker,
    pub storage: PathBuf,
    signing_key: [u8; 32],
    pub test_recipes: std::collections::BTreeMap<String, Map<String, Value>>,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
}

impl RepairBroker {
    pub fn new(mut patches: PatchBroker, test_recipes: &Map<String, Value>, auto_apply_prefixes: &Value,
               protected_paths: &[PathBuf]) -> Result<RepairBroker, Fault> {
        for path in protected_paths {
            let path = util::resolve(path);
            if !patches.protected_paths.contains(&path) {
                patches.protected_paths.push(path);
            }
        }
        let storage = patches.storage.join("repairs");
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&storage).map_err(|e| Fault::os(&e, &storage))?;
        let signing_key = {
            let _lock = patches.locked()?;
            load_signing_key(&storage)?
        };
        let mut recipes = std::collections::BTreeMap::new();
        for (target, recipe) in test_recipes {
            recipes.insert(target.clone(), normalize_recipe(recipe)?);
        }
        if recipes.keys().any(|target| !patches.roots.contains_key(target)) {
            return Err(Fault::value("Repair test recipes require configured patch targets"));
        }
        let narrow = [PathBuf::from("/"), "/home".into(), util::home(), "/etc".into(), "/proc".into(), "/dev".into()];
        for (target, recipe) in &recipes {
            let read_roots = strings(&recipe["read_roots"]);
            provisioning::require_disjoint(&read_roots, &[PathBuf::from(&patches.roots[target]), patches.storage.clone()], &[] as &[PathBuf])
                .map_err(Fault::permission)?;
            if read_roots.iter().any(|root| narrow.iter().any(|n| Path::new(root) == n)) {
                return Err(Fault::permission("Repair read roots must name narrow toolchain or dependency directories"));
            }
        }
        if truthy(auto_apply_prefixes) {
            // Bind trusted class configuration once; model arguments cannot alter it.
            for (target, entries) in patch::items(auto_apply_prefixes)? {
                if !patches.roots.contains_key(&target) {
                    return Err(Fault::value("Auto-apply classes require a configured patch target"));
                }
                let mut normalized = Vec::new();
                for entry in py_iter(&entries)? {
                    let pair = entry.as_array().filter(|a| a.len() == 2);
                    let Some(pair) = pair else { return Err(Fault::value("Each auto-apply prefix must be a path-class/prefix pair")) };
                    let (class, prefix) = (&pair[0], &pair[1]);
                    unhashable(class, "set element")?;
                    let Some(class) = class.as_str().filter(|c| AUTO_APPLY_CLASSES.contains(c)) else {
                        return Err(Fault::value("Unknown auto-apply path class"));
                    };
                    patch::parent_path_parts(prefix)?;
                    normalized.push((class.to_string(), prefix.as_str().expect("a path part list came from a string").to_string()));
                }
                match patches.auto_apply_prefixes.get(&target) {
                    Some(existing) if !existing.is_empty() && *existing != normalized => {
                        return Err(Fault::value("Repair and patch broker path classes must agree"));
                    }
                    Some(existing) if !existing.is_empty() => {}
                    _ => {
                        patches.auto_apply_prefixes.insert(target, normalized);
                    }
                }
            }
        }
        Ok(RepairBroker { patches, storage, signing_key, test_recipes: recipes, jobs: Mutex::new(HashMap::new()) })
    }

    pub fn record_path(&self, repair_id: &Value) -> Result<PathBuf, Fault> {
        match repair_id.as_str() {
            Some(id) if id.len() == 32 && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) => {
                Ok(self.storage.join(format!("{id}.json")))
            }
            _ => Err(Fault::value("Invalid repair_id")),
        }
    }

    fn signature(&self, record: &Map<String, Value>) -> String {
        let mut payload = record.clone();
        payload.remove("record_hmac");
        util::to_hex(&hmac_sha256(&self.signing_key, util::py_dumps_compact(&Value::Object(payload)).as_bytes()))
    }

    pub fn read(&self, repair_id: &Value, workspace: Option<&str>) -> Result<Map<String, Value>, Fault> {
        let path = self.record_path(repair_id)?;
        let bytes = fs::read(&path).map_err(|e| Fault::os(&e, &path))?;
        let record = py_json_loads(patch::decode(&bytes)?)?;
        let Value::Object(record) = record else { return Err(no_attribute(&record, "items")) };
        // A stored hmac that is not a string never matches.
        let stored = record.get("record_hmac").map_or(Some(""), Value::as_str);
        if !stored.is_some_and(|s| constant_time_eq(self.signature(&record).as_bytes(), s.as_bytes())) {
            return Err(Fault::permission("Stored repair differs from its bound digest"));
        }
        if record.get("changes_sha256").and_then(Value::as_str) != Some(&canonical_changes(record.get("changes"))) {
            return Err(Fault::permission("Stored repair contents differ from the tested change digest"));
        }
        if let Some(workspace) = workspace {
            if field(&record, "workspace_id")?.as_str() != Some(workspace) {
                return Err(Fault::permission("Repair belongs to a different workspace"));
            }
        }
        Ok(record)
    }

    fn store(&self, record: &mut Map<String, Value>) -> Result<(), Fault> {
        let signature = self.signature(record);
        record.insert("record_hmac".into(), signature.into());
        let path = self.record_path(field(record, "repair_id")?)?;
        let tmp = path.with_extension("tmp");
        let _ = fs::remove_file(&tmp);
        let text = util::py_dumps(&Value::Object(record.clone()), None);
        util::write_then(&tmp, text.as_bytes(), 0o600, || fs::rename(&tmp, &path)).map_err(|e| Fault::os(&e, &tmp))
    }

    pub fn classify_changes(&self, target: &Value, changes: &Value) -> Result<Value, Fault> {
        let mut rows = Vec::new();
        for change in py_iter(changes)? {
            let path = index(&change, "path")?;
            let class = self.patches.classify(target, path)?;
            rows.push(json!({"path": path, "class": class.class, "self_apply": class.self_apply, "reason": class.reason}));
        }
        Ok(Value::Array(rows))
    }

    fn detect(&self, args: &Map<String, Value>) -> Result<Value, Fault> {
        let target = args.get("target").unwrap_or(&Value::Null);
        let root = patch::root_of(&self.patches.roots, target)?;
        let paths = args.get("paths").and_then(Value::as_array).filter(|p| (1..=MAX_FILES).contains(&p.len()));
        let (Some(root), Some(paths)) = (root, paths) else {
            return Err(Fault::value("detect requires a configured target and 1..16 paths"));
        };
        let root = open_dir(Path::new(root))?;
        let mut found = Vec::new();
        for relative in paths {
            let (dir, name) = patch::parent_fd(root.as_raw_fd(), relative)?;
            let snapshot = patch::file_snapshot(dir.as_raw_fd(), &name)?;
            let class = self.patches.classify(target, relative)?;
            found.push(json!({"path": relative, "sha256": patch::sha_value(&snapshot),
                              "bytes": snapshot.as_ref().map_or(0, |(_, data, _)| data.len()),
                              "class": class.class, "self_apply": class.self_apply, "reason": class.reason}));
        }
        Ok(json!({"status": "detected", "target": target, "files": found}))
    }

    pub fn propose(&self, args: &Map<String, Value>, workspace: &str) -> Result<Value, Fault> {
        let target = args.get("target").unwrap_or(&Value::Null);
        unhashable(target, "dict key")?;
        let Some(recipe) = target.as_str().and_then(|t| self.test_recipes.get(t)) else {
            return Err(Fault::permission("No operator-configured fixed test recipe for target"));
        };
        let changes = args.get("changes").filter(|c| c.as_array().is_some_and(|c| (1..=MAX_FILES).contains(&c.len())));
        let Some(changes) = changes else { return Err(Fault::value("propose requires 1..16 exact file changes")) };
        // The ordinary patch broker provides descriptor-relative path checks,
        // before hashes, bounded diffs, receipts, expiry and approval storage.
        let request: Map<String, Value> = [("target".to_string(), target.clone()), ("changes".to_string(), changes.clone())].into_iter().collect();
        let patch = self.patches.prepare(&request, workspace)?;
        let record = json!({
            "repair_id": util::uuid4().replace('-', ""), "workspace_id": workspace, "target": target,
            "patch_request_id": field(&patch, "request_id")?, "patch_sha256": field(&patch, "patch_sha256")?,
            "changes_sha256": canonical_changes(Some(changes)), "changes": changes,
            "recipe_id": recipe["id"], "recipe_sha256": canonical_recipe(recipe), "created_at": util::now_secs(),
            "classifications": self.classify_changes(target, changes)?,
            "test": {"status": "not_run"}, "status": "proposed",
        });
        let Value::Object(mut record) = record else { unreachable!() };
        self.store(&mut record)?;
        let shown = ["status", "repair_id", "patch_request_id", "patch_sha256", "changes_sha256", "classifications"];
        Ok(Value::Object(shown.iter().map(|k| (k.to_string(), record[*k].clone())).collect()))
    }

    fn check_recipe(&self, record: &Map<String, Value>) -> Result<&Map<String, Value>, Fault> {
        let recipe = field(record, "target")?.as_str().and_then(|t| self.test_recipes.get(t));
        match recipe {
            Some(recipe) if recipe["id"] == *field(record, "recipe_id")?
                && Value::String(canonical_recipe(recipe)) == *field(record, "recipe_sha256")? => Ok(recipe),
            _ => Err(Fault::permission("Trusted test recipe changed since proposal")),
        }
    }

    pub fn test(&self, repair_id: &Value, workspace: &str) -> Result<Value, Fault> {
        let mut record = self.read(repair_id, Some(workspace))?;
        let patch_record = self.patches.inspect(field(&record, "patch_request_id")?, Some(workspace))?;
        if field(&patch_record, "patch_sha256")? != field(&record, "patch_sha256")?
            || Value::String(canonical_changes(Some(field(&patch_record, "changes")?))) != *field(&record, "changes_sha256")? {
            return Err(Fault::permission("Test candidate differs from its pending patch request"));
        }
        let recipe = self.check_recipe(&record)?;
        let target = field(&record, "target")?.clone();
        let source = target.as_str().and_then(|t| self.patches.roots.get(t))
            .ok_or_else(|| Fault::new("KeyError", util::py_repr(&target)))?;
        let directory = util::mkdtemp_in(&util::temp_dir(), "palette-repair-").map_err(Fault::io)?;
        let result = (|| {
            let stage = directory.join("source");
            fs::DirBuilder::new().mode(0o700).create(&stage).map_err(|e| Fault::os(&e, &stage))?;
            let classify = |relative: &str| Ok(self.patches.classify(&target, &relative.into())?.class);
            copy_stage(Path::new(source), &stage, &classify)?;
            write_candidate(&stage, field(&record, "changes")?)?;
            run_isolated_test(&stage, recipe)
        })();
        let _ = fs::remove_dir_all(&directory);
        let mut result = result?;
        let passed = result["status"] == "passed";
        result.insert("recipe_id".into(), recipe["id"].clone());
        result.insert("tested_changes_sha256".into(), field(&record, "changes_sha256")?.clone());
        result.insert("tested_at".into(), util::now_secs().into());
        record.insert("test".into(), Value::Object(result));
        record.insert("status".into(), if passed { "tested" } else { "test_failed" }.into());
        self.store(&mut record)?;
        Ok(json!({"status": record["status"], "repair_id": repair_id, "patch_sha256": record["patch_sha256"], "test": record["test"]}))
    }

    pub fn apply(&self, repair_id: &Value, workspace: &str, capabilities: &Map<String, Value>,
                 client: &mut dyn Client) -> Result<Value, Fault> {
        let mut record = self.read(repair_id, Some(workspace))?;
        let tested = field(&record, "status")? == "tested" && {
            let test = field(&record, "test")?;
            get(test, "status")? == Some(&"passed".into())
                && get(test, "tested_changes_sha256")? == Some(field(&record, "changes_sha256")?)
        };
        if !tested {
            return Err(Fault::permission("Only the exact successfully tested proposal can be applied"));
        }
        self.check_recipe(&record)?;
        let request_id = field(&record, "patch_request_id")?.clone();
        let patch_record = self.patches.inspect(&request_id, Some(workspace))?;
        let status = field(&patch_record, "status")?;
        if field(&patch_record, "patch_sha256")? != field(&record, "patch_sha256")?
            || !(status == "pending" || status == "approved")
            || Value::String(canonical_changes(Some(field(&patch_record, "changes")?))) != *field(&record, "changes_sha256")? {
            return Err(Fault::permission("Tested patch is no longer the pending exact patch"));
        }
        let classifications = self.classify_changes(field(&record, "target")?, field(&record, "changes")?)?;
        if classifications != *field(&record, "classifications")? {
            return Err(Fault::permission("Operator path classification changed since proposal"));
        }
        let rows = classifications.as_array().expect("classify_changes returns a list");
        if rows.iter().any(|row| row["class"] == "forbidden") {
            return Err(Fault::permission("Credential and secret files cannot be patched"));
        }
        let outcome = if rows.iter().all(|row| row["self_apply"] == true) {
            if status == "pending" {
                let recipe_id = index(field(&record, "test")?, "recipe_id")?;
                let recipe_id = recipe_id.as_str().ok_or_else(|| Fault::new("TypeError", format!(
                    "can only concatenate str (not \"{}\") to str", util::py_type_name(recipe_id))))?;
                let sha = record["patch_sha256"].as_str().unwrap_or_default();
                self.patches.approve(&request_id, sha, &format!("isolated_tested_repair:{recipe_id}"))?;
            }
            Value::Object(self.patches.apply(&request_id, workspace)?)
        } else {
            let args: Map<String, Value> = [("action".to_string(), "apply".into()), ("request_id".to_string(), request_id)].into_iter().collect();
            self.patches.call(&args, workspace, capabilities, client)?
        };
        // Keep a successful test live while the trusted operator decides. A
        // later call may consume that exact externally approved request.
        let outcome_status = get(&outcome, "status")?;
        let status = match outcome_status.and_then(Value::as_str) {
            Some("approval_required" | "approval_timeout" | "not_approved") => "tested".into(),
            _ => outcome_status.cloned().unwrap_or_else(|| "approval_pending".into()),
        };
        record.insert("apply".into(), outcome.clone());
        record.insert("status".into(), status);
        self.store(&mut record)?;
        Ok(json!({"repair_id": repair_id, "patch_sha256": record["patch_sha256"], "status": record["status"], "apply": outcome}))
    }

    /// Starts verification on a background thread. It always finishes the
    /// job, and records a failed test when verification raises.
    pub fn start_test(self: &Arc<Self>, repair_id: &Value, workspace: &str) -> Result<Value, Fault> {
        let mut jobs = self.jobs.lock().unwrap();
        unhashable(repair_id, "dict key")?;
        if repair_id.as_str().and_then(|id| jobs.get(id)).is_some_and(|job| job.running()) {
            return Err(Fault::permission("Repair verification is already running"));
        }
        let mut record = self.read(repair_id, Some(workspace))?;
        if !matches!(field(&record, "status")?.as_str(), Some("proposed" | "test_failed" | "tested")) {
            return Err(Fault::permission("Repair is not available for verification"));
        }
        record.insert("status".into(), "testing".into());
        self.store(&mut record)?;
        let id = repair_id.as_str().expect("a stored repair id is a string").to_string();
        let job = Arc::new(Job::default());
        jobs.insert(id.clone(), Arc::clone(&job));
        let (broker, workspace, value) = (Arc::clone(self), workspace.to_string(), repair_id.clone());
        let spawned = std::thread::Builder::new().name(format!("palette-repair-{id}")).spawn(move || {
            let result = broker.test(&value, &workspace);
            if let Err(error) = &result {
                if let Ok(mut failed) = broker.read(&value, Some(&workspace)) {
                    failed.insert("status".into(), "test_failed".into());
                    failed.insert("test".into(), json!({"status": "failed", "error": error.message}));
                    let _ = broker.store(&mut failed);
                }
            }
            job.finish(result);
        });
        if let Err(e) = spawned {
            jobs.remove(&id);
            return Err(Fault::io(e));
        }
        Ok(json!({"status": "testing", "repair_id": repair_id}))
    }

    #[cfg(test)]
    pub fn wait_test(&self, repair_id: &Value, workspace: &str) -> Result<Value, Fault> {
        self.read(repair_id, Some(workspace))?;
        let job = repair_id.as_str().and_then(|id| self.jobs.lock().unwrap().get(id).cloned())
            .ok_or_else(|| Fault::new("KeyError", util::py_repr(repair_id)))?;
        let (result, timeout) = job.done.wait_timeout_while(job.result.lock().unwrap(), WAIT_TIMEOUT, |r| r.is_none()).unwrap();
        if timeout.timed_out() {
            return Err(Fault::new("TimeoutError", ""));
        }
        result.clone().expect("a finished job has a result")
    }

    /// One repair tool call.
    pub fn call(self: &Arc<Self>, args: &Map<String, Value>, workspace: &str, capabilities: &Map<String, Value>,
                client: &mut dyn Client) -> Result<Value, Fault> {
        let action = args.get("action").unwrap_or(&Value::Null);
        let arg = |key: &str| args.get(key).unwrap_or(&Value::Null);
        match action.as_str() {
            Some("detect") => self.detect(args),
            Some("targets") => {
                let targets: Map<String, Value> = self.patches.roots.iter()
                    .map(|(name, root)| (name.clone(), json!({"root": root, "test_recipe": self.test_recipes.contains_key(name)})))
                    .collect();
                Ok(json!({"targets": targets,
                          "configured_by": "PALETTE_PATCH_ROOTS, a JSON object of target name to source directory, set by the host"}))
            }
            Some("classify") => {
                let target = arg("target");
                if patch::root_of(&self.patches.roots, target)?.is_none() {
                    return Err(self.patches.unknown_target(target));
                }
                let changes = arg("changes");
                if !changes.is_array() {
                    return Err(Fault::value("classify requires changes"));
                }
                Ok(json!({"status": "classified", "files": self.classify_changes(target, changes)?}))
            }
            Some("propose") => self.propose(args, workspace),
            Some("test") => self.start_test(arg("repair_id"), workspace),
            Some("apply") => self.apply(arg("repair_id"), workspace, capabilities, client),
            Some("status") => {
                let repair_id = arg("repair_id");
                let mut record = self.read(repair_id, Some(workspace))?;
                let known = repair_id.as_str().is_some_and(|id| self.jobs.lock().unwrap().contains_key(id));
                if field(&record, "status")? == "testing" && !known {
                    record.insert("status".into(), "interrupted_completion_unknown".into());
                    self.store(&mut record)?;
                }
                record.remove("changes");
                Ok(Value::Object(record))
            }
            _ => Err(Fault::value("Unknown repair action; expected targets, detect, classify, propose, test, apply or status")),
        }
    }
}

/// The key that binds stored records to this host: created once, private.
fn load_signing_key(storage: &Path) -> Result<[u8; 32], Fault> {
    let path = storage.join(".repair-signing-key");
    match OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&path) {
        Ok(mut stream) => {
            let key = util::random_bytes::<32>();
            stream.write_all(&key).and_then(|()| stream.sync_all()).map_err(|e| patch::bare_os(&e))?;
            Ok(key)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&path).map_err(|e| Fault::os(&e, &path))?;
            if !path.is_file() || metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
                return Err(Fault::permission("Repair signing key must be a private, unlinked regular file"));
            }
            let key = fs::read(&path).map_err(|e| Fault::os(&e, &path))?;
            key.try_into().map_err(|_| Fault::permission("Repair signing key has an invalid length"))
        }
        Err(e) => Err(Fault::os(&e, &path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::patch::{Options, TRUSTED_TOOLS_TARGET};
    use std::os::unix::fs::symlink;

    /// The palette-host binary `cargo test` builds beside the test binary.
    fn host() -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let host = exe.parent().and_then(Path::parent).unwrap().join("palette-host");
        assert!(host.is_file(), "build palette-host first ({})", host.display());
        host
    }

    fn sha(text: &str) -> String {
        util::sha256_hex(text.as_bytes())
    }

    fn roots(pairs: &[(&str, &Path)]) -> Map<String, Value> {
        pairs.iter().map(|(k, p)| (k.to_string(), Value::String(p.to_string_lossy().into_owned()))).collect()
    }

    fn patches(storage: &Path, roots: &Map<String, Value>) -> PatchBroker {
        let mut broker = PatchBroker::new(storage, roots, Options::default()).unwrap();
        broker.host = host();
        broker
    }

    /// The interpreter on PATH, with its toolchain prefix as the read root.
    fn python_recipe(id: &str, args: &[&str]) -> Value {
        let exe = util::resolve(&util::which("python3").expect("python3 on PATH"));
        let prefix = exe.parent().and_then(Path::parent).unwrap().to_string_lossy().into_owned();
        let argv: Vec<String> = std::iter::once(exe.to_string_lossy().into_owned()).chain(args.iter().map(|a| a.to_string())).collect();
        json!({"id": id, "argv": argv, "read_roots": [prefix]})
    }

    const UNITTEST: [&str; 5] = ["-B", "-m", "unittest", "-v", "test_recipe"];
    const VALUE_TEST: &str = "import unittest\nfrom recipes.value import VALUE\nclass RepairTests(unittest.TestCase):\n    def test_value(self): self.assertEqual(VALUE, 'after')\n";

    struct Fixture {
        tmp: PathBuf,
        root: PathBuf,
        store: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = util::resolve(&util::mkdtemp("palette-repair-test-").unwrap());
            let root = tmp.join("project");
            fs::create_dir(&root).unwrap();
            fs::write(root.join("a.txt"), "before\n").unwrap();
            Fixture { store: tmp.join("grants"), tmp, root }
        }

        /// recipes/value.py and a test that wants VALUE == 'after'.
        fn recipes(self) -> Fixture {
            fs::create_dir(self.root.join("recipes")).unwrap();
            fs::write(self.root.join("recipes/value.py"), "VALUE = 'before'\n").unwrap();
            fs::write(self.root.join("test_recipe.py"), VALUE_TEST).unwrap();
            self
        }

        fn repair(&self, recipes: Value, auto: Value) -> Arc<RepairBroker> {
            let patches = patches(&self.store, &roots(&[("project", &self.root)]));
            Arc::new(RepairBroker::new(patches, recipes.as_object().unwrap(), &auto, &[]).unwrap())
        }

        fn read(&self, path: &str) -> String {
            fs::read_to_string(self.root.join(path)).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.tmp);
        }
    }

    /// A client that answers nothing.
    struct Silent;

    impl Client for Silent {
        fn send(&mut self, _: &Value) -> io::Result<()> {
            Ok(())
        }
        fn recv(&mut self, _: Duration) -> Option<String> {
            None
        }
    }

    fn call(broker: &Arc<RepairBroker>, args: Value) -> Result<Value, Fault> {
        broker.call(args.as_object().unwrap(), "world-a", &Map::new(), &mut Silent)
    }

    fn propose(broker: &RepairBroker, path: &str, before: &str, content: &str) -> Value {
        let args = json!({"target": "project", "changes": [{"path": path, "before_sha256": sha(before), "content": content}]});
        broker.propose(args.as_object().unwrap(), "world-a").unwrap()
    }

    fn apply(broker: &RepairBroker, proposal: &Value) -> Result<Value, Fault> {
        broker.apply(&proposal["repair_id"], "world-a", &Map::new(), &mut Silent)
    }

    fn kind<T: std::fmt::Debug>(result: Result<T, Fault>) -> &'static str {
        result.expect_err("expected a fault").kind
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Case 2 with the key zero-padded to 32 bytes, which HMAC treats as the same key.
        let mut key = [0u8; 32];
        key[..4].copy_from_slice(b"Jefe");
        assert_eq!(util::to_hex(&hmac_sha256(&key, b"what do ya want for nothing?")),
                   "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        let key = [0x0b; 32];
        let mut long = [0x0b; 32];
        long[20..].fill(0);
        // Case 1 uses a 20-byte key: pad it the same way.
        assert_eq!(util::to_hex(&hmac_sha256(&long, b"Hi There")),
                   "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        assert_ne!(hmac_sha256(&key, b"Hi There"), hmac_sha256(&long, b"Hi There"));
    }

    #[test]
    fn recipes_are_fixed_and_bounded() {
        let f = Fixture::new();
        let dir = f.tmp.to_string_lossy().into_owned();
        let file = f.root.join("a.txt").to_string_lossy().into_owned();
        let cases = [
            (json!([]), "Each host test recipe needs id, argv and optional read_roots/julia_depots"),
            (json!({"id": "x"}), "Each host test recipe needs id, argv and optional read_roots/julia_depots"),
            (json!({"id": "x", "argv": ["a"], "env": {}}), "Each host test recipe needs id, argv and optional read_roots/julia_depots"),
            (json!({"id": "", "argv": ["a"]}), "Invalid trusted test recipe id"),
            (json!({"id": "x".repeat(129), "argv": ["a"]}), "Invalid trusted test recipe id"),
            (json!({"id": "x", "argv": []}), "Trusted test recipe argv must be a fixed nonempty string list"),
            (json!({"id": "x", "argv": ["a\u{0}"]}), "Trusted test recipe argv must be a fixed nonempty string list"),
            (json!({"id": "x", "argv": [1]}), "Trusted test recipe argv must be a fixed nonempty string list"),
            (json!({"id": "x", "argv": ["a"], "read_roots": "rel"}), "read_roots must contain bounded absolute host directories"),
            (json!({"id": "x", "argv": ["a"], "read_roots": ["rel"]}), "read_roots must contain bounded absolute host directories"),
            (json!({"id": "x", "argv": ["a"], "read_roots": [file]}), "read_roots must contain directories"),
            (json!({"id": "x", "argv": ["a"], "julia_depots": [dir]}), "Julia depots must be explicitly included in read_roots"),
        ];
        for (recipe, message) in cases {
            assert_eq!(normalize_recipe(&recipe).unwrap_err().message, message, "{recipe}");
        }
        let missing = json!({"id": "x", "argv": ["a"], "read_roots": [format!("{dir}/absent")]});
        assert_eq!(normalize_recipe(&missing).unwrap_err().kind, "FileNotFoundError");
        let recipe = normalize_recipe(&json!({"id": "x", "argv": ["a"], "read_roots": [dir, format!("{dir}/project/..")], "julia_depots": [dir]})).unwrap();
        assert_eq!(Value::Object(recipe), json!({"id": "x", "argv": ["a"], "read_roots": [dir], "julia_depots": [dir]}));
    }

    #[test]
    fn construction_binds_operator_configuration() {
        let f = Fixture::new();
        let project = roots(&[("project", &f.root)]);
        let build = |recipes: Value, auto: Value| {
            RepairBroker::new(patches(&f.store, &project), recipes.as_object().unwrap(), &auto, &[]).map(|_| ())
        };
        let usr = json!({"id": "x", "argv": ["a"], "read_roots": ["/usr"]});
        assert_eq!(build(json!({"other": usr}), json!({})).unwrap_err().message, "Repair test recipes require configured patch targets");
        for root in ["/", "/etc", "/dev"] {
            let recipe = json!({"project": {"id": "x", "argv": ["a"], "read_roots": [root]}});
            assert_eq!(build(recipe, json!({})).unwrap_err().kind, "PermissionError", "{root}");
        }
        let overlap = json!({"project": {"id": "x", "argv": ["a"], "read_roots": [f.tmp]}});
        assert_eq!(kind(build(overlap, json!({}))), "PermissionError");
        let messages = [
            (json!({"other": [["recipes", "r"]]}), "Auto-apply classes require a configured patch target"),
            (json!({"project": [["recipes"]]}), "Each auto-apply prefix must be a path-class/prefix pair"),
            (json!({"project": ["ab"]}), "Each auto-apply prefix must be a path-class/prefix pair"),
            (json!({"project": [["secrets", "r"]]}), "Unknown auto-apply path class"),
            (json!({"project": [["recipes", "../r"]]}), "Traversal and Git metadata are forbidden"),
        ];
        for (auto, message) in messages {
            assert_eq!(build(json!({}), auto.clone()).unwrap_err().message, message, "{auto}");
        }
        assert_eq!(kind(build(json!({}), json!({"project": [[["recipes"], "r"]]}))), "TypeError");
        let mut configured = patches(&f.store, &project);
        configured.auto_apply_prefixes.insert("project".into(), vec![("tools".into(), "tools".into())]);
        let disagree = RepairBroker::new(configured, &Map::new(), &json!({"project": [["recipes", "recipes"]]}), &[]);
        assert_eq!(disagree.map(|_| ()).unwrap_err().message, "Repair and patch broker path classes must agree");
        let bound = RepairBroker::new(patches(&f.store, &project), &Map::new(), &json!({"project": [["recipes", "recipes"]]}),
                                      &[f.root.join("private")]).unwrap();
        assert_eq!(bound.patches.auto_apply_prefixes["project"], vec![("recipes".to_string(), "recipes".to_string())]);
        assert!(bound.patches.protected_paths.contains(&f.root.join("private")));
    }

    #[test]
    fn the_signing_key_is_private_and_stable() {
        let f = Fixture::new();
        let project = roots(&[("project", &f.root)]);
        let build = || RepairBroker::new(patches(&f.store, &project), &Map::new(), &json!({}), &[]);
        let first = build().unwrap().signing_key;
        assert_eq!(build().unwrap().signing_key, first);
        let key = f.store.join("repairs/.repair-signing-key");
        assert_eq!(fs::metadata(&key).unwrap().mode() & 0o777, 0o600);
        fs::set_permissions(&key, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(build().map(|_| ()).unwrap_err().message, "Repair signing key must be a private, unlinked regular file");
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&key, f.tmp.join("second-link")).unwrap();
        assert_eq!(build().map(|_| ()).unwrap_err().message, "Repair signing key must be a private, unlinked regular file");
        fs::remove_file(f.tmp.join("second-link")).unwrap();
        fs::write(&key, b"short").unwrap();
        assert_eq!(build().map(|_| ()).unwrap_err().message, "Repair signing key has an invalid length");
        fs::remove_file(&key).unwrap();
        symlink(f.tmp.join("elsewhere"), &key).unwrap();
        assert_eq!(build().map(|_| ()).unwrap_err().message, "Repair signing key must be a private, unlinked regular file");
    }

    #[test]
    fn unknown_targets_name_the_configuration() {
        let f = Fixture::new();
        let repair = f.repair(json!({}), json!({}));
        let listed = call(&repair, json!({"action": "targets"})).unwrap();
        assert_eq!(listed["targets"], json!({"project": {"root": f.root, "test_recipe": false}}));
        let refused = call(&repair, json!({"action": "classify", "target": "projcet", "changes": []})).unwrap_err();
        assert_eq!(refused.kind, "PermissionError");
        assert!(refused.message.contains("PALETTE_PATCH_ROOTS"), "{refused}");
        assert_eq!(call(&repair, json!({"action": "classify", "target": "project"})).unwrap_err().message, "classify requires changes");
        assert_eq!(call(&repair, json!({"action": "nope"})).unwrap_err().message,
                   "Unknown repair action; expected targets, detect, classify, propose, test, apply or status");
        for id in [json!("x"), json!(null), json!("A".repeat(32))] {
            assert_eq!(call(&repair, json!({"action": "status", "repair_id": id})).unwrap_err().message, "Invalid repair_id");
        }
        assert_eq!(kind(call(&repair, json!({"action": "propose", "target": "project", "changes": []}))), "PermissionError");
    }

    #[test]
    fn detect_reports_hashes_and_classes() {
        let f = Fixture::new().recipes();
        let repair = f.repair(json!({}), json!({"project": [["recipes", "recipes"]]}));
        let found = call(&repair, json!({"action": "detect", "target": "project", "paths": ["recipes/value.py", "new.txt"]})).unwrap();
        assert_eq!(found["status"], "detected");
        assert_eq!(found["files"][0]["sha256"], sha("VALUE = 'before'\n"));
        assert_eq!(found["files"][0]["bytes"], 17);
        assert_eq!(found["files"][0]["class"], "recipes");
        assert_eq!(found["files"][1], json!({"path": "new.txt", "sha256": null, "bytes": 0, "class": "unknown", "self_apply": false,
                                             "reason": "no operator-configured ordinary path class"}));
        for args in [json!({"action": "detect", "target": "nope", "paths": ["a"]}), json!({"action": "detect", "target": "project", "paths": []})] {
            assert_eq!(call(&repair, args).unwrap_err().message, "detect requires a configured target and 1..16 paths");
        }
    }

    #[test]
    fn copy_stage_refuses_links_and_skips_secrets() {
        let f = Fixture::new();
        let source = f.tmp.join("src");
        fs::create_dir_all(source.join("sub/.git")).unwrap();
        fs::write(source.join("sub/keep.txt"), "x").unwrap();
        fs::write(source.join("sub/.git/HEAD"), "x").unwrap();
        fs::write(source.join(".env"), "secret").unwrap();
        let classify = |relative: &str| Ok(if relative == ".env" { "forbidden".to_string() } else { "unknown".to_string() });
        let stage = f.tmp.join("stage");
        assert_eq!(copy_stage(&source, &stage, &classify).unwrap(), 7);
        assert!(stage.join("sub/keep.txt").is_file());
        assert!(!stage.join(".env").exists() && !stage.join("sub/.git").exists());
        symlink(source.join("sub"), source.join("linked")).unwrap();
        assert_eq!(copy_stage(&source, &f.tmp.join("stage2"), &classify).unwrap_err().message,
                   "Repair test trees cannot contain symlinked directories");
        fs::remove_file(source.join("linked")).unwrap();
        symlink(source.join("sub/keep.txt"), source.join("file-link")).unwrap();
        assert_eq!(copy_stage(&source, &f.tmp.join("stage3"), &classify).unwrap_err().message,
                   "Repair test trees can contain regular files only");
    }

    #[test]
    fn merged_output_and_timeouts() {
        let sh = |script: &str| vec!["/bin/sh".to_string(), "-c".into(), script.into()];
        assert_eq!(run_merged(&sh("echo out; echo err >&2; exit 3"), Duration::from_secs(10)).unwrap(), (Some(3), b"out\nerr\n".to_vec()));
        assert_eq!(run_merged(&sh("kill -9 $$"), Duration::from_secs(10)).unwrap().0, Some(-9));
        assert_eq!(run_merged(&sh("echo -n $HOME"), Duration::from_secs(10)).unwrap().1, b"");
        let started = Instant::now();
        assert_eq!(run_merged(&sh("echo early; exec sleep 30"), Duration::from_millis(300)).unwrap(), (None, b"early\n".to_vec()));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn r01_tested_ordinary_repair_applies_exact_digest() {
        let f = Fixture::new().recipes();
        let repair = f.repair(json!({"project": python_recipe("fixed-unittest-v1", &UNITTEST)}), json!({"project": [["recipes", "recipes"]]}));
        let proposal = call(&repair, json!({"action": "propose", "target": "project", "test_command": ["this-is-ignored"],
            "changes": [{"path": "recipes/value.py", "before_sha256": sha("VALUE = 'before'\n"), "content": "VALUE = 'after'\n"}]})).unwrap();
        assert_eq!(proposal["classifications"][0]["class"], "recipes");
        let started = call(&repair, json!({"action": "test", "repair_id": proposal["repair_id"]})).unwrap();
        assert_eq!(started["status"], "testing", "{started}");
        assert_eq!(call(&repair, json!({"action": "test", "repair_id": proposal["repair_id"]})).map(|_| ()).unwrap_err().kind, "PermissionError");
        let tested = repair.wait_test(&proposal["repair_id"], "world-a").unwrap();
        assert_eq!(tested["status"], "tested", "{tested}");
        assert!(tested["test"]["output"].as_str().unwrap().contains("Ran 1 test"), "{tested}");
        let applied = call(&repair, json!({"action": "apply", "repair_id": proposal["repair_id"]})).unwrap();
        assert_eq!(applied["status"], "applied", "{applied}");
        assert_eq!(f.read("recipes/value.py"), "VALUE = 'after'\n");
        let status = call(&repair, json!({"action": "status", "repair_id": proposal["repair_id"]})).unwrap();
        assert_eq!(status["status"], "applied");
        assert!(status.get("changes").is_none());
    }

    #[test]
    fn r02_failing_disposable_test_cannot_apply() {
        let f = Fixture::new().recipes();
        let repair = f.repair(json!({"project": python_recipe("fixed-unittest-v1", &UNITTEST)}), json!({"project": [["recipes", "recipes"]]}));
        let proposal = propose(&repair, "recipes/value.py", "VALUE = 'before'\n", "VALUE = 'still-wrong'\n");
        let tested = repair.test(&proposal["repair_id"], "world-a").unwrap();
        assert_eq!(tested["status"], "test_failed", "{tested}");
        assert!(tested["test"]["exit_code"].as_i64().is_some_and(|c| c != 0), "{tested}");
        assert!(tested["test"]["output"].as_str().unwrap().contains("FAILED (failures=1)"), "{tested}");
        assert_eq!(kind(apply(&repair, &proposal)), "PermissionError");
        assert_eq!(f.read("recipes/value.py"), "VALUE = 'before'\n");
    }

    #[test]
    fn r03_protected_unknown_and_credentials_never_self_apply() {
        let f = Fixture::new();
        let classes = json!({"project": [["runtime", "runtime"], ["tools", "tools"], ["recipes", "recipes"], ["adapters", "adapters"]]});
        let repair = f.repair(json!({"project": python_recipe("fixed-unittest-v1", &["-B", "-m", "unittest"])}), classes);
        let classify = |path: &str| repair.patches.classify(&"project".into(), &path.into()).unwrap();
        assert!(!classify("runtime/security/patch_broker.py").self_apply);
        assert!(!classify("runtime/capabilities/manifest.json").self_apply);
        assert_eq!(classify("settings/credentials.json").class, "forbidden");
        assert!(!classify("unclassified/app.py").self_apply);
        assert!(!classify("app/config.py").self_apply);
        let security = util::resolve(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../security"));
        let options = Options { auto_apply_prefixes: json!({"security-tree": [["runtime", "new_helpers"]]}), ..Options::default() };
        let nested = PatchBroker::new(&f.tmp.join("nested-grants"), &roots(&[("security-tree", &security)]), options).unwrap();
        assert!(!nested.classify(&"security-tree".into(), &"new_helpers/module.py".into()).unwrap().self_apply);
    }

    #[test]
    fn r04_repair_record_tampering_cannot_launder_tests() {
        let f = Fixture::new().recipes();
        let repair = f.repair(json!({"project": python_recipe("fixed-unittest-v1", &["-B", "-m", "unittest"])}), json!({"project": [["recipes", "recipes"]]}));
        let proposal = propose(&repair, "recipes/value.py", "VALUE = 'before'\n", "VALUE = 'tested'\n");
        let path = repair.record_path(&proposal["repair_id"]).unwrap();
        let mut record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        record["changes"][0]["content"] = "VALUE = 'laundered'\n".into();
        fs::write(&path, record.to_string()).unwrap();
        assert_eq!(kind(repair.test(&proposal["repair_id"], "world-a")), "PermissionError");
        assert_eq!(kind(apply(&repair, &proposal)), "PermissionError");
        assert_eq!(f.read("recipes/value.py"), "VALUE = 'before'\n");
        record["record_hmac"] = json!(7);
        fs::write(&path, record.to_string()).unwrap();
        assert_eq!(repair.read(&proposal["repair_id"], None).unwrap_err().message, "Stored repair differs from its bound digest");
    }

    #[test]
    fn r05_host_approval_retry_keeps_test_evidence() {
        let f = Fixture::new();
        fs::create_dir_all(f.root.join("runtime/security")).unwrap();
        fs::write(f.root.join("runtime/security/logic.py"), "VALUE = 'before'\n").unwrap();
        fs::write(f.root.join("test_recipe.py"), "import unittest\nfrom pathlib import Path\nclass RepairTests(unittest.TestCase):\n    def test_candidate(self): self.assertIn(\"after\", Path('runtime/security/logic.py').read_text())\n").unwrap();
        let repair = f.repair(json!({"project": python_recipe("fixed-unittest-v1", &UNITTEST)}), json!({}));
        let proposal = propose(&repair, "runtime/security/logic.py", "VALUE = 'before'\n", "VALUE = 'after'\n");
        assert_eq!(repair.test(&proposal["repair_id"], "world-a").unwrap()["status"], "tested");
        let pending = apply(&repair, &proposal).unwrap();
        assert_eq!(pending["status"], "tested", "{pending}");
        assert_eq!(pending["apply"]["status"], "approval_required", "{pending}");
        repair.patches.approve(&proposal["patch_request_id"], proposal["patch_sha256"].as_str().unwrap(), "trusted_test_admin").unwrap();
        let applied = apply(&repair, &proposal).unwrap();
        assert_eq!(applied["status"], "applied", "{applied}");
        assert_eq!(f.read("runtime/security/logic.py"), "VALUE = 'after'\n");
    }

    #[test]
    fn r06_test_recipe_arguments_are_bound_to_result() {
        let f = Fixture::new().recipes();
        let patches = patches(&f.store, &roots(&[("project", &f.root)]));
        let recipes = json!({"project": python_recipe("fixed-unittest-v1", &["-B", "-m", "unittest", "test_recipe"])});
        let mut repair = RepairBroker::new(patches, recipes.as_object().unwrap(), &json!({"project": [["recipes", "recipes"]]}), &[]).unwrap();
        let proposal = propose(&repair, "recipes/value.py", "VALUE = 'before'\n", "VALUE = 'after'\n");
        assert_eq!(repair.test(&proposal["repair_id"], "world-a").unwrap()["status"], "tested");
        repair.test_recipes.get_mut("project").unwrap().insert("argv".into(), json!(["/usr/bin/false"]));
        assert_eq!(apply(&repair, &proposal).unwrap_err().message, "Trusted test recipe changed since proposal");
        assert_eq!(f.read("recipes/value.py"), "VALUE = 'before'\n");
    }

    #[test]
    fn r10_disposable_candidate_cannot_read_staged_credentials() {
        let f = Fixture::new().recipes();
        let secrets = [(".env", "ENV_SENTINEL_PRIVATE_FIXTURE\n"), ("credentials/token", "CREDENTIALS_SENTINEL_PRIVATE_FIXTURE\n"),
                       (".ssh/id_rsa", "SSH_SENTINEL_PRIVATE_FIXTURE\n")];
        for (relative, content) in secrets {
            let path = f.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        let candidate = "from pathlib import Path\nfor candidate in ('.env', 'credentials/token', '.ssh/id_rsa'):\n    try: print(Path(candidate).read_text(), end='')\n    except FileNotFoundError: pass\nVALUE = 'after'\n";
        let repair = f.repair(json!({"project": python_recipe("fixed-unittest-v1", &UNITTEST)}), json!({"project": [["recipes", "recipes"]]}));
        let proposal = propose(&repair, "recipes/value.py", "VALUE = 'before'\n", candidate);
        let tested = repair.test(&proposal["repair_id"], "world-a").unwrap();
        assert_eq!(tested["status"], "tested", "{tested}");
        let output = tested["test"]["output"].as_str().unwrap();
        for (_, marker) in secrets {
            assert!(!output.contains(marker.trim()), "{output}");
        }
        assert!(output.contains("Ran 1 test"), "{output}");
        assert_eq!(f.read("recipes/value.py"), "VALUE = 'before'\n");
    }

    #[test]
    fn a_failed_verification_is_recorded() {
        let f = Fixture::new().recipes();
        let repair = f.repair(json!({"project": python_recipe("fixed-unittest-v1", &UNITTEST)}), json!({"project": [["recipes", "recipes"]]}));
        let proposal = propose(&repair, "recipes/value.py", "VALUE = 'before'\n", "VALUE = 'after'\n");
        // The staged source no longer matches the proposal's before hash.
        fs::write(f.root.join("recipes/value.py"), "VALUE = 'moved on'\n").unwrap();
        call(&repair, json!({"action": "test", "repair_id": proposal["repair_id"]})).unwrap();
        let failed = repair.wait_test(&proposal["repair_id"], "world-a").unwrap_err();
        assert_eq!(failed.message, "Staged source differs from proposed before hash: recipes/value.py");
        let status = call(&repair, json!({"action": "status", "repair_id": proposal["repair_id"]})).unwrap();
        assert_eq!(status["status"], "test_failed");
        assert_eq!(status["test"], json!({"status": "failed", "error": failed.message}));
        assert_eq!(repair.wait_test(&"0".repeat(32).into(), "world-a").unwrap_err().kind, "FileNotFoundError");
    }

    #[test]
    fn an_interrupted_verification_is_reported() {
        let f = Fixture::new().recipes();
        let repair = f.repair(json!({"project": python_recipe("fixed-unittest-v1", &UNITTEST)}), json!({"project": [["recipes", "recipes"]]}));
        let proposal = propose(&repair, "recipes/value.py", "VALUE = 'before'\n", "VALUE = 'after'\n");
        let mut record = repair.read(&proposal["repair_id"], None).unwrap();
        record.insert("status".into(), "testing".into());
        repair.store(&mut record).unwrap();
        let fresh = f.repair(json!({"project": python_recipe("fixed-unittest-v1", &UNITTEST)}), json!({"project": [["recipes", "recipes"]]}));
        let status = call(&fresh, json!({"action": "status", "repair_id": proposal["repair_id"]})).unwrap();
        assert_eq!(status["status"], "interrupted_completion_unknown");
        assert_eq!(fresh.wait_test(&proposal["repair_id"], "world-a").unwrap_err().kind, "KeyError");
        assert_eq!(call(&fresh, json!({"action": "status", "repair_id": proposal["repair_id"]})).map(|_| ()).err(), None);
        assert_eq!(fresh.read(&proposal["repair_id"], Some("world-b")).unwrap_err().message, "Repair belongs to a different workspace");
    }

    #[test]
    fn a_passing_repair_test_cannot_approve_a_trusted_tool() {
        let f = Fixture::new();
        let trusted = f.tmp.join("trusted/tools");
        fs::create_dir_all(&trusted).unwrap();
        let broker = patches(&f.store, &roots(&[(TRUSTED_TOOLS_TARGET, &trusted)]));
        let recipes = json!({TRUSTED_TOOLS_TARGET: python_recipe("always-passes-v1", &["-B", "-c", "pass"])});
        let repair = Arc::new(RepairBroker::new(broker, recipes.as_object().unwrap(), &json!({}), &[]).unwrap());
        let changes = json!([{"path": "helper.jl", "before_sha256": null, "content": "helper(x) = x + 1\n"}]);
        let proposed = call(&repair, json!({"action": "propose", "target": TRUSTED_TOOLS_TARGET, "changes": changes})).unwrap();
        let rows = repair.classify_changes(&TRUSTED_TOOLS_TARGET.into(), &changes).unwrap();
        assert!(rows.as_array().unwrap().iter().all(|row| row["self_apply"] == false));
        call(&repair, json!({"action": "test", "repair_id": proposed["repair_id"]})).unwrap();
        assert_eq!(repair.wait_test(&proposed["repair_id"], "world-a").unwrap()["status"], "tested");
        let applied = call(&repair, json!({"action": "apply", "repair_id": proposed["repair_id"]})).unwrap();
        assert_ne!(applied["status"], "applied", "{applied}");
        assert!(!trusted.join("helper.jl").exists());
        assert_eq!(repair.patches.inspect(&proposed["patch_request_id"], Some("world-a")).unwrap()["status"], "pending");
    }

    /// Replays a scripted session for the scratch parity check against
    /// `repair_broker.py`: `cargo test repair_parity -- --ignored`.
    #[test]
    #[ignore]
    fn repair_parity() {
        let Ok(request) = std::env::var("PALETTE_REPAIR_PARITY") else { return };
        let request: Value = serde_json::from_str(&fs::read_to_string(request).unwrap()).unwrap();
        let shown = |r: Result<Value, Fault>| r.map_or_else(|f| json!({"error": f.to_string()}), |v| json!({"ok": v}));
        let options = Options { auto_apply_prefixes: request["patch_auto"].clone(), ..Options::default() };
        let built = PatchBroker::new(Path::new(request["storage"].as_str().unwrap()), request["roots"].as_object().unwrap(), options)
            .and_then(|mut patches| {
                patches.host = host();
                let protected: Vec<PathBuf> = strings(&request["protected"]).into_iter().map(PathBuf::from).collect();
                RepairBroker::new(patches, request["recipes"].as_object().unwrap(), &request["auto"], &protected)
            });
        let mut out = Map::new();
        match built {
            Err(f) => {
                out.insert("init".into(), json!({"error": f.to_string()}));
            }
            Ok(repair) => {
                out.insert("init".into(), json!({"ok": null}));
                let repair = Arc::new(repair);
                let mut last = Map::new();
                let mut results = Vec::new();
                for step in request["steps"].as_array().unwrap() {
                    let current = |key: &str| last.get(key).cloned().unwrap_or(Value::Null);
                    let result = match step[0].as_str().unwrap() {
                        "call" => {
                            let mut args = step[1].as_object().unwrap().clone();
                            for (key, slot) in [("repair_id", "$last"), ("request_id", "$patch")] {
                                if args.get(key).and_then(Value::as_str) == Some(slot) {
                                    args.insert(key.into(), current(if key == "repair_id" { "repair_id" } else { "patch_request_id" }));
                                }
                            }
                            repair.call(&args, "world-a", &Map::new(), &mut Silent)
                        }
                        "test" => repair.test(&current("repair_id"), "world-a"),
                        "wait" => repair.wait_test(&current("repair_id"), "world-a"),
                        "approve" => repair.patches.approve(&current("patch_request_id"), current("patch_sha256").as_str().unwrap(), "parity")
                            .map(|()| Value::Null),
                        "write" => {
                            fs::write(step[1].as_str().unwrap(), step[2].as_str().unwrap()).unwrap();
                            Ok(Value::Null)
                        }
                        "tamper" => {
                            let path = repair.record_path(&current("repair_id")).unwrap();
                            let mut record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
                            record[step[1].as_str().unwrap()] = step[2].clone();
                            fs::write(&path, record.to_string()).unwrap();
                            Ok(Value::Null)
                        }
                        other => panic!("unknown step {other}"),
                    };
                    if let Ok(Value::Object(fields)) = &result {
                        for key in ["repair_id", "patch_request_id", "patch_sha256"] {
                            if let Some(value) = fields.get(key) {
                                last.insert(key.into(), value.clone());
                            }
                        }
                    }
                    results.push(shown(result));
                }
                out.insert("steps".into(), Value::Array(results));
            }
        }
        fs::write(std::env::var("PALETTE_REPAIR_PARITY_OUT").unwrap(), Value::Object(out).to_string()).unwrap();
    }
}
