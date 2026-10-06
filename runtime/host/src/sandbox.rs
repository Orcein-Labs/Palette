//! The first trust domain: a bubblewrap sandbox around an unrestricted
//! `julia`. Full language power inside, bounded by the OS outside it. Paths
//! are bound at the same guest path as on the host, because Julia's manifests
//! and precompile caches embed absolute paths. Each worker gets its own
//! reflink clone of the prepared depot as its first writable entry. A later
//! read-only operator package depot exposes broker-installed packages; the
//! full prepared clone remains first so Julia's cached stdlib build ids match.

use crate::util;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

pub fn default_depot() -> String {
    std::env::var("JULIA_DEPOT_PATH")
        .ok()
        .and_then(|v| v.split(':').next_back().map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| util::home().join(".julia").to_string_lossy().into_owned())
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

/// Extract operator-owned command paths once, shared by store initialization
/// and installer namespace validation. These values only come from the frozen
/// operator ceiling; runtime worker argv is never passed here.
pub fn protected_command_paths(ceiling: &serde_json::Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Some(commands) = ceiling.get("host_command").and_then(|v| v.get("commands")).and_then(serde_json::Value::as_object) else {
        return paths;
    };
    let mut add = |path: &Path| {
        if path.is_absolute() {
            paths.push(path.to_path_buf());
            if let Some(parent) = path.parent() { paths.push(parent.to_path_buf()); }
        }
    };
    for spec in commands.values() {
        if let Some(cwd) = spec.get("cwd").and_then(serde_json::Value::as_str) { add(Path::new(cwd)); }
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
    paths
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
    protected.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
    protected.extend(parse_read_roots(std::env::var("PALETTE_READ_ROOTS").ok().as_deref())?);
    for key in ["PALETTE_CAPABILITY_CEILING", "PALETTE_HOST_COMMANDS", "PALETTE_REPAIR_CONFIG", "PALETTE_RUNTIME_ROOT"] {
        if let Ok(path) = std::env::var(key) { if !path.is_empty() { protected.push(PathBuf::from(path)); } }
    }
    if let Some(ceiling) = ceiling { protected.extend(protected_command_paths(ceiling)); }
    let root = util::resolve(&lexical_root);
    for candidate in protected {
        let candidate = util::resolve(&candidate);
        if root == candidate || root.starts_with(&candidate) || candidate.starts_with(&root) {
            return Err(format!("PALETTE_PACKAGE_DEPOT overlaps protected host path: {}", candidate.display()));
        }
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

/// One private, writable prepared-depot clone for a session's worker caches.
/// Broker-authorized package installs live in the separate durable package store.
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
           // The primary depot, writable at its own real path; the source is
           // this worker's private clone, never the shared depot.
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
    let package_store = package_store_paths(&[
        PathBuf::from(r.workspace_dir), PathBuf::from(r.project_dir), PathBuf::from(r.repo_dir),
        r.broker_socket_dir.map(PathBuf::from).unwrap_or_default(),
    ], None)?;
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
        let _ = fs::remove_dir_all(p);
    }
    result
}
