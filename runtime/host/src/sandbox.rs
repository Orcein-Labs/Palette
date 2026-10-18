//! The first trust domain: a bubblewrap sandbox around an unrestricted
//! `julia`. Full language power inside, bounded by the OS outside it. Paths
//! are bound at the same guest path as on the host, because Julia's manifests
//! and precompile caches embed absolute paths. Each worker gets its own
//! reflink clone of the prepared depot as its first writable entry. A later
//! read-only operator package depot exposes broker-installed packages; the
//! full prepared clone remains first so Julia's cached stdlib build ids match.

use crate::receipt_archive;
use crate::util;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

pub const SANDBOX_USER: &str = "palette";
/// The sandbox sets these itself; a task environment cannot move them.
const SANDBOX_OWNED_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "JULIA_DEPOT_PATH", "JULIA_PROJECT", "JULIA_LOAD_PATH",
    "JULIA_PKG_OFFLINE", "PALETTE_PACKAGE_ENVIRONMENT", "PALETTE_REPO_DIR", "PALETTE_STATE_DIR", "PALETTE_BROKER_SOCKET",
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

fn primary_depot(value: Option<&str>, fallback: &Path) -> String {
    value
        .and_then(|v| v.split(':').next())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string_lossy().into_owned())
}

pub fn default_depot() -> String {
    let configured = std::env::var("JULIA_DEPOT_PATH").ok();
    primary_depot(configured.as_deref(), &util::home().join(".julia"))
}

/// Build the isolated command for a broker-owned Pkg operation. Julia package
/// installation executes package build scripts, so environment scrubbing
/// alone is not an authority boundary. Only the durable package store is
/// writable; the Julia runtime and prepared cache inputs are read-only, and
/// HOME/TMPDIR are private namespace filesystems.
pub fn package_installer_argv(
    julia: &str, package_store: &Path, package_depot: &Path,
    package_environment: &Path, base_depot: &Path, offline: bool, script: &str,
    protected_paths: &[PathBuf],
) -> Result<Vec<String>, String> {
    if package_store.is_symlink() || package_depot.is_symlink() || package_environment.is_symlink() {
        return Err("package store, depot, and environment paths must not be symlinks".into());
    }
    let julia = fs::canonicalize(julia).map_err(|e| format!("resolving Julia binary: {e}"))?;
    let toolchain = julia.parent().and_then(Path::parent).ok_or("Julia binary has no toolchain root")?;
    let store = fs::canonicalize(package_store).map_err(|e| format!("resolving package store: {e}"))?;
    let depot = fs::canonicalize(package_depot).map_err(|e| format!("resolving package depot: {e}"))?;
    let environment = fs::canonicalize(package_environment).map_err(|e| format!("resolving package environment: {e}"))?;
    let sources = store.join("sources");
    if fs::symlink_metadata(&sources).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
        return Err("package store sources must be a real directory contained in the store".into());
    }
    if sources.exists() {
        if !sources.is_dir() || !fs::canonicalize(&sources).map_err(|e| e.to_string())?.starts_with(&store) {
            return Err("package store sources must be a real directory contained in the store".into());
        }
    }
    reject_symlink_tree(&sources, "package store sources", true)?;
    let base = fs::canonicalize(base_depot).map_err(|e| format!("resolving Julia base depot: {e}"))?;
    if depot == store || environment == store || !depot.starts_with(&store) || !environment.starts_with(&store) {
        return Err("package depot and environment must be inside the durable package store".into());
    }
    if store == base || store.starts_with(&base) || base.starts_with(&store) {
        return Err("package store and Julia base depot must be disjoint".into());
    }
    if store == toolchain || store.starts_with(toolchain) || toolchain.starts_with(&store) {
        return Err("package store and Julia toolchain must be disjoint".into());
    }
    for protected in protected_paths {
        let path = fs::canonicalize(protected).unwrap_or_else(|_| protected.to_path_buf());
        if store == path || store.starts_with(&path) || path.starts_with(&store) {
            return Err(format!("package store must be disjoint from protected host path {}", path.display()));
        }
    }
    for system in ["/usr", "/lib64", "/etc", "/proc", "/dev", "/run"] {
        let path = Path::new(system);
        if store == path || store.starts_with(path) || path.starts_with(&store) {
            return Err(format!("package store must be disjoint from {system}"));
        }
    }
    let bwrap = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).map(|dir| dir.join("bwrap")).find(|p| p.is_file())
    }).ok_or("bubblewrap (bwrap) is required for package installation")?;

    let mut args = vec![
        bwrap.to_string_lossy().into_owned(),
        "--unshare-user".into(), "--uid".into(), "1000".into(), "--gid".into(), "1000".into(),
        "--disable-userns".into(), "--assert-userns-disabled".into(),
        "--unshare-pid".into(), "--unshare-ipc".into(),
    ];
    if offline { args.push("--unshare-net".into()); }
    args.extend([
        "--proc".into(), "/proc".into(), "--dev".into(), "/dev".into(),
        "--tmpfs".into(), "/tmp".into(), "--dir".into(), "/etc".into(),
        "--dir".into(), "/run/palette".into(), "--tmpfs".into(), "/run/palette".into(),
        "--dir".into(), "/run/palette/home".into(),
        "--clearenv".into(), "--die-with-parent".into(), "--new-session".into(), "--cap-drop".into(), "ALL".into(),
        "--ro-bind".into(), "/usr".into(), "/usr".into(), "--symlink".into(), "usr/lib".into(), "/lib".into(),
        "--ro-bind".into(), "/lib64".into(), "/lib64".into(), "--symlink".into(), "usr/bin".into(), "/bin".into(),
        "--ro-bind".into(), "/etc/ld.so.cache".into(), "/etc/ld.so.cache".into(),
    ]);

    let mut guest_dirs = BTreeSet::<String>::new();
    let mut add_parents = |target: &Path| {
        for parent in target.ancestors().filter(|p| *p != Path::new("/")) {
            let value = parent.to_string_lossy().into_owned();
            if ["/usr", "/lib64", "/etc", "/proc", "/dev", "/tmp", "/run"].contains(&value.as_str()) {
                continue;
            }
            guest_dirs.insert(value);
        }
    };
    add_parents(toolchain);
    add_parents(&store);
    add_parents(&depot);
    add_parents(&environment);
    for name in ["compiled", "packages", "artifacts"] {
        add_parents(&base.join(name));
    }
    let mut guest_dirs: Vec<_> = guest_dirs.into_iter().collect();
    guest_dirs.sort_by_key(|value| (Path::new(value).components().count(), value.clone()));
    for directory in guest_dirs {
        args.extend(["--dir".into(), directory]);
    }

    let toolchain = toolchain.to_string_lossy().into_owned();
    let store_s = store.to_string_lossy().into_owned();
    args.extend(["--ro-bind".into(), toolchain.clone(), toolchain.clone(),
                 "--ro-bind".into(), store_s.clone(), store_s]);
    for name in ["compiled", "packages", "artifacts"] {
        let source = base.join(name);
        if source.is_dir() {
            let value = source.to_string_lossy().into_owned();
            args.extend(["--ro-bind".into(), value.clone(), value]);
        }
    }
    let depot_s = depot.to_string_lossy().into_owned();
    let environment_s = environment.to_string_lossy().into_owned();
    args.extend(["--bind".into(), depot_s.clone(), depot_s,
                 "--bind".into(), environment_s.clone(), environment_s]);
    let etc = identity_files()?;
    for (source, target) in [(etc.join("passwd"), "/etc/passwd"), (etc.join("group"), "/etc/group")] {
        args.extend(["--ro-bind".into(), source.to_string_lossy().into_owned(), target.into()]);
    }
    if !offline {
        for (source, target) in [
            ("/etc/hosts", "/etc/hosts"), ("/etc/resolv.conf", "/etc/resolv.conf"),
            ("/etc/nsswitch.conf", "/etc/nsswitch.conf"), ("/etc/ssl/certs", "/etc/ssl/certs"),
        ] {
            if Path::new(source).exists() {
                args.extend(["--ro-bind".into(), source.into(), target.into()]);
            }
        }
    }
    let depot_path = format!("{}:{}", depot.display(), base.display());
    args.extend([
        "--setenv".into(), "HOME".into(), "/run/palette/home".into(),
        "--setenv".into(), "TMPDIR".into(), "/tmp".into(),
        "--setenv".into(), "PATH".into(), format!("{toolchain}/bin:/usr/bin:/bin"),
        "--setenv".into(), "LANG".into(), "C.UTF-8".into(),
        "--setenv".into(), "JULIA_DEPOT_PATH".into(), depot_path,
        "--setenv".into(), "JULIA_PROJECT".into(), environment.to_string_lossy().into_owned(),
        "--setenv".into(), "JULIA_PKG_PRECOMPILE_AUTO".into(), "0".into(),
    ]);
    if offline { args.extend(["--setenv".into(), "JULIA_PKG_OFFLINE".into(), "true".into()]); }
    args.extend([
        "--chdir".into(), environment.to_string_lossy().into_owned(), "--".into(),
        julia.to_string_lossy().into_owned(), "--startup-file=no".into(), "-e".into(), script.into(),
    ]);
    Ok(args)
}

#[derive(Debug)]
pub struct PackageStore {
    pub root: PathBuf,
    pub depot: PathBuf,
    pub environment: PathBuf,
}

