//! The first trust domain: a bubblewrap sandbox around an unrestricted
//! `julia`. Full language power inside, bounded by the OS outside it. Paths
//! are bound at the same guest path as on the host, because Julia's manifests
//! and precompile caches embed absolute paths. Each worker gets its own
//! reflink clone of the depot as its single writable depot entry: a split
//! JULIA_DEPOT_PATH makes Julia recompute stdlib build ids and invalidate
//! every precompiled cache on each launch.

use crate::util;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SANDBOX_USER: &str = "palette";
/// The sandbox sets these itself; a task environment cannot move them.
const SANDBOX_OWNED_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "JULIA_DEPOT_PATH", "JULIA_PROJECT", "JULIA_LOAD_PATH",
    "JULIA_PKG_OFFLINE", "PALETTE_REPO_DIR", "PALETTE_STATE_DIR", "PALETTE_BROKER_SOCKET",
];

/// The real julia binary, past the juliaup shim. An explicit PALETTE_JULIA_BIN
/// wins: asked under a fresh HOME, the shim installs its current default
/// release, and an endurance run got a Julia the depot was not built for.
/// stdin is never inherited: `julia -e` with the caller's stdin breaks the
/// caller's later reads from it, and the session reads its requests there.
pub fn resolve_julia() -> Result<String, String> {
    if let Ok(pinned) = std::env::var("PALETTE_JULIA_BIN") {
        if !pinned.is_empty() {
            if !Path::new(&pinned).is_file() {
                return Err(format!("PALETTE_JULIA_BIN is not a file: {pinned}"));
            }
            return Ok(pinned);
        }
    }
    let out = Command::new("julia")
        .args(["-e", "print(Sys.BINDIR)"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run julia to find its binary: {e}"))?;
    if !out.status.success() {
        return Err("julia -e print(Sys.BINDIR) failed".into());
    }
    Ok(format!("{}/julia", String::from_utf8_lossy(&out.stdout).trim()))
}

pub fn default_depot() -> String {
    std::env::var("JULIA_DEPOT_PATH")
        .ok()
        .and_then(|v| v.split(':').next_back().map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| util::home().join(".julia").to_string_lossy().into_owned())
}

/// One private, writable depot clone, shared by a whole session: the broker's
/// package installs go into the same directory a running worker has bound.
pub fn create_session_depot(real_depot: Option<&str>) -> Result<PathBuf, String> {
    let depot = real_depot.map(str::to_string).unwrap_or_else(default_depot);
    let root = Path::new(&depot).parent().unwrap_or(Path::new("/")).join(".palette-depot-clones");
    fs::create_dir_all(&root).map_err(|e| format!("creating {}: {e}", root.display()))?;
    let dest = util::mkdtemp_in(&root, "depot-").map_err(|e| e.to_string())?;
    let _ = fs::remove_dir(&dest); // cp wants a destination that does not exist yet
    let status = Command::new("cp")
        .args(["-a", "--reflink=auto"])
        .arg(&depot)
        .arg(&dest)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("cp: {e}"))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&dest);
        return Err(format!("cloning the depot {depot} failed"));
    }
    Ok(dest)
}

/// A benchmark's toolchain activation (KEY=VALUE lines), given to the kernel
/// as the other contestants get it.
pub fn read_task_env(path: Option<&str>) -> Result<BTreeMap<String, String>, String> {
    let mut env = BTreeMap::new();
    let Some(path) = path.filter(|p| !p.is_empty()) else { return Ok(env) };
    let text = fs::read_to_string(path).map_err(|e| format!("PALETTE_TASK_ENV {path}: {e}"))?;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("PALETTE_TASK_ENV: not a KEY=VALUE line: {line:?}"));
        };
        let key = key.trim();
        let ident = !key.is_empty()
            && !key.starts_with(|c: char| c.is_ascii_digit())
            && key.chars().all(|c| c.is_alphanumeric() || c == '_');
        if !ident {
            return Err(format!("PALETTE_TASK_ENV: not a KEY=VALUE line: {line:?}"));
        }
        if !SANDBOX_OWNED_ENV.contains(&key) {
            env.insert(key.to_string(), value.to_string());
        }
    }
    Ok(env)
}

