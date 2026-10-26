//! `palette-host mcp`: the workspace router.
//!
//! The router is the MCP server agents talk to. It keeps a registry of
//! logical workspaces (thread, project or open worlds), starts one
//! `palette-host mcp child` adapter per live workspace and forwards
//! `palette` and `palette_control` calls to it, serves `palette_workspace`
//! itself, and brokers `palette_patch` through the patch broker. It serves
//! stdio, or a private Unix socket with `--socket PATH`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::child::{self, env, get, no_get};
use super::generation::GenerationStore;
use super::patch::{self, PatchBroker};
use super::{atomic_json, lineage, py_json_loads, transport, valid_id, Fault};
use crate::{provisioning, sandbox, util};

const FRAME_LIMIT: usize = 1048576;
const CONTROL_ACTIONS: [&str; 9] =
    ["status", "prepare", "restart", "checkpoints", "restore", "receipts", "generation_status", "activate_generation", "reload_config"];
const INSTRUCTIONS: &str = "Select a logical workspace for each independent experiment. Omit project_root_id to keep using the legacy workspace; otherwise select a configured ID returned by palette_workspace list. Tool arguments cannot supply filesystem paths. Project-scoped workspaces on the same project root share one durable Julia lineage; thread/open workspaces remain independent. Context keys are supplied by callers, not authenticated thread identities.";
const ROUTING: [&str; 2] = ["workspace_id", "context_id"];

/// A mutex lock that outlives a panicked holder, as Python's locks do.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn read_lock<T>(m: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

fn write_lock<T>(m: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

fn mkdir_private(path: &Path) -> Result<(), Fault> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path).map_err(|e| Fault::os(&e, path))
}

fn disjoint<W: AsRef<Path>, P: AsRef<Path>, Q: AsRef<Path>>(writable: &[W], protected: &[P], parents: &[Q]) -> Result<(), Fault> {
    provisioning::require_disjoint(writable, protected, parents).map_err(Fault::permission)
}

const NONE: &[PathBuf] = &[];

#[derive(Clone)]
struct ConfigSnapshot {
    revision: u64,
    default_mode: String,
    read_roots: Vec<PathBuf>,
    read_roots_source: &'static str,
    read_roots_spec: String,
    read_roots_file: Option<PathBuf>,
    host_commands: Option<String>,
    host_commands_source: &'static str,
    ceiling: Value,
    command_parents: Vec<PathBuf>,
}

impl ConfigSnapshot {
    fn session_mode(&self, requested: Option<&str>) -> Result<String, Fault> {
        let mode = requested.unwrap_or(&self.default_mode);
        if !matches!(mode, "operator" | "sandbox") {
            return Err(Fault::value("mode must be operator or sandbox"));
        }
        if self.default_mode == "sandbox" && mode == "operator" {
            return Err(Fault::permission("PALETTE_MODE=sandbox requires sandbox sessions; a tool call cannot select operator mode"));
        }
        Ok(mode.into())
    }

    fn load(revision: u64, var: impl Fn(&str) -> Option<String>) -> Result<ConfigSnapshot, Fault> {
        if var("PALETTE_OPERATOR_CEILING_JSON").is_some() {
            return Err(Fault::value("PALETTE_OPERATOR_CEILING_JSON is reserved for the internal workspace snapshot handoff and cannot be set on the daemon"));
        }
        let config_dir = var("XDG_CONFIG_HOME").map(PathBuf::from)
            .unwrap_or_else(|| util::home().join(".config")).join("palette");
        let roots_path = util::expand_user(&config_dir.join("read-roots").to_string_lossy());
        let read_roots_override = var("PALETTE_READ_ROOTS");
        let read_roots_file_override = var("PALETTE_READ_ROOTS_FILE");
        let read_roots_file = match &read_roots_override {
            Some(_) => None,
            None => match read_roots_file_override.as_deref() {
                Some("") => None,
                Some(path) => Some(util::resolve(&util::expand_user(path))),
                None if roots_path.is_file() => Some(roots_path),
                None => None,
            },
        };
        let (read_roots_spec, read_roots_source) = match read_roots_override {
            Some(spec) => (spec, "environment"),
            None if read_roots_file_override.as_deref() == Some("") => (String::new(), "disabled"),
            None => match read_roots_file.as_ref() {
                Some(path) => {
                    let roots = crate::setup::read_roots_list(path).map_err(Fault::value)?;
                    (roots.join(":"), "file")
                }
                None => (String::new(), "none"),
            },
        };
        let read_roots = sandbox::parse_read_roots(Some(&read_roots_spec)).map_err(Fault::value)?;
        let commands_path = util::expand_user(&config_dir.join("host-commands.json").to_string_lossy());
        let (host_commands, host_commands_source) = match var("PALETTE_HOST_COMMANDS") {
            Some(path) if path.is_empty() => (None, "environment"),
            Some(path) => (Some(path), "environment"),
            None if commands_path.is_file() => (Some(commands_path.to_string_lossy().into_owned()), "file"),
            None => (None, "none"),
        };
        let config_var = |key: &str| match key {
            "PALETTE_READ_ROOTS" => Some(read_roots_spec.clone()),
            "PALETTE_HOST_COMMANDS" => host_commands.clone(),
            "PALETTE_OPERATOR_CEILING_JSON" => None,
            _ => var(key),
        };
        let ceiling = provisioning::with_teamlab(provisioning::operator_ceiling(&config_var).map_err(Fault::value)?, &config_var)
            .map_err(Fault::value)?;
        let command_parents = provisioning::command_parents(&ceiling).map_err(Fault::value)?;
        let default_mode = var("PALETTE_MODE").unwrap_or_else(|| "operator".into());
        if !matches!(default_mode.as_str(), "operator" | "sandbox") {
            return Err(Fault::value("PALETTE_MODE must be operator or sandbox"));
        }
        Ok(ConfigSnapshot { revision, default_mode, read_roots, read_roots_source,
                            read_roots_spec, read_roots_file, host_commands, host_commands_source, ceiling, command_parents })
    }

    fn env(&self, key: &str) -> Option<String> {
        match key {
            "PALETTE_READ_ROOTS" => Some(self.read_roots_spec.clone()),
            "PALETTE_HOST_COMMANDS" => self.host_commands.clone(),
            _ => env(key),
        }
    }

    fn sources(&self) -> Value {
        json!({"read_roots": self.read_roots_source, "host_commands": self.host_commands_source})
    }

}

fn version(repo: &Path) -> Result<String, Fault> {
    provisioning::palette_version(repo).map_err(|e| Fault::new("OSError", e))
}

/// `entry[key]`, as Python's KeyError.
fn key<'a>(entry: &'a Map<String, Value>, key: &str) -> Result<&'a Value, Fault> {
    entry.get(key).ok_or_else(|| Fault::new("KeyError", format!("'{key}'")))
}

fn text<'a>(entry: &'a Map<String, Value>, field: &str) -> &'a str {
    entry.get(field).and_then(Value::as_str).unwrap_or("")
}

/// `entry.get("lineage_id", entry["workspace_id"])`.
fn lineage_or_id(entry: &Map<String, Value>) -> String {
    match entry.get("lineage_id") {
        Some(Value::String(s)) => s.clone(),
        _ => text(entry, "workspace_id").to_string(),
    }
}

fn path_value(value: &Value) -> Result<PathBuf, Fault> {
    value.as_str().map(PathBuf::from).ok_or_else(|| Fault::new("TypeError", format!(
        "expected str, bytes or os.PathLike object, not {}", util::py_type_name(value))))
}

type Lines = Receiver<Result<String, Fault>>;
type Frames = Receiver<Result<Value, Fault>>;

/// The lines of `reader`, lossily decoded. With `limit`, a longer line ends
/// the stream with a ValueError.
fn lines(reader: impl Read + Send + 'static, limit: Option<usize>) -> Lines {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let read = match limit {
                Some(n) => reader.by_ref().take(n as u64 + 1).read_until(b'\n', &mut buf),
                None => reader.read_until(b'\n', &mut buf),
            };
            if matches!(read, Ok(0) | Err(_)) {
                return;
            }
            if limit.is_some_and(|n| buf.len() > n) {
                let _ = tx.send(Err(Fault::value("MCP frame exceeded 1 MiB")));
                return;
            }
            if tx.send(Ok(String::from_utf8_lossy(&buf).into_owned())).is_err() {
                return;
            }
        }
    });
    rx
}

/// A workspace child's stdout as JSON frames, then the error that ended it.
fn pump(stdout: impl Read + Send + 'static) -> Frames {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        let end = loop {
            buf.clear();
            match reader.by_ref().take(FRAME_LIMIT as u64 + 1).read_until(b'\n', &mut buf) {
                Ok(0) => break Fault::new("EOFError", "Workspace child exited; last call completion may be unknown"),
                Err(e) => break patch::bare_os(&e),
                Ok(_) => {}
            }
            if buf.len() > FRAME_LIMIT {
                break Fault::value("Workspace child frame exceeded 1 MiB");
            }
            match py_json_loads(&String::from_utf8_lossy(&buf)) {
                Ok(frame) => {
                    if tx.send(Ok(frame)).is_err() {
                        return;
                    }
                }
                Err(e) => break e,
            }
        };
        let _ = tx.send(Err(end));
    });
    rx
}

/// The next frame before `deadline`, or Python's `queue.Empty`.
fn receive(frames: &Frames, deadline: Instant) -> Result<Value, Fault> {
    let wait = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    match frames.recv_timeout(wait) {
        Ok(item) => item,
        Err(RecvTimeoutError::Timeout) => Err(Fault::new("Timeout", "Timed out waiting for a Palette child response.")),
        Err(RecvTimeoutError::Disconnected) => Err(Fault::new("EOFError", "Palette child closed stdout before replying.")),
    }
}

#[derive(Default)]
struct Proc {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// The flock on the state directory's owner lock; the child holds it too.
    lease: Option<File>,
    generation_id: Option<String>,
    config_revision: Option<String>,
    config_snapshot: Option<Arc<ConfigSnapshot>>,
}

/// One workspace's adapter (Python's `Child`). `call` is held for a whole
/// call and holds its frames; `proc` only briefly. Lock order: the router's
/// `children`, then `proc`.
struct Worker {
    entry: Map<String, Value>,
    workspace_state_dir: PathBuf,
    state_dir: PathBuf,
    mode: String,
    key: String,
    call: Mutex<Option<Frames>>,
    proc: Mutex<Proc>,
    last_used: Mutex<Instant>,
}

#[derive(Default)]
struct Children {
    map: HashMap<String, Arc<Worker>>,
    starting: HashSet<String>,
    stopping: HashSet<String>,
}

pub(crate) struct Router {
    repo: PathBuf,
    base: PathBuf,
    legacy: PathBuf,
    legacy_receipts: PathBuf,
    root: PathBuf,
    generations: GenerationStore,
    generation_lock: Mutex<()>,
    max_live: usize,
    idle: Duration,
    limits: (f64, f64),
    project_roots: BTreeMap<String, PathBuf>,
    tools: Value,
    children: Mutex<Children>,
    config: RwLock<Arc<ConfigSnapshot>>,
    reload_lock: Mutex<()>,
}

fn scratch_root(root: &Path) -> PathBuf {
    env("PALETTE_SCRATCH_ROOT").map(PathBuf::from).unwrap_or_else(|| root.join("scratch"))
}

fn sandbox_state_dir(workspace_state_dir: &Path) -> PathBuf {
    workspace_state_dir.parent().unwrap_or(workspace_state_dir)
        .join(".sandbox")
        .join(workspace_state_dir.file_name().unwrap_or_default())
}

fn state_artifacts(state_dir: &Path) -> Vec<PathBuf> {
    vec![state_dir.to_path_buf(), child::receipts_dir(state_dir), lineage::owner_lock_path(state_dir)]
}

fn legacy_state_protected_paths(r: &Router) -> Vec<PathBuf> {
    let mut protected = vec![r.root.clone(), r.legacy.clone(), r.legacy_receipts.clone(), lineage::owner_lock_path(&r.legacy)];
    protected.extend(state_artifacts(&sandbox_state_dir(&r.legacy)));
    protected
}