fn reject_symlink_tree(path: &Path, label: &str, allow_contained: bool) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let root = if metadata.is_dir() { fs::canonicalize(path).map_err(|e| e.to_string())? }
        else { fs::canonicalize(path.parent().ok_or("path has no parent")?).map_err(|e| e.to_string())? };
    fn walk(path: &Path, root: &Path, label: &str, allow_contained: bool) -> Result<(), String> {
        let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() {
            let target = fs::canonicalize(path).map_err(|e| e.to_string())?;
            if !allow_contained || (target != root && !target.starts_with(root)) {
                return Err(format!("{label} must not contain escaping symlinks: {}", path.display()));
            }
            return Ok(());
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
                walk(&entry.map_err(|e| e.to_string())?.path(), root, label, allow_contained)?;
            }
        } else if !metadata.is_file() {
            return Err(format!("{label} contains a non-regular filesystem entry: {}", path.display()));
        }
        Ok(())
    }
    walk(path, &root, label, allow_contained)
}

fn configured_ceiling_paths() -> Result<Option<serde_json::Value>, String> {
    let mut ceiling = if let Ok(config) = std::env::var("PALETTE_CAPABILITY_CEILING") {
        let text = if Path::new(&config).is_file() { fs::read_to_string(&config).map_err(|e| e.to_string())? } else { config };
        Some(serde_json::from_str::<serde_json::Value>(&text).map_err(|e| format!("operator capability ceiling: {e}"))?)
    } else { None };
    if let Ok(path) = std::env::var("PALETTE_HOST_COMMANDS") {
        let legacy: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("operator host commands: {e}"))?;
        let commands = legacy.as_object().ok_or("PALETTE_HOST_COMMANDS must contain an object")?;
        let value = ceiling.get_or_insert_with(|| serde_json::json!({}));
        let object = value.as_object_mut().ok_or("operator capability ceiling must be an object")?;
        let host = object.entry("host_command").or_insert_with(|| serde_json::json!({}));
        let host = host.as_object_mut().ok_or("host_command grant must be an object")?;
        let entries = host.entry("commands").or_insert_with(|| serde_json::json!({}));
        let entries = entries.as_object_mut().ok_or("host_command.commands must be an object")?;
        for (name, spec) in commands {
            if entries.get(name).is_some_and(|old| old != spec) {
                return Err(format!("PALETTE_HOST_COMMANDS conflicts with host_command.commands entry {name:?}"));
            }
            entries.insert(name.clone(), spec.clone());
        }
    }
    Ok(ceiling)
}

/// Acquire the host-owned lock shared by store initialization and all Pkg
/// mutations. Dropping the returned file releases the advisory lock.
pub fn package_store_lock(package_depot: &str) -> Result<File, String> {
    use std::os::fd::AsRawFd;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let root = Path::new(package_depot).parent().ok_or("package depot has no store root")?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(root.join(".package-store.lock")).map_err(|e| e.to_string())?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(format!("locking package store: {}", std::io::Error::last_os_error()));
    }
    Ok(file)
}

/// Operator-owned host-command paths, taken from the frozen operator ceiling
/// only (runtime worker argv is never passed here). A writable root may not
/// overlap a command's cwd, program, path arguments or lock file, and may
/// not be or contain the directory that holds one, where it could swap that
/// entry for a symlink. A root beside one, inside the same directory, is fine.
#[derive(Default)]
pub struct CommandPaths {
    pub paths: Vec<PathBuf>,
    pub parents: Vec<PathBuf>,
}

pub fn protected_command_paths(ceiling: &serde_json::Value) -> CommandPaths {
    let mut out = CommandPaths::default();
    let Some(commands) = ceiling.get("host_command").and_then(|v| v.get("commands")).and_then(serde_json::Value::as_object) else {
        return out;
    };
    let mut add = |path: &Path| {
        if path.is_absolute() {
            out.paths.push(path.to_path_buf());
            if let Some(parent) = path.parent() { out.parents.push(parent.to_path_buf()); }
        }
    };
    for spec in commands.values() {
        for key in ["cwd", "lock"] {
            if let Some(path) = spec.get(key).and_then(serde_json::Value::as_str) { add(Path::new(path)); }
        }
        let Some(argv) = spec.get("argv").and_then(serde_json::Value::as_array) else { continue; };
        let mut tokens = Vec::<String>::new();
        let mut i = 0;
        while i < argv.len() {
            let Some(item) = argv[i].as_str() else { i += 1; continue; };
            if item == "--fixed-arg" {
                if let Some(value) = argv.get(i + 1).and_then(serde_json::Value::as_str) { tokens.push(value.to_string()); i += 2; continue; }
            } else if let Some(value) = item.strip_prefix("--fixed-arg=") {
                tokens.push(value.to_string()); i += 1; continue;
            }
            tokens.push(item.to_string()); i += 1;
        }
        for token in tokens {
            add(Path::new(&token));
            if let Some((_, value)) = token.split_once('=') { add(Path::new(value)); }
        }
    }
    out
}

/// The first protected path that `root` equals, sits inside, or contains.
/// An empty path names nothing and is skipped: resolving it would yield the
/// current directory, which would make the check depend on where the host
/// was started from.
pub(crate) fn protected_overlap(root: &Path, protected: &[PathBuf]) -> Option<PathBuf> {
    protected.iter().filter(|c| !c.as_os_str().is_empty()).map(|c| util::resolve(c))
        .find(|c| root == c || root.starts_with(c) || c.starts_with(root))
}

/// The first host-command path `root` overlaps, or command directory it is
/// or contains (see [`CommandPaths`]).
pub(crate) fn command_overlap(root: &Path, commands: &CommandPaths) -> Option<PathBuf> {
    protected_overlap(root, &commands.paths).or_else(|| {
        commands.parents.iter().map(|c| util::resolve(c)).find(|c| root == c || c.starts_with(root))
    })
}

/// Persistent broker-written packages live outside sessions and workspaces.
/// Workers receive this tree read-only and keep their writable prewarmed depot
/// first in `JULIA_DEPOT_PATH` for compiled caches.
pub fn package_store_paths(protected_paths: &[PathBuf], ceiling: Option<&serde_json::Value>) -> Result<PackageStore, String> {
    let root = match std::env::var("PALETTE_PACKAGE_DEPOT") {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => util::home().join(".palette").join("packages"),
    };
    let configured_ceiling = if ceiling.is_none() { configured_ceiling_paths()? } else { None };
    package_store_paths_at(root, protected_paths, ceiling.or(configured_ceiling.as_ref()))
}

