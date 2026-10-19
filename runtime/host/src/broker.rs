//! The second trust domain: the capability broker. It runs outside the
//! sandbox; the worker reaches it only through one Unix socket bound into the
//! sandbox. One JSON request per connection, one JSON response:
//!     {"id", "category", "params"}  ->  {"id", "approved", "result" | "reason"}
//! Authorize against the session's fixed ceiling, perform the effect here with
//! host privilege (the worker has no code path to it), and write a receipt
//! for every request, approved or not.

use crate::sandbox::{self, RunWorker};
use crate::util;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the agent host does for a `host_request`: (type, payload) -> reply
/// ({"status": "ok", "result"} or {"status": "error", "error"}). Set by the
/// process that owns the session, never by a request.
pub type HostBridge = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;

#[derive(Debug)]
enum Fail {
    /// A policy decision.
    Denied(String),
    /// A bug or an effect that failed; receipted as such, not as policy.
    Internal(String),
}
use Fail::{Denied, Internal};

fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn str_list(v: Option<&Value>) -> Option<Vec<String>> {
    v?.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
}

/// C_child ⊆ C_caller. Fails closed: a category the parent lacks, or one this
/// broker does not know, is denied -- never ignored.
pub fn ceiling_is_subset(requested: &Value, parent: &Value) -> (bool, String) {
    let (req, par) = (obj(requested), obj(parent));
    for (category, rc) in &req {
        // Key presence, not truthiness: an empty `{}` grant is still a grant.
        let Some(pc) = par.get(category) else {
            return (false, format!("category {} is not present in the parent's ceiling at all", q(category)));
        };
        let (rc, pc) = (obj(rc), obj(pc));
        match category.as_str() {
            "external_fs_write" => {
                let rd: Vec<PathBuf> = str_list(rc.get("allowed_dirs")).unwrap_or_default().iter().map(|d| util::resolve(Path::new(d))).collect();
                let pd: Vec<PathBuf> = str_list(pc.get("allowed_dirs")).unwrap_or_default().iter().map(|d| util::resolve(Path::new(d))).collect();
                for d in &rd {
                    if !pd.iter().any(|p| util::within(d, p)) {
                        return (false, format!("requested external_fs_write dir {} is not within any parent allowed_dir {}", d.display(), qpaths(&pd)));
                    }
                }
            }
            "fs_digest" => {
                let rp: Vec<PathBuf> = str_list(rc.get("allowed_paths")).unwrap_or_default().iter().map(|d| util::resolve(Path::new(d))).collect();
                let pp: Vec<PathBuf> = str_list(pc.get("allowed_paths")).unwrap_or_default().iter().map(|d| util::resolve(Path::new(d))).collect();
                for d in &rp {
                    if !pp.iter().any(|p| util::within(d, p)) {
                        return (false, format!("requested fs_digest allowed_path {} is not within any parent allowed_path {}", d.display(), qpaths(&pp)));
                    }
                }
            }
            "network_access" => {
                let r_allowed = rc.get("allowed").and_then(Value::as_bool).unwrap_or(false);
                if r_allowed && !pc.get("allowed").and_then(Value::as_bool).unwrap_or(false) {
                    return (false, "requested network_access=true but parent's ceiling has network_access=false".into());
                }
                let rh = str_list(rc.get("allowed_hosts"));
                let ph = str_list(pc.get("allowed_hosts"));
                // Omitting allowed_hosts means unrestricted, which is wider
                // than a host-restricted parent.
                if r_allowed && ph.is_some() && rh.is_none() {
                    return (false, "parent's network_access is restricted to allowed_hosts; a child request must name its own allowed_hosts subset explicitly, not omit it (omitting it means unrestricted)".into());
                }
                if let (Some(rh), Some(ph)) = (&rh, &ph) {
                    if !rh.iter().all(|h| ph.contains(h)) {
                        return (false, format!("requested allowed_hosts {} is not a subset of parent's {}", ql(rh), ql(ph)));
                    }
                }
            }
            "package_management" => {
                let Some(rp) = str_list(rc.get("allowed_packages")) else {
                    return (false, "requested package_management must name allowed_packages explicitly (no unrestricted child grant)".into());
                };
                if let Some(pp) = str_list(pc.get("allowed_packages")) {
                    if !rp.iter().all(|p| pp.contains(p)) {
                        return (false, format!("requested allowed_packages {} is not a subset of parent's {}", ql(&rp), ql(&pp)));
                    }
                }
                if pc.get("offline").and_then(Value::as_bool) == Some(true)
                    && rc.get("offline").and_then(Value::as_bool) != Some(true) {
                    return (false, "requested package_management permits online package access but parent's grant is offline-only".into());
                }
            }
            // A grandchild's own ceiling is checked against this child's when it is spawned.
            "spawn_child_worker" => {}
            "host_command" => {
                // A child may keep a subset of the parent's commands, each
                // exactly as the parent defines it: no new argv, cwd or timeout.
                let (rcmds, pcmds) = (obj(rc.get("commands").unwrap_or(&Value::Null)), obj(pc.get("commands").unwrap_or(&Value::Null)));
                for (name, spec) in &rcmds {
                    if pcmds.get(name) != Some(spec) {
                        return (false, format!("requested host_command {} is not one of the parent's commands as the parent defines it", q(name)));
                    }
                }
            }
            "host_request" => {
                let Some(rt) = str_list(rc.get("allowed_types")).filter(|_| rc.get("allowed_types").is_some_and(Value::is_array)) else {
                    return (false, "requested host_request must name allowed_types explicitly (no unrestricted child grant)".into());
                };
                let pt = str_list(pc.get("allowed_types")).unwrap_or_default();
                if !rt.iter().all(|t| pt.contains(t)) {
                    return (false, format!("requested host_request types {} are not a subset of parent's {}", ql(&rt), ql(&pt)));
                }
            }
            _ => return (false, format!("unrecognized category {} cannot be validated as a subset -- denied, not ignored", q(category))),
        }
    }
    (true, String::new())
}

pub struct Broker {
    ceiling: Value,
    receipt_log_path: PathBuf,
    session_id: String,
    // external_fs_write's allowed_dirs, resolved once when the broker starts.
    write_roots: Vec<PathBuf>,
    // Trusted paths fixed when the session starts; never taken from a request.
    package_depot_dir: Option<String>,
    package_environment_dir: Option<String>,
    project_dir: Option<String>,
    repo_dir: Option<String>,
    trusted_files: Vec<PathBuf>,
    child_timeout: Duration,
    host_bridge: Mutex<Option<HostBridge>>,
    log_lock: Mutex<()>,
    depot_lock: Mutex<()>,
    // host_command jobs started with action "start"; dropping the broker
    // (the session ending) kills any still running.
    jobs: Mutex<BTreeMap<String, HostJobEntry>>,
}

struct HostJobEntry {
    name: String,
    job: sandbox::HostJob,
}

/// At most this many host_command jobs run at once per session.
const MAX_HOST_JOBS: usize = 8;
const HOST_COMMAND_MAX_TIMEOUT_S: f64 = 4.0 * 3600.0;
const HOST_COMMAND_OUTPUT: usize = 16 * 1024;

pub struct BrokerConfig {
    pub ceiling: Value,
    pub receipt_log_path: PathBuf,
    pub session_id: String,
    pub package_depot_dir: Option<String>,
    pub package_environment_dir: Option<String>,
    pub project_dir: Option<String>,
    pub repo_dir: Option<String>,
    /// Host files this broker was configured from, such as a ceiling file
    /// given on the command line. Protected like the operator's config files.
    pub trusted_files: Vec<PathBuf>,
    pub child_timeout: Duration,
}

/// The host the HTTP client will connect to, in the form `allowed_hosts`
/// entries are written: a lowercase domain, a dotted IPv4 address, or an IPv6
/// address without brackets. Empty when the URL has no host.
fn allowlist_host(u: &url::Url) -> String {
    match u.host() {
        Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
        Some(url::Host::Ipv4(a)) => a.to_string(),
        Some(url::Host::Ipv6(a)) => a.to_string(),
        None => String::new(),
    }
}

/// Writes `content` to `target`, which the caller has checked lies under
/// `root`, without following a symlink anywhere on the path. The checked
/// path is fully resolved, so a symlink met here was put there after the
/// check, by whoever else can write the allowed directory; the write is
/// refused instead of landing wherever it points.
fn write_beneath(root: &Path, target: &Path, content: &[u8]) -> Result<(), Fail> {
    use std::ffi::CString;
    use std::os::fd::{FromRawFd, OwnedFd, AsRawFd};
    use std::os::unix::ffi::OsStrExt;
    let rel = target.strip_prefix(root).map_err(|_| Internal(format!("{} is not under {}", target.display(), root.display())))?;
    let parts: Vec<&std::ffi::OsStr> = rel.iter().collect();
    let Some((name, dirs)) = parts.split_last() else {
        return Err(Internal(format!("{} is a directory", target.display())));
    };
    let cstr = |p: &std::ffi::OsStr| CString::new(p.as_bytes()).map_err(|e| Internal(e.to_string()));
    let open_at = |dir: libc::c_int, p: &CString, flags: libc::c_int, mode: libc::c_uint| -> std::io::Result<OwnedFd> {
        let fd = unsafe { libc::openat(dir, p.as_ptr(), flags | libc::O_CLOEXEC | libc::O_NOFOLLOW, mode) };
        if fd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
    };
    let refused = |e: std::io::Error, what: &Path| match e.raw_os_error() {
        Some(libc::ELOOP) | Some(libc::ENOTDIR) => Denied(format!("refusing to write through a symlink at {}", what.display())),
        _ => Internal(format!("{}: {e}", what.display())),
    };
    // The root was resolved when the broker started, so the walk starts at
    // `/` and follows no symlink on the way to it either: an allowed
    // directory swapped for a symlink later, by a worker that can write
    // where it lives, is refused instead of redirecting the write.
    let mut fd = open_at(libc::AT_FDCWD, &cstr(std::ffi::OsStr::new("/"))?, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .map_err(|e| Internal(format!("/: {e}")))?;
    let mut here = PathBuf::from("/");
    let above: Vec<&std::ffi::OsStr> = root.components().filter_map(|c| match c {
        std::path::Component::Normal(n) => Some(n),
        _ => None,
    }).collect();
    for part in above.iter().chain(dirs.iter()) {
        here.push(part);
        let c = cstr(part)?;
        let next = match open_at(fd.as_raw_fd(), &c, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                if unsafe { libc::mkdirat(fd.as_raw_fd(), c.as_ptr(), 0o755) } != 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() != ErrorKind::AlreadyExists {
                        return Err(Internal(format!("{}: {e}", here.display())));
                    }
                }
                open_at(fd.as_raw_fd(), &c, libc::O_RDONLY | libc::O_DIRECTORY, 0)
            }
            other => other,
        };
        fd = next.map_err(|e| refused(e, &here))?;
    }
    let file = open_at(fd.as_raw_fd(), &cstr(name)?, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644)
        .map_err(|e| refused(e, target))?;
    fs::File::from(file).write_all(content).map_err(|e| Internal(format!("{}: {e}", target.display())))
}

