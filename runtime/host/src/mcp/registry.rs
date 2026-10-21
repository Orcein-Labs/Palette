//! The host-side index of sandboxed Julia capability source, ported from
//! runtime_registry.py. It reports content and authority facts and hands
//! source text to the Julia worker; it never runs capability code.
//!
//! The active generation is what workers see. A refresh loads the root again
//! into a staged generation, which the caller commits once the turn that
//! asked for it has been snapshotted, or cancels.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::Fault;
use crate::{provisioning, util};

const FIELDS: &[&str] = &["id", "version", "source", "entrypoint", "dependencies", "requires", "description"];
const REQUIRED: &[&str] = &["entrypoint", "id", "source", "version"];
const FORBIDDEN: &[&str] = &[
    "ceiling", "credentials", "credential", "callback", "callbacks", "host_callback", "host_callbacks", "host_command", "argv", "secret",
];
const AUTHORITY: &[&str] = &["external_fs_write", "fs_digest", "host_command", "network_access", "package_management", "spawn_child_worker", "teamlab"];

fn refused(message: impl Into<String>) -> Fault {
    Fault::new("RuntimeRegistryError", message)
}

/// sha256 of `json.dumps(value, sort_keys=True, separators=(",", ":"),
/// ensure_ascii=False)`: serde's compact form, keys already sorted, escapes
/// only what Python escapes. Registry values hold no floats.
fn digest(value: &Value) -> String {
    util::sha256_hex(value.to_string().as_bytes())
}

/// `^[A-Za-z][A-Za-z0-9_.-]{0,63}$`
fn capability_id(s: &str) -> bool {
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    s.len() <= 64 && s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.chars().all(ok)
}

/// `PurePath.suffix` as Python 3.14 reads it: from the last dot, after any
/// leading dots, so `..jl` has none and `a.` has `.`.
fn py_suffix(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = name.trim_start_matches('.');
    name.rfind('.').map_or_else(String::new, |i| name[i..].to_string())
}

/// True when `path` lies strictly inside `dir` (`dir in path.parents`).
fn inside(path: &Path, dir: &Path) -> bool {
    path != dir && path.starts_with(dir)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `Path.read_text(encoding="utf-8")`: undecodable bytes are a
/// UnicodeDecodeError, and newlines are read universally.
fn read_text(path: &Path) -> Result<Result<String, Fault>, std::io::Error> {
    let bytes = fs::read(path)?;
    Ok(match std::str::from_utf8(&bytes) {
        Ok(text) => Ok(text.replace("\r\n", "\n").replace('\r', "\n")),
        Err(e) => Err(Fault::new("UnicodeDecodeError", util::py_utf8_error(&bytes, &e))),
    })
}

fn sha_file(path: &Path) -> Result<String, Fault> {
    fs::read(path).map(|b| util::sha256_hex(&b)).map_err(|e| Fault::os(&e, path))
}

/// The entries of `dir` named `*{suffix}`, as `glob` (or, `recursive`,
/// `rglob`) yields them: hidden names and dangling symlinks included,
/// symlinked directories not descended into.
fn glob(dir: &Path, suffix: &str, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().ends_with(suffix) {
            out.push(path.clone());
        }
        if recursive && entry.file_type().is_ok_and(|t| t.is_dir()) {
            glob(&path, suffix, true, out);
        }
    }
}

/// A capability's normalized manifest, its source and whether the
/// operator's ceiling grants everything it requires.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    pub manifest: Map<String, Value>,
    pub source: String,
    pub source_sha256: String,
    pub authorized: bool,
}

type Entries = BTreeMap<String, Entry>;

/// A workspace's view of the trusted capability root.
pub(crate) struct Registry {
    root: PathBuf,
    repo: Option<PathBuf>,
    host_binary: Option<PathBuf>,
    ceiling: Value,
    adapter_identity: Map<String, Value>,
    active: Entries,
    active_generation: String,
    staged: Option<(Entries, String)>,
    runtime_identity: Value,
}