fn patch_roots_disjoint_from_state(state_dir: &Path, patch_roots: &[PathBuf]) -> Result<(), Fault> {
    disjoint(patch_roots, &state_artifacts(state_dir), NONE)
}

impl Router {
    pub fn new() -> Result<Router, Fault> {
        let (repo, host) = child::repo_and_host().map_err(|e| Fault::new("RuntimeError", e))?;
        let repo = util::resolve(Path::new(&repo));
        let base = util::resolve(Path::new(&env("OPERATOR_WORKSPACE").ok_or_else(|| Fault::new("KeyError", "'OPERATOR_WORKSPACE'"))?));
        let legacy = util::resolve(&env("PALETTE_STATE_DIR").map(PathBuf::from)
            .unwrap_or_else(|| child::default_state_dir(&base.to_string_lossy())));
        let legacy_receipts = child::receipts_dir(&legacy);
        let root = util::resolve(&env("PALETTE_WORKSPACE_STATE_ROOT").map(PathBuf::from).unwrap_or_else(child::default_workspace_root));
        let generations = GenerationStore::new(&root.join("runtime-generations"), &repo, Path::new(&host));
        let max_live = env("PALETTE_MAX_LIVE_WORKSPACES").unwrap_or_else(|| "4".into());
        let max_live: i64 = max_live.trim().replace('_', "").parse().map_err(|_| Fault::value(format!(
            "invalid literal for int() with base 10: {}", util::py_repr(&Value::String(max_live.clone())))))?;
        let idle = env("PALETTE_WORKSPACE_IDLE_SECONDS").unwrap_or_else(|| "900".into());
        let idle: f64 = idle.trim().parse().map_err(|_| Fault::value(format!(
            "could not convert string to float: {}", util::py_repr(&Value::String(idle.clone())))))?;
        let limits = transport::turn_limits(&env).map_err(Fault::value)?;
        if max_live < 1 || idle <= 0.0 {
            return Err(Fault::value("Workspace limits must be positive"));
        }
        let config = Arc::new(ConfigSnapshot::load(1, &env)?);
        disjoint(&[&base], &[&repo, &root, &legacy, &legacy_receipts, &scratch_root(&root)], NONE)?;
        disjoint(&[&root, &legacy, &legacy_receipts], &[&repo], NONE)?;
        disjoint(&[&legacy, &legacy_receipts], &[&root], NONE)?;
        // A missing default workspace failed the first call, not start-up.
        mkdir_private(&base)?;
        let mut router = Router {
            repo, base, legacy, legacy_receipts, root, generations,
            generation_lock: Mutex::new(()),
            max_live: max_live as usize,
            idle: Duration::try_from_secs_f64(idle).unwrap_or(Duration::MAX),
            limits,
            project_roots: BTreeMap::new(),
            tools: Value::Null,
            children: Mutex::new(Children::default()),
            config: RwLock::new(config),
            reload_lock: Mutex::new(()),
        };
        let config = router.config_snapshot();
        router.project_roots = router.configured_project_roots(&config, &env)?;
        router.tools = tools(limits);
        Ok(router)
    }

    fn config_snapshot(&self) -> Arc<ConfigSnapshot> {
        Arc::clone(&read_lock(&self.config))
    }

    fn reload_config(&self) -> Value {
        self.reload_config_with(env)
    }

    fn reload_config_with(&self, var: impl Fn(&str) -> Option<String>) -> Value {
        let _reload = lock(&self.reload_lock);
        let current = self.config_snapshot();
        eprintln!("palette: config reload requested from revision {}", current.revision);
        let loaded = ConfigSnapshot::load(current.revision + 1, &var)
            .and_then(|next| self.configured_project_roots(&next, &var).map(|_| next));
        match loaded {
            Ok(next) => {
                let result_revision = next.revision.to_string();
                let result_mode = next.default_mode.clone();
                let sources = next.sources();
                *write_lock(&self.config) = Arc::new(next);
                eprintln!("palette: config reload published revision {result_revision}");
                json!({"success": true, "reloaded": true, "default_mode": result_mode,
                       "effective_config_revision": current.revision.to_string(),
                       "pending_config_revision": result_revision, "config_sources": sources,
                       "active_sessions": self.active_sessions()})
            }
            Err(error) => {
                eprintln!("palette: config reload rejected at revision {}: {}", current.revision, error.message);
                json!({"success": false, "reloaded": false, "default_mode": current.default_mode,
                       "effective_config_revision": current.revision.to_string(),
                       "pending_config_revision": current.revision.to_string(),
                       "config_sources": current.sources(), "active_sessions": self.active_sessions(),
                       "error": error.message})
            }
        }
    }

    /// What a workspace may not overlap: Palette's own state and install,
    /// the read roots, the host-command paths, the Julia install and depots,
    /// the configured host files and the patch targets.
    fn workspace_protected_paths(&self, config: &ConfigSnapshot) -> Result<Vec<PathBuf>, Fault> {
        let mut protected = vec![self.base.clone(), self.repo.clone(), scratch_root(&self.root)];
        protected.extend(legacy_state_protected_paths(self));
        let split = |key: &str| env(key).unwrap_or_default().split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect::<Vec<_>>();
        protected.extend(config.read_roots.iter().cloned());
        protected.extend(config.read_roots_file.iter().cloned());
        protected.extend(provisioning::protected_host_paths(&config.ceiling, true, true, |key| config.env(key)).map_err(Fault::value)?);
        // The default depot stays protected even when JULIA_DEPOT_PATH points
        // elsewhere: it holds juliaup toolchains and config/startup.jl, which a
        // host started without the override would still run.
        protected.push(util::home().join(".julia"));
        protected.extend(split("JULIA_DEPOT_PATH"));
        let julia = env("PALETTE_JULIA_BIN").filter(|s| !s.is_empty()).map(PathBuf::from).or_else(|| util::which("julia"));
        if let Some(julia) = julia {
            let julia = util::resolve(&util::expand_user(&julia.to_string_lossy()));
            if let Some(prefix) = julia.parent().and_then(Path::parent) {
                protected.push(prefix.to_path_buf());
            }
        }
        for name in ["PALETTE_HOST", "PALETTE_TASK_TOOLS", "PALETTE_TASK_ENV", "PALETTE_PACKAGE_DEPOT",
                     "PALETTE_RUNTIME_ROOT", "PALETTE_SOCKET", "PALETTE_CAPABILITY_CEILING", "PALETTE_REPAIR_CONFIG"] {
            if let Some(value) = env(name).filter(|s| !s.is_empty()) {
                protected.push(util::expand_user(&value));
            }
        }
        for raw in patch::patch_roots_from_env(env)?.values() {
            match raw.as_str() {
                Some(path) if path.starts_with('/') => protected.push(util::expand_user(path)),
                _ => return Err(Fault::value("PALETTE_PATCH_ROOTS must map target IDs to absolute paths")),
            }
        }
        Ok(protected)
    }

    /// Patch decisions follow the latest validated config while retaining
    /// protections captured by any still-running workspace adapter.
    fn patch_protected_paths(&self, current: &Arc<ConfigSnapshot>) -> Result<Vec<PathBuf>, Fault> {
        let workers = lock(&self.children).map.values().cloned().collect::<Vec<_>>();
        let mut snapshots = vec![Arc::clone(current)];
        for worker in &workers {
            if let Some(snapshot) = worker.live_config_snapshot() {
                if !snapshots.iter().any(|known| known.revision == snapshot.revision) {
                    snapshots.push(snapshot);
                }
            }
        }
        let mut protected = legacy_state_protected_paths(self);
        for worker in workers {
            protected.extend(state_artifacts(&worker.state_dir));
        }
        for snapshot in snapshots {
            protected.extend(provisioning::protected_host_paths(
                &snapshot.ceiling,
                true,
                false,
                |key| snapshot.env(key),
            ).map_err(Fault::value)?);
        }
        Ok(protected)
    }

    /// PALETTE_PROJECT_ROOTS: id -> an existing directory a workspace may
    /// write, disjoint from everything protected and from each other.
    fn configured_project_roots(
        &self,
        config: &ConfigSnapshot,
        var: impl Fn(&str) -> Option<String>,
    ) -> Result<BTreeMap<String, PathBuf>, Fault> {
        let text = var("PALETTE_PROJECT_ROOTS").unwrap_or_else(|| "{}".into());
        let Ok(Value::Object(configured)) = serde_json::from_str::<Value>(&text) else {
            return Err(Fault::value("PALETTE_PROJECT_ROOTS must be a JSON object"));
        };
        let mut roots = BTreeMap::from([("default".to_string(), self.base.clone())]);
        let protected = self.workspace_protected_paths(config)?;
        for (id, raw) in &configured {
            if !valid_id(id) || id == "default" {
                return Err(Fault::value("PALETTE_PROJECT_ROOTS keys must be valid non-default project root IDs"));
            }
            let Some(raw) = raw.as_str().filter(|p| p.starts_with('/')) else {
                return Err(Fault::value(format!("Project root {id} must be an absolute path")));
            };
            let path = util::resolve_strict(Path::new(raw)).map_err(|_| Fault::value(format!("Project root {id} is unavailable")))?;
            if !path.is_dir() {
                return Err(Fault::value(format!("Project root {id} is not a directory")));
            }
            let others: Vec<&PathBuf> = protected.iter().chain(roots.values()).collect();
            if let Err(e) = provisioning::require_disjoint(&[&path], &others, &config.command_parents) {
                return Err(Fault::permission(format!(
                    "Project root {id}: {e}. Host-command directories and binaries, their parents, \
                     and Palette's own install and config are protected, because a kernel that could write \
                     them could change what the host runs. Give the kernel read access with PALETTE_READ_ROOTS \
                     instead, and change the tree through host commands.")));
            }
            roots.insert(id.clone(), path);
        }
        Ok(roots)
    }

    // -- Registry -------------------------------------------------------------