fn package_store_paths_at(root: PathBuf, protected_paths: &[PathBuf], ceiling: Option<&serde_json::Value>) -> Result<PackageStore, String> {
    if !root.is_absolute() {
        return Err("PALETTE_PACKAGE_DEPOT must be an absolute path".into());
    }
    let lexical_root = util::resolve(&root);
    let depot_path = lexical_root.join("depot");
    let environment_path = lexical_root.join("environment");
    let lock_path = lexical_root.join(".package-store.lock");
    let marker = lexical_root.join(".seeded");
    let sources_path = lexical_root.join("sources");
    for path in [&root, &depot_path, &environment_path, &sources_path, &depot_path.join("registries"),
        &depot_path.join("packages"), &lock_path, &marker] {
        if fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return Err(format!("package store path must not be a symlink: {}", path.display()));
        }
    }
    reject_symlink_tree(&sources_path, "package store sources", true)?;
    for tree in [&depot_path.join("registries"), &depot_path.join("packages")] {
        reject_symlink_tree(tree, "package store directory", true)?;
    }
    let mut protected = protected_paths.to_vec();
    // The checkout in use, found at run time. A compile-time path would name
    // the build machine's checkout and leave a relocated install unprotected.
    protected.extend(util::installed_checkout());
    protected.extend(parse_read_roots(std::env::var("PALETTE_READ_ROOTS").ok().as_deref())?);
    for key in ["PALETTE_CAPABILITY_CEILING", "PALETTE_HOST_COMMANDS", "PALETTE_REPAIR_CONFIG", "PALETTE_RUNTIME_ROOT", "PALETTE_REPO_DIR"] {
        if let Ok(path) = std::env::var(key) { if !path.is_empty() { protected.push(PathBuf::from(path)); } }
    }
    let commands = ceiling.map(protected_command_paths).unwrap_or_default();
    let root = util::resolve(&lexical_root);
    if let Some(candidate) = protected_overlap(&root, &protected).or_else(|| command_overlap(&root, &commands)) {
        return Err(format!("PALETTE_PACKAGE_DEPOT overlaps protected host path: {}", candidate.display()));
    }
    let base_depot = fs::canonicalize(default_depot()).map_err(|e| format!("resolving Julia depot: {e}"))?;
    if root == base_depot || root.starts_with(&base_depot) || base_depot.starts_with(&root) {
        return Err("PALETTE_PACKAGE_DEPOT must be disjoint from the Julia base depot".into());
    }
    for system in ["/usr", "/etc", "/dev", "/proc", "/run", "/lib64"] {
        let system = Path::new(system);
        if root == system || root.starts_with(system) || system.starts_with(&root) {
            return Err(format!("PALETTE_PACKAGE_DEPOT must be disjoint from {}", system.display()));
        }
    }
    let tmp = Path::new("/tmp");
    if root == tmp || tmp.starts_with(&root) {
        return Err("PALETTE_PACKAGE_DEPOT must not contain the host /tmp mount".into());
    }
    let julia_toolchain = PathBuf::from(resolve_julia()?).canonicalize().map_err(|e| e.to_string())?;
    let julia_toolchain = julia_toolchain.parent().and_then(Path::parent).ok_or("Julia binary has no toolchain root")?;
    if root == julia_toolchain || root.starts_with(julia_toolchain) || julia_toolchain.starts_with(&root) {
        return Err("PALETTE_PACKAGE_DEPOT must be disjoint from the Julia toolchain".into());
    }
    let seed_needed = !marker.exists() && base_depot.is_dir()
        && fs::read_dir(&base_depot).map_err(|e| e.to_string())?.next().is_some();
    if seed_needed {
        for directory in ["registries", "packages"] {
            let source = base_depot.join(directory);
            let destination = depot_path.join(directory);
            if source.is_dir() {
                reject_symlink_tree(&source, "prepared package source", true)?;
                reject_symlink_tree(&destination, "package seed destination", false)?;
            }
        }
    }
    // The package path and all existing children passed symlink and
    // authority checks before the first filesystem mutation.
    fs::create_dir_all(&root).map_err(|e| format!("creating {}: {e}", root.display()))?;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    for path in [&depot_path, &environment_path] {
        fs::create_dir_all(path).map_err(|e| format!("creating {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        }
    }
    let depot = fs::canonicalize(depot_path).map_err(|e| e.to_string())?;
    let environment = fs::canonicalize(environment_path).map_err(|e| e.to_string())?;
    if !depot.starts_with(&root) || !environment.starts_with(&root) {
        return Err("package depot and environment must remain inside PALETTE_PACKAGE_DEPOT".into());
    }
    // Copy registry metadata and package sources only. Prepared artifacts and
    // compiled caches can be multi-gigabyte; source-only packages remain
    // provisionable offline, while packages needing absent artifacts fail
    // under the broker's offline Pkg policy.
    let lock = package_store_lock(&depot.to_string_lossy())?;
    if seed_needed && !marker.exists() {
        for directory in ["registries", "packages"] {
            let source = base_depot.join(directory);
            if !source.is_dir() { continue; }
            let destination = depot.join(directory);
            reject_symlink_tree(&destination, "package seed destination", false)?;
            fs::create_dir_all(&destination).map_err(|e| e.to_string())?;
            let status = Command::new("cp").args(["-a", "--reflink=auto"])
                .arg(format!("{}/.", source.display())).arg(&destination)
                .stdin(Stdio::null()).output().map_err(|e| format!("seeding {directory}: {e}"))?;
            if !status.status.success() {
                let stderr = String::from_utf8_lossy(&status.stderr);
                let tail = stderr.chars().rev().take(2000).collect::<String>().chars().rev().collect::<String>();
                return Err(format!("seeding package {directory} failed: {tail}"));
            }
        }
        fs::write(&marker, "seeded registries and package sources from the configured Julia depot\n").map_err(|e| e.to_string())?;
    }
    drop(lock);
    Ok(PackageStore { root, depot, environment })
}

/// Every directory this host owns outside its own scratch root -- a depot
/// clone `depot-XXXXXXXX`, a default session root `palette-<id>`, a default
/// receipts directory -- has a sidecar `<dir>.owner` naming the process that
/// made it as "<pid namespace> <pid> <start time>". Owners remove their
/// directory when they finish; the sidecar lets a later session remove the
/// directories of owners that were killed first. The Python launcher writes
/// the same record.
const CLONE_OWNER_SUFFIX: &str = ".owner";

pub(crate) fn clone_owner_path(clone: &Path) -> PathBuf {
    let mut path = clone.as_os_str().to_owned();
    path.push(CLONE_OWNER_SUFFIX);
    PathBuf::from(path)
}

/// Field 22 of /proc/<pid>/stat, counted after the parenthesized command name.
fn process_start_time(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(19).map(str::to_string)
}

fn pid_namespace() -> Option<String> {
    fs::read_link("/proc/self/ns/pid").ok().map(|p| p.to_string_lossy().into_owned())
}

pub(crate) fn clone_owner_record() -> Option<String> {
    let pid = std::process::id();
    Some(format!("{} {pid} {}\n", pid_namespace()?, process_start_time(pid)?))
}

/// False only for a well-formed record from this pid namespace whose process
/// has exited or whose pid now belongs to a different process.
fn clone_owner_alive(record: &str) -> bool {
    let fields: Vec<&str> = record.split_whitespace().collect();
    let [namespace, pid, start] = fields[..] else { return true };
    let Ok(pid) = pid.parse::<u32>() else { return true };
    if pid_namespace().as_deref() != Some(namespace) {
        return true;
    }
    process_start_time(pid).as_deref() == Some(start)
}

/// Writes this process's owner record for `dir`.
pub(crate) fn record_owner(dir: &Path) -> Result<(), String> {
    let Some(record) = clone_owner_record() else { return Ok(()) };
    fs::write(clone_owner_path(dir), record).map_err(|e| format!("recording the owner of {}: {e}", dir.display()))
}

/// The newest modification time of `dir` and its direct entries.
pub(crate) fn last_modified(dir: &Path) -> Option<SystemTime> {
    let mut newest = fs::symlink_metadata(dir).ok()?.modified().ok()?;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        if let Some(t) = entry.metadata().ok().and_then(|m| m.modified().ok()) {
            newest = newest.max(t);
        }
    }
    Some(newest)
}

/// Removes the directories `<prefix>*` in `root` whose owner has died and
/// that nobody has written to for at least `min_age`. A directory without a
/// readable owner record, or one that is not a plain directory, is left alone.
pub(crate) fn sweep_orphaned_dirs(root: &Path, prefix: &str, min_age: Duration) {
    for dir in expired_orphaned_dirs(root, prefix, min_age) {
        remove_owned_dir(&dir);
    }
}

/// The directories `sweep_orphaned_dirs` would remove. Owner records whose
/// directory is already gone are removed here.
pub(crate) fn expired_orphaned_dirs(root: &Path, prefix: &str, min_age: Duration) -> Vec<PathBuf> {
    let mut expired = Vec::new();
    let Ok(entries) = fs::read_dir(root) else { return expired };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(dir_name) = name.to_str().and_then(|n| n.strip_suffix(CLONE_OWNER_SUFFIX)) else { continue };
        if !dir_name.starts_with(prefix) || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(record) = fs::read_to_string(entry.path()) else { continue };
        if clone_owner_alive(&record) {
            continue;
        }
        let dir = root.join(dir_name);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {
                let idle = last_modified(&dir).and_then(|t| t.elapsed().ok()).unwrap_or(Duration::ZERO);
                if idle >= min_age {
                    expired.push(dir);
                }
            }
            Ok(_) => {}
            Err(_) => { let _ = fs::remove_file(entry.path()); }
        }
    }
    expired.sort();
    expired
}

/// Removes an owned directory and then its owner record.
pub fn remove_owned_dir(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_file(clone_owner_path(dir));
}

/// Removes a session depot clone and then its owner record.
pub fn remove_session_depot(clone: &Path) {
    remove_owned_dir(clone);
}

/// Where a session's broker receipts go when the caller names no receipts
/// directory: `~/.palette/receipts/<session id>/`, kept after the session
/// closes. `PALETTE_RECEIPT_RETENTION_DAYS` (default 30) sets how long a
/// closed session's receipts stay there before a later session moves them
/// into the monthly archive `~/.palette/docs/receipts/<YYYY-MM>.zip`;
/// 0 restores the old behavior, receipts inside the session scratch root and
/// deleted with it. Returns the root and the retention, or None for 0.
pub fn default_receipts_root() -> Result<Option<(PathBuf, Duration)>, String> {
    let retention = receipt_retention(std::env::var("PALETTE_RECEIPT_RETENTION_DAYS").ok().as_deref())?;
    Ok(retention.map(|r| (util::home().join(".palette").join("receipts"), r)))
}

fn receipt_retention(days: Option<&str>) -> Result<Option<Duration>, String> {
    let days = match days {
        None => DEFAULT_RECEIPT_RETENTION_DAYS,
        Some(v) => Some(v.trim()).filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|t| t.parse::<u64>().ok()).filter(|d| *d <= 36_500)
            .ok_or(format!("PALETTE_RECEIPT_RETENTION_DAYS must be a whole number of days, got {v:?}"))?,
    };
    Ok((days > 0).then(|| Duration::from_secs(days * 86_400)))
}

pub const DEFAULT_RECEIPT_RETENTION_DAYS: u64 = 30;

/// Creates `<root>/<session id>/` for one session's receipts, after
/// archiving the receipts of closed sessions older than `retention`. Receipts
/// that cannot be archived are kept, and a later session tries again.
pub fn create_receipts_dir(root: &Path, session_id: &str, retention: Duration) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(root)
        .map_err(|e| format!("creating {}: {e}", root.display()))?;
    let archive = receipt_archive::archive_dir(root);
    for expired in expired_orphaned_dirs(root, "palette-", retention) {
        if let Err(e) = receipt_archive::retire(&expired, &archive) {
            eprintln!("palette-host: keeping the expired receipts {}: {e}", expired.display());
        }
    }
    let dir = root.join(session_id);
    fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    if let Err(e) = record_owner(&dir) {
        let _ = fs::remove_dir(&dir);
        return Err(e);
    }
    Ok(dir)
}