impl Registry {
    /// Loads the active generation from `root`, which must not overlap the
    /// workspace or its session state. Paths are resolved here; `~` is the
    /// caller's to expand.
    pub fn new(root: &Path, ceiling: &Value, workspace: &Path, state_dir: &Path, repo: Option<&Path>, host_binary: Option<&Path>)
        -> Result<Registry, Fault> {
        let root = util::resolve(root);
        let overlaps = |other: &Path| {
            let other = util::resolve(other);
            root.starts_with(&other) || other.starts_with(&root)
        };
        if overlaps(workspace) {
            return Err(refused("runtime root must be disjoint from the workspace"));
        }
        if overlaps(state_dir) {
            return Err(refused("runtime root must be disjoint from session state"));
        }
        let mut registry = Registry {
            root,
            repo: repo.map(util::resolve),
            host_binary: host_binary.map(util::resolve),
            ceiling: ceiling.clone(),
            adapter_identity: Map::new(),
            active: Entries::new(),
            active_generation: String::new(),
            staged: None,
            runtime_identity: Value::Null,
        };
        registry.adapter_identity = registry.adapter_hashes()?;
        (registry.active, registry.active_generation) = registry.load()?;
        registry.runtime_identity = registry.identity()?;
        Ok(registry)
    }

    /// The adapter source the identity covers: `runtime/security/*.py`
    /// without the tests, by path in the checkout.
    fn adapter_hashes(&self) -> Result<Map<String, Value>, Fault> {
        let mut observed = Map::new();
        let Some(repo) = &self.repo else { return Ok(observed) };
        let dir = repo.join("runtime/security");
        if !dir.is_dir() {
            return Ok(observed);
        }
        let mut paths = Vec::new();
        glob(&dir, ".py", false, &mut paths);
        paths.sort();
        for path in paths {
            if path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("test_")) {
                continue;
            }
            let rel = path.strip_prefix(repo).unwrap_or(&path).to_string_lossy().into_owned();
            observed.insert(rel, Value::String(sha_file(&path)?));
        }
        Ok(observed)
    }

    /// The version and content digest of the runtime that runs capabilities:
    /// Julia source, host source, Project.toml and the host binary.
    fn identity(&self) -> Result<Value, Fault> {
        let mut protected = Map::new();
        if let Some(repo) = &self.repo {
            for (dir, suffix) in [(repo.join("src"), ".jl"), (repo.join("runtime/host/src"), ".rs")] {
                if dir.is_dir() {
                    let mut paths = Vec::new();
                    glob(&dir, suffix, true, &mut paths);
                    for path in paths {
                        let rel = path.strip_prefix(repo).unwrap_or(&path).to_string_lossy().into_owned();
                        protected.insert(rel, Value::String(sha_file(&path)?));
                    }
                }
            }
            let project = repo.join("Project.toml");
            if project.is_file() {
                protected.insert("Project.toml".into(), Value::String(sha_file(&project)?));
            }
        }
        if let Some(binary) = self.host_binary.as_deref().filter(|b| b.is_file()) {
            protected.insert("palette-host".into(), Value::String(sha_file(binary)?));
        }
        let version = self.repo.as_deref().and_then(|r| provisioning::palette_version(r).ok()).unwrap_or_else(|| "unknown".into());
        let adapter = digest(&Value::Object(self.adapter_identity.clone()));
        Ok(json!({
            "version": version,
            "content_sha256": digest(&json!({"runtime": protected, "adapter_content_sha256": adapter})),
            "adapter_content_sha256": adapter,
        }))
    }

    fn generation(entries: &Entries) -> String {
        let content: Map<String, Value> = entries.iter()
            .map(|(name, e)| (name.clone(), json!({"manifest": e.manifest, "source_sha256": e.source_sha256})))
            .collect();
        digest(&Value::Object(content))
    }

    /// Every capability under the root, checked, with its generation digest.
    fn load(&self) -> Result<(Entries, String), Fault> {
        let mut entries = Entries::new();
        if fs::metadata(&self.root).is_err() {
            let generation = Self::generation(&entries);
            return Ok((entries, generation));
        }
        if !self.root.is_dir() {
            return Err(refused("runtime root is not a directory"));
        }
        let mut found: Vec<PathBuf> = fs::read_dir(&self.root).map_err(|e| Fault::os(&e, &self.root))?
            .flatten()
            .map(|e| e.path().join("capability.json"))
            .filter(|p| fs::symlink_metadata(p).is_ok())
            .collect();
        found.sort();
        for path in found {
            let dir_name = path.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let manifest_path = util::resolve_strict(&path).map_err(|(e, at)| Fault::os(&e, &at))?;
            if !inside(&manifest_path, &self.root) {
                return Err(refused("capability manifest escapes runtime root"));
            }
            let unreadable = |what: String| refused(format!("cannot read capability manifest {dir_name}: {what}"));
            let text = read_text(&manifest_path).map_err(|e| unreadable(util::py_os_error(&e, &manifest_path)))??;
            let manifest = super::py_json_loads(&text).map_err(|f| unreadable(f.message))?;
            let Value::Object(manifest) = manifest else {
                return Err(refused(format!("capability manifest {} must be an object", path.display())));
            };
            let entry = self.check(&dir_name, &manifest_path, manifest)?;
            let identifier = entry.manifest["id"].as_str().unwrap_or_default().to_string();
            if entries.contains_key(&identifier) {
                return Err(refused(format!("duplicate capability id {}", util::py_str(&identifier))));
            }
            entries.insert(identifier, entry);
        }
        let generation = Self::generation(&entries);
        Ok((entries, generation))
    }

    fn check(&self, dir_name: &str, manifest_path: &Path, manifest: Map<String, Value>) -> Result<Entry, Fault> {
        reject_forbidden(&Value::Object(manifest.clone()))?;
        let unknown: Vec<Value> = manifest.keys().filter(|k| !FIELDS.contains(&k.as_str())).map(|k| json!(k)).collect();
        let missing: Vec<Value> = REQUIRED.iter().filter(|k| !manifest.contains_key(**k)).map(|k| json!(k)).collect();
        if !unknown.is_empty() || !missing.is_empty() {
            return Err(refused(format!("capability manifest fields invalid (unknown={}, missing={})",
                util::py_repr(&Value::Array(unknown)), util::py_repr(&Value::Array(missing)))));
        }
        let identifier = match manifest["id"].as_str() {
            Some(id) if capability_id(id) && id == dir_name => id.to_string(),
            _ => return Err(refused("capability id must match its directory name")),
        };
        let shown = util::py_str(&identifier);
        if !manifest["version"].as_str().is_some_and(|v| !v.is_empty()) {
            return Err(refused(format!("capability {shown} version must be non-empty")));
        }
        if !manifest["entrypoint"].as_str().is_some_and(provisioning::julia_identifier) {
            return Err(refused(format!("capability {shown} entrypoint must be a Julia identifier")));
        }
        let source_name = match manifest["source"].as_str() {
            Some(s) if !s.is_empty() && !s.starts_with('/') => s,
            _ => return Err(refused(format!("capability {shown} source must be a relative path"))),
        };
        let dir = manifest_path.parent().unwrap_or(Path::new("/"));
        let source_path = util::resolve_strict(&dir.join(source_name)).map_err(|(e, at)| Fault::os(&e, &at))?;
        if !inside(&source_path, &util::resolve(dir)) || !source_path.is_file() || py_suffix(&source_path) != ".jl" {
            return Err(refused(format!("capability {shown} source must be a Julia file inside its directory")));
        }
        let empty = Value::Array(Vec::new());
        let dependencies = strings(manifest.get("dependencies").unwrap_or(&empty), &format!("{identifier}.dependencies"), true)?;
        let requires = strings(manifest.get("requires").unwrap_or(&empty), &format!("{identifier}.requires"), false)?;
        let known = |item: &String| AUTHORITY.contains(&item.as_str()) || item.strip_prefix("host_request:").is_some_and(|t| !t.is_empty());
        if !requires.iter().all(known) {
            return Err(refused(format!("capability {shown} declares an unknown required capability")));
        }
        let authorized = requires.iter().all(|r| self.grants(r));
        let description = match manifest.get("description") {
            None => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(refused(format!("capability {shown} description must be a string"))),
        };
        let mut manifest = manifest;
        manifest.insert("dependencies".into(), json!(dependencies));
        manifest.insert("requires".into(), json!(requires));
        manifest.insert("description".into(), json!(description));
        let source = read_text(&source_path).map_err(|e| Fault::os(&e, &source_path))??;
        let source_sha256 = util::sha256_hex(source.as_bytes());
        Ok(Entry { manifest, source, source_sha256, authorized })
    }

    /// Whether the operator's ceiling grants `requirement`.
    fn grants(&self, requirement: &str) -> bool {
        if let Some(host_type) = requirement.strip_prefix("host_request:") {
            let allowed = self.ceiling.get("host_request").and_then(|g| g.get("allowed_types")).and_then(Value::as_array);
            return !host_type.is_empty() && allowed.is_some_and(|a| a.iter().any(|t| t.as_str() == Some(host_type)));
        }
        let Some(grant) = self.ceiling.get(requirement).filter(|g| g.is_object()) else { return false };
        match requirement {
            "network_access" => grant.get("allowed") == Some(&Value::Bool(true)),
            "package_management" => truthy(grant.get("allowed_packages")),
            "host_command" => truthy(grant.get("commands")),
            "external_fs_write" => truthy(grant.get("allowed_dirs")),
            "fs_digest" => truthy(grant.get("allowed_paths")),
            _ => AUTHORITY.contains(&requirement),
        }
    }

    /// The toolchains a capability can rely on: the operator's host commands
    /// whose binary, working directory and artifacts are all present, then
    /// the native ones the host can find. Python's own interpreter was the
    /// adapter's; the port reports `python3` on PATH instead.
    pub fn toolchains(&self) -> Vec<Value> {
        let mut installed: BTreeMap<String, (bool, &str)> = BTreeMap::new();
        let commands = self.ceiling.get("host_command").and_then(|g| g.get("commands")).and_then(Value::as_object);
        for (name, spec) in commands.into_iter().flatten() {
            let argv: Vec<&str> = spec.get("argv").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            let ready = argv.first().is_some_and(|exe| util::executable(Path::new(exe)))
                && spec.get("cwd").and_then(Value::as_str).is_some_and(|cwd| Path::new(cwd).is_dir())
                && provisioning::configured_path_arguments(&argv).iter().all(|p| Path::new(p).exists());
            installed.insert(name.clone(), (ready, "operator_host_command"));
        }
        let julia = std::env::var_os("PALETTE_JULIA").filter(|s| !s.is_empty()).map(PathBuf::from).or_else(|| util::which("julia"));
        for (name, exe) in [("julia", julia), ("python", util::which("python3")), ("rust", util::which("cargo"))] {
            installed.entry(name.into()).or_insert((exe.is_some_and(|e| util::executable(&e)), "runtime_host"));
        }
        installed.into_iter()
            .map(|(name, (ready, source))| json!({"name": name, "status": if ready { "ready" } else { "missing" }, "source": source}))
            .collect()
    }

    fn summary(&self, entries: &Entries) -> Vec<Value> {
        entries.iter().map(|(id, entry)| {
            let m = &entry.manifest;
            let state = match self.active.get(id) {
                Some(a) if a.source_sha256 == entry.source_sha256 && a.manifest == entry.manifest => "active",
                Some(_) => "available_update",
                None => "available",
            };
            json!({"id": id, "version": m["version"], "content_sha256": entry.source_sha256, "description": m["description"],
                   "dependencies": m["dependencies"], "required_capabilities": m["requires"], "authorized": entry.authorized,
                   "state": state})
        }).collect()
    }

    /// The ceiling as a worker may see it: names and counts, never paths,
    /// hosts or command lines.
    fn authorized_summary(&self) -> Map<String, Value> {
        let mut summary = Map::new();
        let sorted = |v: Option<&Value>| {
            let mut items: Vec<String> = v.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
            items.sort();
            items
        };
        let count = |v: Option<&Value>| v.and_then(Value::as_array).map_or(0, Vec::len);
        for (name, grant) in self.ceiling.as_object().into_iter().flatten() {
            let shown = match name.as_str() {
                "host_request" => json!({"allowed_types": sorted(grant.get("allowed_types"))}),
                "package_management" => json!({"allowed_packages": sorted(grant.get("allowed_packages")),
                                               "offline": grant.get("offline").cloned().unwrap_or(Value::Bool(false))}),
                "host_command" => {
                    let names: Vec<&String> = grant.get("commands").and_then(Value::as_object).into_iter().flatten().map(|(k, _)| k).collect();
                    json!({"command_names": names})
                }
                "external_fs_write" => json!({"configured_path_count": count(grant.get("allowed_dirs"))}),
                "fs_digest" => json!({"configured_path_count": count(grant.get("allowed_paths"))}),
                "network_access" => json!({"allowed": grant.get("allowed") == Some(&Value::Bool(true)),
                                           "configured_host_count": count(grant.get("allowed_hosts"))}),
                "spawn_child_worker" | "teamlab" => json!({"enabled": true}),
                _ => continue,
            };
            summary.insert(name.clone(), shown);
        }
        summary
    }

    /// The active and available capabilities, the runtime identity and the
    /// operator's grants.
    pub fn discover(&self, epoch: &Value) -> Result<Value, Fault> {
        let (available, available_generation) = self.load()?;
        let names = |authorized: bool| available.iter().filter(|(_, e)| e.authorized == authorized).map(|(n, _)| n.clone()).collect::<Vec<_>>();
        let dependencies: Map<String, Value> = available.iter().map(|(n, e)| (n.clone(), e.manifest["dependencies"].clone())).collect();
        Ok(json!({
            "epoch": epoch,
            "runtime": self.runtime_identity,
            "capability_generation": self.active_generation,
            "available_generation": available_generation,
            "installed_toolchains": self.toolchains(),
            "capabilities": self.summary(&self.active),
            "available_capabilities": self.summary(&available),
            "authorized_capabilities": names(true),
            "unauthorized_capabilities": names(false),
            "authorized_ceiling": self.authorized_summary(),
            "dependencies_by_capability": dependencies,
        }))
    }

    /// The source of an active, authorized capability, for the worker to run.
    pub fn invoke(&self, payload: &Map<String, Value>) -> Result<Value, Fault> {
        let Some((identifier, entry)) = payload.get("id").and_then(Value::as_str).and_then(|id| self.active.get_key_value(id)) else {
            return Err(refused("unknown capability in active generation"));
        };
        if !entry.authorized {
            return Err(Fault::permission(format!("capability {} requires authority outside the operator ceiling", util::py_str(identifier))));
        }
        Ok(json!({"id": identifier, "generation": self.active_generation, "manifest": entry.manifest,
                  "source": entry.source, "source_sha256": entry.source_sha256}))
    }

    /// Loads the root again into the staged generation.
    pub fn prepare_refresh(&mut self, epoch: &Value) -> Result<Value, Fault> {
        let previous = self.active_generation.clone();
        let (entries, generation) = self.load()?;
        let reply = json!({"previous_generation": previous, "capability_generation": generation, "available_generation": generation,
                           "changed": previous != generation, "epoch_before_refresh": epoch});
        self.staged = Some((entries, generation));
        Ok(reply)
    }

    /// Makes the staged generation active.
    pub fn commit_refresh(&mut self) -> Result<Value, Fault> {
        let (entries, generation) = self.staged.take().ok_or_else(|| refused("no staged runtime generation is ready"))?;
        let previous = std::mem::replace(&mut self.active_generation, generation);
        self.active = entries;
        self.runtime_identity = self.identity()?;
        Ok(json!({"previous_generation": previous, "capability_generation": self.active_generation,
                  "changed": previous != self.active_generation}))
    }

    pub fn cancel_refresh(&mut self) {
        self.staged = None;
    }

    /// A worker's `palette.runtime` request: discover, invoke or refresh.
    pub fn handle(&mut self, action: &Value, payload: &Map<String, Value>, epoch: &Value) -> Result<Value, Fault> {
        match action.as_str() {
            Some("discover") => self.discover(epoch),
            Some("invoke") => self.invoke(payload),
            Some("refresh") => self.prepare_refresh(epoch),
            _ => Err(refused(format!("unknown runtime action {}", util::py_repr(action)))),
        }
    }
}