    /// Runs `f` on the registry under its lock, saving it if `f` succeeds
    /// and changed it.
    fn with_registry<T>(&self, f: impl FnOnce(&mut Value) -> Result<T, Fault>) -> Result<T, Fault> {
        mkdir_private(&self.root)?;
        let lock_path = self.root.join("registry.lock");
        let lock = OpenOptions::new().read(true).append(true).create(true).open(&lock_path).map_err(|e| Fault::os(&e, &lock_path))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(patch::bare_os(&std::io::Error::last_os_error()));
        }
        let path = self.root.join("registry.json");
        let mut value = if path.exists() {
            py_json_loads(&fs::read_to_string(&path).map_err(|e| Fault::os(&e, &path))?)?
        } else {
            json!({"format": 1, "workspaces": {}, "active": {}})
        };
        let Some(fields) = value.as_object_mut() else { return Err(no_get(&value)) };
        let format_1 = match fields.get("format") {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64() == Some(1.0),
            _ => false,
        };
        if !format_1 || !fields.get("workspaces").is_some_and(Value::is_object) {
            return Err(Fault::value("Unsupported workspace registry"));
        }
        fields.entry("active").or_insert_with(|| json!({}));
        let before = value.clone();
        let out = f(&mut value)?;
        if value != before {
            atomic_json(&path, &value).map_err(Fault::io)?;
        }
        drop(lock);
        Ok(out)
    }

    fn context_key(&self, context: &str) -> String {
        util::sha256_hex(format!("{}\0{context}", self.base.display()).as_bytes())
    }

    fn default_entry(&self) -> Map<String, Value> {
        let base = self.base.to_string_lossy();
        let Value::Object(entry) = json!({"workspace_id": "default", "scope": "project", "project_root_id": "default",
            "workspace_dir": base, "project_dir": base, "state_dir": self.legacy.to_string_lossy(),
            "lineage_id": "legacy:default", "legacy": true}) else { unreachable!() };
        entry
    }

    fn visible(&self, entry: &Map<String, Value>, context: &Value) -> Result<bool, Fault> {
        let root_id = entry.get("project_root_id").map_or(Some("default"), Value::as_str);
        let Some(project_root) = root_id.and_then(|id| self.project_roots.get(id)) else { return Ok(false) };
        let scope = key(entry, "scope")?;
        Ok(scope == "open"
            || scope == "project" && *key(entry, "project_dir")? == *project_root.to_string_lossy()
            || scope == "thread" && patch::truthy(context) && key(entry, "owner_context")? == context)
    }

    /// The registry entry `args` select, checked against the configured
    /// layout and visible to its context.
    fn resolve(&self, args: &Map<String, Value>) -> Result<Map<String, Value>, Fault> {
        let context = args.get("context_id").cloned().unwrap_or(Value::Null);
        self.with_registry(|reg| {
            let explicit = args.get("workspace_id").filter(|v| patch::truthy(v));
            let active = match &context {
                Value::String(c) if !c.is_empty() => reg["active"].get(self.context_key(c)),
                _ => reg["active"].get(""),
            }.filter(|v| patch::truthy(v));
            let identifier = explicit.or(active).cloned().unwrap_or_else(|| "default".into());
            let entry = if identifier == "default" {
                Some(Value::Object(self.default_entry()))
            } else {
                identifier.as_str().and_then(|id| reg["workspaces"].get(id)).filter(|e| !e.is_null()).cloned()
            };
            let Some(entry) = entry else { return Err(Fault::value("Unknown workspace_id")) };
            let identifier = identifier.as_str().unwrap_or_default();
            if !valid_id(identifier) {
                return Err(Fault::value("Invalid workspace_id in registry"));
            }
            let Value::Object(mut entry) = entry else {
                return Err(Fault::new("TypeError", "Workspace registry entry is not an object"));
            };
            entry.entry("project_root_id").or_insert_with(|| "default".into());
            let Some(project_root) = entry["project_root_id"].as_str().and_then(|id| self.project_roots.get(id)) else {
                return Err(Fault::permission("Workspace project root is not configured"));
            };
            if !matches!(entry.get("scope").and_then(Value::as_str), Some("thread" | "project" | "open")) {
                return Err(Fault::permission("Workspace scope is invalid"));
            }
            let layout = || Fault::permission("Workspace registry paths differ from the configured layout");
            if identifier == "default" {
                if util::resolve(Path::new(text(&entry, "state_dir"))) != util::resolve(&self.legacy) {
                    return Err(layout());
                }
            } else {
                entry = lineage::normalize_entry(reg, &self.root, &entry)?;
            }
            let root_text = Value::String(project_root.to_string_lossy().into_owned());
            let expected_project = if entry["scope"] == "project" { root_text.clone() } else { Value::Null };
            if entry.get("workspace_dir") != Some(&root_text) || *get(&Value::Object(entry.clone()), "project_dir") != expected_project {
                return Err(layout());
            }
            if !self.visible(&entry, &context)? {
                return Err(Fault::permission("Workspace is outside this context's scope"));
            }
            Ok(entry)
        })
    }

    // -- Workers --------------------------------------------------------------

    fn child_for(&self, entry: &Map<String, Value>) -> Arc<Worker> {
        self.child_for_mode(entry, self.config_snapshot().default_mode.clone())
    }

    fn child_for_mode(&self, entry: &Map<String, Value>, mode: String) -> Arc<Worker> {
        let workspace_state_dir = PathBuf::from(text(entry, "state_dir"));
        let state_dir = if mode == "sandbox" {
            sandbox_state_dir(&workspace_state_dir)
        } else {
            workspace_state_dir.clone()
        };
        let key = format!("{}\0{mode}", workspace_state_dir.to_string_lossy());
        let mut children = lock(&self.children);
        Arc::clone(children.map.entry(key.clone()).or_insert_with(|| Arc::new(Worker {
            entry: entry.clone(),
            workspace_state_dir,
            state_dir,
            mode,
            key,
            call: Mutex::new(None),
            proc: Mutex::new(Proc::default()),
            last_used: Mutex::new(Instant::now()),
        })))
    }

    fn active_sessions(&self) -> Vec<Value> {
        let workers: Vec<Arc<Worker>> = lock(&self.children).map.values().cloned().collect();
        workers.into_iter().filter_map(|worker| {
            let (running, _) = worker.pid();
            running.then(|| json!({"workspace_id": worker.entry.get("workspace_id"), "mode": worker.mode,
                                  "config_revision": worker.config_revision()}))
        }).collect()
    }

    fn stop_idle(&self, except: Option<&Arc<Worker>>) {
        let idle = |w: &Worker| lock(&w.proc).child.is_some() && lock(&w.last_used).elapsed() >= self.idle;
        let candidates: Vec<Arc<Worker>> = {
            let children = lock(&self.children);
            children.map.values().filter(|w| !except.is_some_and(|e| Arc::ptr_eq(e, w)) && idle(w)).cloned().collect()
        };
        for w in candidates {
            let Ok(_call) = w.call.try_lock() else { continue };
            let still_idle = {
                let _children = lock(&self.children);
                idle(&w)
            };
            if still_idle {
                w.stop_proc(self);
            }
        }
    }

    /// Stops every worker; with `wait`, after its in-flight call.
    fn stop_all(&self, wait: bool) {
        let owned: Vec<Arc<Worker>> = lock(&self.children).map.values().cloned().collect();
        for w in owned {
            let _call = wait.then(|| lock(&w.call));
            w.stop_proc(self);
        }
    }

    // -- Controls -------------------------------------------------------------

    fn workspace_control(&self, args: &Map<String, Value>) -> Result<Value, Fault> {
        let action = key(args, "action")?.as_str().unwrap_or_default();
        let context = args.get("context_id").cloned().unwrap_or(Value::Null);
        let has_context = patch::truthy(&context);
        if action != "create" && args.contains_key("project_root_id") {
            return Err(Fault::value("project_root_id is only valid when creating a workspace"));
        }
        let attach = args.get("attach").map_or(true, patch::truthy);
        if action == "create" {
            let scope = args.get("scope").and_then(Value::as_str).unwrap_or("thread").to_string();
            if scope == "thread" && !has_context {
                return Err(Fault::value("Thread scope requires context_id (a routing key, not authenticated identity)"));
            }
            let project_root_id = args.get("project_root_id").and_then(Value::as_str).unwrap_or("default").to_string();
            let Some(project_root) = self.project_roots.get(&project_root_id) else {
                return Err(Fault::value("Unknown project_root_id"));
            };
            let identifier = match args.get("workspace_id") {
                Some(id) => id.as_str().unwrap_or_default().to_string(),
                None => format!("ws-{}", util::hex(32)),
            };
            if !valid_id(&identifier) || identifier == "default" {
                return Err(Fault::value("Invalid or reserved workspace_id"));
            }
            return self.with_registry(|reg| {
                if reg["workspaces"].get(&identifier).is_some() {
                    return Err(Fault::value("workspace_id already exists"));
                }
                let state = if scope == "project" {
                    lineage::project_state_dir(&self.root, &project_root_id)?
                } else {
                    lineage::workspace_state_dir(&self.root, &identifier)?
                };
                mkdir_private(&state)?;
                let root_text = project_root.to_string_lossy();
                let mut entry = json!({"workspace_id": identifier, "scope": scope, "project_root_id": project_root_id,
                    "owner_context": if scope == "thread" { context.clone() } else { Value::Null },
                    "project_dir": if scope == "project" { json!(root_text) } else { Value::Null },
                    "workspace_dir": root_text, "state_dir": state.to_string_lossy(),
                    "lineage_id": lineage::lineage_id(&scope, &project_root_id, &identifier)?});
                reg["workspaces"][&identifier] = entry.clone();
                if scope == "project" {
                    lineage::ensure_project_lineage(reg, &self.root, &project_root_id)?;
                    entry = reg["workspaces"][&identifier].clone();
                }
                if has_context && attach {
                    reg["active"][self.context_key(context.as_str().unwrap_or_default())] = json!(identifier);
                }
                entry["active_for_context"] = json!(has_context && attach);
                Ok(entry)
            });
        }
        if action == "list" {
            return self.with_registry(|reg| {
                let mut entries = Vec::new();
                for e in reg["workspaces"].as_object().into_iter().flat_map(|w| w.values()) {
                    let Value::Object(e) = e else {
                        return Err(Fault::new("TypeError", "Workspace registry entry is not an object"));
                    };
                    let mut e = e.clone();
                    e.entry("project_root_id").or_insert_with(|| "default".into());
                    entries.push(e);
                }
                let mut workspaces = vec![Value::Object(self.default_entry())];
                for e in &entries {
                    if self.visible(e, &context)? {
                        workspaces.push(Value::Object(e.clone()));
                    }
                }
                let roots: Vec<Value> = self.project_roots.iter()
                    .map(|(id, path)| json!({"project_root_id": id, "workspace_dir": path.to_string_lossy()})).collect();
                let mut listing = json!({"workspaces": workspaces, "available_project_roots": roots,
                                         "identity": "caller-supplied context routing; not authentication"});
                // Counted, not named: another context's threads stay out of this listing.
                let others = entries.iter()
                    .filter(|e| e.get("scope") == Some(&json!("thread")) && *e.get("owner_context").unwrap_or(&Value::Null) != context)
                    .count();
                if others > 0 {
                    listing["other_thread_workspaces"] = json!(others);
                    listing["note"] = json!(format!("{others} thread workspace(s) belong to other context_id values and are not listed; \
                                                     list again with the context_id that created one to see it."));
                }
                Ok(listing)
            });
        }
        let entry = self.resolve(args)?;
        if action == "attach" {
            if !has_context || !args.get("workspace_id").is_some_and(patch::truthy) {
                return Err(Fault::value("Attach requires context_id and workspace_id"));
            }
            self.with_registry(|reg| {
                reg["active"][self.context_key(context.as_str().unwrap_or_default())] = entry["workspace_id"].clone();
                Ok(())
            })?;
            return Ok(json!({"active_workspace_id": entry["workspace_id"]}));
        }
        let state_dir = PathBuf::from(text(&entry, "state_dir"));
        let workers: Vec<Arc<Worker>> = lock(&self.children).map.values()
            .filter(|w| w.workspace_state_dir == state_dir).cloned().collect();
        let current = |w: &Arc<Worker>| lock(&self.children).map.get(&w.key).is_some_and(|c| Arc::ptr_eq(c, w));
        if action == "close" {
            // close is linearized against starts and waits for any in-flight call
            // to finish before stopping the worker. A later call may start it again.
            for w in workers {
                let _call = lock(&w.call);
                let has_proc = lock(&w.proc).child.is_some();
                if current(&w) && has_proc {
                    w.stop_proc(self);
                }
            }
            return Ok(json!({"workspace_id": entry["workspace_id"], "closed": true, "state_retained": true}));
        }
        let mut sessions = Vec::new();
        let mut pids = Vec::new();
        for w in workers {
            let _call = lock(&w.call);
            if current(&w) {
                let (running, pid) = w.pid();
                if running {
                    sessions.push(json!({"workspace_id": entry["workspace_id"], "mode": w.mode,
                                         "config_revision": w.config_revision()}));
                    if let Some(pid) = pid { pids.push(pid); }
                }
            }
        }
        let running = !sessions.is_empty();
        let pid = pids.first().copied();
        let mut status = entry;
        status.insert("running".into(), json!(running));
        status.insert("local_adapter_pid".into(), json!(pid));
        status.insert("active_sessions".into(), Value::Array(sessions));
        status.insert("observation".into(), json!("running describes this router; another router may hold ownership"));
        Ok(Value::Object(status))
    }

    fn generation_control(&self, args: &Map<String, Value>, entry: &Map<String, Value>) -> Result<Value, Fault> {
        let action = key(args, "action")?.as_str().unwrap_or_default();
        if action == "generation_status" {
            let current = self.generations.resolve(None)?;
            return Ok(json!({"generation_id": current.id, "available_generations": self.generations.available()}));
        }
        if action != "activate_generation" {
            return Err(Fault::value("Unknown generation action"));
        }
        let Some(generation_id) = args.get("generation_id").filter(|v| patch::truthy(v)) else {
            return Err(Fault::value("activate_generation requires generation_id"));
        };
        let generation_id = generation_id.as_str().unwrap_or_default();

        let _generation = lock(&self.generation_lock);
        let previous_id = self.generations.current_id()?;
        let target = self.generations.resolve(Some(generation_id))?;
        if target.id == previous_id {
            return Ok(json!({"generation_id": previous_id, "previous_generation_id": previous_id, "changed": false}));
        }

        self.stop_all(true);
        self.generations.activate(generation_id)?;
        let smoke = || -> Result<Value, Fault> {
            let smoke = self.child_for(entry).call(self, "palette", smoke_call())?;
            let value = py_json_loads(content_text(&smoke)?)?;
            if patch::truthy(get(&smoke, "isError")) || *get(&value, "success") == Value::Bool(false) {
                let error = child::text_or_empty(get(&value, "error"));
                return Err(Fault::new("RuntimeError", if error.is_empty() { "new generation smoke call failed".into() } else { error }));
            }
            Ok(json!({"generation_id": generation_id, "previous_generation_id": previous_id, "changed": true,
                      "smoke_epoch": get(&value, "epoch"), "lineage_id": entry.get("lineage_id")}))
        };
        match smoke() {
            Ok(v) => Ok(v),
            Err(e) => {
                self.stop_all(true);
                self.generations.activate(&previous_id)?;
                let mut detail = format!("generation activation failed and rolled back: {e}");
                if let Err(rollback) = self.child_for(entry).call(self, "palette", smoke_call()) {
                    detail += &format!("; rollback restart also failed: {rollback}");
                }
                Err(Fault::new("RuntimeError", detail))
            }
        }
    }
}