/// external_fs_write's allowed_dirs, resolved once, when the broker starts.
/// Each request is checked against these paths and walked to without
/// following a symlink, so a directory that a worker can replace (one inside
/// its workspace, say) cannot be turned into a symlink that moves the root.
/// An allowed_dir may not reach a path the host trusts: the receipt log, the
/// Palette checkout, the package store, the operator's config files and the
/// host command paths. A write there would let the agent erase its receipts
/// or widen its own next ceiling.
fn write_roots(c: &BrokerConfig) -> Result<Vec<PathBuf>, String> {
    let cap = c.ceiling.get("external_fs_write").map(obj).unwrap_or_default();
    let roots: Vec<PathBuf> = str_list(cap.get("allowed_dirs")).unwrap_or_default().iter()
        .map(|d| util::resolve(Path::new(d))).collect();
    if roots.is_empty() {
        return Ok(roots);
    }
    let mut protected = vec![c.receipt_log_path.clone()];
    protected.extend(c.repo_dir.iter().map(PathBuf::from));
    protected.extend(c.package_depot_dir.as_deref().and_then(|d| Path::new(d).parent()).map(Path::to_path_buf));
    protected.extend(util::installed_checkout());
    protected.extend(c.trusted_files.iter().cloned());
    for key in ["PALETTE_CAPABILITY_CEILING", "PALETTE_HOST_COMMANDS", "PALETTE_REPAIR_CONFIG", "PALETTE_RUNTIME_ROOT", "PALETTE_REPO_DIR", "PALETTE_PACKAGE_DEPOT"] {
        if let Ok(path) = std::env::var(key) {
            if !path.is_empty() { protected.push(PathBuf::from(path)); }
        }
    }
    let commands = sandbox::protected_command_paths(&c.ceiling);
    for root in &roots {
        if let Some(hit) = sandbox::protected_overlap(root, &protected).or_else(|| sandbox::command_overlap(root, &commands)) {
            return Err(format!("external_fs_write allowed_dir {} overlaps the protected host path {}", root.display(), hit.display()));
        }
    }
    Ok(roots)
}

impl Broker {
    pub fn new(c: BrokerConfig) -> Result<Broker, String> {
        if let Some(parent) = c.receipt_log_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // A broker that cannot keep receipts must not start: every answer it
        // gives is supposed to leave one.
        OpenOptions::new().create(true).append(true).open(&c.receipt_log_path)
            .map_err(|e| format!("cannot open the receipt log {}: {e}", c.receipt_log_path.display()))?;
        let write_roots = write_roots(&c)?;
        Ok(Broker {
            ceiling: c.ceiling,
            receipt_log_path: c.receipt_log_path,
            session_id: c.session_id,
            write_roots,
            package_depot_dir: c.package_depot_dir,
            package_environment_dir: c.package_environment_dir,
            project_dir: c.project_dir,
            repo_dir: c.repo_dir,
            trusted_files: c.trusted_files,
            child_timeout: c.child_timeout,
            host_bridge: Mutex::new(None),
            log_lock: Mutex::new(()),
            depot_lock: Mutex::new(()),
            jobs: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn set_host_bridge(&self, bridge: HostBridge) {
        *self.host_bridge.lock().unwrap() = Some(bridge);
    }

    fn cap(&self, category: &str) -> Option<Map<String, Value>> {
        self.ceiling.get(category).map(obj)
    }

    fn external_fs_write(&self, p: &Map<String, Value>) -> Result<Value, Fail> {
        let cap = self.cap("external_fs_write").unwrap_or_default();
        let dirs = str_list(cap.get("allowed_dirs")).unwrap_or_default();
        if dirs.is_empty() {
            return Err(Denied("external_fs_write is not in this session's capability ceiling".into()));
        }
        let (Some(path), Some(content)) = (p.get("path").and_then(Value::as_str), p.get("content").and_then(Value::as_str)) else {
            return Err(Denied("external_fs_write requires string 'path' and 'content'".into()));
        };
        if path.contains('\0') {
            return Err(Internal("internal error handling request (not a policy denial): embedded null byte".into()));
        }
        let target = util::resolve(Path::new(path));
        let allowed = &self.write_roots;
        let Some(root) = allowed.iter().find(|a| util::within(&target, a)) else {
            return Err(Denied(format!("path {} is outside the ceiling's allowed_dirs {}", target.display(), qpaths(allowed))));
        };
        write_beneath(root, &target, content.as_bytes())?;
        Ok(json!({"path": target.to_string_lossy(), "bytes_written": content.len()}))
    }

    fn network_access(&self, p: &Map<String, Value>) -> Result<Value, Fail> {
        let cap = self.cap("network_access").unwrap_or_default();
        if !cap.get("allowed").and_then(Value::as_bool).unwrap_or(false) {
            return Err(Denied("network_access is not in this session's capability ceiling".into()));
        }
        let Some(url) = p.get("url").and_then(Value::as_str) else {
            return Err(Denied("network_access requires a string 'url'".into()));
        };
        // Parse once, with the same WHATWG parser the HTTP client uses, and
        // request exactly the parsed URL. A hand-written host split disagrees
        // with that parser on inputs like `http://a\@b/` (the client treats
        // the backslash as a path separator and connects to `a`), so the host
        // that was checked must be the host that is connected to.
        let parsed = url::Url::parse(url).map_err(|e| Denied(format!("network_access could not parse URL {}: {e}", q(url))))?;
        // A scheme allowlist: a network grant must not double as a host file read (file://).
        let scheme = parsed.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(Denied(format!("network_access only permits http/https URLs, got scheme {}", q(scheme))));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(Denied("network_access does not permit credentials (user@) in the URL".into()));
        }
        if let Some(hosts) = str_list(cap.get("allowed_hosts")) {
            let host = allowlist_host(&parsed);
            if !hosts.iter().any(|h| h.to_ascii_lowercase() == host) {
                return Err(Denied(format!("host {host} is not in this session's allowed_hosts {}", ql(&hosts))));
            }
        }
        // No redirect-following: a redirect target is a URL the allowlist was
        // never asked about. A 3xx comes back as data.
        let agent = ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(5)).build();
        let resp = match agent.request_url("GET", &parsed).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(Internal(format!("internal error handling request (not a policy denial): {e}"))),
        };
        let status = resp.status();
        let mut body = Vec::new();
        let _ = resp.into_reader().take(2048).read_to_end(&mut body);
        body.truncate(200);
        Ok(json!({"url": url, "status": status, "body_prefix": String::from_utf8_lossy(&body)}))
    }

    fn package_management(&self, p: &Map<String, Value>) -> Result<Value, Fail> {
        let cap = self.cap("package_management").unwrap_or_default();
        let Some(allowed) = str_list(cap.get("allowed_packages")) else {
            return Err(Denied("package_management is not in this session's capability ceiling".into()));
        };
        let (Some(depot), Some(environment)) = (&self.package_depot_dir, &self.package_environment_dir) else {
            return Err(Denied("no durable package store is configured for this broker -- package_management requires one".into()));
        };
        let name = p.get("name").and_then(Value::as_str).unwrap_or("");
        let ident = !name.is_empty()
            && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !ident {
            return Err(Denied("package_management requires a string 'name' that is a valid Julia identifier".into()));
        }
        let version = match p.get("version") {
            None | Some(Value::Null) => None,
            Some(Value::String(v)) if valid_version(v) => Some(v.clone()),
            _ => return Err(Denied("package_management's 'version', if given, must look like '1', '1.2', or '1.2.3'".into())),
        };
        if !allowed.iter().any(|a| a == name) {
            return Err(Denied(format!("package {} is not in this session's allowed_packages {}", q(name), ql(&allowed))));
        }
        let spec = match &version {
            Some(v) => format!("name=\"{name}\", version=\"{v}\""),
            None => format!("name=\"{name}\""),
        };
        let julia = sandbox::resolve_julia().map_err(Internal)?;
        // Pkg executes registered package build scripts. Keep the same warm
        // prepared cache, but run the installer in its own filesystem (and,
        // when offline, network) namespace with only the durable store writable.
        let store = Path::new(depot).parent().ok_or_else(|| Internal("package depot has no store root".into()))?;
        let mut protected_paths = vec![self.receipt_log_path.clone()];
        for path in [&self.project_dir, &self.repo_dir].into_iter().flatten() {
            protected_paths.push(PathBuf::from(path));
        }
        protected_paths.extend(self.trusted_files.iter().cloned());
        for key in ["PALETTE_CAPABILITY_CEILING", "PALETTE_HOST_COMMANDS", "PALETTE_REPAIR_CONFIG", "PALETTE_RUNTIME_ROOT"] {
            if let Ok(path) = std::env::var(key) {
                if !path.is_empty() { protected_paths.push(PathBuf::from(path)); }
            }
        }
        let commands = sandbox::protected_command_paths(&self.ceiling);
        if let Some(hit) = sandbox::command_overlap(&util::resolve(store), &commands) {
            return Err(Internal(format!("package store must be disjoint from protected host path {}", hit.display())));
        }
        protected_paths.extend(commands.paths);
        let argv = sandbox::package_installer_argv(
            &julia, store, Path::new(depot), Path::new(environment),
            Path::new(&sandbox::default_depot()),
            cap.get("offline").and_then(Value::as_bool).unwrap_or(false),
            &format!("using Pkg; Pkg.add(Pkg.PackageSpec({spec}))"), &protected_paths,
        ).map_err(Internal)?;
        let result = {
            let _g = self.depot_lock.lock().unwrap();
            let _store_guard = sandbox::package_store_lock(depot).map_err(Internal)?;
            sandbox::run_captured_clean(&argv, &BTreeMap::new(), Duration::from_secs(300))
        };
        let r = result.map_err(Internal)?;
        if r.returncode != 0 || r.timed_out {
            return Err(Denied(format!("Pkg.add({}) failed: {}", q(name), util::tail(&r.stderr, 800))));
        }
        Ok(json!({"name": name, "version": version, "installed": true, "stdout_tail": util::tail(&r.stdout, 500)}))
    }

