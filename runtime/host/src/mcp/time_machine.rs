//! The host side of the Time Machine (src/time_machine.jl), ported from
//! `time_machine.py`: lists checkpoints without a kernel and asks the next
//! kernel to restore one.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::{atomic_json, Fault};
use crate::util;

const KEPT_ACROSS_FRESH_WORLDS: [&str; 2] = ["checkpoints", "blobs"];
const KEPT_RETIRED_WORLDS: usize = 3;

/// `[A-Za-z0-9._-]{1,64}`, as CHECKPOINT_NAME in src/time_machine.jl.
fn checkpoint_name(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn read_object(path: &Path) -> Option<Map<String, Value>> {
    match serde_json::from_str(&fs::read_to_string(path).ok()?).ok()? {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

fn pick(record: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter().map(|k| (k.to_string(), record.get(*k).cloned().unwrap_or(Value::Null))).collect()
}

/// The world record and every complete checkpoint, oldest first.
pub(crate) fn list_checkpoints(state_dir: &Path) -> Value {
    let world = read_object(&state_dir.join("world.json")).map(|w| Value::Object(pick(&w, &["id", "parent", "created"])));
    let mut records: Vec<_> = fs::read_dir(state_dir.join("checkpoints")).into_iter().flatten().flatten()
        .filter_map(|entry| read_object(&entry.path().join("checkpoint.json")))
        .map(|r| pick(&r, &["name", "call", "created", "note", "world"]))
        .collect();
    let key = |r: &Map<String, Value>, k: &str| r[k].as_str().unwrap_or("").to_string();
    records.sort_by_cached_key(|r| (key(r, "created"), key(r, "name")));
    json!({"world": world, "checkpoints": records})
}

/// Writes restore.json, which the next kernel applies before revival. An
/// absent `fork` is false.
pub(crate) fn request_restore(state_dir: &Path, checkpoint: Option<&Value>, fork: Option<&Value>) -> Result<(), Fault> {
    let Some(name) = checkpoint.and_then(Value::as_str).filter(|s| checkpoint_name(s)) else {
        return Err(Fault::value("checkpoint must be a checkpoint name (1-64 letters, digits, '.', '_' or '-')"));
    };
    let Some(fork) = fork.map_or(Some(false), Value::as_bool) else {
        return Err(Fault::value("fork must be a boolean"));
    };
    if !state_dir.join("checkpoints").join(name).join("checkpoint.json").is_file() {
        let listed = list_checkpoints(state_dir);
        let names: Vec<_> = listed["checkpoints"].as_array().into_iter().flatten()
            .map(|r| r["name"].as_str().map_or_else(|| r["name"].to_string(), str::to_string)).collect();
        let names = if names.is_empty() { "none".to_string() } else { names.join(", ") };
        return Err(Fault::value(format!("no checkpoint is named {}; checkpoints: {names}", util::py_str(name))));
    }
    atomic_json(&state_dir.join("restore.json"), &json!({"checkpoint": name, "fork": fork})).map_err(Fault::io)
}

/// Moves the world aside for a fresh one, which keeps its checkpoints, and
/// removes all but the newest KEPT_RETIRED_WORLDS worlds retired before.
/// Returns the path the world was moved to.
pub(crate) fn retire_world(state_dir: &Path) -> Result<PathBuf, Fault> {
    let name = state_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let prefix = format!("{name}.retired-");
    let retired = state_dir.with_file_name(format!("{prefix}{}", util::hex(32)));
    fs::rename(state_dir, &retired)
        .map_err(|e| Fault::new("OSError", format!("{e}: {} -> {}", state_dir.display(), retired.display())))?;
    carry_checkpoints(&retired, state_dir).map_err(Fault::io)?;

    let parent = state_dir.parent().unwrap_or(Path::new("."));
    let mut worlds: Vec<_> = fs::read_dir(parent).into_iter().flatten().flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        // Real directories only: a symlink planted beside the state never
        // leads the cleanup elsewhere.
        .filter_map(|e| e.metadata().ok().filter(|m| m.is_dir()).and_then(|m| m.modified().ok()).map(|t| (t, e.path())))
        .collect();
    worlds.sort();
    for (_, p) in &worlds[..worlds.len().saturating_sub(KEPT_RETIRED_WORLDS)] {
        if *p != retired {
            let _ = fs::remove_dir_all(p);
        }
    }
    Ok(retired)
}

/// A fresh world keeps the checkpoints of the one it replaced.
pub(crate) fn carry_checkpoints(retired: &Path, state_dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    for name in KEPT_ACROSS_FRESH_WORLDS {
        if retired.join(name).is_dir() {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(state_dir)?;
            fs::rename(retired.join(name), state_dir.join(name))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn checkpoint(state: &Path, name: &str, record: Value) {
        let dir = state.join("checkpoints").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("checkpoint.json"), record.to_string()).unwrap();
    }

    #[test]
    fn lists_and_restores_checkpoints() {
        let t = Tmp(util::mkdtemp("palette-tm-").unwrap());
        let state = t.0.join("state");
        assert_eq!(list_checkpoints(&state), json!({"world": null, "checkpoints": []}));
        fs::create_dir_all(state.join("checkpoints/broken")).unwrap();
        fs::write(state.join("world.json"), r#"{"id":"w1","parent":null,"created":"2026","extra":1}"#).unwrap();
        checkpoint(&state, "b", json!({"name": "b", "call": 2, "created": "2026-10-20T02", "secret": 1}));
        checkpoint(&state, "a", json!({"name": "a", "call": 1, "created": "2026-10-20T01", "note": "first"}));
        checkpoint(&state, "odd", json!([1]));
        let listed = list_checkpoints(&state);
        assert_eq!(listed["world"], json!({"id": "w1", "parent": null, "created": "2026"}));
        assert_eq!(listed["checkpoints"][0], json!({"name": "a", "call": 1, "created": "2026-10-20T01", "note": "first", "world": null}));
        assert_eq!(listed["checkpoints"].as_array().unwrap().len(), 2);

        let err = |c: Value, f: Option<Value>| request_restore(&state, Some(&c), f.as_ref()).unwrap_err().to_string();
        assert_eq!(err(json!("../a"), None), "ValueError: checkpoint must be a checkpoint name (1-64 letters, digits, '.', '_' or '-')");
        assert_eq!(err(json!("a"), Some(json!(null))), "ValueError: fork must be a boolean");
        assert_eq!(err(json!("zz"), None), "ValueError: no checkpoint is named 'zz'; checkpoints: a, b");
        assert!(request_restore(&state, None, None).is_err());
        request_restore(&state, Some(&json!("b")), None).unwrap();
        assert_eq!(fs::read_to_string(state.join("restore.json")).unwrap(), "{\"checkpoint\":\"b\",\"fork\":false}\n");
        request_restore(&state, Some(&json!("a")), Some(&json!(true))).unwrap();
        assert_eq!(fs::read_to_string(state.join("restore.json")).unwrap(), "{\"checkpoint\":\"a\",\"fork\":true}\n");

        let empty = t.0.join("empty");
        assert_eq!(request_restore(&empty, Some(&json!("a")), None).unwrap_err().message, "no checkpoint is named 'a'; checkpoints: none");
    }

    #[test]
    fn retiring_keeps_checkpoints_and_three_old_worlds() {
        let t = Tmp(util::mkdtemp("palette-tm-").unwrap());
        let state = t.0.join("state");
        let outside = t.0.join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, t.0.join("state.retired-link")).unwrap();
        let mut retired = Vec::new();
        for i in 0..5 {
            checkpoint(&state, &format!("c{i}"), json!({"name": format!("c{i}")}));
            fs::write(state.join("world.json"), format!("{{\"id\":\"w{i}\"}}")).unwrap();
            let r = retire_world(&state).unwrap();
            assert!(r.file_name().unwrap().to_string_lossy().starts_with("state.retired-"));
            assert!(r.join("world.json").is_file() && !r.join("checkpoints").exists());
            assert!(!state.join("world.json").exists());
            assert_eq!(list_checkpoints(&state)["checkpoints"].as_array().unwrap().len(), i + 1);
            retired.push(r);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let left: Vec<_> = retired.iter().filter(|p| p.exists()).collect();
        assert_eq!(left, retired[2..].iter().collect::<Vec<_>>());
        assert!(outside.is_dir() && t.0.join("state.retired-link").exists());
    }
}