/// passwd and group files naming the sandbox's uid 1000, written once per host.
fn identity_files() -> Result<PathBuf, String> {
    let uid = unsafe { libc::getuid() };
    let etc = util::temp_dir().join(format!("palette-etc-{uid}"));
    let passwd = format!("{SANDBOX_USER}:x:1000:1000:Palette sandbox:/run/palette/home:/bin/bash\n");
    let group = format!("{SANDBOX_USER}:x:1000:\n");
    if fs::read_to_string(etc.join("passwd")).ok().as_deref() != Some(passwd.as_str()) {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let _ = fs::DirBuilder::new().mode(0o755).create(&etc);
        for (name, text) in [("passwd", &passwd), ("group", &group)] {
            let tmp = etc.join(format!(".{name}.{}", std::process::id()));
            fs::write(&tmp, text).map_err(|e| e.to_string())?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).map_err(|e| e.to_string())?;
            fs::rename(&tmp, etc.join(name)).map_err(|e| e.to_string())?;
        }
    }
    Ok(etc)
}

pub struct SandboxSpec<'a> {
    pub workspace_dir: &'a str,
    pub broker_socket_dir: Option<&'a str>,
    pub project_dir: &'a str,
    pub repo_dir: &'a str,
    pub julia_bin: &'a str,
    pub julia_depot: &'a str,
    pub network_enabled: bool,
    pub depot_clone_dir: Option<&'a str>,
    pub state_dir: Option<&'a str>,
}

fn canonical_dir(name: &str, path: &Path) -> Result<PathBuf, String> {
    let resolved = fs::canonicalize(path).map_err(|e| format!("resolving {name} {}: {e}", path.display()))?;
    if !resolved.is_dir() {
        return Err(format!("{name} is not a directory: {}", resolved.display()));
    }
    Ok(resolved)
}

pub(crate) fn check_writable_mounts_disjoint(
    writable: &[(&str, &Path)],
    protected: &[(&str, &Path)],
) -> Result<(), String> {
    for (writable_name, writable_path) in writable {
        for (protected_name, protected_path) in protected {
            if writable_path.starts_with(protected_path) || protected_path.starts_with(writable_path) {
                return Err(format!(
                    "writable {writable_name} mount {} overlaps protected {protected_name} mount {}",
                    writable_path.display(), protected_path.display()
                ));
            }
        }
    }
    Ok(())
}

fn check_writable_roots_not_masked(
    writable: &[(&str, &Path)],
    required_roots: &[(&str, &Path)],
) -> Result<(), String> {
    for (writable_name, writable_path) in writable {
        for (root_name, root_path) in required_roots {
            if root_path.starts_with(writable_path) {
                return Err(format!(
                    "writable {writable_name} mount {} masks required {root_name} mount {}",
                    writable_path.display(), root_path.display()
                ));
            }
        }
    }
    Ok(())
}

/// Host directories the kernel may read, from PALETTE_READ_ROOTS
/// (':'-separated absolute paths), each bound read-only at its own path so
/// paths read in the sandbox are the paths outside it.
pub(crate) fn parse_read_roots(spec: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for entry in spec.unwrap_or("").split(':').filter(|e| !e.is_empty()) {
        if !Path::new(entry).is_absolute() {
            return Err(format!("PALETTE_READ_ROOTS entries must be absolute paths: {entry}"));
        }
        roots.push(canonical_dir("read root", Path::new(entry))?);
    }
    Ok(roots)
}

/// A read root may not contain any other mount, so no mount order can let it
/// hide one (a writable depot clone, the synthetic /tmp), and it may not lie
/// inside a writable mount, where the worker could write what it reads.
pub(crate) fn check_read_roots(
    roots: &[PathBuf],
    writable: &[(&str, &Path)],
    others: &[(&str, &Path)],
) -> Result<(), String> {
    for root in roots {
        for (name, path) in writable.iter().chain(others) {
            if path.starts_with(root) {
                return Err(format!("read root {} contains the {name} mount {}", root.display(), path.display()));
            }
        }
        for (name, path) in writable {
            if root.starts_with(path) {
                return Err(format!("read root {} lies inside the writable {name} mount {}", root.display(), path.display()));
            }
        }
    }
    Ok(())
}