/// A manifest may not carry authority or secrets, at any depth.
fn reject_forbidden(value: &Value) -> Result<(), Fault> {
    match value {
        Value::Object(o) => o.iter().try_for_each(|(key, child)| {
            if FORBIDDEN.contains(&key.to_lowercase().as_str()) {
                return Err(refused(format!("manifest contains forbidden field {}", util::py_str(key))));
            }
            reject_forbidden(child)
        }),
        Value::Array(a) => a.iter().try_for_each(reject_forbidden),
        _ => Ok(()),
    }
}

/// A list of distinct non-empty strings, Julia identifiers if `julia`.
fn strings(value: &Value, field: &str, julia: bool) -> Result<Vec<String>, Fault> {
    let items: Vec<String> = value.as_array()
        .and_then(|a| a.iter().map(|v| v.as_str().filter(|s| !s.is_empty()).map(String::from)).collect::<Option<_>>())
        .ok_or_else(|| refused(format!("{field} must be a list of non-empty strings")))?;
    if julia && !items.iter().all(|i| provisioning::julia_identifier(i)) {
        return Err(refused(format!("{field} contains an invalid identifier")));
    }
    let distinct: std::collections::BTreeSet<&String> = items.iter().collect();
    if distinct.len() != items.len() {
        return Err(refused(format!("{field} must not contain duplicates")));
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    struct Dirs {
        base: PathBuf,
        root: PathBuf,
        workspace: PathBuf,
        state: PathBuf,
    }

    impl Dirs {
        fn new() -> Dirs {
            let base = util::mkdtemp("palette-registry-").unwrap();
            let d = Dirs { root: base.join("root"), workspace: base.join("ws"), state: base.join("state"), base };
            for p in [&d.root, &d.workspace, &d.state] {
                fs::create_dir(p).unwrap();
            }
            d
        }

        fn capability(&self, id: &str, manifest: Value, source: &str) {
            let dir = self.root.join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("main.jl"), source).unwrap();
            fs::write(dir.join("capability.json"), manifest.to_string()).unwrap();
        }

        fn open(&self, ceiling: Value) -> Result<Registry, Fault> {
            Registry::new(&self.root, &ceiling, &self.workspace, &self.state, None, None)
        }
    }

    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn manifest(id: &str) -> Value {
        json!({"id": id, "version": "1", "source": "main.jl", "entrypoint": "safe_entry"})
    }

    fn message(r: Result<Registry, Fault>) -> String {
        r.err().expect("refused").to_string()
    }

    #[test]
    fn suffixes_follow_python_3_14() {
        for (name, suffix) in [("a.jl", ".jl"), (".jl", ""), ("a.", "."), ("a..jl", ".jl"), ("..jl", ""), ("x.tar.jl", ".jl"), ("a.JL", ".JL")] {
            assert_eq!(py_suffix(Path::new(name)), suffix, "{name}");
        }
    }

    #[test]
    fn the_digest_is_pythons_compact_unescaped_dump() {
        // hashlib.sha256(json.dumps({"b": "é\n", "a": [1]}, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode())
        assert_eq!(json!({"b": "é\n", "a": [1]}).to_string(), "{\"a\":[1],\"b\":\"é\\n\"}");
    }

    #[test]
    fn manifests_cannot_carry_authority_or_escape_their_directory() {
        // test_workspace_router.py C03
        let d = Dirs::new();
        let mut bad = manifest("bad-authority");
        bad["ceiling"] = json!({"network_access": {"allowed": true}});
        d.capability("bad-authority", bad, "function safe_entry(args); 1; end\n");
        assert_eq!(message(d.open(json!({}))), "RuntimeRegistryError: manifest contains forbidden field 'ceiling'");
        let mut nested = manifest("bad-authority");
        nested["description"] = json!("x");
        nested["dependencies"] = json!([]);
        nested["version"] = json!({"Callback": 1});
        d.capability("bad-authority", nested, "");
        assert_eq!(message(d.open(json!({}))), "RuntimeRegistryError: manifest contains forbidden field 'Callback'");
        let mut escape = manifest("bad-authority");
        escape["source"] = json!("../outside.jl");
        d.capability("bad-authority", escape, "");
        fs::write(d.root.join("outside.jl"), "").unwrap();
        assert_eq!(message(d.open(json!({}))), "RuntimeRegistryError: capability 'bad-authority' source must be a Julia file inside its directory");
    }

    #[test]
    fn field_checks_read_as_python_wrote_them() {
        let cases = [
            (json!({"id": "a", "zed": 1, "version": "1"}), "capability manifest fields invalid (unknown=['zed'], missing=['entrypoint', 'source'])"),
            (json!({"id": "b", "version": "1", "source": "main.jl", "entrypoint": "e"}), "capability id must match its directory name"),
            (json!({"id": "a", "version": "", "source": "main.jl", "entrypoint": "e"}), "capability 'a' version must be non-empty"),
            (json!({"id": "a", "version": "1", "source": "main.jl", "entrypoint": "1e"}), "capability 'a' entrypoint must be a Julia identifier"),
            (json!({"id": "a", "version": "1", "source": "/main.jl", "entrypoint": "e"}), "capability 'a' source must be a relative path"),
            (json!({"id": "a", "version": "1", "source": "main.jl", "entrypoint": "e", "dependencies": ["X", ""]}), "a.dependencies must be a list of non-empty strings"),
            (json!({"id": "a", "version": "1", "source": "main.jl", "entrypoint": "e", "dependencies": ["X-1"]}), "a.dependencies contains an invalid identifier"),
            (json!({"id": "a", "version": "1", "source": "main.jl", "entrypoint": "e", "requires": ["teamlab", "teamlab"]}), "a.requires must not contain duplicates"),
            (json!({"id": "a", "version": "1", "source": "main.jl", "entrypoint": "e", "requires": ["host_request:"]}), "capability 'a' declares an unknown required capability"),
            (json!({"id": "a", "version": "1", "source": "main.jl", "entrypoint": "e", "description": 3}), "capability 'a' description must be a string"),
            (json!([]), "capability manifest {root}/a/capability.json must be an object"),
        ];
        for (m, expected) in cases {
            let d = Dirs::new();
            d.capability("a", m, "");
            let expected = expected.replace("{root}", &d.root.display().to_string());
            assert_eq!(message(d.open(json!({}))), format!("RuntimeRegistryError: {expected}"));
        }
        let d = Dirs::new();
        d.capability("a", manifest("a"), "");
        fs::write(d.root.join("a/capability.json"), "{\r\n").unwrap();
        let parse = super::super::py_json_loads("{\n").unwrap_err().message;
        assert_eq!(message(d.open(json!({}))), format!("RuntimeRegistryError: cannot read capability manifest a: {parse}"));
        fs::write(d.root.join("a/capability.json"), b"\xff").unwrap();
        assert_eq!(message(d.open(json!({}))), "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 0: invalid start byte");
        fs::remove_file(d.root.join("a/capability.json")).unwrap();
        symlink("nowhere", d.root.join("a/capability.json")).unwrap();
        assert!(message(d.open(json!({}))).starts_with("FileNotFoundError: [Errno 2] No such file or directory: "));
    }

    #[test]
    fn the_root_must_stand_apart_from_the_workspace_and_state() {
        let d = Dirs::new();
        let inner = d.workspace.join("caps");
        assert_eq!(message(Registry::new(&inner, &json!({}), &d.workspace, &d.state, None, None)),
            "RuntimeRegistryError: runtime root must be disjoint from the workspace");
        assert_eq!(message(Registry::new(&d.base, &json!({}), &d.workspace, &d.state, None, None)),
            "RuntimeRegistryError: runtime root must be disjoint from the workspace");
        assert_eq!(message(Registry::new(&d.state.join("caps"), &json!({}), &d.workspace, &d.state, None, None)),
            "RuntimeRegistryError: runtime root must be disjoint from session state");
        let file = d.base.join("file");
        fs::write(&file, "").unwrap();
        assert_eq!(message(Registry::new(&file, &json!({}), &d.workspace, &d.state, None, None)),
            "RuntimeRegistryError: runtime root is not a directory");
        assert!(Registry::new(&d.base.join("absent"), &json!({}), &d.workspace, &d.state, None, None).unwrap().active.is_empty());
    }

    #[test]
    fn grants_decide_authorization_and_invoke_honours_them() {
        let d = Dirs::new();
        let mut net = manifest("net");
        net["requires"] = json!(["network_access", "host_request:shell"]);
        d.capability("net", net, "function safe_entry(args)\r\n 1\rend\n");
        d.capability("plain", manifest("plain"), "x");
        let closed = d.open(json!({"network_access": {"allowed": false}})).unwrap();
        assert_eq!(closed.invoke(&Map::from_iter([("id".into(), json!("net"))])).unwrap_err().to_string(),
            "PermissionError: capability 'net' requires authority outside the operator ceiling");
        assert_eq!(closed.invoke(&Map::new()).unwrap_err().to_string(), "RuntimeRegistryError: unknown capability in active generation");
        let open = d.open(json!({"network_access": {"allowed": true}, "host_request": {"allowed_types": ["shell"]}})).unwrap();
        let reply = open.invoke(&Map::from_iter([("id".into(), json!("net"))])).unwrap();
        assert_eq!(reply["source"], "function safe_entry(args)\n 1\nend\n");
        assert_eq!(reply["source_sha256"], util::sha256_hex(b"function safe_entry(args)\n 1\nend\n"));
        assert_eq!(reply["manifest"]["description"], "");
        let found = open.discover(&json!("e1")).unwrap();
        assert_eq!(found["authorized_capabilities"], json!(["net", "plain"]));
        assert_eq!(found["authorized_ceiling"], json!({"host_request": {"allowed_types": ["shell"]},
            "network_access": {"allowed": true, "configured_host_count": 0}}));
        assert_eq!(found["capabilities"][0]["state"], "active");
    }

    #[test]
    fn a_refresh_is_staged_until_committed() {
        let d = Dirs::new();
        d.capability("a", manifest("a"), "1");
        let mut registry = d.open(json!({})).unwrap();
        let before = registry.active_generation.clone();
        assert_eq!(registry.commit_refresh().unwrap_err().to_string(), "RuntimeRegistryError: no staged runtime generation is ready");
        d.capability("a", manifest("a"), "2");
        d.capability("b", manifest("b"), "1");
        let found = registry.discover(&Value::Null).unwrap();
        let states: Vec<&Value> = found["available_capabilities"].as_array().unwrap().iter().map(|c| &c["state"]).collect();
        assert_eq!(states, [&json!("available_update"), &json!("available")]);
        let staged = registry.handle(&json!("refresh"), &Map::new(), &json!("e")).unwrap();
        assert_eq!((staged["previous_generation"].as_str(), staged["changed"].as_bool()), (Some(before.as_str()), Some(true)));
        registry.cancel_refresh();
        assert!(registry.commit_refresh().is_err());
        registry.prepare_refresh(&Value::Null).unwrap();
        let committed = registry.commit_refresh().unwrap();
        assert_eq!(committed["capability_generation"], staged["capability_generation"]);
        assert_eq!(registry.active.len(), 2);
        assert_eq!(registry.handle(&json!(7), &Map::new(), &Value::Null).unwrap_err().to_string(), "RuntimeRegistryError: unknown runtime action 7");
    }

    #[test]
    fn toolchains_check_nested_fixed_artifacts() {
        // test_workspace_router.py C01
        let d = Dirs::new();
        let cwd = d.base.join("configured-tool");
        fs::create_dir(&cwd).unwrap();
        let exe = cwd.join("runner");
        fs::write(&exe, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let artifact = cwd.join("model.bin");
        let ceiling = json!({"host_command": {"commands": {"model": {"argv": [exe, format!("--fixed-arg=--model={}", artifact.display())], "cwd": cwd}}}});
        let registry = d.open(ceiling).unwrap();
        let status = |r: &Registry| r.toolchains().into_iter().find(|t| t["name"] == "model").unwrap()["status"].clone();
        assert_eq!(status(&registry), "missing");
        fs::write(&artifact, "model").unwrap();
        assert_eq!(status(&registry), "ready");
        let names: Vec<Value> = registry.toolchains().iter().map(|t| t["name"].clone()).collect();
        assert_eq!(names, [json!("julia"), json!("model"), json!("python"), json!("rust")]);
    }

    /// Drives the registry from a request file for the scratch parity check
    /// against runtime_registry.py: `PALETTE_REGISTRY_PARITY=<request.json>
    /// PALETTE_REGISTRY_PARITY_OUT=<out.json> cargo test -- --ignored
    /// registry_parity`. The request names root, ceiling, workspace,
    /// state_dir, repo and host_binary, and lists steps: `["handle", action,
    /// payload, epoch]`, `["commit"]`, `["cancel"]` or `["toolchains"]`.
    #[test]
    #[ignore]
    fn registry_parity() {
        let Ok(request) = std::env::var("PALETTE_REGISTRY_PARITY") else { return };
        let request: Value = serde_json::from_str(&fs::read_to_string(request).unwrap()).unwrap();
        let path = |k: &str| request[k].as_str().map(util::expand_user);
        let shown = |r: Result<Value, Fault>| r.map_or_else(|f| json!({"error": f.to_string()}), |v| json!({"ok": v}));
        let mut out = Map::new();
        match Registry::new(&path("root").unwrap(), &request["ceiling"], &path("workspace").unwrap(), &path("state_dir").unwrap(),
                            path("repo").as_deref(), path("host_binary").as_deref()) {
            Err(f) => {
                out.insert("init".into(), json!({"error": f.to_string()}));
            }
            Ok(mut registry) => {
                out.insert("init".into(), json!({"ok": {"generation": registry.active_generation, "identity": registry.runtime_identity,
                                                        "adapter": registry.adapter_identity}}));
                let steps = request["steps"].as_array().cloned().unwrap_or_default();
                let results: Vec<Value> = steps.iter().map(|step| match step[0].as_str() {
                    Some("handle") => shown(match step[2].as_object() {
                        Some(payload) => registry.handle(&step[1], payload, &step[3]),
                        None => Err(Fault::value("palette.runtime payload must be an object")),
                    }),
                    Some("commit") => shown(registry.commit_refresh()),
                    Some("cancel") => {
                        registry.cancel_refresh();
                        json!({"ok": null})
                    }
                    Some("toolchains") => json!({"ok": registry.toolchains()}),
                    other => panic!("unknown step {other:?}"),
                }).collect();
                out.insert("steps".into(), Value::Array(results));
            }
        }
        fs::write(std::env::var("PALETTE_REGISTRY_PARITY_OUT").unwrap(), Value::Object(out).to_string()).unwrap();
    }
}
