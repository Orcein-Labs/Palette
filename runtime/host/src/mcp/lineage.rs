//! Canonical durable state for project-scoped workspaces, ported from
//! `project_lineage.py`.
//!
//! Project scope is a continuity boundary, not a caller or workspace-name
//! boundary: every alias that selects the same configured project_root_id
//! converges on one state directory, `<root>/lineages/<id>`. Thread and open
//! workspaces keep their own, `<root>/states/<id>`.
//!
//! Migration from the old per-workspace layout fails closed: a legacy state
//! another router holds is never moved, two non-empty legacy project states
//! are never merged, and registry paths outside the old or canonical layouts
//! are refused.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use super::{valid_id, Fault};
use crate::util;

fn valid<'a>(id: &'a str, field: &str) -> Result<&'a str, Fault> {
    if valid_id(id) { Ok(id) } else { Err(Fault::value(format!("Invalid {field}"))) }
}

/// A string field of a registry entry; anything else reads as "", which no
/// id or path check accepts.
fn text<'a>(entry: &'a Map<String, Value>, field: &str) -> &'a str {
    entry.get(field).and_then(Value::as_str).unwrap_or("")
}

/// The stable logical identity exposed to clients and kernels.
pub(crate) fn lineage_id(scope: &str, project_root_id: &str, workspace_id: &str) -> Result<String, Fault> {
    match scope {
        "project" => Ok(format!("project:{}", valid(project_root_id, "project_root_id")?)),
        "thread" | "open" => Ok(format!("workspace:{}", valid(workspace_id, "workspace_id")?)),
        _ => Err(Fault::value("Invalid workspace scope")),
    }
}

pub(crate) fn project_state_dir(root: &Path, project_root_id: &str) -> Result<PathBuf, Fault> {
    Ok(util::resolve(root).join("lineages").join(valid(project_root_id, "project_root_id")?))
}

pub(crate) fn workspace_state_dir(root: &Path, workspace_id: &str) -> Result<PathBuf, Fault> {
    Ok(util::resolve(root).join("states").join(valid(workspace_id, "workspace_id")?))
}

/// The lock a router holds while it owns `state_dir`, beside it so a worker
/// cannot unlink it.
pub(crate) fn owner_lock_path(state_dir: &Path) -> PathBuf {
    let state = util::resolve(state_dir);
    let name = state.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    state.with_file_name(name + ".owner.lock")
}

/// True for a directory with entries, false for none at all; anything but a
/// real directory is refused.
fn is_nonempty_dir(path: &Path) -> Result<bool, Fault> {
    if fs::metadata(path).is_err() {
        return Ok(false);
    }
    let meta = fs::symlink_metadata(path).map_err(Fault::io)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(Fault::permission(format!("Palette state path must be a real directory: {}", path.display())));
    }
    Ok(fs::read_dir(path).map_err(Fault::io)?.next().is_some())
}

fn mkdir_private(dir: &Path) -> Result<(), Fault> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(Fault::io)
}

/// Takes the legacy owner's own lock before moving its state; dropping the
/// file releases it.
fn acquire_migration_lock(state_dir: &Path) -> Result<File, Fault> {
    let path = owner_lock_path(state_dir);
    if let Some(parent) = path.parent() {
        mkdir_private(parent)?;
    }
    let lock = OpenOptions::new().read(true).append(true).create(true).mode(0o600).open(&path).map_err(Fault::io)?;
    // A child another thread forked holds a copy of a just-released lock
    // until it execs, so a busy lock is retried briefly before it counts.
    let deadline = Instant::now() + Duration::from_millis(500);
    while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(Fault::io(e));
        }
        if Instant::now() >= deadline {
            return Err(Fault::new("LineageBusy", format!(
                "Project lineage migration is blocked by an active legacy owner: {}", state_dir.display())));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(lock)
}