pub fn build_bwrap_argv(s: &SandboxSpec) -> Result<Vec<String>, String> {
    let toolchain = Path::new(s.julia_bin).parent().and_then(Path::parent).unwrap_or(Path::new("/"));
    let workspace = canonical_dir("workspace", Path::new(s.workspace_dir))?;
    let repo = canonical_dir("repository", Path::new(s.repo_dir))?;
    let project = canonical_dir("project", Path::new(s.project_dir))?;
    let original_depot = canonical_dir("shared Julia depot", Path::new(s.julia_depot))?;
    let depot_clone = s.depot_clone_dir.map(|p| canonical_dir("private Julia depot clone", Path::new(p))).transpose()?;
    let state_dir = s.state_dir.map(|p| canonical_dir("state directory", Path::new(p))).transpose()?;
    let task_tools_input = std::env::var("PALETTE_TASK_TOOLS").ok().filter(|t| !t.is_empty());
    if let Some(t) = &task_tools_input {
        if !Path::new(t).join("bin").is_dir() {
            return Err(format!("PALETTE_TASK_TOOLS has no bin directory: {t}"));
        }
    }
    let task_tools = task_tools_input.as_deref();
    let task_tools_root = task_tools.map(|p| canonical_dir("task tools", Path::new(p))).transpose()?;
    let broker_socket_dir = s.broker_socket_dir;
    let broker_root = broker_socket_dir.map(|p| canonical_dir("broker socket directory", Path::new(p))).transpose()?;
    let task_env = read_task_env(std::env::var("PALETTE_TASK_ENV").ok().as_deref())?;

    let toolchain_root = canonical_dir("Julia toolchain", toolchain)?;
    let system_usr = canonical_dir("system /usr", Path::new("/usr"))?;
    let system_lib64 = canonical_dir("system /lib64", Path::new("/lib64"))?;
    let system_etc = canonical_dir("system /etc", Path::new("/etc"))?;
    let system_proc = canonical_dir("system /proc", Path::new("/proc"))?;
    let system_dev = canonical_dir("system /dev", Path::new("/dev"))?;
    let system_tmp = canonical_dir("system /tmp", Path::new("/tmp"))?;
    let system_run = canonical_dir("system /run", Path::new("/run"))?;
    let mut protected = vec![
        ("repository", repo.as_path()), ("project", project.as_path()),
        ("Julia toolchain", toolchain_root.as_path()), ("shared Julia depot", original_depot.as_path()),
        ("system /usr", system_usr.as_path()), ("system /lib64", system_lib64.as_path()),
        ("system /etc", system_etc.as_path()), ("system /proc", system_proc.as_path()),
        ("system /dev", system_dev.as_path()), ("synthetic /run/palette", Path::new("/run/palette")),
    ];
    if let Some(path) = depot_clone.as_deref() { protected.push(("private Julia depot clone source", path)); }
    if let Some(path) = task_tools_root.as_deref() { protected.push(("task tools", path)); }
    if let Some(path) = broker_root.as_deref() { protected.push(("broker socket directory", path)); }
    let mut writable = vec![("workspace", workspace.as_path())];
    if let Some(path) = state_dir.as_deref() { writable.push(("state directory", path)); }
    let required_roots = [("system /tmp", system_tmp.as_path()), ("system /run", system_run.as_path())];
    // The private depot clone is protected as a host source while its guest depot
    // mapping remains intentional; the original shared depot is also protected.
    check_writable_mounts_disjoint(&writable, &protected)?;
    // /tmp/workspace-style child mounts are intentional; only an equal or
    // ancestor workspace/state mount could hide the synthetic /tmp or /run root.
    check_writable_roots_not_masked(&writable, &required_roots)?;
    let read_roots = parse_read_roots(std::env::var("PALETTE_READ_ROOTS").ok().as_deref())?;
    let others: Vec<(&str, &Path)> = protected.iter().chain(required_roots.iter()).copied().collect();
    check_read_roots(&read_roots, &writable, &others)?;
    let read_roots: Vec<String> = read_roots.iter().map(|r| r.to_string_lossy().into_owned()).collect();

    let workspace = workspace.to_str().ok_or("workspace path is not valid UTF-8")?;
    let state_dir = state_dir.as_deref().map(|p| p.to_str().ok_or("state directory path is not valid UTF-8")).transpose()?;
    let toolchain = toolchain.to_string_lossy().into_owned();
    let depot_source = s.depot_clone_dir.unwrap_or(s.julia_depot);

    let mut a: Vec<String> = Vec::new();
    let mut push = |xs: &[&str]| a.extend(xs.iter().map(|x| x.to_string()));
    // A new PID namespace whose PID 1 is bwrap's reaper, with julia as its
    // child: when julia exits, everything left in the namespace is killed --
    // background runs, double forks, daemons that ignore TERM and HUP.
    // --die-with-parent ends it if this process dies first.
    push(&["bwrap", "--unshare-user", "--uid", "1000", "--gid", "1000", "--disable-userns", "--assert-userns-disabled",
           "--unshare-pid", "--unshare-ipc"]);
    if !s.network_enabled {
        push(&["--unshare-net"]);
    }
    push(&["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--dir", "/run/palette", "--tmpfs", "/run/palette",
           "--dir", "/run/palette/home", "--clearenv", "--die-with-parent", "--new-session", "--cap-drop", "ALL",
           // Preserve the host ELF loader layout (Ubuntu differs from Arch).
           "--ro-bind", "/usr", "/usr", "--symlink", "usr/lib", "/lib", "--ro-bind", "/lib64", "/lib64",
           "--symlink", "usr/bin", "/bin", "--ro-bind", "/etc/ld.so.cache", "/etc/ld.so.cache",
           "--ro-bind", &toolchain, &toolchain,
           // The depot, writable at its own real path; the source is this
           // worker's private clone, never the shared depot.
           "--bind", depot_source, s.julia_depot,
           "--ro-bind", s.repo_dir, s.repo_dir,
           "--ro-bind", s.project_dir, s.project_dir,
           "--bind", workspace, workspace]);
    if let Some(t) = task_tools {
        push(&["--ro-bind", t, t]);
    }
    for r in &read_roots {
        push(&["--ro-bind", r, r]);
    }
    if let Some(d) = broker_socket_dir {
        push(&["--ro-bind", d, d]);
    }
    let etc = identity_files()?;
    let (pw, gr) = (etc.join("passwd"), etc.join("group"));
    push(&["--ro-bind", &pw.to_string_lossy(), "/etc/passwd", "--ro-bind", &gr.to_string_lossy(), "/etc/group"]);
    // The persistent kernel's saved state, outside the task workspace.
    if let Some(st) = state_dir {
        push(&["--bind", st, st, "--setenv", "PALETTE_STATE_DIR", st]);
    }
    let path = match task_tools {
        Some(t) => format!("{t}/bin:{toolchain}/bin:/usr/bin:/bin"),
        None => format!("{toolchain}/bin:/usr/bin:/bin"),
    };
    let load_path = format!("@:{}:@v#.#:@stdlib", s.project_dir);
    push(&["--setenv", "HOME", "/run/palette/home", "--setenv", "JULIA_DEPOT_PATH", s.julia_depot,
           "--setenv", "JULIA_PROJECT", s.project_dir,
           // The session project stays loadable after turn code activates another.
           "--setenv", "JULIA_LOAD_PATH", &load_path,
           "--setenv", "PALETTE_REPO_DIR", s.repo_dir, "--setenv", "PATH", &path,
           "--setenv", "LANG", "en_US.UTF-8", "--setenv", "USER", SANDBOX_USER, "--setenv", "LOGNAME", SANDBOX_USER,
           "--chdir", workspace]);
    for (k, v) in &task_env {
        push(&["--setenv", k, v]);
    }
    if let Some(d) = broker_socket_dir {
        let sock = format!("{d}/broker.sock");
        push(&["--setenv", "PALETTE_BROKER_SOCKET", &sock]);
    }
    if !s.network_enabled {
        // Without it Pkg.add spends ~18 s per package on DNS retries.
        push(&["--setenv", "JULIA_PKG_OFFLINE", "true"]);
    }
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn dir(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn case_boundary_native_001_rejects_nested_and_symlink_overlaps() {
        let root = util::mkdtemp("palette-mount-boundary-").unwrap();
        let repo = dir(&root, "repo");
        let workspace = dir(&repo, "workspace");
        let project_parent = dir(&root, "project-parent");
        let project = dir(&project_parent, "project");
        let alias = root.join("repo-alias");
        symlink(&repo, &alias).unwrap();
        let repo = canonical_dir("repository", &repo).unwrap();
        let workspace = canonical_dir("workspace", &workspace).unwrap();
        let state = canonical_dir("state", &alias).unwrap();
        let project_parent = canonical_dir("state parent", &project_parent).unwrap();
        let project = canonical_dir("project", &project).unwrap();
        assert!(check_writable_mounts_disjoint(&[("workspace", &workspace)], &[("repository", &repo)]).is_err());
        assert!(check_writable_mounts_disjoint(&[("state", &project_parent)], &[("project", &project)]).is_err());
        assert!(check_writable_mounts_disjoint(&[("state", &state)], &[("repository", &repo)]).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn case_boundary_native_002_allows_disjoint_mounts_and_tmp_children() {
        let root = util::mkdtemp("palette-mount-boundary-").unwrap();
        let workspace = canonical_dir("workspace", &dir(&root, "workspace")).unwrap();
        let state = canonical_dir("state", &dir(&root, "state")).unwrap();
        let repo = canonical_dir("repository", &dir(&root, "repo")).unwrap();
        let project = canonical_dir("project", &dir(&root, "project")).unwrap();
        let toolchain = canonical_dir("toolchain", &dir(&root, "toolchain")).unwrap();
        let depot = canonical_dir("shared depot", &dir(&root, "depot")).unwrap();
        let clone = canonical_dir("private clone", &dir(&root, "clone")).unwrap();
        let broker = canonical_dir("broker", &dir(&root, "broker")).unwrap();
        let tools = canonical_dir("task tools", &dir(&root, "tools")).unwrap();
        let usr = canonical_dir("/usr", Path::new("/usr")).unwrap();
        let lib64 = canonical_dir("/lib64", Path::new("/lib64")).unwrap();
        let etc = canonical_dir("/etc", Path::new("/etc")).unwrap();
        let proc = canonical_dir("/proc", Path::new("/proc")).unwrap();
        let dev = canonical_dir("/dev", Path::new("/dev")).unwrap();
        let protected = [("repo", repo.as_path()), ("project", project.as_path()), ("toolchain", toolchain.as_path()), ("shared depot", depot.as_path()),
            ("broker", broker.as_path()), ("task tools", tools.as_path()), ("private clone", clone.as_path()),
            ("/usr", usr.as_path()), ("/lib64", lib64.as_path()), ("/etc", etc.as_path()),
            ("/proc", proc.as_path()), ("/dev", dev.as_path()), ("/run/palette", Path::new("/run/palette"))];
        let writable = [("workspace", workspace.as_path()), ("state", state.as_path())];
        let tmp = canonical_dir("/tmp", Path::new("/tmp")).unwrap();
        let run = canonical_dir("/run", Path::new("/run")).unwrap();
        assert!(check_writable_mounts_disjoint(&writable, &protected).is_ok());
        for (name, path) in &protected {
            assert!(check_writable_mounts_disjoint(&[("workspace", *path)], &[(*name, *path)]).is_err());
        }
        assert!(check_writable_roots_not_masked(&writable, &[("/tmp", &tmp), ("/run", &run)]).is_ok());
        assert!(clone != depot); // The private clone remains the valid source for the depot guest mount.
        let clone_ancestor = canonical_dir("clone ancestor", &root).unwrap();
        assert!(check_writable_mounts_disjoint(&[("workspace", clone_ancestor.as_path())], &[("private clone", clone.as_path())]).is_err());
        assert!(check_writable_roots_not_masked(&[("workspace", Path::new("/tmp"))], &[("/tmp", &tmp)]).is_err());
        assert!(check_writable_roots_not_masked(&[("workspace", Path::new("/run"))], &[("/run", &run)]).is_err());
        assert!(check_writable_roots_not_masked(&[("workspace", Path::new("/"))], &[("/tmp", &tmp), ("/run", &run)]).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_roots_parse_and_refuse_relative_or_missing() {
        let root = util::mkdtemp("palette-read-roots-").unwrap();
        let a = canonical_dir("a", &dir(&root, "a")).unwrap();
        let b = canonical_dir("b", &dir(&root, "b")).unwrap();
        let spec = format!("{}::{}", a.display(), b.display());
        assert_eq!(parse_read_roots(Some(&spec)).unwrap(), vec![a.clone(), b]);
        assert!(parse_read_roots(None).unwrap().is_empty());
        assert!(parse_read_roots(Some("relative/dir")).is_err());
        assert!(parse_read_roots(Some(&root.join("missing").to_string_lossy())).is_err());
        let file = root.join("file");
        fs::write(&file, "x").unwrap();
        assert!(parse_read_roots(Some(&file.to_string_lossy())).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_roots_never_contain_a_mount_or_sit_in_a_writable_one() {
        let root = util::mkdtemp("palette-read-roots-").unwrap();
        let data = canonical_dir("data", &dir(&root, "data")).unwrap();
        let workspace = canonical_dir("workspace", &dir(&root, "ws")).unwrap();
        let depot = canonical_dir("depot", &dir(&root, "home/depot")).unwrap();
        let home = canonical_dir("home", &root.join("home")).unwrap();
        let writable = [("workspace", workspace.as_path())];
        let others = [("shared Julia depot", depot.as_path()), ("system /tmp", Path::new("/tmp")),
                      ("system /usr", Path::new("/usr"))];
        // Disjoint from every mount: accepted.
        assert!(check_read_roots(&[data.clone()], &writable, &others).is_ok());
        // Containing a mount (the depot, a system root, the workspace): refused.
        assert!(check_read_roots(&[home], &writable, &others).unwrap_err().contains("contains the shared Julia depot"));
        assert!(check_read_roots(&[PathBuf::from("/")], &writable, &others).is_err());
        assert!(check_read_roots(&[root.clone()], &writable, &others).unwrap_err().contains("contains"));
        // Inside the writable workspace: refused.
        let inner = canonical_dir("inner", &dir(&workspace, "inner")).unwrap();
        assert!(check_read_roots(&[inner], &writable, &others).unwrap_err().contains("inside the writable workspace"));
        let _ = fs::remove_dir_all(root);
    }
}

pub struct WorkerResult {
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Run a child process to completion or `timeout`, capturing its output
/// (invalid UTF-8 replaced). A timed-out child is killed.
pub fn run_captured(argv: &[String], env: Option<&BTreeMap<String, Option<String>>>, timeout: Duration)
    -> Result<WorkerResult, String> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(env) = env {
        for (k, v) in env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("starting {}: {e}", argv[0]))?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().map_err(|e| e.to_string())?;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
    use std::os::unix::process::ExitStatusExt;
    let returncode = status.code().unwrap_or_else(|| -status.signal().unwrap_or(0));
    Ok(WorkerResult { returncode, stdout, stderr, timed_out })
}

/// Keeps only the last `cap` bytes a pipe produces: a renderer may print for
/// minutes, and only the end of its output is reported.
fn read_tail(mut r: impl Read + Send + 'static, cap: usize) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let (mut kept, mut buf) = (Vec::new(), [0u8; 8192]);
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&buf[..n]);
            if kept.len() > 2 * cap {
                kept.drain(..kept.len() - cap);
            }
        }
        if kept.len() > cap {
            kept.drain(..kept.len() - cap);
        }
        kept
    })
}

/// An operator-named host command (the broker's host_command), started in a
/// process group of its own. A watchdog kills the whole group at the
/// deadline whether or not anyone is waiting, so helper processes a
/// renderer starts never outlive it; dropping the job kills it too. The
/// group is always signalled before the command is reaped, under one lock
/// with the watchdog, so a signal can never reach a reused pid.
pub struct HostJob {
    child: std::process::Child,
    out: Option<std::thread::JoinHandle<Vec<u8>>>,
    err: Option<std::thread::JoinHandle<Vec<u8>>>,
    started: Instant,
    // (finished, timed_out)
    state: std::sync::Arc<std::sync::Mutex<(bool, bool)>>,
}

pub struct HostJobResult {
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub seconds: f64,
}

fn kill_group(pgid: i32) {
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

/// Whether `pid` has exited, without reaping it (WNOWAIT).
fn exited_unreaped(pid: i32, block: bool) -> Result<bool, String> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOWAIT | if block { 0 } else { libc::WNOHANG };
    loop {
        let rc = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) };
        if rc == 0 {
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("waitid: {e}"));
        }
    }
}

