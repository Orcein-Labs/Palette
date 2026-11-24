//! High Inquisitor Private Lab isolation (`PALETTE_HOME` / `PALETTE_PRIVATE_MODE`).
//!
//! Enforces strict single-root containment under `PALETTE_HOME` with `0o700`
//! directory permissions and `0o600` file/socket permissions, disables shared
//! host state/logs/TeamLab paths, and audits `/proc` for unboxed same-UID
//! builder processes sharing our mount namespace.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::util;

pub const DEFAULT_BUILDER_COMMS: &[&str] = &["grok", "gemini", "agy", "antigravity"];

pub fn is_truthy_str(value: Option<&str>) -> bool {
    value.is_some_and(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub fn is_private_strict(explicit_strict: bool, var: &dyn Fn(&str) -> Option<String>) -> bool {
    explicit_strict || is_truthy_str(var("PALETTE_PRIVATE_STRICT").as_deref())
}

/// Resolves the private lab root if private mode is enabled via `--private-dir`,
/// `PALETTE_PRIVATE_DIR`, `PALETTE_HOME`, or `PALETTE_PRIVATE_MODE=1`.
///
/// Fails closed if `PALETTE_PRIVATE_MODE=1` is set without any root directory.
pub fn resolve_private_root(
    explicit_dir: Option<&str>,
    var: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<PathBuf>, String> {
    let explicit = explicit_dir.map(str::trim).filter(|s| !s.is_empty());
    let env_private_dir = var("PALETTE_PRIVATE_DIR").filter(|s| !s.trim().is_empty());
    let env_home = var("PALETTE_HOME").filter(|s| !s.trim().is_empty());
    let mode_on = is_truthy_str(var("PALETTE_PRIVATE_MODE").as_deref());

    if let Some(raw) = explicit
        .or(env_private_dir.as_deref())
        .or(env_home.as_deref())
    {
        return Ok(Some(util::resolve(&util::expand_user(raw.trim()))));
    }

    if mode_on {
        if let Some(data_home) = var("PALETTE_DATA_HOME").filter(|s| !s.trim().is_empty()) {
            return Ok(Some(util::resolve(&util::expand_user(data_home.trim()))));
        }
        return Err(
            "PALETTE_PRIVATE_MODE=1 requires PALETTE_HOME, PALETTE_PRIVATE_DIR, or --private-dir"
                .into(),
        );
    }

    Ok(None)
}

/// Active private lab root from the current process environment, if any.
pub fn active_private_root() -> Option<PathBuf> {
    resolve_private_root(None, &|k| std::env::var(k).ok()).ok().flatten()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivateLabLayout {
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub state: PathBuf,
    pub worlds: PathBuf,
    pub scratch: PathBuf,
    pub packages: PathBuf,
    pub logs: PathBuf,
    pub run: PathBuf,
    pub socket: PathBuf,
    pub provenance: PathBuf,
    pub pharos: PathBuf,
    pub pinned_cache: PathBuf,
    pub sessions: PathBuf,
    pub receipts: PathBuf,
    pub config: PathBuf,
}

impl PrivateLabLayout {
    pub fn from_root(root: &Path) -> Self {
        let root = util::resolve(root);
        let run = root.join("run");
        let socket = run.join("daemon.sock");
        Self {
            workspace: root.join("workspace"),
            state: root.join("state"),
            worlds: root.join("worlds"),
            scratch: root.join("scratch"),
            packages: root.join("packages"),
            logs: root.join("logs"),
            provenance: root.join("provenance"),
            pharos: root.join("pharos"),
            pinned_cache: root.join("pinned_cache"),
            sessions: root.join("sessions"),
            receipts: root.join("receipts"),
            config: root.join("config"),
            run,
            socket,
            root,
        }
    }

    /// Creates the root and every private lab subdirectory with strict `0o700`
    /// permissions, refusing symlinks or foreign-owned directories.
    pub fn ensure_dirs(&self) -> Result<(), String> {
        ensure_private_dir(&self.root)?;
        for dir in [
            &self.workspace,
            &self.state,
            &self.worlds,
            &self.scratch,
            &self.packages,
            &self.logs,
            &self.run,
            &self.provenance,
            &self.pharos,
            &self.pinned_cache,
            &self.sessions,
            &self.receipts,
            &self.config,
        ] {
            ensure_private_dir(dir)?;
        }
        Ok(())
    }

    /// Environment bindings that route every Palette state, log, cache, socket,
    /// and workspace artifact strictly inside `self.root` and disable shared
    /// TeamLab access.
    pub fn env_bindings(&self, strict: bool) -> Vec<(&'static str, String)> {
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut out = vec![
            ("PALETTE_HOME", s(&self.root)),
            ("PALETTE_PRIVATE_DIR", s(&self.root)),
            ("PALETTE_DATA_HOME", s(&self.root)),
            ("PALETTE_PRIVATE_MODE", "1".into()),
            ("OPERATOR_WORKSPACE", s(&self.workspace)),
            ("PALETTE_STATE_DIR", s(&self.state)),
            ("PALETTE_WORKSPACE_STATE_ROOT", s(&self.worlds)),
            ("PALETTE_SCRATCH_ROOT", s(&self.scratch)),
            ("PALETTE_PACKAGE_DEPOT", s(&self.packages)),
            ("PALETTE_LOGS_DIR", s(&self.logs)),
            ("PALETTE_PROVENANCE_DIR", s(&self.provenance)),
            ("PALETTE_PHAROS_CACHE_DIR", s(&self.pharos)),
            ("PALETTE_PINNED_CACHE_DIR", s(&self.pinned_cache)),
            ("PALETTE_TEAMLAB_DIR", "off".into()),
        ];
        if strict {
            out.push(("PALETTE_PRIVATE_STRICT", "1".into()));
        }
        out
    }
}

/// Creates `dir` (recursively) and enforces `0o700` permissions on `dir`,
/// refusing symlinks or directories owned by another user.
pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("creating private dir {}: {e}", dir.display()))?;
    let meta = fs::symlink_metadata(dir)
        .map_err(|e| format!("checking private dir {}: {e}", dir.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(format!(
            "private lab path must be a real directory (not a symlink): {}",
            dir.display()
        ));
    }
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        return Err(format!(
            "private lab directory {} is owned by uid {}, expected uid {uid}",
            dir.display(),
            meta.uid()
        ));
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("chmod 0700 {}: {e}", dir.display()))?;
    Ok(())
}

/// Verifies `path` stays strictly inside `root` (after resolving symlinks/`..`).
pub fn assert_contained(path: &Path, root: &Path, label: &str) -> Result<(), String> {
    let resolved_path = util::resolve(path);
    let resolved_root = util::resolve(root);
    if !util::within(&resolved_path, &resolved_root) {
        return Err(format!(
            "private lab containment violation: {label} ({}) escapes PALETTE_HOME ({})",
            resolved_path.display(),
            resolved_root.display()
        ));
    }
    Ok(())
}

/// True if a `/proc/<pid>/comm` value matches a known builder process name.
pub fn matches_builder_comm(comm: &str) -> bool {
    let normalized = comm.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }
    DEFAULT_BUILDER_COMMS.iter().any(|&known| {
        normalized == known
            || normalized.starts_with(&format!("{known}-"))
            || normalized.starts_with(&format!("{known}_"))
            || normalized.starts_with(&format!("{known}."))
            || (known == "antigravity" && normalized.starts_with("antigravity"))
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuilderProcess {
    pub pid: u32,
    pub comm: String,
    pub uid: u32,
    pub mnt_ns: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IsolationAudit {
    pub isolated: bool,
    pub own_pid: u32,
    pub own_uid: u32,
    pub own_mnt_ns: Option<String>,
    pub unboxed_builders: Vec<BuilderProcess>,
    pub boxed_builders: Vec<BuilderProcess>,
    pub other_uid_builders: Vec<BuilderProcess>,
    pub diagnostic: String,
}

fn parse_status_uids(status: &str) -> Option<(u32, u32)> {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            let mut parts = rest.split_whitespace();
            let real = parts.next()?.parse::<u32>().ok()?;
            let eff = parts.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(real);
            return Some((real, eff));
        }
    }
    None
}

/// Scans `proc_root` (normally `/proc`) for running builder processes matching
/// `DEFAULT_BUILDER_COMMS` that share both our UID and our mount namespace.
pub fn audit_private_isolation_in(
    proc_root: &Path,
    own_pid: u32,
    own_uid: u32,
    own_mnt_ns: Option<String>,
) -> IsolationAudit {
    let mut unboxed_builders = Vec::new();
    let mut boxed_builders = Vec::new();
    let mut other_uid_builders = Vec::new();

    if let Ok(entries) = fs::read_dir(proc_root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let Ok(pid) = name_str.parse::<u32>() else {
                continue;
            };
            if pid == own_pid {
                continue;
            }
            let pdir = entry.path();
            let Ok(comm_raw) = fs::read_to_string(pdir.join("comm")) else {
                continue;
            };
            let comm = comm_raw.trim().to_string();
            if !matches_builder_comm(&comm) {
                continue;
            }
            let Ok(status) = fs::read_to_string(pdir.join("status")) else {
                continue;
            };
            let Some((ruid, euid)) = parse_status_uids(&status) else {
                continue;
            };
            let mnt_ns = fs::read_link(pdir.join("ns/mnt"))
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            let proc_info = BuilderProcess {
                pid,
                comm,
                uid: ruid,
                mnt_ns: mnt_ns.clone(),
            };
            if ruid != own_uid && euid != own_uid {
                other_uid_builders.push(proc_info);
            } else if own_mnt_ns.is_some() && mnt_ns.is_some() && mnt_ns != own_mnt_ns {
                boxed_builders.push(proc_info);
            } else {
                // Same UID and same mount namespace (or unreadable ns for same UID: fail closed).
                unboxed_builders.push(proc_info);
            }
        }
    }

    unboxed_builders.sort_by_key(|p| p.pid);
    boxed_builders.sort_by_key(|p| p.pid);
    other_uid_builders.sort_by_key(|p| p.pid);

    let isolated = unboxed_builders.is_empty();
    let diagnostic = if isolated {
        format!(
            "private lab isolation OK (uid={own_uid}, mnt_ns={}, boxed_builders={}, other_uid_builders={})",
            own_mnt_ns.as_deref().unwrap_or("unknown"),
            boxed_builders.len(),
            other_uid_builders.len()
        )
    } else {
        let offenders: Vec<String> = unboxed_builders
            .iter()
            .map(|p| {
                format!(
                    "pid={} comm={} uid={} mnt_ns={}",
                    p.pid,
                    p.comm,
                    p.uid,
                    p.mnt_ns.as_deref().unwrap_or("unreadable")
                )
            })
            .collect();
        format!(
            "unboxed same-user builder process(es) detected in the same mount namespace (uid={own_uid}, mnt_ns={}): [{}]. \
             A same-UID process in the same mount namespace can read 0o700 files under PALETTE_HOME. \
             Real fixes: (1) run builders inside `builder-box` (separate mount namespace hiding PALETTE_HOME), \
             or (2) run this Private Lab under a dedicated Linux user (e.g. `inquisitor`) owning 0o700 PALETTE_HOME.",
            own_mnt_ns.as_deref().unwrap_or("unknown"),
            offenders.join(", ")
        )
    };

    IsolationAudit {
        isolated,
        own_pid,
        own_uid,
        own_mnt_ns,
        unboxed_builders,
        boxed_builders,
        other_uid_builders,
        diagnostic,
    }
}

/// Runs the live `/proc` private-lab isolation audit for this process.
pub fn audit_private_isolation() -> IsolationAudit {
    let own_pid = std::process::id();
    let own_uid = unsafe { libc::getuid() };
    let own_mnt_ns = fs::read_link("/proc/self/ns/mnt")
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    audit_private_isolation_in(Path::new("/proc"), own_pid, own_uid, own_mnt_ns)
}

/// Enforces private-lab isolation at startup: refuses to start when `strict` is
/// true and unboxed same-user builders are present; emits a structured warning
/// on stderr when `strict` is false.
pub fn enforce_private_isolation(strict: bool) -> Result<IsolationAudit, String> {
    let audit = audit_private_isolation();
    if !audit.isolated {
        if strict {
            return Err(format!("PALETTE_PRIVATE_STRICT refusal: {}", audit.diagnostic));
        }
        eprintln!("[palette:private-lab WARNING] {}", audit.diagnostic);
    }
    Ok(audit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::symlink;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mk_fake_proc_entry(
        proc_root: &Path,
        pid: u32,
        comm: &str,
        uid: u32,
        mnt_ns: &str,
    ) {
        let dir = proc_root.join(pid.to_string());
        fs::create_dir_all(dir.join("ns")).unwrap();
        fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
        fs::write(
            dir.join("status"),
            format!("Name:\t{comm}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        symlink(mnt_ns, dir.join("ns/mnt")).unwrap();
    }

    #[test]
    fn resolves_private_root_and_enforces_0700_single_root_layout() {
        let tmp = Tmp(util::resolve(&util::mkdtemp("palette-priv-").unwrap()));
        let home = tmp.0.join("inquisitor-lab");
        let vars = HashMap::from([("PALETTE_HOME".to_string(), home.to_string_lossy().into_owned())]);
        let resolved = resolve_private_root(None, &|k| vars.get(k).cloned()).unwrap().unwrap();
        assert_eq!(resolved, home);

        let layout = PrivateLabLayout::from_root(&resolved);
        layout.ensure_dirs().unwrap();
        assert_eq!(fs::metadata(&layout.root).unwrap().permissions().mode() & 0o777, 0o700);
        for d in [
            &layout.workspace,
            &layout.state,
            &layout.worlds,
            &layout.scratch,
            &layout.packages,
            &layout.logs,
            &layout.run,
            &layout.provenance,
            &layout.pharos,
            &layout.pinned_cache,
            &layout.sessions,
            &layout.receipts,
            &layout.config,
        ] {
            assert_eq!(fs::metadata(d).unwrap().permissions().mode() & 0o777, 0o700);
            assert_contained(d, &layout.root, "subdir").unwrap();
        }
        assert!(assert_contained(&tmp.0.join("outside"), &layout.root, "leak").is_err());

        // PALETTE_PRIVATE_MODE=1 without any root directory fails closed.
        let bare = HashMap::from([("PALETTE_PRIVATE_MODE".to_string(), "1".to_string())]);
        let err = resolve_private_root(None, &|k| bare.get(k).cloned()).unwrap_err();
        assert!(err.contains("requires PALETTE_HOME"));
    }

    #[test]
    fn proc_audit_distinguishes_unboxed_vs_boxed_vs_separate_user_builders() {
        let tmp = Tmp(util::resolve(&util::mkdtemp("palette-proc-audit-").unwrap()));
        let proc_root = tmp.0.join("proc");
        fs::create_dir_all(&proc_root).unwrap();

        let my_pid = 1000;
        let my_uid = 1001;
        let my_ns = "mnt:[4026531840]";

        // Self process (must be ignored)
        mk_fake_proc_entry(&proc_root, my_pid, "gemini", my_uid, my_ns);
        // Non-builder same UID + same ns (safe)
        mk_fake_proc_entry(&proc_root, 1001, "julia", my_uid, my_ns);
        // Builder in builder-box (same UID, DIFFERENT mount namespace -> boxed, safe)
        mk_fake_proc_entry(&proc_root, 1002, "grok", my_uid, "mnt:[4026539999]");
        // Builder under a different Linux user (DIFFERENT UID, same ns -> isolated, safe)
        mk_fake_proc_entry(&proc_root, 1003, "antigravity", 2002, my_ns);

        let clean = audit_private_isolation_in(&proc_root, my_pid, my_uid, Some(my_ns.into()));
        assert!(clean.isolated, "expected isolated, got: {}", clean.diagnostic);
        assert!(clean.unboxed_builders.is_empty());
        assert_eq!(clean.boxed_builders.len(), 1);
        assert_eq!(clean.boxed_builders[0].pid, 1002);
        assert_eq!(clean.other_uid_builders.len(), 1);
        assert_eq!(clean.other_uid_builders[0].pid, 1003);

        // Now introduce unboxed same-UID same-ns builders (`agy` and `gemini`)
        mk_fake_proc_entry(&proc_root, 1004, "agy", my_uid, my_ns);
        mk_fake_proc_entry(&proc_root, 1005, "gemini", my_uid, my_ns);

        let dirty = audit_private_isolation_in(&proc_root, my_pid, my_uid, Some(my_ns.into()));
        assert!(!dirty.isolated);
        let pids: Vec<u32> = dirty.unboxed_builders.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![1004, 1005]);
        assert!(dirty.diagnostic.contains("pid=1004 comm=agy"));
        assert!(dirty.diagnostic.contains("pid=1005 comm=gemini"));
        assert!(dirty.diagnostic.contains("builder-box"));
        assert!(dirty.diagnostic.contains("inquisitor"));
    }
}