/// Migrates every project alias of one configured project root to the
/// canonical lineage, and points their registry entries at it. Returns the
/// lineage's state directory and id.
pub(crate) fn ensure_project_lineage(registry: &mut Value, root: &Path, project_root_id: &str) -> Result<(PathBuf, String), Fault> {
    let root = util::resolve(root);
    let target = project_state_dir(&root, project_root_id)?;
    let lid = lineage_id("project", project_root_id, project_root_id)?;
    let mut empty = Map::new();
    let workspaces = registry.get_mut("workspaces").and_then(Value::as_object_mut).unwrap_or(&mut empty);

    // (workspace id, its legacy state directory) for each alias.
    let mut aliases = Vec::new();
    for (id, raw) in workspaces.iter() {
        let Some(raw) = raw.as_object() else { continue };
        let root_id = raw.get("project_root_id").map_or(Some("default"), Value::as_str);
        if raw.get("scope").and_then(Value::as_str) != Some("project") || root_id != Some(project_root_id) {
            continue;
        }
        let legacy = workspace_state_dir(&root, valid(id, "workspace_id")?)?;
        let recorded = util::resolve(Path::new(text(raw, "state_dir")));
        if recorded != legacy && recorded != target {
            return Err(Fault::permission(format!(
                "Workspace registry state path differs from the canonical project lineage: {}", recorded.display())));
        }
        aliases.push((id.clone(), legacy));
    }

    let mut legacy_nonempty = Vec::new();
    for (_, legacy) in &aliases {
        if *legacy != target && is_nonempty_dir(legacy)? {
            legacy_nonempty.push(legacy.clone());
        }
    }
    if is_nonempty_dir(&target)? && !legacy_nonempty.is_empty() {
        return Err(Fault::new("LineageConflict", format!(
            "Canonical project lineage and legacy workspace state are both non-empty for {project_root_id}")));
    }
    if legacy_nonempty.len() > 1 {
        return Err(Fault::new("LineageConflict", format!(
            "Multiple non-empty legacy project states exist for {project_root_id}; refusing to choose")));
    }

    // Every state directory this call removes or replaces is locked first,
    // empty ones included: a live owner may not have written anything yet.
    let source = legacy_nonempty.first();
    let mut locks = Vec::new();
    for (_, legacy) in &aliases {
        if *legacy != target && fs::metadata(legacy).is_ok() {
            locks.push(acquire_migration_lock(legacy)?);
        }
    }
    if source.is_some() && fs::metadata(&target).is_ok() {
        locks.push(acquire_migration_lock(&target)?);
    }

    match source {
        Some(source) => {
            if let Some(parent) = target.parent() {
                mkdir_private(parent)?;
            }
            if fs::metadata(&target).is_ok() {
                let meta = fs::symlink_metadata(&target).map_err(Fault::io)?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(Fault::permission(format!("Palette lineage path must be a real directory: {}", target.display())));
                }
                if fs::read_dir(&target).map_err(Fault::io)?.next().is_some() {
                    return Err(Fault::new("LineageConflict", format!(
                        "Project lineage became non-empty during migration: {}", target.display())));
                }
                fs::remove_dir(&target).map_err(Fault::io)?;
            }
            fs::rename(source, &target).map_err(Fault::io)?;
        }
        None => mkdir_private(&target)?,
    }

    for (id, legacy) in &aliases {
        if let Some(entry) = workspaces.get_mut(id).and_then(Value::as_object_mut) {
            entry.insert("lineage_id".into(), lid.clone().into());
            entry.insert("state_dir".into(), target.to_string_lossy().into_owned().into());
        }
        if *legacy != target && fs::metadata(legacy).is_ok() && !is_nonempty_dir(legacy)? {
            fs::remove_dir(legacy).map_err(Fault::io)?;
        }
    }
    drop(locks);
    Ok((target, lid))
}