impl HostJob {
    pub fn start(argv: &[String], cwd: &Path, timeout: Duration, output_cap: usize) -> Result<HostJob, String> {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).current_dir(cwd).process_group(0)
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("starting {}: {e}", argv[0]))?;
        let out = read_tail(child.stdout.take().unwrap(), output_cap);
        let err = read_tail(child.stderr.take().unwrap(), output_cap);
        let state = std::sync::Arc::new(std::sync::Mutex::new((false, false)));
        let (pgid, watched) = (child.id() as i32, state.clone());
        let deadline = Instant::now() + timeout;
        std::thread::spawn(move || loop {
            {
                let mut st = watched.lock().unwrap();
                if st.0 {
                    return;
                }
                if Instant::now() >= deadline {
                    st.1 = true;
                    kill_group(pgid);
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        });
        Ok(HostJob { child, out: Some(out), err: Some(err), started: Instant::now(), state })
    }

    /// The result once the command has exited, `None` while it runs.
    pub fn poll(&mut self) -> Result<Option<HostJobResult>, String> {
        if !exited_unreaped(self.child.id() as i32, false)? {
            return Ok(None);
        }
        self.finish().map(Some)
    }

    pub fn wait(&mut self) -> Result<HostJobResult, String> {
        exited_unreaped(self.child.id() as i32, true)?;
        self.finish()
    }

    fn finish(&mut self) -> Result<HostJobResult, String> {
        use std::os::unix::process::ExitStatusExt;
        let timed_out = {
            let mut st = self.state.lock().unwrap();
            st.0 = true;
            // Helpers left in the group after the command exited go too:
            // their open pipes would otherwise keep the output readers waiting.
            kill_group(self.child.id() as i32);
            st.1
        };
        let status = self.child.wait().map_err(|e| e.to_string())?;
        let text = |h: Option<std::thread::JoinHandle<Vec<u8>>>| {
            String::from_utf8_lossy(&h.map(|h| h.join().unwrap_or_default()).unwrap_or_default()).into_owned()
        };
        Ok(HostJobResult {
            returncode: status.code().unwrap_or_else(|| -status.signal().unwrap_or(0)),
            stdout: text(self.out.take()),
            stderr: text(self.err.take()),
            timed_out,
            seconds: self.started.elapsed().as_secs_f64(),
        })
    }
}