    /// C_child ⊆ C_caller, enforced here. A request chooses which capabilities
    /// the child gets, never where its filesystem view points. A child's
    /// network namespace is always unshared: network_access for a child means
    /// asking its own broker, which enforces its already-validated ceiling.
    fn spawn_child_worker(&self, p: &Map<String, Value>) -> Result<Value, Fail> {
        let requested = p.get("ceiling").cloned().unwrap_or_else(|| json!({}));
        if !requested.is_object() {
            return Err(Denied("spawn_child_worker requires a 'ceiling' object".into()));
        }
        let (ok, reason) = ceiling_is_subset(&requested, &self.ceiling);
        if !ok {
            return Err(Denied(format!("requested child ceiling is not a subset of this session's ceiling: {reason}")));
        }
        // The subset check resolves both ceilings now; a child's directory must
        // also lie within this broker's roots as they resolved at start.
        let child_dirs = str_list(requested.get("external_fs_write").and_then(|c| c.get("allowed_dirs"))).unwrap_or_default();
        if let Some(d) = child_dirs.iter().map(|d| util::resolve(Path::new(d))).find(|d| !self.write_roots.iter().any(|r| util::within(d, r))) {
            return Err(Denied(format!("requested external_fs_write dir {} is not within this session's allowed_dirs {}", d.display(), qpaths(&self.write_roots))));
        }
        let (Some(project), Some(repo)) = (&self.project_dir, &self.repo_dir) else {
            return Err(Denied("no session project/repo is configured for this broker -- spawn_child_worker requires one".into()));
        };
        let Some(script) = p.get("script").and_then(Value::as_str) else {
            return Err(Denied("spawn_child_worker requires a string 'script'".into()));
        };
        let ws = util::mkdtemp("palette-child-ws-").map_err(|e| Internal(e.to_string()))?;
        let child_depot = if requested.get("package_management").is_some() {
            Some(sandbox::create_session_depot(None).map_err(Internal)?)
        } else {
            None
        };
        let sock_dir = util::mkdtemp("palette-child-broker-").map_err(|e| Internal(e.to_string()))?;
        let result = (|| {
            let child = Arc::new(Broker::new(BrokerConfig {
                ceiling: requested.clone(),
                receipt_log_path: sock_dir.join("receipts.jsonl"),
                session_id: format!("{}/child", self.session_id),
                package_depot_dir: self.package_depot_dir.clone(),
                package_environment_dir: self.package_environment_dir.clone(),
                project_dir: Some(project.clone()),
                repo_dir: Some(repo.clone()),
                trusted_files: self.trusted_files.clone(),
                child_timeout: Duration::from_secs(90),
            }).map_err(Internal)?);
            let server = serve(&sock_dir.join("broker.sock"), child).map_err(Internal)?;
            let r = sandbox::run_worker(&RunWorker {
                workspace_dir: &ws.to_string_lossy(),
                project_dir: project,
                repo_dir: repo,
                script,
                broker_socket_dir: Some(&sock_dir.to_string_lossy()),
                network_enabled: false,
                depot_clone_dir: child_depot.as_ref().map(|d| d.to_str().unwrap_or_default()),
                timeout: self.child_timeout,
            });
            server.shutdown();
            let r = r.map_err(Internal)?;
            if r.timed_out {
                return Err(Denied(format!("child worker exceeded its {}s time limit and was killed",
                                          util::fmt_g(self.child_timeout.as_secs_f64()))));
            }
            Ok(json!({"returncode": r.returncode, "stdout": util::tail(&r.stdout, 4000), "stderr": util::tail(&r.stderr, 2000)}))
        })();
        let _ = fs::remove_dir_all(&sock_dir);
        let _ = fs::remove_dir_all(&ws);
        if let Some(d) = child_depot {
            sandbox::remove_session_depot(&d);
        }
        result
    }

    /// Ask the agent host to act for the kernel (an RLM child, for instance),
    /// for the request types the ceiling names. The host's reply -- its
    /// success or its own error -- is the result.
    fn host_request(&self, p: &Map<String, Value>) -> Result<Value, Fail> {
        let Some(cap) = self.cap("host_request") else {
            return Err(Denied("host_request not in this session's ceiling".into()));
        };
        let rtype = p.get("type").and_then(Value::as_str).unwrap_or("");
        if rtype.is_empty() {
            return Err(Denied("host_request requires a non-empty string 'type'".into()));
        }
        if !str_list(cap.get("allowed_types")).unwrap_or_default().iter().any(|t| t == rtype) {
            return Err(Denied(format!("host_request type {} is not in this session's allowed_types", q(rtype))));
        }
        let payload = p.get("payload").cloned().unwrap_or_else(|| json!({}));
        if !payload.is_object() {
            return Err(Denied("host_request 'payload' must be an object".into()));
        }
        let bridge = self.host_bridge.lock().unwrap().clone();
        let Some(bridge) = bridge else {
            return Err(Denied("no agent host is attached to this session".into()));
        };
        let reply = bridge(rtype, &payload);
        match reply.get("status").and_then(Value::as_str) {
            Some("ok") | Some("error") => Ok(reply),
            _ => Err(Internal(format!("internal error handling request (not a policy denial): host returned a malformed reply to {}", q(rtype)))),
        }
    }

    /// Digest files with host privilege, as a check the sandbox cannot forge,
    /// mirroring runtime/security/broker.py's _handle_fs_digest: a digest computed
    /// inside the sandbox is the kernel grading its own writes, so this reads
    /// the requested paths OUTSIDE the sandbox, on the host, and the request
    /// is receipted regardless of outcome -- a mutation claim can be
    /// independently recomputed, and the receipt survives the kernel (probe
    /// P15). Out-of-scope paths deny the WHOLE request (policy); per-path
    /// filesystem conditions come back as per-path error_class entries.
    fn fs_digest(&self, p: &Map<String, Value>) -> Result<Value, Fail> {
        let cap = self.cap("fs_digest").ok_or_else(|| Denied("fs_digest not in this session's ceiling".into()))?;
        let paths_v = p.get("paths").and_then(Value::as_array).ok_or_else(|| Denied("fs_digest requires a non-empty 'paths' list".into()))?;
        if paths_v.is_empty() {
            return Err(Denied("fs_digest requires a non-empty 'paths' list".into()));
        }
        if paths_v.len() > 64 {
            return Err(Denied("fs_digest accepts at most 64 paths per request".into()));
        }
        let mut paths: Vec<&str> = Vec::with_capacity(paths_v.len());
        for v in paths_v {
            let Some(s) = v.as_str().filter(|s| !s.is_empty()) else {
                return Err(Denied("fs_digest 'paths' entries must be non-empty strings".into()));
            };
            paths.push(s);
        }
        let allowed: Vec<PathBuf> = str_list(cap.get("allowed_paths")).unwrap_or_default().iter().map(|d| util::resolve(Path::new(d))).collect();
        for p in &paths {
            let rp = util::resolve(Path::new(p));
            if !allowed.iter().any(|a| util::within(&rp, a)) {
                return Err(Denied(format!("path {} is not under this session's fs_digest allowed_paths", q(p))));
            }
        }
        // Keys are the paths as the requester wrote them, not as resolved.
        let mut digests = Map::new();
        for p in &paths {
            let rp = util::resolve(Path::new(p));
            let entry = match fs::metadata(&rp) {
                Err(e) => match e.kind() {
                    // Python's Path.exists() swallows OSError and returns
                    // False, so even a stat permission failure (parent dir
                    // not traversable) reads as not_found there; replicated
                    // here for cross-host parity (verified standalone).
                    ErrorKind::NotFound | ErrorKind::PermissionDenied => json!({"error_class": "not_found"}),
                    _ => json!({"error_class": "os_error", "detail": e.to_string()}),
                },
                Ok(m) if m.is_dir() => json!({"error_class": "is_directory"}),
                Ok(_) => match sha256_file(&rp) {
                    Ok((hex, bytes, mtime)) => json!({"sha256": hex, "bytes": bytes, "mtime": mtime}),
                    Err(e) => match e.kind() {
                        ErrorKind::PermissionDenied => json!({"error_class": "permission_denied"}),
                        _ => json!({"error_class": "os_error", "detail": e.to_string()}),
                    },
                },
            };
            digests.insert((*p).to_string(), entry);
        }
        Ok(json!({"digests": digests, "observer": "host", "algorithm": "sha256"}))
    }

