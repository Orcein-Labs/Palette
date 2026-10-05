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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the agent host does for a `host_request`: (type, payload) -> reply
/// ({"status": "ok", "result"} or {"status": "error", "error"}). Set by the
/// process that owns the session, never by a request.
pub type HostBridge = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;

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
    // Trusted paths fixed when the session starts; never taken from a request.
    depot_dir: Option<String>,
    project_dir: Option<String>,
    repo_dir: Option<String>,
    child_timeout: Duration,
    host_bridge: Mutex<Option<HostBridge>>,
    log_lock: Mutex<()>,
    depot_lock: Mutex<()>,
    // host_command jobs started with action "start"; dropping the broker
    // (the session ending) kills any still running.
    jobs: Mutex<BTreeMap<String, sandbox::HostJob>>,
}

/// At most this many host_command jobs run at once per session.
const MAX_HOST_JOBS: usize = 8;
const HOST_COMMAND_MAX_TIMEOUT_S: f64 = 4.0 * 3600.0;
const HOST_COMMAND_OUTPUT: usize = 16 * 1024;

pub struct BrokerConfig {
    pub ceiling: Value,
    pub receipt_log_path: PathBuf,
    pub session_id: String,
    pub depot_dir: Option<String>,
    pub project_dir: Option<String>,
    pub repo_dir: Option<String>,
    pub child_timeout: Duration,
}