fn smoke_call() -> Map<String, Value> {
    let Value::Object(args) = json!({"code": "nothing", "view": "full"}) else { unreachable!() };
    args
}

/// `result["content"][0]["text"]`.
fn content_text(result: &Value) -> Result<&str, Fault> {
    let item = result.get("content").ok_or_else(|| Fault::new("KeyError", "'content'"))?
        .get(0).ok_or_else(|| Fault::new("IndexError", "list index out of range"))?;
    let text = item.get("text").ok_or_else(|| Fault::new("KeyError", "'text'"))?;
    text.as_str().ok_or_else(|| Fault::new("TypeError", format!(
        "the JSON object must be str, bytes or bytearray, not {}", util::py_type_name(text))))
}

impl Worker {
    fn alive(&self) -> bool {
        lock(&self.proc).child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    fn pid(&self) -> (bool, Option<u32>) {
        let mut proc = lock(&self.proc);
        match proc.child.as_mut() {
            Some(c) => match c.try_wait() {
                Ok(None) => (true, Some(c.id())),
                _ => (false, None),
            },
            None => (false, None),
        }
    }

    fn config_revision(&self) -> Option<String> {
        lock(&self.proc).config_revision.clone()
    }

    fn live_config_snapshot(&self) -> Option<Arc<ConfigSnapshot>> {
        lock(&self.proc).config_snapshot.clone()
    }

    fn call(self: &Arc<Self>, r: &Router, name: &str, args: Map<String, Value>) -> Result<Value, Fault> {
        r.stop_idle(Some(self));
        let mut frames = lock(&self.call);
        if lock(&self.proc).child.is_some() && !self.alive() {
            self.stop_proc(r);
        }
        if self.reserve(r)? {
            let started = self.start(r, &mut frames);
            lock(&r.children).starting.remove(&self.key);
            started?;
        }
        let result = self.exchange(r, &mut frames, "tools/call", json!({"name": name, "arguments": args}));
        if result.is_err() {
            self.stop_proc(r);
        }
        *lock(&self.last_used) = Instant::now();
        result
    }

    /// Reserves a live slot when this worker has no adapter. A workspace
    /// that is live and also starting or stopping counts once.
    fn reserve(&self, r: &Router) -> Result<bool, Fault> {
        let mut children = lock(&r.children);
        if lock(&self.proc).child.is_some() {
            return Ok(false);
        }
        let mut occupied: HashSet<&String> = children.map.iter().filter(|(_, w)| w.alive()).map(|(k, _)| k).collect();
        occupied.extend(children.starting.iter());
        occupied.extend(children.stopping.iter());
        if occupied.len() >= r.max_live {
            return Err(Fault::new("RuntimeError", format!("Live workspace limit ({}); close an unused workspace first", r.max_live)));
        }
        children.starting.insert(self.key.clone());
        Ok(true)
    }

    /// Starts this workspace's adapter under the active generation, holding
    /// the state directory's ownership lease.
    fn start(&self, r: &Router, frames: &mut Option<Frames>) -> Result<(), Fault> {
        if self.alive() {
            return Ok(());
        }
        self.stop_proc(r);
        // Publish the starting snapshot while reload is excluded. A patch
        // apply that begins after a new config publishes must also see any
        // worker that captured the prior config and is still starting.
        let config = {
            let _reload = lock(&r.reload_lock);
            let config = r.config_snapshot();
            lock(&self.proc).config_snapshot = Some(Arc::clone(&config));
            config
        };
        let started = self.start_with_snapshot(r, frames, &config);
        if started.is_err() {
            self.stop_proc(r);
        }
        started
    }

    fn start_with_snapshot(&self, r: &Router, frames: &mut Option<Frames>, config: &ConfigSnapshot) -> Result<(), Fault> {
        config.session_mode(Some(&self.mode))?;
        let state = util::resolve(&self.state_dir);
        let workspace = util::resolve(Path::new(text(&self.entry, "workspace_dir")));
        let generation = r.generations.resolve(None)?;
        let generation_repo = util::resolve(&generation.repo);
        let generation_host = util::resolve(&generation.host);
        // The builtin generation's adapter is this binary; another
        // generation's is its own host.
        let launcher = if generation.id == "builtin" { PathBuf::from(child::current_exe()) } else { generation_host.clone() };
        let mut protected = r.workspace_protected_paths(&config)?;
        protected.extend([generation_repo.clone(), generation_host.clone(), util::resolve(&launcher)]);
        if workspace == r.base {
            protected.retain(|p| *p != r.base);
        } else {
            protected.push(r.base.clone());
        }
        disjoint(&[&workspace], &protected, &config.command_parents)?;
        let mut state_protected = vec![r.repo.clone(), generation_repo.clone()];
        state_protected.extend(["patches", "registry.json", "registry.lock", "client-capabilities.json", "scratch", "runtime-generations"]
            .map(|name| r.root.join(name)));
        disjoint(&[&state], &state_protected, NONE)?;
        let patch_roots = patch::patch_roots_from_env(env)?.values().map(path_value).collect::<Result<Vec<_>, _>>()?;
        patch_roots_disjoint_from_state(&state, &patch_roots)?;
        if let Some(parent) = state.parent() {
            mkdir_private(parent)?;
        }
        // Outside the state mount; a worker cannot unlink its ownership lock.
        let lease_path = lineage::owner_lock_path(&state);
        let lease = OpenOptions::new().read(true).append(true).create(true).open(&lease_path).map_err(|e| Fault::os(&e, &lease_path))?;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(Fault::new("RuntimeError", "Workspace already owned by another router; close it there before attaching here"));
            }
            return Err(patch::bare_os(&e));
        }
        let lease_fd = lease.as_raw_fd();
        lock(&self.proc).lease = Some(lease);