impl Drop for HostJob {
    fn drop(&mut self) {
        let mut st = self.state.lock().unwrap();
        if !st.0 {
            st.0 = true;
            kill_group(self.child.id() as i32);
            drop(st);
            let _ = self.child.wait();
        }
    }
}

pub struct RunWorker<'a> {
    pub workspace_dir: &'a str,
    pub project_dir: &'a str,
    pub repo_dir: &'a str,
    pub script: &'a str,
    pub broker_socket_dir: Option<&'a str>,
    pub network_enabled: bool,
    pub depot_clone_dir: Option<&'a str>,
    pub timeout: Duration,
}

/// One sandboxed `julia -e script`. A caller that owns a session-lifetime
/// depot clone passes it in; otherwise a fresh clone is made and removed.
pub fn run_worker(r: &RunWorker) -> Result<WorkerResult, String> {
    let julia = resolve_julia()?;
    let depot = default_depot();
    fs::create_dir_all(r.workspace_dir).map_err(|e| e.to_string())?;
    let owned = match r.depot_clone_dir {
        Some(_) => None,
        None => Some(create_session_depot(Some(&depot))?),
    };
    let clone = owned.as_ref().map(|p| p.to_string_lossy().into_owned());
    let result = (|| {
        let mut argv = build_bwrap_argv(&SandboxSpec {
            workspace_dir: r.workspace_dir,
            broker_socket_dir: r.broker_socket_dir,
            project_dir: r.project_dir,
            repo_dir: r.repo_dir,
            julia_bin: &julia,
            julia_depot: &depot,
            network_enabled: r.network_enabled,
            depot_clone_dir: r.depot_clone_dir.or(clone.as_deref()),
            state_dir: None,
        })?;
        argv.extend(["--".into(), julia.clone(), "--startup-file=no".into(), "-e".into(), r.script.into()]);
        run_captured(&argv, None, r.timeout)
    })();
    if let Some(p) = owned {
        let _ = fs::remove_dir_all(p);
    }
    result
}