    /// Run a command the operator named in the ceiling, on the host, outside
    /// the sandbox: the way a kernel reaches a GPU, a renderer or a build it
    /// cannot run itself. The ceiling fixes each command's argv (absolute
    /// program), cwd and timeout; a request only picks a command by name,
    /// may shorten (never extend) its timeout with `timeout_s`, and passes
    /// arguments only to a command that allows them. Never a shell.
    /// Actions: "run" (default) waits for the result; "start" returns a job
    /// id at once, for commands longer than a turn; "poll" with that id
    /// returns {"running": true} and the output since the last poll, or the
    /// result; "list" names the jobs still held; "commands" names the
    /// commands the ceiling allows. A command with a "lock" file runs under
    /// an exclusive flock on it, shared with work outside Palette (a GPU
    /// lock); waiting for the lock counts against its timeout.
    /// Ceiling: {"host_command": {"commands": {"<name>": {"argv": [...],
    ///   "cwd": "/abs", "timeout_s": 600, "extra_args": false,
    ///   "lock": "/abs/file", "description": "..."}}}}
    fn host_command(&self, p: &Map<String, Value>) -> Result<Value, Fail> {
        let cap = self.cap("host_command").ok_or_else(|| Denied("host_command not in this session's ceiling".into()))?;
        let action = p.get("action").and_then(Value::as_str).unwrap_or("run");
        if action == "commands" {
            let commands = obj(cap.get("commands").unwrap_or(&Value::Null));
            let listed: Vec<Value> = commands.iter().map(|(name, spec)| {
                let mut v = json!({"name": name, "argv": spec.get("argv"), "cwd": spec.get("cwd"),
                                   "timeout_s": spec.get("timeout_s").cloned().unwrap_or(json!(60.0)),
                                   "extra_args": spec.get("extra_args").and_then(Value::as_bool).unwrap_or(false)});
                for key in ["lock", "description"] {
                    if let Some(x) = spec.get(key) { v[key] = x.clone(); }
                }
                v
            }).collect();
            return Ok(json!({"commands": listed}));
        }
        if action == "list" {
            let jobs = self.jobs.lock().unwrap();
            let mut listed: Vec<(f64, Value)> = Vec::new();
            for (id, e) in jobs.iter() {
                let seconds = e.job.seconds();
                let running = e.job.running().map_err(Internal)?;
                listed.push((seconds, json!({"job": id, "name": e.name, "running": running, "seconds": round_ms(seconds)})));
            }
            listed.sort_by(|a, b| b.0.total_cmp(&a.0));
            return Ok(json!({"jobs": listed.into_iter().map(|(_, v)| v).collect::<Vec<_>>()}));
        }
        if action == "poll" || action == "cancel" {
            let id = p.get("job").and_then(Value::as_str)
                .ok_or_else(|| Denied(format!("host_command {action} requires a string 'job'")))?;
            let mut e = {
                let mut jobs = self.jobs.lock().unwrap();
                let e = jobs.get_mut(id).ok_or_else(|| Denied(format!("no running host_command job {}", q(id))))?;
                if action == "poll" && e.job.running().map_err(Internal)? {
                    let o = e.job.read_new();
                    let mut v = json!({"job": id, "name": e.name, "running": true, "seconds": round_ms(o.seconds),
                                       "stdout": o.stdout, "stderr": o.stderr});
                    add_skipped(&mut v, o.skipped);
                    return Ok(v);
                }
                jobs.remove(id).unwrap()
            };
            // Finishing joins the output readers, so it runs outside the lock
            // that every other job call (and each turn's jobs line) takes.
            let r = if action == "cancel" { e.job.cancel() } else { e.job.wait() }.map_err(Internal)?;
            let mut v = host_job_json(Some(id), &r);
            v["name"] = json!(e.name);
            return Ok(v);
        }
        if action != "run" && action != "start" {
            return Err(Denied(format!("host_command action must be run, start, poll, cancel, list or commands, not {}", q(action))));
        }
        let name = p.get("name").and_then(Value::as_str).filter(|n| !n.is_empty())
            .ok_or_else(|| Denied("host_command requires a non-empty string 'name'".into()))?;
        let commands = obj(cap.get("commands").unwrap_or(&Value::Null));
        let spec = obj(commands.get(name).ok_or_else(|| Denied(format!("host command {} is not in this session's ceiling", q(name))))?);
        let misconfigured = |why: &str| Denied(format!("host command {} is misconfigured in the ceiling: {why}", q(name)));
        let mut argv = str_list(spec.get("argv")).filter(|a| !a.is_empty() && spec["argv"].as_array().is_some_and(|x| x.len() == a.len()))
            .ok_or_else(|| misconfigured("'argv' must be a non-empty list of strings"))?;
        if !Path::new(&argv[0]).is_absolute() || !Path::new(&argv[0]).is_file() {
            return Err(misconfigured("argv[0] must be an absolute path to a program"));
        }
        let cwd = spec.get("cwd").and_then(Value::as_str).map(PathBuf::from)
            .filter(|c| c.is_absolute() && c.is_dir()).ok_or_else(|| misconfigured("'cwd' must be an absolute directory"))?;
        let timeout_s = spec.get("timeout_s").map(|t| t.as_f64().filter(|t| *t > 0.0 && *t <= HOST_COMMAND_MAX_TIMEOUT_S))
            .unwrap_or(Some(60.0)).ok_or_else(|| misconfigured("'timeout_s' must be a number in (0, 14400]"))?;
        let timeout_s = match p.get("timeout_s") {
            None | Some(Value::Null) => timeout_s,
            Some(t) => t.as_f64().filter(|t| *t > 0.0 && *t <= timeout_s).ok_or_else(|| Denied(format!(
                "host_command 'timeout_s' must be a number in (0, {timeout_s}], the ceiling's limit for {}", q(name))))?,
        };
        let args = match p.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => {
                let args: Vec<String> = a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                if args.len() != a.len() {
                    return Err(Denied("host_command 'args' must be a list of strings".into()));
                }
                args
            }
            Some(_) => return Err(Denied("host_command 'args' must be a list of strings".into())),
        };
        if !args.is_empty() && spec.get("extra_args").and_then(Value::as_bool) != Some(true) {
            return Err(Denied(format!("host command {} takes no arguments (its ceiling entry does not set extra_args)", q(name))));
        }
        if args.len() > 64 || args.iter().any(|a| a.contains('\0') || a.len() > 4096) {
            return Err(Denied("host_command accepts at most 64 arguments of at most 4096 bytes, without NUL".into()));
        }
        argv.extend(args);
        let lock = match spec.get("lock") {
            None | Some(Value::Null) => None,
            Some(l) => Some(l.as_str().map(PathBuf::from)
                .filter(|l| l.is_absolute() && l.parent().is_some_and(Path::is_dir) && !l.is_dir())
                .ok_or_else(|| misconfigured("'lock' must be an absolute file path in an existing directory"))?),
        };
        if let Some(lock) = &lock {
            let flock = ["/usr/bin/flock", "/bin/flock"].into_iter().find(|f| Path::new(f).is_file())
                .ok_or_else(|| misconfigured("'lock' needs flock(1) from util-linux, which is not installed"))?;
            argv.splice(0..0, [flock.to_string(), "--exclusive".into(), lock.to_string_lossy().into_owned()]);
        }
        let timeout = Duration::from_secs_f64(timeout_s);
        if action == "start" {
            let mut jobs = self.jobs.lock().unwrap();
            if jobs.len() >= MAX_HOST_JOBS {
                return Err(Denied(format!("at most {MAX_HOST_JOBS} host_command jobs may run at once; poll one to completion or cancel one first")));
            }
            let job = sandbox::HostJob::start(&argv, &cwd, timeout, HOST_COMMAND_OUTPUT).map_err(Internal)?;
            let id = util::hex(16);
            jobs.insert(id.clone(), HostJobEntry { name: name.to_string(), job });
            let mut v = json!({"job": id, "name": name, "running": true});
            if let Some(lock) = &lock { v["lock"] = json!(lock); }
            return Ok(v);
        }
        let mut job = sandbox::HostJob::start(&argv, &cwd, timeout, HOST_COMMAND_OUTPUT).map_err(Internal)?;
        let r = job.wait().map_err(Internal)?;
        let mut v = host_job_json(None, &r);
        v["name"] = json!(name);
        if let Some(lock) = &lock { v["lock"] = json!(lock); }
        Ok(v)
    }

    pub fn handle_request(&self, req: &Value) -> Value {
        let req_id = req.get("id").cloned().unwrap_or_else(|| json!(util::uuid4()));
        let category = req.get("category").cloned().unwrap_or(Value::Null);
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
        let p = obj(&params);
        let outcome = match category.as_str() {
            Some("external_fs_write") => self.external_fs_write(&p),
            Some("network_access") => self.network_access(&p),
            Some("package_management") => self.package_management(&p),
            Some("spawn_child_worker") => self.spawn_child_worker(&p),
            Some("host_request") => self.host_request(&p),
            Some("fs_digest") => self.fs_digest(&p),
            Some("host_command") => self.host_command(&p),
            _ => Err(Denied(format!("unknown capability category: {}", py_repr(&category)))),
        };
        let (approved, result, reason) = match outcome {
            Ok(r) => (true, r, Value::Null),
            Err(Denied(r)) => (false, Value::Null, json!(r)),
            Err(Internal(r)) => {
                let r = if r.starts_with("internal error handling request") { r } else { format!("internal error handling request (not a policy denial): {r}") };
                (false, Value::Null, json!(r))
            }
        };
        if let Err(e) = self.write_receipt(&req_id, &category, &params, approved, &result, &reason) {
            // Fail closed, as the Python broker does: no result leaves the
            // broker without its receipt. The effect may already have happened,
            // so the operator hears about it as well as the worker.
            eprintln!("palette-host: receipt for request {req_id} not recorded: {e}");
            return json!({"id": req_id, "approved": false, "reason": format!("broker error: receipt not recorded: {e}")});
        }
        let mut resp = json!({"id": req_id, "approved": approved});
        if approved {
            resp["result"] = result;
        } else {
            resp["reason"] = reason;
        }
        resp
    }

    fn write_receipt(&self, req_id: &Value, category: &Value, params: &Value, approved: bool, result: &Value, reason: &Value)
        -> std::io::Result<()> {
        let receipt = json!({
            "receipt_id": util::uuid4(), "request_id": req_id, "session_id": self.session_id,
            "timestamp": util::now_secs(), "category": category, "params": params,
            "approved": approved, "result": result, "reason": reason,
        });
        let _g = self.log_lock.lock().unwrap();
        let mut f = OpenOptions::new().create(true).append(true).open(&self.receipt_log_path)?;
        writeln!(f, "{receipt}")
    }
}