        let scratch = scratch_root(&r.root).join(util::sha256_hex(state.to_string_lossy().as_bytes()));
        mkdir_private(&scratch)?;
        let mut command = Command::new(&launcher);
        command.args(["mcp", "child"])
            .current_dir(text(&self.entry, "workspace_dir"))
            .env("OPERATOR_WORKSPACE", text(&self.entry, "workspace_dir"))
            .env("PALETTE_STATE_DIR", &state)
            .env("PALETTE_WORKSPACE_ID", lineage_or_id(&self.entry))
            .env("PALETTE_REPO", &generation_repo)
            .env("PALETTE_HOST", &generation_host)
            .env("PALETTE_READ_ROOTS", &config.read_roots_spec)
            .env("PALETTE_MODE", &self.mode)
            .env("PALETTE_CONFIG_REVISION", config.revision.to_string())
            .env("PALETTE_OPERATOR_CEILING_JSON", config.ceiling.to_string())
            .env("PALETTE_SCRATCH_ROOT", &scratch)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        if let Some(path) = &config.host_commands {
            command.env("PALETTE_HOST_COMMANDS", path);
        } else {
            command.env_remove("PALETTE_HOST_COMMANDS");
        }
        unsafe {
            command.pre_exec(move || {
                // The child inherits the lease, and none of the signals this
                // router blocks to wait for them on one thread.
                let flags = libc::fcntl(lease_fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(lease_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut none: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut none);
                libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
                Ok(())
            });
        }
        let spawned = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.stop_proc(r);
                return Err(Fault::os(&e, &launcher));
            }
        };
        let mut spawned = spawned;
        let stdout = spawned.stdout.take().expect("piped stdout");
        {
            let mut proc = lock(&self.proc);
            proc.stdin = spawned.stdin.take();
            proc.child = Some(spawned);
            proc.generation_id = Some(generation.id.clone());
            proc.config_revision = Some(config.revision.to_string());
        }
        *frames = Some(pump(stdout));
        let initialized = version(&r.repo).and_then(|v| self.exchange(r, frames, "initialize", json!({
            "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "palette-router", "version": v}})));
        if let Err(e) = initialized {
            self.stop_proc(r);
            return Err(e);
        }
        Ok(())
    }

    fn exchange(&self, r: &Router, frames: &mut Option<Frames>, method: &str, params: Value) -> Result<Value, Fault> {
        let rid = util::hex(32);
        let line = json!({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}).to_string() + "\n";
        {
            let mut proc = lock(&self.proc);
            let stdin = proc.stdin.as_mut().ok_or_else(|| Fault::new("AttributeError", "'NoneType' object has no attribute 'stdin'"))?;
            stdin.write_all(line.as_bytes()).and_then(|()| stdin.flush()).map_err(|e| patch::bare_os(&e))?;
        }
        // A start, its preparation and the call, whose own limit the child enforces.
        let deadline = Instant::now() + transport::response_timeout(method, &params, r.limits);
        let Some(frames) = frames.as_ref() else {
            return Err(Fault::new("EOFError", "Palette child has no response stream."));
        };
        loop {
            let frame = receive(frames, deadline)?;
            if frame.get("id") != Some(&json!(rid)) {
                continue;
            }
            if let Some(error) = frame.get("error") {
                return Err(Fault::new("RuntimeError", match error {
                    Value::String(s) => s.clone(),
                    e => util::py_repr(e),
                }));
            }
            return frame.get("result").cloned().ok_or_else(|| Fault::new("KeyError", "'result'"));
        }
    }

    /// Closes the adapter's stdin, waits for it to save and exit (40 s, then
    /// SIGTERM, then SIGKILL), then releases the lease.
    fn stop_proc(&self, r: &Router) {
        let (child, stdin, lease) = {
            let mut children = lock(&r.children);
            let mut proc = lock(&self.proc);
            let child = proc.child.take();
            if child.is_some() {
                children.stopping.insert(self.key.clone());
            }
            (child, proc.stdin.take(), proc.lease.take())
        };
        drop(stdin);
        let stopped = child.is_some();
        if let Some(mut child) = child {
            child::retire(&mut child, Duration::from_secs(40));
        }
        drop(lease);
        lock(&self.proc).config_snapshot = None;
        if stopped {
            lock(&r.children).stopping.remove(&self.key);
        }
    }
}

// -- Tools --------------------------------------------------------------------

fn patch_schema() -> Value {
    json!({"type": "object", "properties": {
        "action": {"enum": ["read", "prepare", "status", "apply"]},
        "target": {"type": "string"}, "path": {"type": "string"}, "request_id": {"type": "string"},
        "changes": {"type": "array", "maxItems": 16, "items": {"type": "object", "properties": {
            "path": {"type": "string"}, "before_sha256": {"type": ["string", "null"]}, "content": {"type": "string"}},
            "required": ["path", "before_sha256", "content"], "additionalProperties": false}}},
        "required": ["action"], "additionalProperties": false})
}

fn tools(limits: (f64, f64)) -> Value {
    let mut palette = child::palette_schema(limits);
    palette["required"] = json!([]);
    palette["properties"]["mode"] = json!({"enum": ["operator", "sandbox"], "description": "Omitted mode uses the active config default. A configured sandbox default cannot be loosened to operator."});
    let mut control = child::control_schema(&CONTROL_ACTIONS);
    control["properties"]["mode"] = palette["properties"]["mode"].clone();
    control["properties"]["generation_id"] = json!({"type": "string", "maxLength": 64});
    let workspace = json!({"type": "object", "properties": {
        "action": {"enum": ["create", "attach", "list", "status", "close"]}, "scope": {"enum": ["thread", "project", "open"]},
        "attach": {"type": "boolean"}, "project_root_id": {"type": "string", "maxLength": 64}},
        "required": ["action"], "additionalProperties": false});
    let tools = [
        ("palette", format!("Persistent Julia world. mode selects operator or sandbox for this workspace; omitted mode uses the active config default. source is transport-safe opaque Julia text; legacy code remains accepted. Explicit workspace_id wins over context_id's active workspace. Without either, uses the shared legacy world. context_id is a caller-chosen routing key, not verified identity. Files may remain shared. Quiet/full projection, payload and scratch retain their existing semantics. {}", transport::PALETTE_GUIDE), palette),
        ("palette_control", format!("Status, offline preparation, config reload or restart of exactly the selected workspace. Status distinguishes the running session snapshot from pending config. Reload keeps live sessions on their captured config; new sessions use the published revision. Missing routing fields select the shared legacy world. restore=false retires that world's snapshot and keeps its checkpoints. {}", transport::CONTROL_GUIDE), control),
        ("palette_workspace", "Create/attach/list/status/close thread, project or explicitly open worlds. On create, project_root_id selects a host-configured project directory; omit it for the legacy workspace. Paths are never accepted from tool arguments. Project-scoped aliases selecting the same root share one durable Julia lineage; thread/open worlds remain independent. Thread scope compares supplied context IDs; it does not authenticate callers. Close retains snapshots. Live worlds are limited; idle ones are stopped on subsequent calls.".into(), workspace),
        ("palette_patch", "Read bounded target files, prepare, inspect or apply exact file replacements in a host-configured target. Application requires explicit user approval through supported client elicitation or the local administrator CLI. No argument grants authority. Before hashes and expiry are rechecked. Multi-file application is not atomic; use a quiescent target.".into(), patch_schema()),
    ];
    Value::Array(tools.into_iter().map(|(name, description, mut schema)| {
        schema["properties"]["workspace_id"] = json!({"type": "string", "maxLength": 64});
        schema["properties"]["context_id"] = json!({"type": "string", "maxLength": 256});
        json!({"name": name, "description": description, "inputSchema": schema,
               "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": false}})
    }).collect())
}

/// The top-level checks the router makes against a tool's schema.
fn validate(args: &Value, schema: &Value) -> Result<(), Fault> {
    let invalid = || Fault::value("Invalid tool arguments");
    let properties = schema["properties"].as_object().ok_or_else(invalid)?;
    let args = args.as_object().ok_or_else(invalid)?;
    let required = schema["required"].as_array().map(Vec::as_slice).unwrap_or_default();
    if args.keys().any(|k| !properties.contains_key(k)) || required.iter().any(|k| !k.as_str().is_some_and(|k| args.contains_key(k))) {
        return Err(invalid());
    }
    for (key, value) in args {
        let spec = &properties[key];
        let bad = || Fault::value(format!("Invalid {key}"));
        if let Some(choices) = spec.get("enum").and_then(Value::as_array) {
            if !choices.contains(value) {
                return Err(bad());
            }
        }
        match spec.get("type").and_then(Value::as_str) {
            Some("string") => {
                let longest = spec.get("maxLength").and_then(Value::as_u64).unwrap_or(FRAME_LIMIT as u64);
                if !value.as_str().is_some_and(|s| !s.is_empty() && s.chars().count() as u64 <= longest) {
                    return Err(bad());
                }
            }
            Some("boolean") if !value.is_boolean() => return Err(bad()),
            Some("number") => {
                let lo = spec.get("exclusiveMinimum").and_then(Value::as_f64).unwrap_or(f64::NEG_INFINITY);
                let hi = spec.get("maximum").and_then(Value::as_f64).unwrap_or(f64::INFINITY);
                if !value.as_f64().is_some_and(|v| lo < v && v <= hi) {
                    return Err(Fault::value(format!("Invalid {key}: must be a number in ({}, {}]", util::py_g(lo), util::py_g(hi))));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The patch broker's channel to the MCP client: the same input and output
/// the router serves.
struct StreamClient<'a> {
    input: &'a Lines,
    out: &'a mut dyn Write,
}

impl patch::Client for StreamClient<'_> {
    fn send(&mut self, message: &Value) -> std::io::Result<()> {
        self.out.write_all(format!("{message}\n").as_bytes())?;
        self.out.flush()
    }
    fn recv(&mut self, timeout: Duration) -> Option<String> {
        match self.input.recv_timeout(timeout) {
            Ok(Ok(line)) => Some(line),
            _ => None,
        }
    }
}

struct Session<'a> {
    r: &'a Router,
    capabilities: Map<String, Value>,
    caller: Option<String>,
}

impl Session<'_> {
    fn patch_broker(&self) -> Result<PatchBroker, Fault> {
        let config = self.r.config_snapshot();
        let protected = self.r.patch_protected_paths(&config)?;
        let repair = child::repair_config()?;
        let runtime_root = util::resolve(&util::expand_user(&env("PALETTE_RUNTIME_ROOT")
            .unwrap_or_else(|| self.r.repo.join("runtime/capabilities").to_string_lossy().into_owned())));
        child::patch_broker(&self.r.root.join("patches"), &repair, &protected, &runtime_root)
    }

    /// One message's result, or None for a notification.
    fn handle(&mut self, raw: &str, mid: &mut Value, input: &Lines, out: &mut dyn Write) -> Result<Option<Value>, Fault> {
        let msg = py_json_loads(raw)?;
        let Some(fields) = msg.as_object() else { return Err(no_get(&msg)) };
        *mid = get(&msg, "id").clone();
        let method = match fields.get("method") {
            None => "",
            Some(Value::String(s)) => s.as_str(),
            Some(v) => return Err(patch::no_attribute(v, "startswith")),
        };
        if method.starts_with("notifications/") {
            return Ok(None);
        }
        let empty = json!({});
        let params = match fields.get("params") {
            None => &empty,
            Some(p) => p,
        };
        let result = match method {
            "initialize" => {
                if !params.is_object() {
                    return Err(no_get(params));
                }
                let client_info = get(params, "clientInfo");
                self.caller = if !patch::truthy(client_info) {
                    None
                } else if client_info.is_object() {
                    provisioning::agent_name(client_info.get("name"))
                } else {
                    return Err(no_get(client_info));
                };
                let capabilities = params.get("capabilities").unwrap_or(&empty);
                let Some(capabilities) = capabilities.as_object() else { return Err(no_get(capabilities)) };
                self.capabilities = capabilities.clone();
                atomic_json(&self.r.root.join("client-capabilities.json"), &json!({
                    "elicitation": capabilities.get("elicitation"), "protocolVersion": params.get("protocolVersion")}))
                    .map_err(Fault::io)?;
                json!({"protocolVersion": params.get("protocolVersion").cloned().unwrap_or_else(|| json!("2024-11-05")),
                       "capabilities": {"tools": {}},
                       "serverInfo": {"name": "palette", "version": version(&self.r.repo)?},
                       "instructions": INSTRUCTIONS})
            }
            "ping" => json!({}),
            "tools/list" => json!({"tools": self.r.tools}),
            "tools/call" => {
                if !params.is_object() {
                    return Err(no_get(params));
                }
                let name = get(params, "name");
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                self.tool_call(name, args, input, out)
                    .unwrap_or_else(|e| child::text_result(&json!({"success": false, "error": e.to_string()}), true))
            }
            _ => return Err(Fault::value("Unsupported method")),
        };
        Ok(Some(result))
    }

    fn tool_call(&mut self, name: &Value, args: Value, input: &Lines, out: &mut dyn Write) -> Result<Value, Fault> {
        let r = self.r;
        let tool = r.tools.as_array().into_iter().flatten().find(|t| t["name"] == *name)
            .ok_or_else(|| Fault::value("Unknown tool"))?;
        validate(&args, &tool["inputSchema"])?;
        let name = name.as_str().unwrap_or_default();
        let Value::Object(mut args) = args else { unreachable!("validated") };
        if name == "palette" {
            let (routing, routed): (Map<String, Value>, Map<String, Value>) = args.into_iter().partition(|(k, _)|
                ROUTING.contains(&k.as_str()) || k == "mode");
            let normalized = transport::normalize_palette_call(&Value::Object(routed)).map_err(|e| Fault::new("SourceTransportError", e))?;
            args = routing;
            args.extend(normalized);
        }
        if name == "palette_workspace" {
            return Ok(child::text_result(&r.workspace_control(&args)?, false));
        }
        if name == "palette_control" && args.get("action").and_then(Value::as_str) == Some("reload_config") {
            let result = r.reload_config();
            let failed = result.get("success") == Some(&json!(false));
            return Ok(child::text_result(&result, failed));
        }
        let config = r.config_snapshot();
        let requested_mode = args.remove("mode");
        let mode = config.session_mode(requested_mode.as_ref().and_then(Value::as_str))?;
        let entry = r.resolve(&args)?;
        if name == "palette_patch" {
            let broker = self.patch_broker()?;
            let mut client = StreamClient { input, out };
            let workspace = lineage_or_id(&entry);
            return Ok(child::text_result(&broker.call_with_apply(
                &args,
                &workspace,
                &self.capabilities,
                &mut client,
                |identifier, workspace| {
                    let _reload = lock(&r.reload_lock);
                    let current = self.patch_broker()?;
                    current.apply(identifier, workspace)
                },
            )?, false));
        }
        if name == "palette_control" && matches!(args["action"].as_str(), Some("generation_status" | "activate_generation")) {
            return Ok(child::text_result(&r.generation_control(&args, &entry)?, false));
        }
        let control_action = args.get("action").and_then(Value::as_str).map(str::to_string);
        let mut forwarded: Map<String, Value> = args.into_iter().filter(|(k, _)| !ROUTING.contains(&k.as_str())).collect();
        if name == "palette" {
            if let Some(caller) = &self.caller {
                forwarded.insert("caller".into(), json!(caller));
            }
        }
        let worker = r.child_for_mode(&entry, mode.clone());
        if name == "palette_control"
            && matches!(control_action.as_deref(), Some("restart" | "restore"))
            && worker.config_revision().is_some_and(|revision| revision != config.revision.to_string())
        {
            // An explicit restart after reload is the point where this
            // durable world's adapter adopts the new validated snapshot.
            worker.stop_proc(r);
        }
        let mut result = worker.call(r, name, forwarded)?;
        let mut value = py_json_loads(content_text(&result)?)?;
        let Some(fields) = value.as_object_mut() else { return Err(patch::no_attribute(&value, "update")) };
        let generation_id = match lock(&worker.proc).generation_id.clone() {
            Some(id) => id,
            None => r.generations.current_id()?,
        };
        fields.insert("workspace_id".into(), entry["workspace_id"].clone());
        fields.insert("workspace_scope".into(), entry["scope"].clone());
        fields.insert("project_root_id".into(), entry["project_root_id"].clone());
        fields.insert("lineage_id".into(), entry.get("lineage_id").cloned().unwrap_or(Value::Null));
        fields.insert("generation_id".into(), json!(generation_id));
        fields.insert("workspace_dir".into(), entry["workspace_dir"].clone());
        fields.insert("legacy_shared".into(), entry.get("legacy").cloned().unwrap_or(json!(false)));
        fields.insert("mode".into(), json!(worker.mode));
        if name == "palette_control" && control_action.as_deref() == Some("status") {
            let active_config = r.config_snapshot();
            let effective_revision = worker.config_revision().unwrap_or_else(|| active_config.revision.to_string());
            let connected = fields.get("connected").cloned().unwrap_or(Value::Bool(false));
            let epoch = fields.get("epoch").cloned().unwrap_or(Value::Null);
            fields.insert("effective_mode".into(), json!(worker.mode));
            fields.insert("default_mode".into(), json!(active_config.default_mode));
            fields.insert("effective_config_revision".into(), json!(effective_revision));
            fields.insert("pending_config_revision".into(), json!(active_config.revision.to_string()));
            fields.insert("config_pending".into(), json!(effective_revision != active_config.revision.to_string()));
            fields.insert("config_sources".into(), active_config.sources());
            fields.insert("config_snapshot".into(), json!({"revision": active_config.revision.to_string(),
                "default_mode": active_config.default_mode, "sources": active_config.sources()}));
            fields.insert("session_snapshot".into(), json!({"mode": worker.mode,
                "config_revision": effective_revision, "connected": connected, "epoch": epoch}));
            fields.insert("active_sessions".into(), Value::Array(r.active_sessions()));
        }
        result["content"][0]["text"] = json!(value.to_string());
        Ok(result)
    }
}

/// Serves MCP frames from `input` to `out` until the input ends.
fn serve_stream(r: &Router, input: &Lines, out: &mut dyn Write) -> Result<(), Fault> {
    let roots: Vec<PathBuf> = patch::patch_roots_from_env(env)?.values().map(path_value).collect::<Result<_, _>>()?;
    let protected = legacy_state_protected_paths(r);
    disjoint(&roots, &protected, NONE)?;
    let mut session = Session { r, capabilities: Map::new(), caller: None };
    loop {
        let raw = match input.recv() {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Ok(()),
        };
        let mut mid = Value::Null;
        let frame = match session.handle(&raw, &mut mid, input, out) {
            Ok(None) => continue,
            Ok(Some(result)) => json!({"jsonrpc": "2.0", "id": mid, "result": result}),
            Err(e) => json!({"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": e.message}}),
        };
        out.write_all(format!("{frame}\n").as_bytes()).and_then(|()| out.flush()).map_err(Fault::io)?;
    }
}

// -- Socket mode --------------------------------------------------------------

fn lstat(path: &Path) -> std::io::Result<fs::Metadata> {
    fs::symlink_metadata(path)
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// `path` made absolute, its directories created 0700 where missing, once
/// every ancestor is a real directory nobody else can swap and a stale
/// socket there is removed.
fn safe_socket_path(path: &str) -> Result<PathBuf, Fault> {
    let path = util::absolute(&util::expand_user(path));
    let parts: Vec<_> = path.components().skip(1).collect();
    let mut current = PathBuf::from("/");
    let mut directories = Vec::new();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        current.push(part);
        let st = match lstat(&current) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(&current).map_err(|e| Fault::os(&e, &current))?;
                lstat(&current).map_err(|e| Fault::os(&e, &current))?
            }
            r => r.map_err(|e| Fault::os(&e, &current))?,
        };
        if !st.file_type().is_dir() {
            return Err(Fault::value("Palette socket path components must be real directories"));
        }
        directories.push(st);
    }
    for (index, st) in directories.iter().enumerate() {
        let mode = st.permissions().mode();
        if mode & 0o022 != 0 {
            if mode & libc::S_ISVTX as u32 == 0 || index + 1 == directories.len() {
                return Err(Fault::permission("Palette socket path has a writable non-sticky ancestor"));
            }
            let child = &directories[index + 1];
            if child.uid() != uid() || child.permissions().mode() & 0o077 != 0 {
                return Err(Fault::permission("A sticky socket-path ancestor must contain a private directory owned by this user"));
            }
        }
    }
    let parent = path.parent().unwrap_or(Path::new("/"));
    let parent_stat = lstat(parent).map_err(|e| Fault::os(&e, parent))?;
    if parent_stat.uid() != uid() || parent_stat.permissions().mode() & 0o077 != 0 {
        return Err(Fault::permission("Palette socket parent must be owned by this user with private permissions"));
    }
    let st = match lstat(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(path),
        r => r.map_err(|e| Fault::os(&e, &path))?,
    };
    use std::os::unix::fs::FileTypeExt;
    if !st.file_type().is_socket() {
        return Err(Fault::value("Palette socket path exists and is not a socket"));
    }
    if st.uid() != uid() {
        return Err(Fault::permission("Palette socket is owned by another user"));
    }
    // A stale socket may be removed; a live one is rejected by the connect probe.
    if UnixStream::connect(&path).is_ok() {
        return Err(Fault::new("RuntimeError", "Palette daemon socket is already accepting connections"));
    }
    fs::remove_file(&path).map_err(|e| Fault::os(&e, &path))?;
    Ok(path)
}