/// The resolved file a `--ceiling` argument names, or `None` when the argument
/// is the ceiling's JSON text itself.
pub fn ceiling_file(arg: &str) -> Option<PathBuf> {
    let path = Path::new(arg);
    path.is_file().then(|| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// Creates `path` and its parents and makes it a directory only this user can
/// enter. A symlink, or a directory another user owns, is refused. The broker's
/// socket is world-connectable for the sandbox, so the directory around it is
/// what keeps other local users from reaching the broker.
pub(crate) fn private_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    fs::create_dir_all(path).map_err(|e| format!("creating {}: {e}", path.display()))?;
    let meta = fs::symlink_metadata(path).map_err(|e| format!("checking {}: {e}", path.display()))?;
    let uid = unsafe { libc::getuid() };
    if !meta.file_type().is_dir() || meta.uid() != uid {
        return Err(format!("refusing {}: it must be a directory owned by uid {uid}", path.display()));
    }
    if meta.mode() & 0o777 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("restricting {}: {e}", path.display()))?;
    }
    Ok(())
}

/// One private, writable prepared-depot clone for a session's worker caches.
/// Broker-authorized package installs live in the separate durable package store.
/// The clone comes from the newest snapshot of the depot when one exists,
/// otherwise from the live depot. Callers release it with `remove_session_depot`.
pub fn create_session_depot(real_depot: Option<&str>) -> Result<PathBuf, String> {
    let depot = real_depot.map(str::to_string).unwrap_or_else(default_depot);
    let root = Path::new(&depot).parent().unwrap_or(Path::new("/")).join(".palette-depot-clones");
    fs::create_dir_all(&root).map_err(|e| format!("creating {}: {e}", root.display()))?;
    sweep_orphaned_dirs(&root, "depot-", Duration::ZERO);
    let dest = util::mkdtemp_in(&root, "depot-").map_err(|e| e.to_string())?;
    if let Err(e) = record_owner(&dest) {
        remove_session_depot(&dest);
        return Err(e);
    }
    let copied = match hold_depot_snapshot(Path::new(&depot)) {
        // Nothing writes a snapshot, so one attempt copies it or fails for real.
        Ok(Some(snapshot)) => {
            let copied = copy_tree(&snapshot.path, &dest, 1);
            drop(snapshot);
            try_sweep_depot_snapshots(Path::new(&depot));
            copied
        }
        // The live depot may be precompiling. A Julia process removes its
        // temporary files, and one removed between cp listing a directory and
        // copying it fails the copy. A later attempt copies the settled tree.
        Ok(None) => copy_tree(Path::new(&depot), &dest, DEPOT_CLONE_ATTEMPTS),
        Err(e) => Err(e),
    };
    match copied {
        Ok(()) => Ok(dest),
        Err(e) => {
            remove_session_depot(&dest);
            Err(e)
        }
    }
}

/// How many times a live depot clone is tried before the session gives up.
const DEPOT_CLONE_ATTEMPTS: usize = 3;

/// `cp -a --reflink=auto source dest`, tried `attempts` times.
fn copy_tree(source: &Path, dest: &Path, attempts: usize) -> Result<(), String> {
    let mut failure = String::new();
    for attempt in 0..attempts {
        let _ = fs::remove_dir_all(dest); // cp wants a destination that does not exist yet
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        let status = Command::new("cp")
            .args(["-a", "--reflink=auto"])
            .arg(source)
            .arg(dest)
            .stdin(Stdio::null())
            .status();
        match status {
            Ok(status) if status.success() => return Ok(()),
            Ok(_) => failure = format!("cloning the depot {} failed", source.display()),
            Err(e) => {
                failure = format!("cp: {e}");
                break;
            }
        }
    }
    let _ = fs::remove_dir_all(dest);
    Err(failure)
}

/// Snapshots of a prepared depot live in
/// `<depot parent>/.palette-depot-snapshots/<depot name>/snap-<sequence>`.
/// A successful prewarm publishes one; no sandbox binds them, so no Julia
/// process writes them. Session clones copy the newest one while holding a
/// shared flock on its directory, and a snapshot is removed only once it is
/// no longer the newest and an exclusive flock on it succeeds without
/// waiting. Publishing and sweeping hold an exclusive flock on `.lock`.
/// The Python launcher uses the same layout and locks.
const SNAPSHOT_PREFIX: &str = "snap-";

pub fn depot_snapshot_root(depot: &Path) -> Option<PathBuf> {
    let name = depot.file_name()?;
    Some(depot.parent()?.join(".palette-depot-snapshots").join(name))
}

/// A snapshot held open under a shared flock; dropping it releases the hold.
struct HeldSnapshot {
    path: PathBuf,
    _hold: File,
}

fn open_dir_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path)
}

fn flock(file: &File, operation: libc::c_int) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The snapshot names in `root`, oldest first.
fn snapshot_names(root: &Path) -> std::io::Result<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(root)?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.strip_prefix(SNAPSHOT_PREFIX).is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())))
        .collect();
    names.sort_by_key(|n| n[SNAPSHOT_PREFIX.len()..].parse::<u128>().unwrap_or(0));
    Ok(names)
}

/// The newest snapshot of `depot`, held, or None when it has none.
fn hold_depot_snapshot(depot: &Path) -> Result<Option<HeldSnapshot>, String> {
    use std::os::unix::fs::MetadataExt;
    let Some(root) = depot_snapshot_root(depot) else { return Ok(None) };
    match fs::symlink_metadata(&root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", root.display())),
        Ok(meta) if !meta.is_dir() => return Err(format!("{} is not a directory", root.display())),
        Ok(_) => {}
    }
    // A sweep can take the chosen snapshot between listing and locking, but
    // only after a newer one was published, so the next listing finds that.
    for _ in 0..8 {
        let names = snapshot_names(&root).map_err(|e| format!("reading {}: {e}", root.display()))?;
        let Some(name) = names.last() else { return Ok(None) };
        let path = root.join(name);
        let Ok(dir) = open_dir_nofollow(&path) else { continue };
        flock(&dir, libc::LOCK_SH).map_err(|e| format!("holding {}: {e}", path.display()))?;
        let (Ok(held), Ok(named)) = (dir.metadata(), fs::symlink_metadata(&path)) else { continue };
        if (held.dev(), held.ino()) == (named.dev(), named.ino()) {
            return Ok(Some(HeldSnapshot { path, _hold: dir }));
        }
    }
    Err(format!("the depot snapshots in {} kept changing", root.display()))
}

fn snapshot_lock(root: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new().create(true).read(true).write(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW).open(root.join(".lock"))
}

/// Copies `depot` into a new snapshot, then sweeps the older ones. Run it
/// only right after the depot was prepared, while nothing else prepares it.
pub fn publish_depot_snapshot(depot: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    let root = depot_snapshot_root(depot).ok_or_else(|| format!("{} has no parent", depot.display()))?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&root)
        .map_err(|e| format!("creating {}: {e}", root.display()))?;
    if !fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir()) {
        return Err(format!("{} is not a directory", root.display()));
    }
    let lock = snapshot_lock(&root).map_err(|e| format!("opening {}/.lock: {e}", root.display()))?;
    flock(&lock, libc::LOCK_EX).map_err(|e| format!("locking {}/.lock: {e}", root.display()))?;
    remove_abandoned_snapshot_dirs(&root);
    let names = snapshot_names(&root).map_err(|e| format!("reading {}: {e}", root.display()))?;
    let next = names.last().map_or(0, |n| n[SNAPSHOT_PREFIX.len()..].parse::<u128>().unwrap_or(0)) + 1;
    let tmp = root.join(format!("tmp-{}", util::hex(8)));
    // Prewarm just finished, but a user's own Julia may share the depot.
    copy_tree(depot, &tmp, DEPOT_CLONE_ATTEMPTS)?;
    let snapshot = root.join(format!("{SNAPSHOT_PREFIX}{next:020}"));
    if let Err(e) = fs::rename(&tmp, &snapshot) {
        let _ = fs::remove_dir_all(&tmp);
        return Err(format!("publishing {}: {e}", snapshot.display()));
    }
    if let Ok(dir) = File::open(&root) {
        let _ = dir.sync_all();
    }
    sweep_depot_snapshots(&root);
    Ok(snapshot)
}

/// A `tmp-` or `trash-` directory seen under the publish lock belongs to a
/// publish or sweep that died.
fn remove_abandoned_snapshot_dirs(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let abandoned = name.to_str().is_some_and(|n| n.starts_with("tmp-") || n.starts_with("trash-"));
        if abandoned && entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Removes every snapshot but the newest that no clone holds. The caller
/// holds the publish lock.
fn sweep_depot_snapshots(root: &Path) {
    let Ok(mut names) = snapshot_names(root) else { return };
    names.pop();
    for name in names {
        let path = root.join(&name);
        let Ok(dir) = open_dir_nofollow(&path) else { continue };
        if flock(&dir, libc::LOCK_EX | libc::LOCK_NB).is_err() {
            continue;
        }
        // Renamed first, so no later clone can pick it while it is removed.
        let trash = root.join(format!("trash-{name}"));
        if fs::rename(&path, &trash).is_ok() {
            let _ = fs::remove_dir_all(&trash);
        }
    }
}

/// Sweeps the snapshots of `depot` unless a publish or sweep is under way.
fn try_sweep_depot_snapshots(depot: &Path) {
    let Some(root) = depot_snapshot_root(depot) else { return };
    let Ok(lock) = snapshot_lock(&root) else { return };
    if flock(&lock, libc::LOCK_EX | libc::LOCK_NB).is_ok() {
        sweep_depot_snapshots(&root);
    }
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
/// They go in the user's private runtime directory when there is one, where
/// no other user can claim the name first, and in the temp directory otherwise.
fn identity_files() -> Result<PathBuf, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    identity_files_in(&identity_base(runtime.as_deref()).unwrap_or_else(util::temp_dir))
}

/// `$XDG_RUNTIME_DIR` when it is an absolute path to a directory this user
/// owns that nobody else can enter.
fn identity_base(runtime_dir: Option<&Path>) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let dir = runtime_dir.filter(|d| d.is_absolute())?;
    let meta = fs::symlink_metadata(dir).ok()?;
    let private = meta.file_type().is_dir() && meta.uid() == unsafe { libc::getuid() } && meta.mode() & 0o077 == 0;
    private.then(|| dir.to_path_buf())
}