impl Broker {
    pub fn new(c: BrokerConfig) -> Result<Broker, String> {
        if let Some(parent) = c.receipt_log_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        Ok(Broker {
            ceiling: c.ceiling,
            receipt_log_path: c.receipt_log_path,
            session_id: c.session_id,
            depot_dir: c.depot_dir,
            project_dir: c.project_dir,
            repo_dir: c.repo_dir,
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
        let allowed: Vec<PathBuf> = dirs.iter().map(|d| util::resolve(Path::new(d))).collect();
        if !allowed.iter().any(|a| util::within(&target, a)) {
            return Err(Denied(format!("path {} is outside the ceiling's allowed_dirs {}", target.display(), qpaths(&allowed))));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| Internal(e.to_string()))?;
        }
        fs::write(&target, content).map_err(|e| Internal(e.to_string()))?;
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
        // A scheme allowlist: a network grant must not double as a host file read (file://).
        let (scheme, rest) = url.split_once("://").unwrap_or(("", ""));
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(Denied(format!("network_access only permits http/https URLs, got scheme {}", q(&scheme))));
        }
        if let Some(hosts) = str_list(cap.get("allowed_hosts")) {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            let hostport = authority.rsplit('@').next().unwrap_or("");
            let host = if let Some(h) = hostport.strip_prefix('[') {
                h.split(']').next().unwrap_or("").to_ascii_lowercase()
            } else {
                hostport.split(':').next().unwrap_or("").to_ascii_lowercase()
            };
            if !hosts.contains(&host) {
                return Err(Denied(format!("host {host} is not in this session's allowed_hosts {}", ql(&hosts))));
            }
        }
        // No redirect-following: a redirect target is a URL the allowlist was
        // never asked about. A 3xx comes back as data.
        let agent = ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(5)).build();
        let resp = match agent.get(url).call() {
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
        let Some(depot) = &self.depot_dir else {
            return Err(Denied("no session depot is configured for this broker -- package_management requires one".into()));
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
        let mut env = BTreeMap::new();
        env.insert("JULIA_DEPOT_PATH".to_string(), Some(depot.clone()));
        env.insert("JULIA_PROJECT".to_string(), None);
        let argv: Vec<String> = vec!["julia".into(), "--startup-file=no".into(), "-e".into(),
                                     format!("using Pkg; Pkg.add(Pkg.PackageSpec({spec}))")];
        let r = {
            let _g = self.depot_lock.lock().unwrap();
            sandbox::run_captured(&argv, Some(&env), Duration::from_secs(300)).map_err(Internal)?
        };
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
                depot_dir: child_depot.as_ref().map(|d| d.to_string_lossy().into_owned()),
                project_dir: Some(project.clone()),
                repo_dir: Some(repo.clone()),
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
            let _ = fs::remove_dir_all(d);
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

    /// Digest files with host privilege -- the independent eye (FM-SLICE-A2b),
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
    /// and passes arguments only to a command that allows them. Never a
    /// shell. Actions: "run" (default) waits for the result; "start"
    /// returns a job id at once, for commands longer than a turn; "poll"
    /// with that id returns {"running": true} or the result.
    /// Ceiling: {"host_command": {"commands": {"<name>": {"argv": [...],
    ///   "cwd": "/abs", "timeout_s": 600, "extra_args": false}}}}
    fn host_command(&self, p: &Map<String, Value>) -> Result<Value, Fail> {
        let cap = self.cap("host_command").ok_or_else(|| Denied("host_command not in this session's ceiling".into()))?;
        let action = p.get("action").and_then(Value::as_str).unwrap_or("run");
        if action == "poll" {
            let id = p.get("job").and_then(Value::as_str).ok_or_else(|| Denied("host_command poll requires a string 'job'".into()))?;
            let mut jobs = self.jobs.lock().unwrap();
            let job = jobs.get_mut(id).ok_or_else(|| Denied(format!("no running host_command job {}", q(id))))?;
            return match job.poll().map_err(Internal)? {
                None => Ok(json!({"job": id, "running": true})),
                Some(r) => {
                    jobs.remove(id);
                    Ok(host_job_json(Some(id), &r))
                }
            };
        }
        if action != "run" && action != "start" {
            return Err(Denied(format!("host_command action must be run, start or poll, not {}", q(action))));
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
        let timeout = Duration::from_secs_f64(timeout_s);
        if action == "start" {
            let mut jobs = self.jobs.lock().unwrap();
            if jobs.len() >= MAX_HOST_JOBS {
                return Err(Denied(format!("at most {MAX_HOST_JOBS} host_command jobs may run at once; poll one to completion first")));
            }
            let job = sandbox::HostJob::start(&argv, &cwd, timeout, HOST_COMMAND_OUTPUT).map_err(Internal)?;
            let id = util::hex(16);
            jobs.insert(id.clone(), job);
            return Ok(json!({"job": id, "name": name, "running": true}));
        }
        let mut job = sandbox::HostJob::start(&argv, &cwd, timeout, HOST_COMMAND_OUTPUT).map_err(Internal)?;
        let r = job.wait().map_err(Internal)?;
        let mut v = host_job_json(None, &r);
        v["name"] = json!(name);
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
        self.write_receipt(&req_id, &category, &params, approved, &result, &reason);
        let mut resp = json!({"id": req_id, "approved": approved});
        if approved {
            resp["result"] = result;
        } else {
            resp["reason"] = reason;
        }
        resp
    }

    fn write_receipt(&self, req_id: &Value, category: &Value, params: &Value, approved: bool, result: &Value, reason: &Value) {
        let receipt = json!({
            "receipt_id": util::uuid4(), "request_id": req_id, "session_id": self.session_id,
            "timestamp": util::now_secs(), "category": category, "params": params,
            "approved": approved, "result": result, "reason": reason,
        });
        let _g = self.log_lock.lock().unwrap();
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.receipt_log_path) {
            let _ = writeln!(f, "{receipt}");
        }
    }
}

/// Stream a file through SHA-256 in 1 MiB chunks, as the Python broker does,
/// and report (hex, bytes, mtime in seconds since the epoch).
fn host_job_json(job: Option<&str>, r: &sandbox::HostJobResult) -> Value {
    let mut v = json!({"returncode": r.returncode, "timed_out": r.timed_out, "seconds": (r.seconds * 1000.0).round() / 1000.0,
                       "stdout": r.stdout, "stderr": r.stderr, "running": false});
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
    let thread = std::thread::spawn(move || {
        for conn in listener.incoming() {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let Ok(conn) = conn else { continue };
            let b = broker.clone();
            std::thread::spawn(move || handle_conn(conn, &b));
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
// runtime/security/test_fs_digest.py check-for-check (FM-SLICE-A2b), plus the 64-path
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
            depot_dir: None,
            project_dir: None,
            repo_dir: None,
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

    #[test]
    fn digest_correct_for_files() {
        let t = tmpdir("ok");
        let dir = t.0.join("scoped");
        fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.txt");
        fs::write(&f1, b"hello fm-slice-a2b\n").unwrap();
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
        assert!(reason(json!({"name": "pwd", "action": "sudo"})).contains("run, start or poll"));
        let none = broker(json!({}), &d.0.join("r2.jsonl"));
        assert_eq!(cmd(&none, json!({"name": "pwd"}))["approved"], json!(false));
    }

    #[test]
    fn host_command_timeout_kills_the_whole_process_group() {
        let d = tmpdir("hostcmd-timeout");
        let dir = d.0.to_string_lossy().into_owned();
        let b = cmd_broker(&d.0, json!({
            "hang": {"argv": ["/bin/sh", "-c", "sleep 300 & echo $! > helper.pid; wait"], "cwd": dir, "timeout_s": 0.5},
        }));
        let started = std::time::Instant::now();
        let r = cmd(&b, json!({"name": "hang"}));
        assert_eq!(r["result"]["timed_out"], json!(true), "{r}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let helper = fs::read_to_string(d.0.join("helper.pid")).unwrap();
        assert!(pid_dead(helper.trim()), "the backgrounded helper outlived the timeout");
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
        std::thread::sleep(Duration::from_millis(700));
        let r = cmd(&b, json!({"action": "poll", "job": id}));
        assert_eq!(r["result"]["running"], json!(false), "{r}");
        assert_eq!(r["result"]["stdout"], json!("done\n"));
        assert!(cmd(&b, json!({"action": "poll", "job": id}))["reason"].as_str().unwrap().contains("no running"));
        // A session that ends kills the jobs it started.
        cmd(&b, json!({"name": "long", "action": "start"}));
        std::thread::sleep(Duration::from_millis(200));
        let helper = fs::read_to_string(d.0.join("long.pid")).unwrap();
        drop(b);
        assert!(pid_dead(helper.trim()), "a job outlived its session");
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