pub(crate) fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len)
    };
    (r == 0).then_some(cred.uid)
}

/// The Python OSError family, which a socket client's disconnect raises.
fn is_os_error(kind: &str) -> bool {
    kind == "OSError" || kind.ends_with("Error") && [
        "FileNotFoundError", "PermissionError", "BlockingIOError", "BrokenPipeError", "ConnectionError",
        "ConnectionResetError", "ConnectionAbortedError", "ConnectionRefusedError", "TimeoutError",
        "NotADirectoryError", "IsADirectoryError", "FileExistsError", "InterruptedError",
        "ProcessLookupError", "ChildProcessError",
    ].contains(&kind)
}

fn serve_connection(r: &Router, stream: UnixStream) {
    if peer_uid(&stream) != Some(uid()) {
        return;
    }
    let Ok(reader) = stream.try_clone() else { return };
    let input = lines(reader, Some(FRAME_LIMIT));
    let mut out = &stream;
    match serve_stream(r, &input, &mut out) {
        Err(e) if !is_os_error(e.kind) => eprintln!("Palette socket client rejected: {e}"),
        _ => {}
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn serve_socket(r: Arc<Router>, path: &str) -> Result<(), Fault> {
    let path = safe_socket_path(path)?;
    let old = unsafe { libc::umask(0o077) };
    let listener = UnixListener::bind(&path);
    unsafe { libc::umask(old) };
    let listener = listener.map_err(|e| Fault::os(&e, &path))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| Fault::os(&e, &path))?;
    let owned_inode = fs::metadata(&path).map_err(|e| Fault::os(&e, &path))?.ino();
    let on_signal = Arc::clone(&r);
    let socket = path.clone();
    child::exit_on_signal(move || {
        on_signal.stop_all(false);
        use std::os::unix::fs::FileTypeExt;
        if lstat(&socket).is_ok_and(|st| st.file_type().is_socket() && st.ino() == owned_inode) {
            let _ = fs::remove_file(&socket);
        }
    });
    eprintln!("Palette daemon listening on {}", path.display());
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let r = Arc::clone(&r);
                std::thread::spawn(move || serve_connection(&r, stream));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// `palette-host mcp [--socket PATH]`. Returns the exit code.
pub(crate) fn main(rest: &[String]) -> Result<i32, String> {
    let socket = match rest {
        [] => None,
        [flag, path] if flag == "--socket" => Some(path.as_str()),
        [flag] if flag.starts_with("--socket=") => Some(&flag["--socket=".len()..]),
        _ => return Err("usage: palette-host mcp [--socket PATH] | palette-host mcp start | palette-host mcp connect | palette-host mcp child".into()),
    }.filter(|p| !p.is_empty());
    let r = match Router::new() {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("palette-host mcp: {e}");
            return Ok(1);
        }
    };
    if let Some(path) = socket {
        if let Err(e) = serve_socket(r, path) {
            eprintln!("palette-host mcp: {e}");
        }
        return Ok(1);
    }
    let on_signal = Arc::clone(&r);
    child::exit_on_signal(move || on_signal.stop_all(false));
    let input = lines(std::io::stdin(), None);
    let served = serve_stream(&r, &input, &mut std::io::stdout().lock());
    r.stop_all(true);
    match served {
        Ok(()) => Ok(0),
        Err(e) => {
            eprintln!("palette-host mcp: {e}");
            Ok(1)
        }
    }
}

/// `palette-host mcp start`, the clients' entry:
/// checks the instance is set up, fills the local instance's defaults for
/// whatever its launcher left unset, then serves stdio.
pub(crate) fn start() -> Result<i32, String> {
    for (key, value) in start_defaults(&|k| std::env::var_os(k))? {
        std::env::set_var(key, value);
    }
    main(&[])
}

/// The variables `start` sets, read through `var`: like setdefault, a
/// variable set, even empty, is the launcher's.
fn start_defaults(var: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<Vec<(&'static str, PathBuf)>, String> {
    let given = |k: &str| var(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let repo = match given("PALETTE_REPO") {
        Some(r) => r,
        None => crate::util::installed_checkout().ok_or("Palette setup incomplete: this binary is not inside a Palette checkout; set PALETTE_REPO")?,
    };
    let repo = crate::util::resolve(&repo);
    let host = given("PALETTE_HOST").unwrap_or_else(|| repo.join("runtime/host/target/release/palette-host"));
    for required in [repo.join("Project.toml"), repo.join("Manifest.toml"), host.clone()] {
        if !required.is_file() {
            return Err(format!("Palette setup incomplete: missing {}; instantiate Julia and build the host before starting", required.display()));
        }
    }
    if !crate::util::executable(&host) {
        return Err(format!("Palette host is not executable: {}", host.display()));
    }
    let home = given("HOME").unwrap_or_default();
    let data = crate::util::resolve(&given("PALETTE_DATA_HOME").unwrap_or_else(|| home.join(".local/share/palette")));
    let defaults = [
        ("PALETTE_REPO", repo),
        ("PALETTE_HOST", host),
        ("OPERATOR_WORKSPACE", data.join("workspace")),
        ("PALETTE_STATE_DIR", data.join("state")),
        ("PALETTE_WORKSPACE_STATE_ROOT", data.join("worlds")),
        // A private instance keeps its packages beside its other data instead
        // of sharing the host-wide store, as the installers already arrange.
        ("PALETTE_PACKAGE_DEPOT", data.join("packages")),
    ];
    Ok(defaults.into_iter().filter(|(key, _)| var(key).is_none()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(name: &str) -> Value {
        tools((60.0, 1800.0)).as_array().unwrap().iter().find(|t| t["name"] == name).unwrap()["inputSchema"].clone()
    }

    #[test]
    fn child_receive_names_timeout_and_disconnect() {
        let (_tx, timeout_frames) = mpsc::channel::<Result<Value, Fault>>();
        let timeout = receive(&timeout_frames, Instant::now()).unwrap_err();
        assert_eq!(timeout.kind, "Timeout");
        assert!(timeout.message.contains("Palette child response"));

        let (tx, closed_frames) = mpsc::channel::<Result<Value, Fault>>();
        drop(tx);
        let closed = receive(&closed_frames, Instant::now() + Duration::from_secs(1)).unwrap_err();
        assert_eq!(closed.kind, "EOFError");
        assert!(closed.message.contains("closed stdout"));
    }

    fn err(args: Value, name: &str) -> String {
        validate(&args, &schema(name)).unwrap_err().message
    }

    #[test]
    fn op007_configured_sandbox_is_a_mode_floor() {
        let (_tmp, r) = router(4);
        let mut config = (*r.config_snapshot()).clone();
        for default in ["operator", "sandbox"] {
            config.default_mode = default.into();
            assert_eq!(config.session_mode(None).unwrap(), default);
            assert_eq!(config.session_mode(Some("sandbox")).unwrap(), "sandbox");
            let result = config.session_mode(Some("operator"));
            if default == "sandbox" {
                let fault = result.unwrap_err();
                assert_eq!(fault.kind, "PermissionError");
                assert!(fault.message.contains("PALETTE_MODE=sandbox"));
            } else {
                assert_eq!(result.unwrap(), "operator");
            }
        }
    }

    #[test]
    fn tools_carry_routing_fields_and_the_generation_actions() {
        let tools = tools((60.0, 1800.0));
        let names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["palette", "palette_control", "palette_workspace", "palette_patch"]);
        for tool in tools.as_array().unwrap() {
            let props = &tool["inputSchema"]["properties"];
            assert_eq!(props["workspace_id"], json!({"type": "string", "maxLength": 64}));
            assert_eq!(props["context_id"], json!({"type": "string", "maxLength": 256}));
            assert_eq!(tool["annotations"]["destructiveHint"], json!(true));
        }
        assert_eq!(schema("palette")["required"], json!([]));
        assert_eq!(schema("palette_control")["properties"]["action"]["enum"].as_array().unwrap().len(), 9);
        assert_eq!(schema("palette")["properties"]["mode"]["enum"], json!(["operator", "sandbox"]));
        assert_eq!(schema("palette_control")["properties"]["mode"]["enum"], json!(["operator", "sandbox"]));
        assert_eq!(schema("palette_control")["properties"]["generation_id"]["maxLength"], json!(64));
        assert!(tools[0]["description"].as_str().unwrap().ends_with(transport::PALETTE_GUIDE));
    }

    #[test]
    fn config_reload_keeps_existing_snapshots_and_rejects_invalid_files_atomically() {
        let (_tmp, r) = router(4);
        let config_home = util::resolve(&util::mkdtemp("palette-config-reload-").unwrap());
        let config_dir = config_home.join("palette");
        fs::create_dir_all(&config_dir).unwrap();
        let first_root = util::resolve(&util::mkdtemp("palette-read-root-a-").unwrap());
        let second_root = util::resolve(&util::mkdtemp("palette-read-root-b-").unwrap());
        let roots_file = config_dir.join("read-roots");
        fs::write(&roots_file, format!("{}\n", first_root.display())).unwrap();
        let commands_file = config_dir.join("host-commands.json");
        fs::write(&commands_file, r#"{"first":{"argv":["/bin/true"],"cwd":"/var/tmp"}}"#).unwrap();
        let vars = |extra: &[(&str, &str)]| {
            let mut values = HashMap::from([("XDG_CONFIG_HOME".to_string(), config_home.to_string_lossy().into_owned())]);
            values.extend(extra.iter().map(|(key, value)| (key.to_string(), value.to_string())));
            move |key: &str| values.get(key).cloned()
        };

        let original = Arc::new(ConfigSnapshot::load(1, vars(&[])).unwrap());
        *write_lock(&r.config) = original.clone();
        fs::write(&roots_file, format!("{}\n", second_root.display())).unwrap();
        fs::write(&commands_file, r#"{"second":{"argv":["/bin/false"],"cwd":"/var/tmp"}}"#).unwrap();
        assert_eq!(original.read_roots, vec![first_root.clone()], "a live session keeps its captured config");
        assert!(original.ceiling["host_command"]["commands"].get("first").is_some());
        assert!(original.ceiling["host_command"]["commands"].get("second").is_none());
        let reloaded = r.reload_config_with(vars(&[]));
        assert_eq!(reloaded["success"], true, "{reloaded}");
        assert_eq!(reloaded["pending_config_revision"], "2");
        assert!(!Arc::ptr_eq(&r.config_snapshot(), &original));
        assert_eq!(r.config_snapshot().read_roots, vec![second_root.clone()]);
        assert!(r.config_snapshot().ceiling["host_command"]["commands"].get("second").is_some());
        assert_eq!(original.read_roots, vec![first_root.clone()]);

        let first_root_text = first_root.to_string_lossy().into_owned();
        let overridden = ConfigSnapshot::load(3, vars(&[("PALETTE_READ_ROOTS", &first_root_text)])).unwrap();
        assert_eq!(overridden.read_roots_source, "environment");
        assert_eq!(overridden.read_roots, vec![first_root.clone()], "explicit env wins over a changed file");
        let internal_ceiling = ConfigSnapshot::load(3, vars(&[("PALETTE_OPERATOR_CEILING_JSON", "{}")])).err().unwrap();
        assert!(internal_ceiling.message.contains("reserved for the internal workspace snapshot handoff"));

        let custom_file = config_dir.join("custom-roots");
        fs::write(&custom_file, format!("{}\n", first_root.display())).unwrap();
        let custom_path = custom_file.to_string_lossy().into_owned();
        let custom = ConfigSnapshot::load(3, vars(&[("PALETTE_READ_ROOTS_FILE", &custom_path)])).unwrap();
        assert_eq!(custom.read_roots_source, "file");
        assert_eq!(custom.read_roots, vec![first_root.clone()]);
        let disabled = ConfigSnapshot::load(3, vars(&[("PALETTE_READ_ROOTS_FILE", "")])).unwrap();
        assert!(disabled.read_roots.is_empty(), "explicit empty file selection disables the canonical file");

        fs::write(&roots_file, "relative/path\n").unwrap();
        let before_rejection = r.config_snapshot();
        let rejected = r.reload_config_with(vars(&[]));
        assert_eq!(rejected["success"], false);
        assert_eq!(rejected["reloaded"], false);
        assert_eq!(rejected["pending_config_revision"], "2");
        assert!(Arc::ptr_eq(&r.config_snapshot(), &before_rejection));
        assert_eq!(r.config_snapshot().revision, 2, "invalid config must not advance revision");
        assert_eq!(r.config_snapshot().read_roots, vec![second_root.clone()]);
        fs::remove_dir_all(config_home).unwrap();
        fs::remove_dir_all(first_root).unwrap();
        fs::remove_dir_all(second_root).unwrap();
    }

    #[test]
    fn config_reload_rechecks_protected_layout_against_project_roots() {
        let (_tmp, mut r) = router(4);
        let config_home = util::resolve(&util::mkdtemp("palette-config-layout-").unwrap());
        let config_dir = config_home.join("palette");
        fs::create_dir_all(&config_dir).unwrap();
        let project_root = util::resolve(&util::mkdtemp("palette-project-layout-").unwrap());
        let roots_file = config_dir.join("read-roots");
        fs::write(&roots_file, "").unwrap();
        let config_home = config_home.to_string_lossy().into_owned();
        let project_roots = json!({"project": project_root.to_string_lossy()}).to_string();
        let vars = |key: &str| match key {
            "XDG_CONFIG_HOME" => Some(config_home.clone()),
            "PALETTE_PROJECT_ROOTS" => Some(project_roots.clone()),
            "PALETTE_MODE" => Some("sandbox".into()),
            _ => None,
        };
        let original = Arc::new(ConfigSnapshot::load(1, &vars).unwrap());
        r.project_roots = r.configured_project_roots(&original, &vars).unwrap();
        *write_lock(&r.config) = original;

        fs::write(&roots_file, format!("{}\n", project_root.display())).unwrap();
        let rejected = r.reload_config_with(&vars);
        assert_eq!(rejected["success"], false, "read roots cannot overlap a sandbox workspace root");
        assert_eq!(r.config_snapshot().revision, 1, "failed layout validation is atomic");
        fs::remove_dir_all(config_home).unwrap();
        fs::remove_dir_all(project_root).unwrap();
    }

    #[test]
    fn patch_protections_refresh_after_reload_and_retain_live_session_paths() {
        let (_tmp, r) = router(4);
        let config_home = util::resolve(&util::mkdtemp("palette-patch-reload-").unwrap());
        let config_dir = config_home.join("palette");
        fs::create_dir_all(&config_dir).unwrap();
        let commands_file = config_dir.join("host-commands.json");
        let old_command = config_home.join("bin/old-command");
        let new_command = config_home.join("bin/new-command");
        let command = |path: &Path| json!({"argv": [path.to_string_lossy().into_owned()], "cwd": "/var/tmp"});
        fs::write(&commands_file, json!({"old": command(&old_command)}).to_string()).unwrap();
        let vars = |key: &str| match key {
            "XDG_CONFIG_HOME" => Some(config_home.to_string_lossy().into_owned()),
            "PALETTE_MODE" => Some("operator".into()),
            _ => None,
        };
        let original = Arc::new(ConfigSnapshot::load(1, &vars).unwrap());
        *write_lock(&r.config) = original.clone();
        let entry = r.workspace_control(json!({"action": "create", "workspace_id": "patch-world", "scope": "open"})
            .as_object().unwrap()).unwrap();
        let worker = r.child_for_mode(entry.as_object().unwrap(), "operator".into());
        make_live(&worker);
        lock(&worker.proc).config_snapshot = Some(Arc::clone(&original));

        // This Session stands for one MCP connection spanning a reload.
        let session = Session { r: &r, capabilities: Map::new(), caller: None };
        let first = session.patch_broker().unwrap();
        assert!(first.protected_paths.contains(&util::resolve(&old_command)));
        assert!(!first.protected_paths.contains(&util::resolve(&new_command)));

        fs::write(&commands_file, json!({"new": command(&new_command)}).to_string()).unwrap();
        assert_eq!(r.reload_config_with(&vars)["success"], true);
        let after = session.patch_broker().unwrap();
        assert!(after.protected_paths.contains(&util::resolve(&new_command)), "new command path was not protected after reload");
        assert!(after.protected_paths.contains(&util::resolve(&old_command)), "live adapter's captured command path lost protection");
        let roots = [("target".into(), json!(config_home.to_string_lossy().into_owned()))].into_iter().collect::<Map<_, _>>();
        let classify = PatchBroker::new(
            &r.root.join("patch-test-storage"),
            &roots,
            patch::Options { protected_paths: after.protected_paths.clone(), ..patch::Options::default() },
        ).unwrap();
        assert_eq!(classify.classify(&json!("target"), &json!("bin/new-command")).unwrap().class, "protected");
        worker.stop_proc(&r);
        fs::remove_dir_all(config_home).unwrap();
    }

    #[test]
    fn validation_follows_the_python_checks() {
        assert!(validate(&json!({"action": "list"}), &schema("palette_workspace")).is_ok());
        assert_eq!(err(json!([]), "palette_workspace"), "Invalid tool arguments");
        assert_eq!(err(json!({}), "palette_workspace"), "Invalid tool arguments");
        assert_eq!(err(json!({"action": "list", "x": 1}), "palette_workspace"), "Invalid tool arguments");
        assert_eq!(err(json!({"action": "nope"}), "palette_workspace"), "Invalid action");
        assert_eq!(err(json!({"action": "list", "attach": 1}), "palette_workspace"), "Invalid attach");
        assert_eq!(err(json!({"action": "list", "workspace_id": ""}), "palette_workspace"), "Invalid workspace_id");
        assert_eq!(err(json!({"action": "list", "workspace_id": "a".repeat(65)}), "palette_workspace"), "Invalid workspace_id");
        assert!(validate(&json!({"action": "list", "context_id": "ü".repeat(256)}), &schema("palette_workspace")).is_ok());
        assert_eq!(err(json!({"action": "receipts", "since": true}), "palette_control"),
                   "Invalid since: must be a number in (-inf, inf]");
        assert!(validate(&json!({"action": "receipts", "since": 1.5}), &schema("palette_control")).is_ok());
        let timeout = &schema("palette")["properties"]["timeout_s"];
        if timeout.get("exclusiveMinimum").is_some() {
            assert_eq!(err(json!({"code": "1", "timeout_s": 0}), "palette"), "Invalid timeout_s: must be a number in (0, 1800]");
        }
    }

    #[test]
    fn socket_paths_must_be_private() {
        let dir = util::mkdtemp("palette-router-sock-").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let sock = dir.join("new/palette.sock");
        assert_eq!(safe_socket_path(sock.to_str().unwrap()).unwrap(), sock);
        assert_eq!(fs::metadata(dir.join("new")).unwrap().permissions().mode() & 0o777, 0o700);

        fs::write(dir.join("new/file"), "").unwrap();
        assert_eq!(safe_socket_path(dir.join("new/file").to_str().unwrap()).unwrap_err().message,
                   "Palette socket path exists and is not a socket");
        std::os::unix::fs::symlink(dir.join("new"), dir.join("link")).unwrap();
        assert_eq!(safe_socket_path(dir.join("link/s").to_str().unwrap()).unwrap_err().message,
                   "Palette socket path components must be real directories");
        fs::create_dir(dir.join("open")).unwrap();
        fs::set_permissions(dir.join("open"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(safe_socket_path(dir.join("open/s").to_str().unwrap()).unwrap_err().message,
                   "Palette socket parent must be owned by this user with private permissions");
        fs::set_permissions(dir.join("open"), fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(safe_socket_path(dir.join("open/s").to_str().unwrap()).unwrap_err().message,
                   "Palette socket path has a writable non-sticky ancestor");

        // A stale socket is removed; a live one is refused.
        let live = dir.join("new/live.sock");
        let listener = UnixListener::bind(&live).unwrap();
        assert_eq!(safe_socket_path(live.to_str().unwrap()).unwrap_err().message,
                   "Palette daemon socket is already accepting connections");
        drop(listener);
        // A test forking concurrently can hold the listener between its fork
        // and exec, which keeps the socket accepting for a moment.
        let mut stale = safe_socket_path(live.to_str().unwrap());
        for _ in 0..50 {
            if stale.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            stale = safe_socket_path(live.to_str().unwrap());
        }
        assert_eq!(stale.unwrap(), live);
        assert!(!live.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stream_lines_stop_at_the_frame_limit() {
        let input = lines(std::io::Cursor::new(b"a\nbcd\n".to_vec()), Some(3));
        assert_eq!(input.recv().unwrap().unwrap(), "a\n");
        assert_eq!(input.recv().unwrap().unwrap_err().message, "MCP frame exceeded 1 MiB");
        assert!(input.recv().is_err());
        let input = lines(std::io::Cursor::new(b"x\ny".to_vec()), None);
        assert_eq!((input.recv().unwrap().unwrap(), input.recv().unwrap().unwrap()), ("x\n".into(), "y".into()));
    }

    #[test]
    fn child_frames_end_with_why() {
        let frames = pump(std::io::Cursor::new(b"{\"id\":1}\n".to_vec()));
        assert_eq!(frames.recv().unwrap().unwrap(), json!({"id": 1}));
        assert_eq!(frames.recv().unwrap().unwrap_err().kind, "EOFError");
        let frames = pump(std::io::Cursor::new(b"nope\n".to_vec()));
        assert_eq!(frames.recv().unwrap().unwrap_err().kind, "JSONDecodeError");
        let frames = pump(std::io::Cursor::new(vec![b' '; FRAME_LIMIT + 2]));
        assert_eq!(frames.recv().unwrap().unwrap_err().message, "Workspace child frame exceeded 1 MiB");
    }

    #[test]
    fn a_private_data_home_gets_its_own_package_store() {
        let tmp = Tmp(util::resolve(&util::mkdtemp("palette-start-").unwrap()));
        let (repo, data) = (tmp.0.join("repo"), tmp.0.join("data"));
        let host = repo.join("runtime/host/target/release/palette-host");
        fs::create_dir_all(host.parent().unwrap()).unwrap();
        fs::write(repo.join("Project.toml"), "").unwrap();
        let given = |extra: &[(&str, &str)]| {
            let mut vars: HashMap<String, String> = HashMap::from([
                ("PALETTE_REPO".into(), repo.to_string_lossy().into()), ("PALETTE_DATA_HOME".into(), data.to_string_lossy().into())]);
            vars.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
            start_defaults(&move |k| vars.get(k).map(Into::into)).map(|d| d.into_iter().collect::<HashMap<_, _>>())
        };
        assert!(given(&[]).unwrap_err().starts_with("Palette setup incomplete: missing "));
        fs::write(repo.join("Manifest.toml"), "").unwrap();
        fs::write(&host, "").unwrap();
        assert!(given(&[]).unwrap_err().starts_with("Palette host is not executable: "));
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let defaults = given(&[]).unwrap();
        assert_eq!(defaults["PALETTE_PACKAGE_DEPOT"], data.join("packages"));
        assert_eq!(defaults["PALETTE_HOST"], host);
        assert!(!defaults.contains_key("PALETTE_REPO"), "the launcher's value stays");
        let shared = given(&[("PALETTE_PACKAGE_DEPOT", "/srv/shared-packages"), ("OPERATOR_WORKSPACE", "")]).unwrap();
        assert!(!shared.contains_key("PALETTE_PACKAGE_DEPOT") && !shared.contains_key("OPERATOR_WORKSPACE"));
    }

    // -- Worker synchronization ------------------------------------------------

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A router over a scratch layout, built without the environment.
    fn router(max_live: usize) -> (Tmp, Router) {
        let tmp = Tmp(util::resolve(&util::mkdtemp("palette-router-").unwrap()));
        let (repo, base, legacy, root) = (tmp.0.join("repo"), tmp.0.join("project"), tmp.0.join("legacy"), tmp.0.join("worlds"));
        fs::create_dir(&base).unwrap();
        let r = Router {
            generations: GenerationStore::new(&root.join("runtime-generations"), &repo, &repo.join("host")),
            legacy_receipts: child::receipts_dir(&legacy),
            project_roots: BTreeMap::from([("default".to_string(), base.clone())]),
            repo, base, legacy, root,
            generation_lock: Mutex::new(()),
            max_live,
            idle: Duration::from_secs(900),
            limits: (60.0, 1800.0),
            tools: Value::Null,
            children: Mutex::new(Children::default()),
            config: RwLock::new(Arc::new(ConfigSnapshot {
                revision: 1,
                default_mode: "operator".into(),
                read_roots: Vec::new(),
                read_roots_source: "none",
                read_roots_spec: String::new(),
                read_roots_file: None,
                host_commands: None,
                host_commands_source: "none",
                ceiling: json!({}),
                command_parents: Vec::new(),
            })),
            reload_lock: Mutex::new(()),
        };
        (tmp, r)
    }

    fn open_worker(r: &Router, id: &str) -> Arc<Worker> {
        let entry = r.workspace_control(json!({"action": "create", "workspace_id": id, "scope": "open"}).as_object().unwrap()).unwrap();
        r.child_for(entry.as_object().unwrap())
    }

    #[test]
    fn sandbox_worker_uses_a_separate_state_dir_and_key() {
        let (_tmp, r) = router(4);
        let entry = r.workspace_control(json!({"action": "create", "workspace_id": "mode-world", "scope": "open"})
            .as_object().unwrap()).unwrap();
        let operator = r.child_for_mode(entry.as_object().unwrap(), "operator".into());
        let sandbox = r.child_for_mode(entry.as_object().unwrap(), "sandbox".into());
        assert_eq!(operator.workspace_state_dir, sandbox.workspace_state_dir);
        assert_ne!(operator.state_dir, sandbox.state_dir);
        assert_ne!(operator.key, sandbox.key);
        assert_eq!(operator.state_dir, operator.workspace_state_dir);
        assert_eq!(sandbox.state_dir, operator.workspace_state_dir.parent().unwrap().join(".sandbox").join("mode-world"));

        let lookalike = r.workspace_control(json!({"action": "create", "workspace_id": "mode-world.sandbox", "scope": "open"})
            .as_object().unwrap()).unwrap();
        let lookalike = r.child_for_mode(lookalike.as_object().unwrap(), "operator".into());
        assert_ne!(sandbox.state_dir, lookalike.state_dir, "sandbox state collided with a valid operator workspace ID");
        assert_ne!(lineage::owner_lock_path(&sandbox.state_dir), lineage::owner_lock_path(&lookalike.state_dir));
        fs::create_dir_all(&operator.state_dir).unwrap();
        fs::create_dir_all(&sandbox.state_dir).unwrap();
        fs::create_dir_all(&lookalike.state_dir).unwrap();
        fs::write(operator.state_dir.join("marker"), "operator").unwrap();
        fs::write(sandbox.state_dir.join("marker"), "sandbox").unwrap();
        fs::write(lookalike.state_dir.join("marker"), "lookalike").unwrap();
        assert_eq!(fs::read_to_string(operator.state_dir.join("marker")).unwrap(), "operator");
        assert_eq!(fs::read_to_string(sandbox.state_dir.join("marker")).unwrap(), "sandbox");
        assert_eq!(fs::read_to_string(lookalike.state_dir.join("marker")).unwrap(), "lookalike");
    }

    #[test]
    fn failed_worker_start_releases_its_provisional_config_snapshot() {
        let (_tmp, r) = router(4);
        let entry = r.workspace_control(json!({"action": "create", "workspace_id": "failed-start", "scope": "open"})
            .as_object().unwrap()).unwrap();
        let mut entry = entry.as_object().unwrap().clone();
        entry.insert("workspace_dir".into(), json!(r.repo.to_string_lossy()));
        let worker = r.child_for_mode(&entry, "operator".into());
        let mut frames = None;
        assert!(worker.start(&r, &mut frames).is_err());
        assert!(worker.live_config_snapshot().is_none(), "failed startup must not retain a stale protection snapshot");
    }

    #[test]
    fn patch_roots_exclude_legacy_and_active_sandbox_state_artifacts() {
        let (_tmp, r) = router(4);
        let legacy_lab = sandbox_state_dir(&r.legacy);
        let legacy_artifacts = state_artifacts(&legacy_lab);
        let legacy_protected = legacy_state_protected_paths(&r);
        for artifact in &legacy_artifacts {
            assert!(legacy_protected.contains(artifact), "missing legacy sandbox artifact: {}", artifact.display());
        }
        assert!(disjoint(&[&legacy_lab], &legacy_protected, NONE).is_err(), "stream patch-root check must reject the legacy lab state");

        let entry = r.default_entry();
        let worker = r.child_for_mode(&entry, "sandbox".into());
        make_live(&worker);
        lock(&worker.proc).config_snapshot = Some(r.config_snapshot());
        let protected = r.patch_protected_paths(&r.config_snapshot()).unwrap();
        for artifact in state_artifacts(&worker.state_dir) {
            assert!(protected.contains(&artifact), "dynamic patch protection omitted {}", artifact.display());
            assert!(patch_roots_disjoint_from_state(&worker.state_dir, &[artifact]).is_err(),
                "worker state must reject overlapping patch roots");
        }
        worker.stop_proc(&r);
    }

    /// Gives `w` a live adapter: `cat` exits when its stdin closes.
    fn make_live(w: &Worker) {
        let mut cat = Command::new("cat").stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
        let mut proc = lock(&w.proc);
        proc.stdin = cat.stdin.take();
        proc.child = Some(cat);
    }

    fn control(r: &Router, action: &str, id: &str) -> Value {
        r.workspace_control(json!({"action": action, "workspace_id": id}).as_object().unwrap()).unwrap()
    }

    #[test]
    fn close_waits_for_an_in_flight_call_and_status_sees_the_stop() {
        let (_tmp, r) = router(4);
        let w = open_worker(&r, "sync-world");
        make_live(&w);
        assert_eq!(control(&r, "status", "sync-world")["running"], json!(true));
        let in_flight = lock(&w.call);
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| tx.send(control(&r, "close", "sync-world")).unwrap());
            assert_eq!(rx.recv_timeout(Duration::from_millis(300)), Err(RecvTimeoutError::Timeout),
                       "close returned while a call held the worker");
            assert!(w.alive());
            drop(in_flight);
            assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap()["state_retained"], json!(true));
        });
        assert!(!w.alive());
        assert_eq!(control(&r, "status", "sync-world")["running"], json!(false));
    }

    #[test]
    fn waiting_on_one_workspace_does_not_hold_the_registry_lock() {
        let (_tmp, r) = router(4);
        let w = open_worker(&r, "busy-world");
        make_live(&w);
        let in_flight = lock(&w.call);
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| tx.send(w.call(&r, "palette", Map::new()).is_err()).unwrap());
            assert_eq!(rx.recv_timeout(Duration::from_millis(300)), Err(RecvTimeoutError::Timeout),
                       "the second call did not wait for the first");
            assert!(r.children.try_lock().is_ok(), "a busy workspace held the router's registry lock");
            drop(in_flight);
            // No frames pump, so the forwarded call fails and stops the adapter.
            assert!(rx.recv_timeout(Duration::from_secs(10)).unwrap());
        });
        assert!(!w.alive());
    }

    #[test]
    fn start_reservations_do_not_double_count_live_workers() {
        let (_tmp, r) = router(2);
        let (a, b, c) = (open_worker(&r, "starting-a"), open_worker(&r, "starting-b"), open_worker(&r, "starting-c"));
        make_live(&a);
        lock(&r.children).starting.insert(a.key.clone());
        lock(&r.children).stopping.insert(a.key.clone());
        assert!(!a.reserve(&r).unwrap(), "a live worker needs no slot");
        assert!(b.reserve(&r).unwrap(), "a live worker that is also starting counted twice");
        assert_eq!(c.reserve(&r).unwrap_err().message, "Live workspace limit (2); close an unused workspace first");
        lock(&r.children).starting.remove(&a.key);
        a.stop_proc(&r);
        assert!(c.reserve(&r).unwrap());
        assert_eq!(lock(&r.children).starting, HashSet::from([b.key.clone(), c.key.clone()]));
    }
}