/// Stream a file through SHA-256 in 1 MiB chunks, as the Python broker does,
/// and report (hex, bytes, mtime in seconds since the epoch).
fn round_ms(seconds: f64) -> f64 {
    (seconds * 1000.0).round() / 1000.0
}

/// Notes output the 16 KiB tail dropped before anyone read it.
fn add_skipped(v: &mut Value, skipped: [u64; 2]) {
    for (key, n) in [("stdout_skipped_bytes", skipped[0]), ("stderr_skipped_bytes", skipped[1])] {
        if n > 0 {
            v[key] = json!(n);
        }
    }
}

fn host_job_json(job: Option<&str>, r: &sandbox::HostJobResult) -> Value {
    let mut v = json!({"returncode": r.returncode, "timed_out": r.timed_out, "cancelled": r.cancelled,
                       "seconds": round_ms(r.seconds), "stdout": r.stdout, "stderr": r.stderr, "running": false});
    add_skipped(&mut v, r.skipped);
    if let Some(id) = job {
        v["job"] = json!(id);
    }
    v
}

fn sha256_file(path: &Path) -> std::io::Result<(String, u64, f64)> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let mtime = file
        .metadata()?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    Ok((hex, size, mtime))
}

fn valid_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    (1..=3).contains(&parts.len()) && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Python's repr of a string, as the messages have always read.
fn q(s: &str) -> String {
    format!("'{s}'")
}

fn ql<S: AsRef<str>>(xs: &[S]) -> String {
    format!("[{}]", xs.iter().map(|x| q(x.as_ref())).collect::<Vec<_>>().join(", "))
}

fn qpaths(ps: &[PathBuf]) -> String {
    format!("[{}]", ps.iter().map(|p| format!("PosixPath('{}')", p.display())).collect::<Vec<_>>().join(", "))
}

fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::String(s) => format!("'{s}'"),
        other => other.to_string(),
    }
}

/// A worker holding unbounded power inside its sandbox is still an untrusted
/// client of this host process: an unterminated stream must not buffer forever.
const MAX_REQUEST_BYTES: usize = 1 << 20;
/// How many connections the broker serves at once. Each one holds a host
/// thread, so an idle or flooding worker must not be able to hold them all.
/// Nested child workers each hold one while they run, well below this.
const MAX_CONNECTIONS: usize = 64;
/// How long a connection may take to send its request line, and the
/// broker to send its reply. Handling the request itself is not limited.
const CONN_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Holds one of the MAX_CONNECTIONS slots until dropped.
struct ConnSlot(Arc<AtomicUsize>);
impl ConnSlot {
    fn take(active: &Arc<AtomicUsize>) -> Option<ConnSlot> {
        active.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < MAX_CONNECTIONS).then_some(n + 1)).ok()?;
        Some(ConnSlot(active.clone()))
    }
}
impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct BrokerServer {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BrokerServer {
    pub fn shutdown(mut self) {
        self.stop_now();
    }

    fn stop_now(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path); // wake the accept loop
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for BrokerServer {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop_now();
        }
    }
}

pub fn serve(sock_path: &Path, broker: Arc<Broker>) -> Result<BrokerServer, String> {
    let _ = fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path).map_err(|e| format!("binding {}: {e}", sock_path.display()))?;
    // The sandboxed worker connects under a different uid view.
    fs::set_permissions(sock_path, fs::Permissions::from_mode(0o666)).map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let active = Arc::new(AtomicUsize::new(0));
    let thread = std::thread::spawn(move || {
        for conn in listener.incoming() {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let Ok(mut conn) = conn else { continue };
            let _ = conn.set_read_timeout(Some(CONN_IO_TIMEOUT));
            let _ = conn.set_write_timeout(Some(CONN_IO_TIMEOUT));
            let Some(slot) = ConnSlot::take(&active) else {
                let _ = writeln!(conn, "{}", json!({"approved": false, "reason": "broker busy: too many open connections"}));
                continue;
            };
            let b = broker.clone();
            std::thread::spawn(move || {
                let _slot = slot;
                handle_conn(conn, &b)
            });
        }
    });
    Ok(BrokerServer { path: sock_path.to_path_buf(), stop, thread: Some(thread) })
}

fn handle_conn(conn: UnixStream, broker: &Broker) {
    let mut writer = match conn.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(conn.take(MAX_REQUEST_BYTES as u64));
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).unwrap_or(0) == 0 {
        return;
    }
    let reply = if line.len() >= MAX_REQUEST_BYTES && !line.ends_with(b"\n") {
        json!({"approved": false, "reason": "request exceeds maximum size"})
    } else {
        match serde_json::from_slice::<Value>(&line) {
            Ok(req) => broker.handle_request(&req),
            Err(e) => json!({"approved": false, "reason": format!("broker error: {e}")}),
        }
    };
    let _ = writeln!(writer, "{reply}");
    let _ = writer.flush();
}

// The host's first Rust test module: the fs_digest parity suite, mirroring
// runtime/security/test_fs_digest.py check-for-check, plus the 64-path
// cap. Direct Broker construction -- no sockets.
#[cfg(test)]
mod tests {
    use super::*;