/// The entry with its canonical lineage id and state directory, migrating a
/// project's legacy state when needed. Refuses an entry whose recorded
/// lineage or state differs from the configured layout.
pub(crate) fn normalize_entry(registry: &mut Value, root: &Path, entry: &Map<String, Value>) -> Result<Map<String, Value>, Fault> {
    let mut value = entry.clone();
    value.entry("project_root_id").or_insert_with(|| "default".into());
    let scope = text(&value, "scope").to_string();
    let id = valid(text(&value, "workspace_id"), "workspace_id")?.to_string();
    let project_root_id = text(&value, "project_root_id").to_string();
    let mut lid = lineage_id(&scope, &project_root_id, &id)?;
    if value.get("lineage_id").is_some_and(|v| !v.is_null() && v.as_str() != Some(&lid)) {
        return Err(Fault::permission("Workspace registry lineage_id differs from the configured layout"));
    }

    let state = if scope == "project" {
        let (state, project_lid) = ensure_project_lineage(registry, root, &project_root_id)?;
        lid = project_lid;
        state
    } else {
        let state = workspace_state_dir(root, &id)?;
        if util::resolve(Path::new(text(&value, "state_dir"))) != state {
            return Err(Fault::permission("Workspace registry state path differs from the configured layout"));
        }
        state
    };
    value.insert("lineage_id".into(), lid.into());
    value.insert("state_dir".into(), state.to_string_lossy().into_owned().into());
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn tmp() -> Tmp {
        Tmp(util::resolve(&util::mkdtemp("palette-lineage-").unwrap()))
    }
    fn project(root: &Path, id: &str, state: &Path) -> Value {
        json!({"workspace_id": id, "scope": "project", "project_root_id": "p1", "state_dir": state, "workspace_dir": root})
    }

    #[test]
    fn ids_and_layout() {
        assert_eq!(lineage_id("project", "p1", "w").unwrap(), "project:p1");
        assert_eq!(lineage_id("thread", "bad id", "w1").unwrap(), "workspace:w1");
        assert_eq!(lineage_id("open", "p", "").unwrap_err().message, "Invalid workspace_id");
        assert_eq!(lineage_id("project", ".p", "w").unwrap_err().message, "Invalid project_root_id");
        assert_eq!(lineage_id("other", "p", "w").unwrap_err().to_string(), "ValueError: Invalid workspace scope");
        assert_eq!(project_state_dir(Path::new("/r/x/.."), "p").unwrap(), Path::new("/r/lineages/p"));
        assert_eq!(workspace_state_dir(Path::new("/r"), "w").unwrap(), Path::new("/r/states/w"));
        assert_eq!(owner_lock_path(Path::new("/r/states/w")), Path::new("/r/states/w.owner.lock"));
    }

    #[test]
    fn a_legacy_project_state_moves_to_the_lineage() {
        let t = tmp();
        let legacy = t.0.join("states/a");
        fs::create_dir_all(legacy.join("world")).unwrap();
        fs::create_dir_all(t.0.join("states/b")).unwrap();
        let mut reg = json!({"workspaces": {
            "a": project(&t.0, "a", &legacy), "b": project(&t.0, "b", &t.0.join("states/b")),
            "c": {"workspace_id": "c", "scope": "thread", "state_dir": "/elsewhere"}}});
        let (state, lid) = ensure_project_lineage(&mut reg, &t.0, "p1").unwrap();
        assert_eq!((state.clone(), lid.as_str()), (t.0.join("lineages/p1"), "project:p1"));
        assert!(state.join("world").is_dir());
        assert!(!legacy.exists() && !t.0.join("states/b").exists());
        assert_eq!(fs::metadata(t.0.join("lineages")).unwrap().permissions().mode() & 0o777, 0o700);
        for id in ["a", "b"] {
            assert_eq!(reg["workspaces"][id]["state_dir"], json!(state));
            assert_eq!(reg["workspaces"][id]["lineage_id"], "project:p1");
        }
        assert_eq!(reg["workspaces"]["c"]["state_dir"], "/elsewhere");
        // A second pass is a no-op.
        assert_eq!(ensure_project_lineage(&mut reg, &t.0, "p1").unwrap().0, state);
    }

    #[test]
    fn migration_refuses_conflicts_and_busy_owners() {
        let t = tmp();
        let (a, b) = (t.0.join("states/a"), t.0.join("states/b"));
        fs::create_dir_all(a.join("x")).unwrap();
        fs::create_dir_all(b.join("y")).unwrap();
        let mut reg = json!({"workspaces": {"a": project(&t.0, "a", &a), "b": project(&t.0, "b", &b)}});
        let e = ensure_project_lineage(&mut reg, &t.0, "p1").unwrap_err();
        assert_eq!(e.to_string(), "LineageConflict: Multiple non-empty legacy project states exist for p1; refusing to choose");

        fs::remove_dir_all(&b).unwrap();
        fs::create_dir_all(t.0.join("lineages/p1/z")).unwrap();
        let e = ensure_project_lineage(&mut reg, &t.0, "p1").unwrap_err();
        assert_eq!(e.message, "Canonical project lineage and legacy workspace state are both non-empty for p1");

        fs::remove_dir_all(t.0.join("lineages/p1/z")).unwrap();
        let held = OpenOptions::new().append(true).create(true).open(owner_lock_path(&a)).unwrap();
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        let e = ensure_project_lineage(&mut reg, &t.0, "p1").unwrap_err();
        assert_eq!(e.to_string(), format!("LineageBusy: Project lineage migration is blocked by an active legacy owner: {}", a.display()));
        assert!(a.join("x").is_dir());
        drop(held);
        ensure_project_lineage(&mut reg, &t.0, "p1").unwrap();
        assert!(t.0.join("lineages/p1/x").is_dir());

        reg["workspaces"]["a"]["state_dir"] = json!("/somewhere/else");
        let e = ensure_project_lineage(&mut reg, &t.0, "p1").unwrap_err();
        assert_eq!(e.to_string(), "PermissionError: Workspace registry state path differs from the canonical project lineage: /somewhere/else");
    }

    #[test]
    fn owned_empty_states_are_never_removed() {
        let t = tmp();
        let (a, b, target) = (t.0.join("states/a"), t.0.join("states/b"), t.0.join("lineages/p1"));
        fs::create_dir_all(a.join("x")).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::create_dir_all(&target).unwrap();
        let mut reg = json!({"workspaces": {"a": project(&t.0, "a", &a), "b": project(&t.0, "b", &b)}});
        for owned in [&b, &target] {
            let held = OpenOptions::new().append(true).create(true).open(owner_lock_path(owned)).unwrap();
            assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
            let e = ensure_project_lineage(&mut reg, &t.0, "p1").unwrap_err();
            assert_eq!(e.kind, "LineageBusy", "{}", owned.display());
            assert!(a.join("x").is_dir() && b.is_dir() && target.is_dir());
        }
        ensure_project_lineage(&mut reg, &t.0, "p1").unwrap();
        assert!(target.join("x").is_dir() && !a.exists() && !b.exists());
    }

    #[test]
    fn a_symlinked_state_is_refused() {
        let t = tmp();
        fs::create_dir_all(t.0.join("real/x")).unwrap();
        fs::create_dir_all(t.0.join("states")).unwrap();
        std::os::unix::fs::symlink(t.0.join("real"), t.0.join("states/a")).unwrap();
        let mut reg = json!({"workspaces": {"a": project(&t.0, "a", &t.0.join("lineages/p1"))}});
        let e = ensure_project_lineage(&mut reg, &t.0, "p1").unwrap_err();
        assert_eq!(e.message, format!("Palette state path must be a real directory: {}", t.0.join("states/a").display()));
    }

    #[test]
    fn normalize_checks_the_recorded_layout() {
        let t = tmp();
        let mut reg = json!({"workspaces": {}});
        let thread = json!({"workspace_id": "w", "scope": "thread", "state_dir": t.0.join("states/w")});
        let out = normalize_entry(&mut reg, &t.0, thread.as_object().unwrap()).unwrap();
        assert_eq!((out["lineage_id"].as_str(), out["project_root_id"].as_str()), (Some("workspace:w"), Some("default")));

        let mut moved = thread.clone();
        moved["state_dir"] = json!("/tmp/w");
        let e = normalize_entry(&mut reg, &t.0, moved.as_object().unwrap()).unwrap_err();
        assert_eq!(e.message, "Workspace registry state path differs from the configured layout");
        let mut relabeled = thread.clone();
        relabeled["lineage_id"] = json!("workspace:other");
        let e = normalize_entry(&mut reg, &t.0, relabeled.as_object().unwrap()).unwrap_err();
        assert_eq!(e.message, "Workspace registry lineage_id differs from the configured layout");
        let e = normalize_entry(&mut reg, &t.0, json!({"workspace_id": 5, "scope": "open"}).as_object().unwrap()).unwrap_err();
        assert_eq!(e.message, "Invalid workspace_id");

        let p = project(&t.0, "a", &t.0.join("states/a"));
        reg["workspaces"]["a"] = p.clone();
        let out = normalize_entry(&mut reg, &t.0, p.as_object().unwrap()).unwrap();
        assert_eq!(out["state_dir"], json!(t.0.join("lineages/p1")));
        assert_eq!(reg["workspaces"]["a"]["state_dir"], out["state_dir"]);
    }
}