/// The directory lives in a shared temp dir under a predictable name, so it
/// is trusted only if this user owns it, it is a real directory (not a
/// symlink), and nobody else can write to it. Otherwise another local user
/// could pre-create it and choose the sandbox's passwd, or plant a symlink
/// where the temp file is written.
fn identity_files_in(base: &Path) -> Result<PathBuf, String> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    let uid = unsafe { libc::getuid() };
    let etc = base.join(format!("palette-etc-{uid}"));
    match fs::DirBuilder::new().mode(0o755).create(&etc) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("creating {}: {e}", etc.display())),
    }
    let meta = fs::symlink_metadata(&etc).map_err(|e| format!("checking {}: {e}", etc.display()))?;
    if !meta.file_type().is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "refusing sandbox identity dir {}: it must be a directory owned by uid {uid} and writable only by its owner",
            etc.display()
        ));
    }
    let passwd = format!("{SANDBOX_USER}:x:1000:1000:Palette sandbox:/run/palette/home:/bin/bash\n");
    let group = format!("{SANDBOX_USER}:x:1000:\n");
    let current = |name: &str| fs::read_to_string(etc.join(name)).ok();
    if current("passwd").as_deref() != Some(passwd.as_str()) || current("group").as_deref() != Some(group.as_str()) {
        for (name, text) in [("passwd", &passwd), ("group", &group)] {
            let tmp = etc.join(format!(".{name}.{}", std::process::id()));
            let _ = fs::remove_file(&tmp);
            let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o644)
                .custom_flags(libc::O_NOFOLLOW).open(&tmp).map_err(|e| e.to_string())?;
            f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
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
    pub package_store_root: Option<&'a str>,
    pub package_depot_dir: Option<&'a str>,
    pub package_environment_dir: Option<&'a str>,
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
    let package_store = s.package_store_root.map(|p| canonical_dir("persistent package store", Path::new(p))).transpose()?;
    let package_depot = s.package_depot_dir.map(|p| canonical_dir("persistent package depot", Path::new(p))).transpose()?;
    let package_environment = s.package_environment_dir.map(|p| canonical_dir("persistent package environment", Path::new(p))).transpose()?;
    if let (Some(root), Some(depot), Some(environment)) = (package_store.as_deref(), package_depot.as_deref(), package_environment.as_deref()) {
        if !depot.starts_with(root) || !environment.starts_with(root) {
            return Err("persistent package depot and environment must be inside PALETTE_PACKAGE_DEPOT".into());
        }
    }
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
    if let Some(path) = package_store.as_deref() { protected.push(("persistent package store", path)); }
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
    // Every bind source is the canonical path that was checked above, not the
    // raw string: a symlink swapped in after the checks cannot redirect a mount.
    // Guest-side targets keep their configured paths.
    let utf8 = |p: &Path, what: &str| p.to_str().map(str::to_owned).ok_or(format!("{what} path is not valid UTF-8"));
    let toolchain_src = utf8(&toolchain_root, "Julia toolchain")?;
    let depot_source = utf8(depot_clone.as_deref().unwrap_or(&original_depot), "Julia depot")?;
    let repo_src = utf8(&repo, "repository")?;
    let project_src = utf8(&project, "project")?;
    let task_tools_src = task_tools_root.as_deref().map(|p| utf8(p, "task tools")).transpose()?;
    let broker_src = broker_root.as_deref().map(|p| utf8(p, "broker socket directory")).transpose()?;

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
           "--ro-bind", &toolchain_src, &toolchain,
           // The primary depot, writable at its own real path; the source is
           // this worker's private clone, never the shared depot.
           "--bind", &depot_source, s.julia_depot,
           "--ro-bind", &repo_src, s.repo_dir,
           "--ro-bind", &project_src, s.project_dir,
           "--bind", workspace, workspace]);
    if let (Some(src), Some(t)) = (task_tools_src.as_deref(), task_tools) {
        push(&["--ro-bind", src, t]);
    }
    for r in &read_roots {
        push(&["--ro-bind", r, r]);
    }
    if let (Some(src), Some(d)) = (broker_src.as_deref(), broker_socket_dir) {
        push(&["--ro-bind", src, d]);
    }
    if let Some(root) = package_store.as_deref() {
        let root = root.to_str().ok_or("package store path is not valid UTF-8")?;
        push(&["--ro-bind", root, root]);
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
    let load_path = match package_environment.as_deref() {
        Some(env) => format!("@:{}:@v#.#:{}:@stdlib", s.project_dir, env.display()),
        None => format!("@:{}:@v#.#:@stdlib", s.project_dir),
    };
    let depot_path = match package_depot.as_deref() {
        Some(depot) => format!("{}:{}", s.julia_depot, depot.display()),
        None => s.julia_depot.to_string(),
    };
    push(&["--setenv", "HOME", "/run/palette/home", "--setenv", "JULIA_DEPOT_PATH", &depot_path,
           "--setenv", "JULIA_PROJECT", s.project_dir,
           // The session project stays loadable after turn code activates another.
           "--setenv", "JULIA_LOAD_PATH", &load_path,
           "--setenv", "PALETTE_REPO_DIR", s.repo_dir, "--setenv", "PATH", &path,
           "--setenv", "LANG", "en_US.UTF-8", "--setenv", "USER", SANDBOX_USER, "--setenv", "LOGNAME", SANDBOX_USER,
           "--chdir", workspace]);
    if let Some(environment) = package_environment.as_deref() {
        push(&["--setenv", "PALETTE_PACKAGE_ENVIRONMENT", environment.to_str().ok_or("package environment path is not UTF-8")?]);
    }
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

    #[test]
    fn case_p01_package_installer_namespace_confines_writes_and_offline_network() {
        struct Temp(PathBuf);
        impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
        let root = util::mkdtemp("palette-pkg-installer-namespace-").unwrap();
        let _guard = Temp(root.clone());
        let store = dir(&root, "store");
        let depot = dir(&store, "depot");
        let environment = dir(&store, "environment");
        let base = root.join("readonly-base-depot");
        fs::create_dir_all(base.join("packages")).unwrap();
        fs::create_dir_all(base.join("compiled")).unwrap();
        fs::create_dir_all(base.join("artifacts")).unwrap();
        let julia = PathBuf::from(resolve_julia().unwrap());
        let outside = base.join("packages").join("outside-marker");
        let store_write = depot.join("allowed-write");
        let lock = store.join(".package-store.lock");
        let source = dir(&store, "sources/gesso");
        fs::write(source.join("Project.toml"), "operator source fixture").unwrap();
        let source_marker = source.join("pkg-build-must-not-write");
        fs::write(&lock, "host-owned lock").unwrap();
        let quote = |path: &Path| serde_json::to_string(&path.to_string_lossy()).unwrap();
        let write_script = format!(
            concat!("function trywrite(p); try; open(p, \"w\") do io; write(io, \"x\"); end; true; catch; false; end; end; ",
                    "println(\"STORE_WRITE=\", trywrite({})); ",
                    "println(\"OUTSIDE_BLOCKED=\", !trywrite({})); ",
                    "println(\"LOCK_BLOCKED=\", !trywrite({})); ",
                    "println(\"SOURCE_BLOCKED=\", !trywrite({}));"),
            quote(&store_write), quote(&outside), quote(&lock), quote(&source_marker),
        );
        let argv = package_installer_argv(
            julia.to_str().unwrap(), &store, &depot, &environment, &base, true,
            &write_script, &[],
        ).unwrap();
        let mut env = BTreeMap::new();
        env.insert("JULIA_PROJECT".into(), Some(environment.to_string_lossy().into_owned()));
        env.insert("JULIA_DEPOT_PATH".into(), Some(format!("{}:{}", depot.display(), base.display())));
        env.insert("JULIA_PKG_PRECOMPILE_AUTO".into(), Some("0".into()));
        env.insert("JULIA_PKG_OFFLINE".into(), Some("true".into()));
        env.insert("JULIA_PKG_SERVER".into(), Some(String::new()));
        env.insert("HOME".into(), Some(root.join("home").to_string_lossy().into_owned()));
        fs::create_dir_all(root.join("home")).unwrap();
        let confined = run_captured_clean(&argv, &env, Duration::from_secs(60)).unwrap();
        assert_eq!(confined.returncode, 0, "{}{}", confined.stdout, confined.stderr);
        for expected in ["STORE_WRITE=true", "OUTSIDE_BLOCKED=true", "LOCK_BLOCKED=true", "SOURCE_BLOCKED=true"] {
            assert!(confined.stdout.contains(expected), "missing {expected}: {}{}", confined.stdout, confined.stderr);
        }
        assert!(store_write.is_file());
        assert_eq!(fs::read_to_string(&lock).unwrap(), "host-owned lock");
        assert_eq!(fs::read_to_string(source.join("Project.toml")).unwrap(), "operator source fixture");
        assert!(!source_marker.exists());
        assert!(!outside.exists());

        // Negative control: preserve the isolated Julia project/depot/offline
        // settings, but run Julia directly. The same host-marker write must
        // succeed without the namespace.
        let direct_script = format!("open({}, \"w\") do io; write(io, \"escaped\"); end", quote(&outside));
        let direct = vec![julia.to_string_lossy().into_owned(), "--startup-file=no".into(), "-e".into(), direct_script];
        let unconstrained = run_captured_clean(&direct, &env, Duration::from_secs(30)).unwrap();
        assert_eq!(unconstrained.returncode, 0, "{}{}", unconstrained.stdout, unconstrained.stderr);
        assert_eq!(fs::read_to_string(&outside).unwrap(), "escaped");

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        struct ListenerGuard {
            stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
            join: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for ListenerGuard {
            fn drop(&mut self) {
                self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(join) = self.join.take() { let _ = join.join(); }
            }
        }
        let listener_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let listener_thread_stop = listener_stop.clone();
        let listener_join = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(240);
            while !listener_thread_stop.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        let mut payload = [0; 4];
                        let ok = stream.read_exact(&mut payload).is_ok() && &payload == b"ping";
                        let _ = accepted_tx.send(ok);
                        return;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    _ => { let _ = accepted_tx.send(false); return; }
                }
            }
            let _ = accepted_tx.send(false);
        });
        let _listener_guard = ListenerGuard { stop: listener_stop, join: Some(listener_join) };
        let socket_script = format!(
            concat!("using Sockets; try; s=connect(\"127.0.0.1\", {}); write(s, \"ping\"); close(s); println(\"CONNECTED\"); ",
                    "catch; println(\"BLOCKED\"); end"), port,
        );
        let offline_argv = package_installer_argv(
            julia.to_str().unwrap(), &store, &depot, &environment, &base, true, &socket_script, &[],
        ).unwrap();
        let denied = run_captured_clean(&offline_argv, &env, Duration::from_secs(60)).unwrap();
        assert_eq!(denied.returncode, 0, "{}{}", denied.stdout, denied.stderr);
        assert!(denied.stdout.contains("BLOCKED"), "{}{}", denied.stdout, denied.stderr);
        let online_argv = package_installer_argv(
            julia.to_str().unwrap(), &store, &depot, &environment, &base, false, &socket_script, &[],
        ).unwrap();
        let allowed = run_captured_clean(&online_argv, &env, Duration::from_secs(60)).unwrap();
        assert_eq!(allowed.returncode, 0, "{}{}", allowed.stdout, allowed.stderr);
        assert!(allowed.stdout.contains("CONNECTED"), "{}{}", allowed.stdout, allowed.stderr);
        assert!(accepted_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn case_p02_package_store_rejects_symlink_seed_before_mutation() {
        struct Temp(PathBuf);
        impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
        let root = util::mkdtemp("palette-pkg-store-validation-").unwrap();
        let _guard = Temp(root.clone());
        let store = root.join("store");
        let depot = store.join("depot");
        let outside = root.join("outside");
        fs::create_dir_all(&depot).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "untouched").unwrap();
        #[cfg(unix)] {
            use std::os::unix::fs::{PermissionsExt, symlink};
            fs::set_permissions(&store, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&depot, fs::Permissions::from_mode(0o755)).unwrap();
            symlink(&outside, depot.join("registries")).unwrap();
            let error = package_store_paths_at(store.clone(), &[], None).unwrap_err();
            assert!(error.contains("must not be a symlink"), "{error}");
            assert_eq!(fs::read_to_string(outside.join("sentinel")).unwrap(), "untouched");
            assert_eq!(fs::metadata(&store).unwrap().permissions().mode() & 0o777, 0o755);
            assert_eq!(fs::metadata(&depot).unwrap().permissions().mode() & 0o777, 0o755);
            fs::remove_file(depot.join("registries")).unwrap();
            let nested = dir(&depot.join("packages"), "Fixture/slug/src");
            symlink(outside.join("sentinel"), nested.join("module.jl")).unwrap();
            let error = package_store_paths_at(store.clone(), &[], None).unwrap_err();
            assert!(error.contains("symlinks"), "{error}");
            assert_eq!(fs::read_to_string(outside.join("sentinel")).unwrap(), "untouched");
            assert_eq!(fs::metadata(&store).unwrap().permissions().mode() & 0o777, 0o755);
        }
    }

    #[test]
    fn case_p03_package_store_rejects_protected_root_before_creation() {
        struct Temp(PathBuf);
        impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
        let parent = util::mkdtemp("palette-pkg-store-collision-").unwrap();
        let _guard = Temp(parent.clone());
        let root = parent.join("runtime-root");
        let error = package_store_paths_at(root.clone(), &[root.clone()], None).unwrap_err();
        assert!(error.contains("overlaps protected host path"), "{error}");
        assert!(!root.exists(), "rejected protected path was created");
    }

    #[test]
    fn depot_clones_record_their_owner_and_dead_owners_are_swept() {
        let root = util::mkdtemp("palette-depot-clones-").unwrap();
        let depot = dir(&root, "depot");
        fs::write(depot.join("cache"), "warm").unwrap();
        let clones = root.join(".palette-depot-clones");

        let live = create_session_depot(depot.to_str()).unwrap();
        assert_eq!(fs::read_to_string(live.join("cache")).unwrap(), "warm");
        let record = fs::read_to_string(clone_owner_path(&live)).unwrap();
        assert!(clone_owner_alive(&record), "own record judged dead: {record}");

        let namespace = pid_namespace().unwrap();
        let mut child = Command::new("true").spawn().unwrap();
        let reaped = child.id();
        child.wait().unwrap();
        let orphan = |name: &str, record: String| {
            let clone = dir(&clones, name);
            fs::write(clone.join("cache"), "stale").unwrap();
            fs::write(clone_owner_path(&clone), record).unwrap();
            clone
        };
        let exited = orphan("depot-exited", format!("{namespace} {reaped} 1\n"));
        let reused = orphan("depot-reused", format!("{namespace} {} 0\n", std::process::id()));
        let foreign = orphan("depot-foreign", format!("pid:[1] {reaped} 1\n"));
        let garbled = orphan("depot-garbled", "not a record".into());
        let unrecorded = dir(&clones, "depot-unrecorded");
        let lone_record = clones.join("depot-gone.owner");
        fs::write(&lone_record, format!("{namespace} {reaped} 1\n")).unwrap();
        let target = dir(&root, "elsewhere");
        let link = clones.join("depot-link");
        symlink(&target, &link).unwrap();
        fs::write(clone_owner_path(&link), format!("{namespace} {reaped} 1\n")).unwrap();

        let second = create_session_depot(depot.to_str()).unwrap();
        for gone in [&exited, &reused, &lone_record] {
            assert!(fs::symlink_metadata(gone).is_err(), "{} survived the sweep", gone.display());
        }
        assert!(!clone_owner_path(&exited).exists() && !clone_owner_path(&reused).exists());
        for kept in [&live, &second, &foreign, &garbled, &unrecorded, &target] {
            assert!(kept.is_dir(), "{} was swept", kept.display());
        }
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());

        remove_session_depot(&live);
        assert!(fs::symlink_metadata(&live).is_err() && fs::symlink_metadata(clone_owner_path(&live)).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_depot_clone_leaves_no_owner_record() {
        let root = util::mkdtemp("palette-depot-clone-failure-").unwrap();
        let missing = root.join("no-such-depot");
        assert!(create_session_depot(missing.to_str()).is_err());
        let clones = root.join(".palette-depot-clones");
        assert_eq!(fs::read_dir(&clones).unwrap().count(), 0, "a failed clone left residue");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn session_depots_clone_the_newest_snapshot_not_the_live_depot() {
        let root = util::mkdtemp("palette-depot-snapshots-").unwrap();
        let depot = dir(&root, "depot");
        fs::write(depot.join("cache"), "prepared").unwrap();
        let first = publish_depot_snapshot(&depot).unwrap();
        let snapshots = root.join(".palette-depot-snapshots").join("depot");
        assert_eq!(first.parent().unwrap(), snapshots);

        // A Julia process writing the live depot does not reach the clone.
        fs::write(depot.join("cache"), "churned").unwrap();
        let clone = create_session_depot(depot.to_str()).unwrap();
        assert_eq!(fs::read_to_string(clone.join("cache")).unwrap(), "prepared");
        fs::remove_dir_all(&depot).unwrap();
        let without_live = create_session_depot(depot.to_str()).unwrap();
        assert_eq!(fs::read_to_string(without_live.join("cache")).unwrap(), "prepared");

        fs::create_dir(&depot).unwrap();
        fs::write(depot.join("cache"), "reprepared").unwrap();
        let second = publish_depot_snapshot(&depot).unwrap();
        assert!(second > first);
        let newer = create_session_depot(depot.to_str()).unwrap();
        assert_eq!(fs::read_to_string(newer.join("cache")).unwrap(), "reprepared");
        let mut left: Vec<_> = fs::read_dir(&snapshots).unwrap().map(|e| e.unwrap().path()).collect();
        left.sort();
        assert_eq!(left, [snapshots.join(".lock"), second], "the unheld older snapshot was kept");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_held_snapshot_outlives_the_sweep_and_abandoned_publishes_are_removed() {
        let root = util::mkdtemp("palette-depot-snapshot-hold-").unwrap();
        let depot = dir(&root, "depot");
        fs::write(depot.join("cache"), "first").unwrap();
        let first = publish_depot_snapshot(&depot).unwrap();
        let snapshots = first.parent().unwrap().to_path_buf();
        let dead_publish = dir(&snapshots, "tmp-0123abcd");
        let dead_sweep = dir(&snapshots, "trash-snap-00000000000000000000");

        let held = hold_depot_snapshot(&depot).unwrap().unwrap();
        assert_eq!(held.path, first);
        fs::write(depot.join("cache"), "second").unwrap();
        let second = publish_depot_snapshot(&depot).unwrap();
        assert!(first.is_dir(), "a held snapshot was swept");
        assert!(!dead_publish.exists() && !dead_sweep.exists(), "abandoned dirs survived a publish");
        assert_eq!(fs::read_to_string(held.path.join("cache")).unwrap(), "first");
        drop(held);

        let clone = create_session_depot(depot.to_str()).unwrap();
        assert_eq!(fs::read_to_string(clone.join("cache")).unwrap(), "second");
        assert!(!first.exists(), "the released snapshot was not swept");
        assert!(second.is_dir());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_snapshot_root_that_is_not_a_directory_fails_closed() {
        let root = util::mkdtemp("palette-depot-snapshot-link-").unwrap();
        let depot = dir(&root, "depot");
        let elsewhere = dir(&root, "elsewhere");
        fs::create_dir(root.join(".palette-depot-snapshots")).unwrap();
        symlink(&elsewhere, root.join(".palette-depot-snapshots").join("depot")).unwrap();
        assert!(create_session_depot(depot.to_str()).is_err());
        assert!(publish_depot_snapshot(&depot).is_err());
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
        assert_eq!(fs::read_dir(root.join(".palette-depot-clones")).unwrap().count(), 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn owned_dirs_are_swept_by_prefix_once_idle_long_enough() {
        let root = util::mkdtemp("palette-owned-dirs-").unwrap();
        let namespace = pid_namespace().unwrap();
        let mut child = Command::new("true").spawn().unwrap();
        let reaped = child.id();
        child.wait().unwrap();
        let dead = format!("{namespace} {reaped} 1\n");
        let make = |name: &str, record: &str, age_days: u64| {
            let d = dir(&root, name);
            let f = d.join("receipts.jsonl");
            fs::write(&f, "{}\n").unwrap();
            let when = SystemTime::now() - Duration::from_secs(age_days * 86_400);
            File::options().write(true).open(&f).unwrap().set_modified(when).unwrap();
            File::open(&d).unwrap().set_modified(when).unwrap();
            fs::write(clone_owner_path(&d), record).unwrap();
            d
        };
        let stale = make("palette-stale", &dead, 40);
        let recent = make("palette-recent", &dead, 2);
        let live = make("palette-live", &clone_owner_record().unwrap(), 40);
        let other_prefix = make("depot-stale", &dead, 40);

        sweep_orphaned_dirs(&root, "palette-", Duration::from_secs(30 * 86_400));
        assert!(fs::symlink_metadata(&stale).is_err() && !clone_owner_path(&stale).exists());
        for kept in [&recent, &live, &other_prefix] {
            assert!(kept.is_dir() && clone_owner_path(kept).exists(), "{} was swept", kept.display());
        }
        sweep_orphaned_dirs(&root, "palette-", Duration::ZERO);
        assert!(fs::symlink_metadata(&recent).is_err(), "a dead owner's dir survived a zero-age sweep");
        assert!(live.is_dir() && other_prefix.is_dir());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn receipts_dirs_are_private_owned_and_retention_is_parsed_strictly() {
        let root = util::mkdtemp("palette-receipts-").unwrap().join("receipts");
        let d = create_receipts_dir(&root, "palette-0123456789abcdef", Duration::from_secs(86_400)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(clone_owner_alive(&fs::read_to_string(clone_owner_path(&d)).unwrap()));
        assert!(create_receipts_dir(&root, "palette-0123456789abcdef", Duration::ZERO).is_err(), "an existing receipts dir was reused");
        assert!(d.is_dir(), "a live session's receipts were pruned");

        assert_eq!(receipt_retention(None).unwrap(), Some(Duration::from_secs(30 * 86_400)));
        assert_eq!(receipt_retention(Some("7")).unwrap(), Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(receipt_retention(Some("0")).unwrap(), None);
        for bad in ["", "-1", "+7", "1.5", "forever", "99999999"] {
            assert!(receipt_retention(Some(bad)).is_err(), "accepted {bad:?}");
        }
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn identity_dir_is_refused_unless_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;
        struct Temp(PathBuf);
        impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
        let uid = unsafe { libc::getuid() };
        let base = util::mkdtemp("palette-identity-test-").unwrap();
        let _guard = Temp(base.clone());
        let etc = identity_files_in(&base).unwrap();
        assert!(fs::read_to_string(etc.join("passwd")).unwrap().contains(":x:1000:1000:"));
        assert!(fs::read_to_string(etc.join("group")).unwrap().contains(":x:1000:"));
        // Writable by others: another user could swap the files.
        fs::set_permissions(&etc, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(identity_files_in(&base).unwrap_err().contains("refusing"));
        fs::remove_dir_all(&etc).unwrap();
        // A symlink planted at the predictable name.
        let elsewhere = base.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, base.join(format!("palette-etc-{uid}"))).unwrap();
        assert!(identity_files_in(&base).unwrap_err().contains("refusing"));
        assert!(!elsewhere.join("passwd").exists(), "wrote through a planted symlink");
    }

    #[test]
    fn identity_files_prefer_a_private_runtime_dir() {
        use std::os::unix::fs::PermissionsExt;
        struct Temp(PathBuf);
        impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
        let base = util::mkdtemp("palette-runtime-test-").unwrap();
        let _guard = Temp(base.clone());
        assert_eq!(identity_base(Some(&base)), Some(base.clone()));
        assert_eq!(identity_base(None), None);
        assert_eq!(identity_base(Some(Path::new("relative/run"))), None);
        assert_eq!(identity_base(Some(&base.join("missing"))), None);
        // Others can enter it: another user could have made the name first.
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(identity_base(Some(&base)), None);
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let link = base.join("link");
        let real = base.join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(identity_base(Some(&link)), None);
    }

    #[test]
    fn private_dir_is_closed_to_other_users() {
        use std::os::unix::fs::PermissionsExt;
        struct Temp(PathBuf);
        impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
        let base = util::mkdtemp("palette-private-test-").unwrap();
        let _guard = Temp(base.clone());
        let mode = |p: &Path| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        let nested = base.join("a/b");
        private_dir(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);
        // An existing, open directory is tightened.
        let open = base.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&open).unwrap();
        assert_eq!(mode(&open), 0o700);
        // A symlink is refused and its target left alone.
        std::os::unix::fs::symlink(&open, base.join("link")).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&base.join("link")).unwrap_err().contains("refusing"));
        assert_eq!(mode(&open), 0o755);
    }

    #[test]
    fn the_running_checkout_is_found_at_run_time() {
        // The test binary runs from target/ inside the checkout, as an
        // installed host runs from its own checkout.
        // With CARGO_TARGET_DIR outside the checkout there is nothing to find.
        let expected = fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
        let exe = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        if !exe.starts_with(&expected) {
            eprintln!("skipped: the test binary is outside the checkout");
            return;
        }
        assert_eq!(util::installed_checkout(), Some(expected));
    }

    #[test]
    fn an_absent_protected_path_does_not_protect_the_current_directory() {
        // Regression: run-worker without --broker-socket-dir passed an empty
        // path, which resolved to the cwd and refused every store under it.
        let cwd = std::env::current_dir().unwrap();
        let root = util::resolve(&cwd.join("store"));
        assert_eq!(protected_overlap(&root, &[PathBuf::new()]), None);
        assert_eq!(protected_overlap(Path::new("/"), &[PathBuf::new()]), None);
        assert_eq!(protected_overlap(&root, &[PathBuf::new(), cwd.clone()]), Some(util::resolve(&cwd)));
    }

    #[test]
    fn a_command_parent_refuses_only_roots_that_contain_it() {
        // The parent of a command path is protected so the path cannot be
        // swapped by renaming in it; a sibling root inside the parent cannot
        // do that. Regression: a store under ~/.local refused when a command
        // ran in ~/project.
        let home = util::mkdtemp("palette-cmd-parent-").unwrap();
        let project = dir(&home, "project");
        let ceiling = serde_json::json!({"host_command": {"commands": {"c": {"argv": ["/bin/true"], "cwd": project.to_str().unwrap()}}}});
        let commands = protected_command_paths(&ceiling);
        let r = |p: &Path| util::resolve(p);
        assert_eq!(command_overlap(&r(&home.join(".local/store")), &commands), None);
        assert_eq!(command_overlap(&r(&home), &commands), Some(r(&project)));
        assert_eq!(command_overlap(&r(home.parent().unwrap()), &commands), Some(r(&project)));
        assert_eq!(command_overlap(&r(&project.join("sub")), &commands), Some(r(&project)));
        // The home itself is the parent: refused even though it is not a
        // command path, since a root there could rename `project`.
        let only_parent = CommandPaths { paths: vec![], parents: vec![home.clone()] };
        assert_eq!(command_overlap(&r(&home), &only_parent), Some(r(&home)));
        assert_eq!(command_overlap(&r(&home.join("sibling")), &only_parent), None);
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn bind_sources_are_the_checked_canonical_paths() {
        let root = util::mkdtemp("palette-bind-sources-").unwrap();
        let real_repo = dir(&root, "real-repo");
        let real_project = dir(&root, "real-project");
        let depot = dir(&root, "depot");
        let workspace = dir(&root, "workspace");
        dir(&root, "toolchain/bin");
        let repo_link = root.join("repo-link");
        let project_link = root.join("project-link");
        symlink(&real_repo, &repo_link).unwrap();
        symlink(&real_project, &project_link).unwrap();
        let s = |p: &Path| p.to_str().unwrap().to_owned();
        let (repo_s, project_s, depot_s, ws_s) = (s(&repo_link), s(&project_link), s(&depot), s(&workspace));
        let julia = s(&root.join("toolchain/bin/julia"));
        let argv = build_bwrap_argv(&SandboxSpec {
            workspace_dir: &ws_s, broker_socket_dir: None, project_dir: &project_s, repo_dir: &repo_s,
            julia_bin: &julia, julia_depot: &depot_s, network_enabled: false, depot_clone_dir: None,
            package_store_root: None, package_depot_dir: None, package_environment_dir: None, state_dir: None,
        }).unwrap();
        let bind_of = |target: &str| argv.windows(3)
            .find(|w| w[0] == "--ro-bind" && w[2] == target).map(|w| w[1].clone());
        // The guest still sees the configured path, but the host source is the
        // resolved directory that passed the overlap checks, so retargeting the
        // link afterwards cannot change what is mounted.
        assert_eq!(bind_of(&repo_s).as_deref(), Some(s(&fs::canonicalize(&real_repo).unwrap()).as_str()));
        assert_eq!(bind_of(&project_s).as_deref(), Some(s(&fs::canonicalize(&real_project).unwrap()).as_str()));
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
    run_captured_with_env(argv, env, timeout, false)
}

pub fn run_captured_clean(argv: &[String], env: &BTreeMap<String, Option<String>>, timeout: Duration)
    -> Result<WorkerResult, String> {
    run_captured_with_env(argv, Some(env), timeout, true)
}

fn run_captured_with_env(argv: &[String], env: Option<&BTreeMap<String, Option<String>>>, timeout: Duration, clear_env: bool)
    -> Result<WorkerResult, String> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if clear_env {
        cmd.env_clear();
    }
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

/// The last `cap` bytes a pipe produced, shared with the thread reading it:
/// a renderer may print for minutes, and only the end is kept. `start` is
/// the stream offset of `kept[0]`, so a reader holding an offset can tell
/// what it has not seen yet, and how much of that was dropped.
#[derive(Default)]
struct Tail {
    kept: Vec<u8>,
    start: u64,
}

type SharedTail = std::sync::Arc<std::sync::Mutex<Tail>>;

fn read_tail(mut r: impl Read + Send + 'static, cap: usize) -> (SharedTail, std::thread::JoinHandle<()>) {
    let tail = SharedTail::default();
    let shared = tail.clone();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            let mut t = shared.lock().unwrap();
            t.kept.extend_from_slice(&buf[..n]);
            if t.kept.len() > cap {
                let drop = t.kept.len() - cap;
                t.kept.drain(..drop);
                t.start += drop as u64;
            }
        }
    });
    (tail, reader)
}

/// Output a reader at stream offset `*seen` has not read yet, and how many
/// of those bytes the tail had already dropped. While the command runs,
/// `whole` is false and the text stops before a UTF-8 sequence still being
/// written, so a character is never split between two reads.
fn read_since(tail: &SharedTail, seen: &mut u64, whole: bool) -> (String, u64) {
    let t = tail.lock().unwrap();
    let skipped = t.start.saturating_sub(*seen);
    let from = (*seen).max(t.start) - t.start;
    let mut bytes = &t.kept[from as usize..];
    if !whole {
        if let Err(e) = std::str::from_utf8(bytes) {
            if e.error_len().is_none() {
                bytes = &bytes[..e.valid_up_to()];
            }
        }
    }
    *seen = t.start + from + bytes.len() as u64;
    (String::from_utf8_lossy(bytes).into_owned(), skipped)
}

/// An operator-named host command (the broker's host_command), started in a
/// process group of its own. A watchdog kills the whole group at the
/// deadline whether or not anyone is waiting, so helper processes a
/// renderer starts never outlive it; cancelling or dropping the job kills
/// it too. The group is always signalled before the command is reaped,
/// under one lock with the watchdog, so a signal can never reach a reused
/// pid. Output is read incrementally: each `read_new` and the final result
/// return only what earlier reads did not.
pub struct HostJob {
    child: std::process::Child,
    tails: [SharedTail; 2],
    readers: Vec<std::thread::JoinHandle<()>>,
    seen: [u64; 2],
    started: Instant,
    state: std::sync::Arc<std::sync::Mutex<JobState>>,
}

#[derive(Default)]
struct JobState {
    finished: bool,
    timed_out: bool,
    cancelled: bool,
}

pub struct HostJobResult {
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
    /// Bytes of each stream dropped from the tail before they were read.
    pub skipped: [u64; 2],
    pub timed_out: bool,
    pub cancelled: bool,
    pub seconds: f64,
}

/// Output a running job produced since the last read.
pub struct HostJobOutput {
    pub stdout: String,
    pub stderr: String,
    pub skipped: [u64; 2],
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
        let (out, out_reader) = read_tail(child.stdout.take().unwrap(), output_cap);
        let (err, err_reader) = read_tail(child.stderr.take().unwrap(), output_cap);
        let state = std::sync::Arc::new(std::sync::Mutex::new(JobState::default()));
        let (pgid, watched) = (child.id() as i32, state.clone());
        let deadline = Instant::now() + timeout;
        std::thread::spawn(move || loop {
            {
                let mut st = watched.lock().unwrap();
                if st.finished {
                    return;
                }
                if Instant::now() >= deadline {
                    st.timed_out = true;
                    kill_group(pgid);
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        });
        Ok(HostJob {
            child, tails: [out, err], readers: vec![out_reader, err_reader], seen: [0, 0],
            started: Instant::now(), state,
        })
    }

    pub fn seconds(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    pub fn running(&self) -> Result<bool, String> {
        exited_unreaped(self.child.id() as i32, false).map(|exited| !exited)
    }

    pub fn wait(&mut self) -> Result<HostJobResult, String> {
        exited_unreaped(self.child.id() as i32, true)?;
        self.finish()
    }

    /// Kills the command's whole group and returns its result, marked
    /// cancelled unless it had already finished or timed out.
    pub fn cancel(&mut self) -> Result<HostJobResult, String> {
        {
            let mut st = self.state.lock().unwrap();
            if !st.finished && !st.timed_out && self.running()? {
                st.cancelled = true;
                kill_group(self.child.id() as i32);
            }
        }
        self.wait()
    }

    /// Output produced since the last read, while the command runs.
    pub fn read_new(&mut self) -> HostJobOutput {
        let (stdout, s0) = read_since(&self.tails[0], &mut self.seen[0], false);
        let (stderr, s1) = read_since(&self.tails[1], &mut self.seen[1], false);
        HostJobOutput { stdout, stderr, skipped: [s0, s1], seconds: self.started.elapsed().as_secs_f64() }
    }

    fn finish(&mut self) -> Result<HostJobResult, String> {
        use std::os::unix::process::ExitStatusExt;
        let (timed_out, cancelled) = {
            let mut st = self.state.lock().unwrap();
            st.finished = true;
            // Helpers left in the group after the command exited go too:
            // their open pipes would otherwise keep the output readers waiting.
            kill_group(self.child.id() as i32);
            (st.timed_out, st.cancelled)
        };
        let status = self.child.wait().map_err(|e| e.to_string())?;
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        let (stdout, s0) = read_since(&self.tails[0], &mut self.seen[0], true);
        let (stderr, s1) = read_since(&self.tails[1], &mut self.seen[1], true);
        Ok(HostJobResult {
            returncode: status.code().unwrap_or_else(|| -status.signal().unwrap_or(0)),
            stdout, stderr, skipped: [s0, s1], timed_out, cancelled,
            seconds: self.started.elapsed().as_secs_f64(),
        })
    }
}

impl Drop for HostJob {
    fn drop(&mut self) {
        let mut st = self.state.lock().unwrap();
        if !st.finished {
            st.finished = true;
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
    let package_store = package_store_paths(&[
        Some(r.workspace_dir), Some(r.project_dir), Some(r.repo_dir), r.broker_socket_dir,
    ].into_iter().flatten().map(PathBuf::from).collect::<Vec<_>>(), None)?;
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
            package_store_root: Some(package_store.root.to_str().ok_or("package store path is not valid UTF-8")?),
            package_depot_dir: Some(package_store.depot.to_str().ok_or("package depot path is not valid UTF-8")?),
            package_environment_dir: Some(package_store.environment.to_str().ok_or("package environment path is not valid UTF-8")?),
            state_dir: None,
        })?;
        argv.extend(["--".into(), julia.clone(), "--startup-file=no".into(), "-e".into(), r.script.into()]);
        run_captured(&argv, None, r.timeout)
    })();
    if let Some(p) = owned {
        remove_session_depot(&p);
    }
    result
}