    struct TmpDir(PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tmpdir(tag: &str) -> TmpDir {
        let d = std::env::temp_dir().join(format!("fs-digest-host-test-{tag}-{}", util::hex(8)));
        fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }

    fn broker(ceiling: Value, receipts: &Path) -> Broker {
        Broker::new(BrokerConfig {
            ceiling,
            receipt_log_path: receipts.to_path_buf(),
            session_id: "test".into(),
            package_depot_dir: None,
            package_environment_dir: None,
            project_dir: None,
            repo_dir: None,
            trusted_files: vec![],
            child_timeout: Duration::from_secs(90),
        })
        .unwrap()
    }

    fn digest_req(paths: &[&str]) -> Value {
        json!({"category": "fs_digest", "params": {"paths": paths}})
    }

    fn expected_sha256(path: &Path) -> String {
        let mut h = Sha256::new();
        h.update(fs::read(path).unwrap());
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn net_req(url: &str) -> Value {
        json!({"category": "network_access", "params": {"url": url}})
    }

    /// A loopback listener that is never served: any connection to it means the
    /// broker connected somewhere the allowlist did not approve.
    fn idle_listener() -> (std::net::TcpListener, u16) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let port = l.local_addr().unwrap().port();
        (l, port)
    }

    fn assert_no_connection(l: &std::net::TcpListener) {
        match l.accept() {
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            other => panic!("broker connected to a host outside allowed_hosts: {other:?}"),
        }
    }

    #[test]
    fn network_backslash_userinfo_cannot_smuggle_a_host() {
        // Regression: the old hand split read this as host allowed.example,
        // while the HTTP client connected to 127.0.0.1.
        let t = tmpdir("net-bs");
        let (l, port) = idle_listener();
        let b = broker(json!({"network_access": {"allowed": true, "allowed_hosts": ["allowed.example"]}}), &t.0.join("receipts.jsonl"));
        for url in [
            format!("http://127.0.0.1:{port}\\@allowed.example/"),
            format!("http://127.0.0.1:{port}\\allowed.example/"),
            format!("http://allowed.example@127.0.0.1:{port}/"),
            format!("http://allowed.example:x@127.0.0.1:{port}/"),
            format!("http://127.0.0.1:{port}#@allowed.example/"),
            format!("http://127.0.0.1:{port}?@allowed.example/"),
            format!("HTTP://127.0.0.1:{port}\\@allowed.example/"),
        ] {
            let resp = b.handle_request(&net_req(&url));
            assert_eq!(resp["approved"], json!(false), "{url} was approved: {resp}");
        }
        assert_no_connection(&l);
    }

    #[test]
    fn network_userinfo_is_denied_even_for_an_allowed_host() {
        let t = tmpdir("net-ui");
        let (l, port) = idle_listener();
        let b = broker(json!({"network_access": {"allowed": true, "allowed_hosts": ["127.0.0.1"]}}), &t.0.join("receipts.jsonl"));
        let resp = b.handle_request(&net_req(&format!("http://user@127.0.0.1:{port}/")));
        assert_eq!(resp["approved"], json!(false));
        assert!(resp["reason"].as_str().unwrap().contains("credentials"), "{resp}");
        assert_no_connection(&l);
    }

    #[test]
    fn network_non_http_schemes_and_garbage_are_denied() {
        let t = tmpdir("net-scheme");
        let b = broker(json!({"network_access": {"allowed": true}}), &t.0.join("receipts.jsonl"));
        for url in ["file:///etc/passwd", "ftp://allowed.example/", "not a url", "http//allowed.example"] {
            let resp = b.handle_request(&net_req(url));
            assert_eq!(resp["approved"], json!(false), "{url} was approved: {resp}");
        }
    }

    #[test]
    fn network_allowed_host_is_the_host_connected_to() {
        use std::io::Write as _;
        let t = tmpdir("net-ok");
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = c.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello").unwrap();
            req
        });
        // Mixed case in both the ceiling and the URL still matches.
        let b = broker(json!({"network_access": {"allowed": true, "allowed_hosts": ["LocalHost", "127.0.0.1"]}}), &t.0.join("receipts.jsonl"));
        let resp = b.handle_request(&net_req(&format!("http://127.0.0.1:{port}/probe")));
        assert_eq!(resp["approved"], json!(true), "{resp}");
        assert_eq!(resp["result"]["status"], json!(200));
        assert_eq!(resp["result"]["body_prefix"], json!("hello"));
        assert!(server.join().unwrap().starts_with("GET /probe "));
    }

    #[test]
    fn allowlist_host_normalises_like_the_client() {
        let h = |s: &str| allowlist_host(&url::Url::parse(s).unwrap());
        assert_eq!(h("http://Allowed.Example/x"), "allowed.example");
        assert_eq!(h("http://evil.example\\@allowed.example/"), "evil.example");
        assert_eq!(h("http://[::1]:8080/"), "::1");
        assert_eq!(h("http://0x7f000001/"), "127.0.0.1");
    }

    #[test]
    fn digest_correct_for_files() {
        let t = tmpdir("ok");
        let dir = t.0.join("scoped");
        fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.txt");
        fs::write(&f1, b"hello digest\n").unwrap();
        let f2 = dir.join("b.bin");
        let big: Vec<u8> = (0..=255u8).cycle().take(256 * 100).collect();
        fs::write(&f2, &big).unwrap();
        let b = broker(json!({"fs_digest": {"allowed_paths": [dir.to_str().unwrap()]}}), &t.0.join("receipts.jsonl"));
        let resp = b.handle_request(&digest_req(&[f1.to_str().unwrap(), f2.to_str().unwrap()]));
        assert_eq!(resp["approved"], json!(true), "{}", resp["reason"]);
        let d = &resp["result"]["digests"];
        assert_eq!(d[f1.to_str().unwrap()]["sha256"], json!(expected_sha256(&f1)));
        assert_eq!(d[f2.to_str().unwrap()]["sha256"], json!(expected_sha256(&f2)));
        assert_eq!(d[f1.to_str().unwrap()]["bytes"], json!(fs::metadata(&f1).unwrap().len()));
        assert!(d[f1.to_str().unwrap()]["mtime"].as_f64().unwrap() > 0.0);
        assert_eq!(resp["result"]["observer"], json!("host"));
        assert_eq!(resp["result"]["algorithm"], json!("sha256"));
    }

    #[test]
    fn per_path_conditions_not_request_failures() {
        let t = tmpdir("cond");
        let dir = t.0.join("scoped");
        fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing.txt");
        let b = broker(json!({"fs_digest": {"allowed_paths": [dir.to_str().unwrap()]}}), &t.0.join("receipts.jsonl"));
        let resp = b.handle_request(&digest_req(&[missing.to_str().unwrap(), dir.to_str().unwrap()]));
        assert_eq!(resp["approved"], json!(true), "{}", resp["reason"]);
        let d = &resp["result"]["digests"];
        assert_eq!(d[missing.to_str().unwrap()]["error_class"], json!("not_found"));
        assert_eq!(d[dir.to_str().unwrap()]["error_class"], json!("is_directory"));
    }

    #[test]
    fn out_of_scope_path_denies_whole_request() {
        let t = tmpdir("scope");
        let dir = t.0.join("scoped");
        fs::create_dir_all(&dir).unwrap();
        let b = broker(json!({"fs_digest": {"allowed_paths": [dir.to_str().unwrap()]}}), &t.0.join("receipts.jsonl"));
        // t.0 exists, but is NOT under allowed_paths (scoped is).
        let resp = b.handle_request(&digest_req(&[t.0.to_str().unwrap()]));
        assert_eq!(resp["approved"], json!(false));
        assert!(resp["reason"].as_str().unwrap().contains("allowed_paths"));
    }

    #[test]
    fn category_absent_from_ceiling_denies() {
        let t = tmpdir("absent");
        let dir = t.0.join("scoped");
        fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.txt");
        fs::write(&f1, b"x").unwrap();
        let b = broker(json!({}), &t.0.join("receipts.jsonl"));
        let resp = b.handle_request(&digest_req(&[f1.to_str().unwrap()]));
        assert_eq!(resp["approved"], json!(false));
    }

    #[test]
    fn child_ceiling_cannot_widen_scope() {
        let t = tmpdir("widen");
        let dir = t.0.join("scoped");
        fs::create_dir_all(&dir).unwrap();
        let (ok, _) = ceiling_is_subset(
            &json!({"fs_digest": {"allowed_paths": [t.0.to_str().unwrap()]}}),
            &json!({"fs_digest": {"allowed_paths": [dir.to_str().unwrap()]}}),
        );
        assert!(!ok);
    }

    #[test]
    fn at_most_64_paths_per_request() {
        let t = tmpdir("cap");
        let dir = t.0.join("scoped");
        fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.txt");
        fs::write(&f1, b"x").unwrap();
        let b = broker(json!({"fs_digest": {"allowed_paths": [dir.to_str().unwrap()]}}), &t.0.join("receipts.jsonl"));
        let many: Vec<&str> = vec![f1.to_str().unwrap(); 65];
        let resp = b.handle_request(&digest_req(&many));
        assert_eq!(resp["approved"], json!(false));
        assert!(resp["reason"].as_str().unwrap().contains("at most 64"));
    }

    #[test]
    fn known_answer_vectors_nist() {
        // FIPS 180-2 reference digests -- external ground truth, so the suite
        // does not merely verify sha2 against itself.
        let t = tmpdir("nist");
        let dir = t.0.join("scoped");
        fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.bin");
        fs::write(&empty, b"").unwrap();
        let abc = dir.join("abc.txt");
        fs::write(&abc, b"abc").unwrap();
        let b = broker(json!({"fs_digest": {"allowed_paths": [dir.to_str().unwrap()]}}), &t.0.join("receipts.jsonl"));
        let resp = b.handle_request(&digest_req(&[empty.to_str().unwrap(), abc.to_str().unwrap()]));
        assert_eq!(resp["approved"], json!(true), "{}", resp["reason"]);
        let d = &resp["result"]["digests"];
        assert_eq!(
            d[empty.to_str().unwrap()]["sha256"],
            json!("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(
            d[abc.to_str().unwrap()]["sha256"],
            json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[test]
    fn idle_connections_cannot_hold_every_broker_thread() {
        let t = tmpdir("conn-cap");
        let sock = t.0.join("b.sock");
        let server = serve(&sock, Arc::new(broker(json!({}), &t.0.join("receipts.jsonl")))).unwrap();
        // Fill every slot with a connection that never sends a request.
        let idle: Vec<UnixStream> = (0..MAX_CONNECTIONS).map(|_| UnixStream::connect(&sock).unwrap()).collect();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let busy = loop {
            let mut extra = UnixStream::connect(&sock).unwrap();
            extra.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let _ = writeln!(extra, "{}", json!({"id": "x", "category": "nope"}));
            let mut reply = String::new();
            let _ = BufReader::new(extra).read_line(&mut reply);
            if reply.contains("broker busy") || std::time::Instant::now() > deadline {
                break reply;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(busy.contains("broker busy"), "connection past the cap was served: {busy:?}");
        // Freeing a slot lets the next request through.
        drop(idle);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let served = loop {
            let mut c = UnixStream::connect(&sock).unwrap();
            writeln!(c, "{}", json!({"id": "y", "category": "nope"})).unwrap();
            let mut reply = String::new();
            BufReader::new(c).read_line(&mut reply).unwrap();
            if reply.contains("unknown capability") || std::time::Instant::now() > deadline {
                break reply;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(served.contains("unknown capability"), "{served:?}");
        server.shutdown();
    }

    #[test]
    fn external_fs_write_never_follows_a_symlink_swapped_in_after_the_check() {
        let t = tmpdir("fs-write");
        let allowed = t.0.join("allowed");
        let outside = t.0.join("outside");
        fs::create_dir_all(allowed.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let root = util::resolve(&allowed);
        // An ordinary write, creating a missing directory on the way.
        write_beneath(&root, &root.join("new/dir/f.txt"), b"ok").unwrap();
        assert_eq!(fs::read_to_string(allowed.join("new/dir/f.txt")).unwrap(), "ok");
        // The path was checked while sub/ was a directory and f.txt absent;
        // then each was swapped for a symlink out of the allowed dir.
        std::os::unix::fs::symlink(outside.join("victim"), allowed.join("sub/f.txt")).unwrap();
        let e = write_beneath(&root, &root.join("sub/f.txt"), b"x").unwrap_err();
        assert!(matches!(e, Denied(ref r) if r.contains("symlink")), "{e:?}");
        fs::remove_dir_all(allowed.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside, allowed.join("sub")).unwrap();
        let e = write_beneath(&root, &root.join("sub/f.txt"), b"x").unwrap_err();
        assert!(matches!(e, Denied(ref r) if r.contains("symlink")), "{e:?}");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "a write escaped the allowed dir");
    }

    #[test]
    fn allowed_dirs_are_pinned_at_start_and_kept_off_trusted_paths() {
        let t = tmpdir("fs-write-pin");
        let receipts = t.0.join("receipts/receipts.jsonl");
        let allowed = t.0.join("allowed");
        let later = t.0.join("later");
        let outside = t.0.join("outside");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let ceiling = json!({"external_fs_write": {"allowed_dirs": [allowed.to_str().unwrap(), later.to_str().unwrap()]}});
        let b = broker(ceiling, &receipts);
        let write = |path: &Path| b.handle_request(&json!({"category": "external_fs_write",
            "params": {"path": path.to_str().unwrap(), "content": "x"}}));
        assert_eq!(write(&allowed.join("a.txt"))["approved"], json!(true));
        // A worker that can write where the allowed dirs live swaps one for a
        // symlink, and creates the other (missing at start) as one.
        fs::remove_dir_all(&allowed).unwrap();
        std::os::unix::fs::symlink(&outside, &allowed).unwrap();
        std::os::unix::fs::symlink(&outside, &later).unwrap();
        for path in [allowed.join("b.txt"), later.join("c.txt")] {
            let r = write(&path);
            assert_eq!(r["approved"], json!(false), "{r}");
        }
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "a write followed a swapped-in symlink");
        // An allowed dir that reaches the receipt log never starts a broker.
        let e = Broker::new(BrokerConfig {
            ceiling: json!({"external_fs_write": {"allowed_dirs": [t.0.to_str().unwrap()]}}),
            receipt_log_path: receipts.clone(),
            session_id: "test".into(),
            package_depot_dir: None,
            package_environment_dir: None,
            project_dir: None,
            repo_dir: None,
            trusted_files: vec![],
            child_timeout: Duration::from_secs(90),
        }).err().expect("an allowed_dir holding the receipt log was accepted");
        assert!(e.contains("overlaps the protected host path"), "{e}");
    }

    #[test]
    fn a_receipt_that_cannot_be_written_denies_the_request() {
        let t = tmpdir("receipt-fail");
        let receipts = t.0.join("receipts.jsonl");
        let dir = t.0.join("data");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f"), "x").unwrap();
        let b = broker(json!({"fs_digest": {"allowed_paths": [dir.to_str().unwrap()]}}), &receipts);
        // Replace the log with a directory: opening it for append now fails.
        fs::remove_file(&receipts).unwrap();
        fs::create_dir(&receipts).unwrap();
        let r = b.handle_request(&digest_req(&[dir.join("f").to_str().unwrap()]));
        assert_eq!(r["approved"], json!(false), "{r}");
        assert!(r["reason"].as_str().unwrap().contains("receipt not recorded"), "{r}");
        assert!(r.get("result").is_none(), "a result left without its receipt: {r}");
    }

    #[test]
    fn a_broker_that_cannot_open_its_receipt_log_does_not_start() {
        let t = tmpdir("receipt-open");
        let receipts = t.0.join("receipts.jsonl");
        fs::create_dir(&receipts).unwrap();
        let e = Broker::new(BrokerConfig {
            ceiling: json!({}), receipt_log_path: receipts, session_id: "s".into(),
            package_depot_dir: None, package_environment_dir: None, project_dir: None, repo_dir: None,
            trusted_files: vec![], child_timeout: Duration::from_secs(5),
        }).err().expect("broker started without a receipt log");
        assert!(e.contains("receipt log"), "{e}");
    }

    #[test]
    fn an_allowed_dir_that_reaches_the_ceiling_file_never_starts() {
        // A ceiling read from a file given on the command line is as trusted
        // as PALETTE_CAPABILITY_CEILING: a write there widens the next ceiling.
        let t = tmpdir("ceiling-file");
        let config = t.0.join("config");
        fs::create_dir(&config).unwrap();
        let ceiling_file = config.join("ceiling.json");
        fs::write(&ceiling_file, "{}").unwrap();
        let start = |trusted: Vec<PathBuf>| Broker::new(BrokerConfig {
            ceiling: json!({"external_fs_write": {"allowed_dirs": [config.to_str().unwrap()]}}),
            receipt_log_path: t.0.join("receipts").join("receipts.jsonl"), session_id: "s".into(),
            package_depot_dir: None, package_environment_dir: None, project_dir: None, repo_dir: None,
            trusted_files: trusted, child_timeout: Duration::from_secs(5),
        });
        assert!(start(vec![]).is_ok(), "the allowed dir alone should be accepted");
        let e = start(vec![ceiling_file.clone()]).err().expect("an allowed_dir holding the ceiling file was accepted");
        assert!(e.contains("overlaps the protected host path") && e.contains("ceiling.json"), "{e}");
    }

    #[test]
    fn receipt_written_regardless_of_outcome() {
        let t = tmpdir("receipt");
        let receipts = t.0.join("receipts.jsonl");
        let dir = t.0.join("scoped");
        fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.txt");
        fs::write(&f1, b"x").unwrap();
        let b = broker(json!({"fs_digest": {"allowed_paths": [dir.to_str().unwrap()]}}), &receipts);
        b.handle_request(&digest_req(&[f1.to_str().unwrap()]));
        let deny = broker(json!({}), &receipts);
        deny.handle_request(&digest_req(&[f1.to_str().unwrap()]));
        let text = fs::read_to_string(&receipts).unwrap();
        let outcomes: Vec<bool> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|r| r["category"] == json!("fs_digest"))
            .map(|r| r["approved"].as_bool().unwrap())
            .collect();
        assert!(outcomes.contains(&true));
        assert!(outcomes.contains(&false));
    }

    fn cmd_broker(dir: &Path, commands: Value) -> Broker {
        broker(json!({"host_command": {"commands": commands}}), &dir.join("receipts.jsonl"))
    }

    fn cmd(b: &Broker, params: Value) -> Value {
        b.handle_request(&json!({"category": "host_command", "params": params}))
    }

    /// Gone, or a zombie waiting for init: no longer running either way.
    /// The pid a test command wrote, waiting for it: a fixed sleep raced the
    /// shell under load and read a file that did not exist yet.
    fn wait_for_pid(path: &Path) -> String {
        for _ in 0..500 {
            if let Ok(text) = fs::read_to_string(path) {
                if text.ends_with('\n') {
                    return text.trim().to_string();
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("{} was never written", path.display());
    }

    fn pid_dead(pid: &str) -> bool {
        for _ in 0..100 {
            match fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => return true,
                Ok(stat) if stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z') => return true,
                Ok(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        false
    }

    #[test]
    fn host_command_runs_a_named_command_with_its_fixed_argv_and_cwd() {
        let d = tmpdir("hostcmd-run");
        let dir = d.0.to_string_lossy().into_owned();
        let b = cmd_broker(&d.0, json!({
            "echo": {"argv": ["/bin/echo", "fixed"], "cwd": dir, "extra_args": true},
            "pwd": {"argv": ["/bin/pwd"], "cwd": dir},
        }));
        let r = cmd(&b, json!({"name": "echo", "args": ["one", "two words"]}));
        assert_eq!(r["approved"], json!(true), "{r}");
        assert_eq!(r["result"]["stdout"], json!("fixed one two words\n"));
        assert_eq!(r["result"]["returncode"], json!(0));
        let r = cmd(&b, json!({"name": "pwd"}));
        assert_eq!(r["result"]["stdout"].as_str().unwrap().trim(), fs::canonicalize(&d.0).unwrap().to_string_lossy());
        // Every request is receipted, approved or not.
        cmd(&b, json!({"name": "nope"}));
        let receipts = fs::read_to_string(d.0.join("receipts.jsonl")).unwrap();
        assert_eq!(receipts.lines().filter(|l| l.contains("\"host_command\"")).count(), 3);
    }

    #[test]
    fn host_command_refuses_unnamed_commands_unasked_args_and_bad_specs() {
        let d = tmpdir("hostcmd-deny");
        let dir = d.0.to_string_lossy().into_owned();
        let b = cmd_broker(&d.0, json!({
            "pwd": {"argv": ["/bin/pwd"], "cwd": dir},
            "relative": {"argv": ["sh", "-c", "true"], "cwd": dir},
            "nocwd": {"argv": ["/bin/true"]},
            "slow": {"argv": ["/bin/true"], "cwd": dir, "timeout_s": 999999},
        }));
        let reason = |params: Value| cmd(&b, params)["reason"].as_str().unwrap_or("APPROVED").to_string();
        assert!(reason(json!({"name": "rm"})).contains("not in this session's ceiling"));
        assert!(reason(json!({"name": "pwd", "args": ["-P"]})).contains("takes no arguments"));
        assert!(reason(json!({"name": "pwd", "args": "-P"})).contains("list of strings"));
        assert!(reason(json!({"name": "relative"})).contains("absolute path"));
        assert!(reason(json!({"name": "nocwd"})).contains("'cwd'"));
        assert!(reason(json!({"name": "slow"})).contains("timeout_s"));
        assert!(reason(json!({"name": "pwd", "action": "sudo"})).contains("run, start, poll, cancel, list or commands"));
        // A request may shorten a command's timeout, never extend it.
        assert!(reason(json!({"name": "pwd", "timeout_s": 61})).contains("(0, 60]"));
        assert!(reason(json!({"name": "pwd", "timeout_s": 0})).contains("timeout_s"));
        assert!(reason(json!({"name": "pwd", "timeout_s": "5"})).contains("timeout_s"));
        assert_eq!(reason(json!({"name": "pwd", "timeout_s": 5})), "APPROVED");
        let none = broker(json!({}), &d.0.join("r2.jsonl"));
        assert_eq!(cmd(&none, json!({"name": "pwd"}))["approved"], json!(false));
    }

    #[test]
    fn host_command_lock_runs_the_command_holding_the_lock() {
        let Some(flock) = ["/usr/bin/flock", "/bin/flock"].into_iter().find(|f| Path::new(f).is_file()) else {
            eprintln!("skipped: flock(1) is not installed");
            return;
        };
        let d = tmpdir("hostcmd-lock");
        let dir = d.0.to_string_lossy().into_owned();
        let lock = d.0.join("gpu.lock").to_string_lossy().into_owned();
        // A nested non-blocking flock on the same file fails with 7 only if
        // the command already holds the lock.
        let probe = json!([flock, "--nonblock", "--conflict-exit-code", "7", lock, "/bin/true"]);
        let b = cmd_broker(&d.0, json!({
            "locked": {"argv": probe, "cwd": dir, "lock": lock, "description": "probe the lock"},
            "unlocked": {"argv": probe, "cwd": dir},
            "relative": {"argv": ["/bin/true"], "cwd": dir, "lock": "gpu.lock"},
            "nodir": {"argv": ["/bin/true"], "cwd": dir, "lock": d.0.join("missing/gpu.lock")},
            "isdir": {"argv": ["/bin/true"], "cwd": dir, "lock": dir},
        }));
        let r = cmd(&b, json!({"name": "locked"}));
        assert_eq!(r["result"]["returncode"], json!(7), "{r}");
        assert_eq!(r["result"]["lock"], json!(lock));
        let r = cmd(&b, json!({"name": "unlocked"}));
        assert_eq!(r["result"]["returncode"], json!(0), "{r}");
        assert!(r["result"].get("lock").is_none());
        for name in ["relative", "nodir", "isdir"] {
            let r = cmd(&b, json!({"name": name}));
            assert!(r["reason"].as_str().unwrap_or("").contains("'lock' must be"), "{name}: {r}");
        }
        let r = cmd(&b, json!({"name": "locked", "action": "start"}));
        assert_eq!(r["result"]["lock"], json!(lock), "{r}");
    }

    #[test]
    fn host_command_commands_lists_what_the_ceiling_allows() {
        let d = tmpdir("hostcmd-commands");
        let dir = d.0.to_string_lossy().into_owned();
        let b = cmd_broker(&d.0, json!({
            "check": {"argv": ["/bin/true"], "cwd": dir, "extra_args": true, "timeout_s": 900, "description": "cargo check"},
            "pwd": {"argv": ["/bin/pwd"], "cwd": dir},
        }));
        let r = cmd(&b, json!({"action": "commands"}));
        assert_eq!(r["approved"], json!(true), "{r}");
        let listed = r["result"]["commands"].as_array().unwrap();
        let by = |n: &str| listed.iter().find(|c| c["name"] == json!(n)).unwrap().clone();
        assert_eq!(by("check")["description"], json!("cargo check"));
        assert_eq!(by("check")["extra_args"], json!(true));
        assert_eq!(by("check")["timeout_s"], json!(900));
        assert_eq!(by("pwd")["extra_args"], json!(false));
        assert_eq!(by("pwd")["timeout_s"], json!(60.0));
        assert_eq!(by("pwd")["cwd"], json!(dir));
        assert!(by("pwd").get("description").is_none());
    }

    #[test]
    fn host_command_timeout_kills_the_whole_process_group() {
        let d = tmpdir("hostcmd-timeout");
        let dir = d.0.to_string_lossy().into_owned();
        let b = cmd_broker(&d.0, json!({
            "hang": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $! > helper.pid; wait"], "cwd": dir, "timeout_s": 2.0},
        }));
        let started = std::time::Instant::now();
        let r = cmd(&b, json!({"name": "hang"}));
        assert_eq!(r["result"]["timed_out"], json!(true), "{r}");
        assert!(started.elapsed() < Duration::from_secs(8));
        let helper = wait_for_pid(&d.0.join("helper.pid"));
        assert!(pid_dead(&helper), "the backgrounded helper outlived the timeout");
    }

    #[test]
    fn host_command_helpers_do_not_outlive_a_finished_command() {
        let d = tmpdir("hostcmd-orphan");
        let dir = d.0.to_string_lossy().into_owned();
        let b = cmd_broker(&d.0, json!({
            "spawn": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $!"], "cwd": dir, "timeout_s": 30},
        }));
        let started = std::time::Instant::now();
        let r = cmd(&b, json!({"name": "spawn"}));
        assert_eq!(r["result"]["returncode"], json!(0), "{r}");
        assert!(started.elapsed() < Duration::from_secs(5), "an orphan's open pipe held the reply");
        assert!(pid_dead(r["result"]["stdout"].as_str().unwrap().trim()));
    }

    #[test]
    fn host_command_start_and_poll_for_work_longer_than_a_turn() {
        let d = tmpdir("hostcmd-job");
        let dir = d.0.to_string_lossy().into_owned();
        let b = cmd_broker(&d.0, json!({
            "job": {"argv": ["/bin/sh", "-c", "sleep 0.4; echo done"], "cwd": dir},
            "long": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $! > long.pid; wait"], "cwd": dir},
        }));
        let r = cmd(&b, json!({"name": "job", "action": "start"}));
        let id = r["result"]["job"].as_str().unwrap().to_string();
        assert_eq!(r["result"]["running"], json!(true));
        assert_eq!(cmd(&b, json!({"action": "poll", "job": id}))["result"]["running"], json!(true));
        // Poll until the job finishes; a fixed sleep raced the job under load.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let r = loop {
            let r = cmd(&b, json!({"action": "poll", "job": id}));
            if r["result"]["running"] != json!(true) || std::time::Instant::now() > deadline {
                break r;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(r["result"]["running"], json!(false), "{r}");
        assert_eq!(r["result"]["stdout"], json!("done\n"));
        assert!(cmd(&b, json!({"action": "poll", "job": id}))["reason"].as_str().unwrap().contains("no running"));
        // A session that ends kills the jobs it started.
        cmd(&b, json!({"name": "long", "action": "start"}));
        let helper = wait_for_pid(&d.0.join("long.pid"));
        drop(b);
        assert!(pid_dead(&helper), "a job outlived its session");
    }

    fn poll_until(b: &Broker, id: &str, mut each: impl FnMut(&Value) -> bool) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let r = cmd(b, json!({"action": "poll", "job": id}));
            if r["result"]["running"] != json!(true) || !each(&r) || std::time::Instant::now() > deadline {
                return r;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn host_command_poll_streams_output_since_the_last_poll() {
        let d = tmpdir("hostcmd-stream");
        let dir = d.0.to_string_lossy().into_owned();
        let b = cmd_broker(&d.0, json!({
            "ticks": {"argv": ["/bin/sh", "-c", "echo one; while [ ! -e go ]; do sleep 0.05; done; echo two; echo err >&2"], "cwd": dir},
        }));
        let id = cmd(&b, json!({"name": "ticks", "action": "start"}))["result"]["job"].as_str().unwrap().to_string();
        let mut seen = String::new();
        let r = poll_until(&b, &id, |r| {
            seen.push_str(r["result"]["stdout"].as_str().unwrap());
            assert_eq!(r["result"]["name"], json!("ticks"), "{r}");
            seen != "one\n"
        });
        assert_eq!(r["result"]["running"], json!(true), "{r}");
        assert_eq!(seen, "one\n");
        // Nothing new since: the next poll returns no output again.
        assert_eq!(cmd(&b, json!({"action": "poll", "job": id}))["result"]["stdout"], json!(""));
        fs::write(d.0.join("go"), "").unwrap();
        let (mut rest, mut err) = (String::new(), String::new());
        let mut take = |r: &Value| {
            rest.push_str(r["result"]["stdout"].as_str().unwrap());
            err.push_str(r["result"]["stderr"].as_str().unwrap());
        };
        let r = poll_until(&b, &id, |r| { take(r); true });
        take(&r);
        assert_eq!(r["result"]["running"], json!(false), "{r}");
        assert_eq!(rest, "two\n", "the final result repeated output a poll had returned");
        assert_eq!(err, "err\n");
        assert_eq!(r["result"]["cancelled"], json!(false));
    }

    #[test]
    fn host_command_cancel_kills_the_job_group_and_list_names_jobs() {
        let d = tmpdir("hostcmd-cancel");
        let dir = d.0.to_string_lossy().into_owned();
        let b = cmd_broker(&d.0, json!({
            "long": {"argv": ["/bin/sh", "-c", "echo started; sleep 300 & echo $! > long.pid; wait"], "cwd": dir},
            "quick": {"argv": ["/bin/true"], "cwd": dir},
        }));
        let id = cmd(&b, json!({"name": "long", "action": "start"}))["result"]["job"].as_str().unwrap().to_string();
        let helper = wait_for_pid(&d.0.join("long.pid"));
        let listed = cmd(&b, json!({"action": "list"}));
        assert_eq!(listed["result"]["jobs"][0]["job"], json!(id), "{listed}");
        assert_eq!(listed["result"]["jobs"][0]["name"], json!("long"));
        assert_eq!(listed["result"]["jobs"][0]["running"], json!(true));
        let r = cmd(&b, json!({"action": "cancel", "job": id}));
        assert_eq!(r["result"]["cancelled"], json!(true), "{r}");
        assert_eq!(r["result"]["running"], json!(false));
        assert_eq!(r["result"]["timed_out"], json!(false));
        assert_eq!(r["result"]["stdout"], json!("started\n"));
        assert!(pid_dead(&helper), "a cancelled job's helper survived");
        assert_eq!(cmd(&b, json!({"action": "list"}))["result"]["jobs"], json!([]));
        assert!(cmd(&b, json!({"action": "cancel", "job": id}))["reason"].as_str().unwrap().contains("no running"));
        // Cancelling a job that already finished reports it finished, not cancelled.
        let id = cmd(&b, json!({"name": "quick", "action": "start"}))["result"]["job"].as_str().unwrap().to_string();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while cmd(&b, json!({"action": "list"}))["result"]["jobs"][0]["running"] == json!(true) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let r = cmd(&b, json!({"action": "cancel", "job": id}));
        assert_eq!((r["result"]["cancelled"].clone(), r["result"]["returncode"].clone()), (json!(false), json!(0)), "{r}");
    }

    #[test]
    fn host_command_reports_output_the_tail_dropped() {
        let d = tmpdir("hostcmd-skip");
        let dir = d.0.to_string_lossy().into_owned();
        let b = cmd_broker(&d.0, json!({
            "loud": {"argv": ["/bin/sh", "-c", "head -c 40000 /dev/zero | tr '\\0' x; echo"], "cwd": dir},
        }));
        let r = cmd(&b, json!({"name": "loud"}));
        assert_eq!(r["result"]["stdout"].as_str().unwrap().len(), HOST_COMMAND_OUTPUT, "{}", r["result"]["returncode"]);
        assert_eq!(r["result"]["stdout_skipped_bytes"], json!(40001 - HOST_COMMAND_OUTPUT));
        assert!(r["result"].get("stderr_skipped_bytes").is_none());
    }

    #[test]
    fn host_command_request_timeout_shortens_the_ceiling_timeout() {
        let d = tmpdir("hostcmd-reqtimeout");
        let dir = d.0.to_string_lossy().into_owned();
        let b = cmd_broker(&d.0, json!({"hang": {"argv": ["/bin/sleep", "300"], "cwd": dir, "timeout_s": 600}}));
        let started = std::time::Instant::now();
        let r = cmd(&b, json!({"name": "hang", "timeout_s": 0.5}));
        assert_eq!(r["result"]["timed_out"], json!(true), "{r}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn child_ceiling_keeps_only_parent_commands_unchanged() {
        let parent = json!({"host_command": {"commands": {"render": {"argv": ["/bin/true"], "cwd": "/tmp"}}}});
        assert!(ceiling_is_subset(&json!({"host_command": {"commands": {}}}), &parent).0);
        assert!(ceiling_is_subset(&parent, &parent).0);
        let widened = json!({"host_command": {"commands": {"render": {"argv": ["/bin/sh"], "cwd": "/tmp"}}}});
        assert!(!ceiling_is_subset(&widened, &parent).0);
        let extra = json!({"host_command": {"commands": {"shell": {"argv": ["/bin/sh"], "cwd": "/tmp"}}}});
        assert!(!ceiling_is_subset(&extra, &parent).0);
        assert!(!ceiling_is_subset(&parent, &json!({})).0);
    }

}
