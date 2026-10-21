//! The host-owned registry of Palette backend generations, ported from
//! `generation_runtime.py`.
//!
//! A generation is a Palette checkout and the `palette-host` that runs its
//! sessions. `builtin` is the checkout and host this router runs from; others
//! are manifests at `<root>/generations/<id>/generation.json`, and
//! `<root>/active-generation.json` names the active one. A manifest may still
//! carry the Python adapter's path as `adapter`; it is accepted and unused.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{atomic_json, valid_id, Fault};
use crate::util;

const FIELDS: [&str; 5] = ["format", "generation_id", "repo", "adapter", "host"];
const HOST_IN_REPO: &str = "runtime/host/target/release/palette-host";

fn fault(message: impl Into<String>) -> Fault {
    Fault::new("GenerationError", message)
}

fn valid(id: &str) -> Result<&str, Fault> {
    if valid_id(id) { Ok(id) } else { Err(Fault::value("Invalid generation_id")) }
}

fn is_format_1(v: Option<&Value>) -> bool {
    v.and_then(Value::as_f64) == Some(1.0)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Generation {
    pub id: String,
    pub repo: PathBuf,
    pub host: PathBuf,
}

pub(crate) struct GenerationStore {
    manifests: PathBuf,
    pointer: PathBuf,
    fallback_repo: PathBuf,
    fallback_host: PathBuf,
}

impl GenerationStore {
    pub fn new(root: &Path, fallback_repo: &Path, fallback_host: &Path) -> GenerationStore {
        let root = util::resolve(root);
        GenerationStore {
            manifests: root.join("generations"),
            pointer: root.join("active-generation.json"),
            fallback_repo: util::resolve(fallback_repo),
            fallback_host: util::resolve(fallback_host),
        }
    }

    /// The active generation's id; `builtin` until one is activated.
    pub fn current_id(&self) -> Result<String, Fault> {
        let malformed = || fault("Active generation pointer is malformed");
        let text = match fs::read_to_string(&self.pointer) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok("builtin".into()),
            r => r.map_err(|_| malformed())?,
        };
        let raw: Value = serde_json::from_str(&text).map_err(|_| malformed())?;
        let fields_ok = raw.as_object().is_some_and(|o| o.len() == 2 && o.contains_key("generation_id"));
        if !fields_ok || !is_format_1(raw.get("format")) {
            return Err(malformed());
        }
        Ok(valid(raw["generation_id"].as_str().unwrap_or(""))?.to_string())
    }

    /// The active generation, or the one named, checked to be complete.
    pub fn resolve(&self, id: Option<&str>) -> Result<Generation, Fault> {
        let id = match id {
            Some(id) => valid(id)?.to_string(),
            None => self.current_id()?,
        };
        if id == "builtin" {
            if !self.fallback_host.is_file() {
                return Err(fault(format!("Builtin host is missing: {}", self.fallback_host.display())));
            }
            return Ok(Generation { id, repo: self.fallback_repo.clone(), host: self.fallback_host.clone() });
        }

        let malformed = || fault(format!("Generation manifest is malformed: {id}"));
        let text = match fs::read_to_string(self.manifests.join(&id).join("generation.json")) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Err(fault(format!("Unknown generation_id: {id}"))),
            r => r.map_err(|_| malformed())?,
        };
        let raw: Value = serde_json::from_str(&text).map_err(|_| malformed())?;
        let Some(fields) = raw.as_object() else { return Err(malformed()) };
        if fields.keys().any(|k| !FIELDS.contains(&k.as_str()))
            || !is_format_1(fields.get("format"))
            || fields.get("generation_id").and_then(Value::as_str) != Some(id.as_str())
        {
            return Err(malformed());
        }
        // A path field: a string, or empty/absent for the default.
        let path = |field: &str| match fields.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.is_empty() => Ok(None),
            Some(Value::String(s)) => Ok(Some(util::expand_user(s))),
            Some(_) => Err(malformed()),
        };
        let repo = path("repo")?.unwrap_or_default();
        let host = path("host")?.unwrap_or_else(|| repo.join(HOST_IN_REPO));
        if !repo.is_absolute() || !host.is_absolute() {
            return Err(fault("Generation paths must be absolute"));
        }
        let (repo, host) = (util::resolve(&repo), util::resolve(&host));
        if !repo.is_dir() || !host.is_file() {
            return Err(fault(format!("Generation files are incomplete: {id}")));
        }
        Ok(Generation { id, repo, host })
    }

    /// Makes the named generation active once it checks out complete.
    pub fn activate(&self, id: &str) -> Result<Generation, Fault> {
        let generation = self.resolve(Some(id))?;
        atomic_json(&self.pointer, &json!({"format": 1, "generation_id": generation.id})).map_err(Fault::io)?;
        Ok(generation)
    }

    /// `builtin` and every generation with a manifest, sorted.
    pub fn available(&self) -> Vec<String> {
        let mut ids = vec!["builtin".to_string()];
        for entry in fs::read_dir(&self.manifests).into_iter().flatten().flatten() {
            let (name, path) = (entry.file_name().to_string_lossy().into_owned(), entry.path());
            if path.is_dir() && valid_id(&name) && path.join("generation.json").is_file() {
                ids.push(name);
            }
        }
        ids.sort();
        ids.dedup();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn setup() -> (Tmp, GenerationStore) {
        let t = Tmp(util::resolve(&util::mkdtemp("palette-generations-").unwrap()));
        fs::create_dir_all(t.0.join("repo")).unwrap();
        fs::write(t.0.join("host"), "").unwrap();
        let store = GenerationStore::new(&t.0.join("gens"), &t.0.join("repo"), &t.0.join("host"));
        (t, store)
    }

    fn manifest(t: &Tmp, id: &str, value: Value) {
        let dir = t.0.join("gens/generations").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("generation.json"), value.to_string()).unwrap();
    }

    #[test]
    fn builtin_until_another_is_activated() {
        let (t, store) = setup();
        assert_eq!(store.current_id().unwrap(), "builtin");
        let builtin = store.resolve(None).unwrap();
        assert_eq!((builtin.repo, builtin.host), (t.0.join("repo"), t.0.join("host")));

        let repo2 = t.0.join("repo2");
        fs::create_dir_all(repo2.join("runtime/host/target/release")).unwrap();
        fs::write(repo2.join(HOST_IN_REPO), "").unwrap();
        manifest(&t, "g2", json!({"format": 1, "generation_id": "g2", "repo": repo2, "adapter": "/legacy/operator_mcp.py"}));
        assert_eq!(store.available(), ["builtin", "g2"]);
        let g2 = store.activate("g2").unwrap();
        assert_eq!(g2.host, repo2.join(HOST_IN_REPO));
        assert_eq!(store.current_id().unwrap(), "g2");
        assert_eq!(store.resolve(None).unwrap(), g2);
        assert_eq!(fs::read_to_string(t.0.join("gens/active-generation.json")).unwrap(), "{\"format\":1,\"generation_id\":\"g2\"}\n");
        store.activate("builtin").unwrap();
        assert_eq!(store.current_id().unwrap(), "builtin");
    }

    #[test]
    fn bad_pointers_and_manifests_are_refused() {
        let (t, store) = setup();
        let msg = |r: Result<Generation, Fault>| r.unwrap_err().to_string();
        assert_eq!(msg(store.resolve(Some("../x"))), "ValueError: Invalid generation_id");
        assert_eq!(msg(store.resolve(Some("nope"))), "GenerationError: Unknown generation_id: nope");
        for bad in [json!([]), json!({"format": 2, "generation_id": "g"}), json!({"format": 1, "generation_id": "other"}),
                    json!({"format": 1, "generation_id": "g", "extra": 1}), json!({"format": 1, "generation_id": "g", "repo": 5})] {
            manifest(&t, "g", bad);
            assert_eq!(msg(store.resolve(Some("g"))), "GenerationError: Generation manifest is malformed: g");
        }
        manifest(&t, "g", json!({"format": 1, "generation_id": "g", "repo": "rel"}));
        assert_eq!(msg(store.resolve(Some("g"))), "GenerationError: Generation paths must be absolute");
        manifest(&t, "g", json!({"format": 1, "generation_id": "g", "repo": t.0.join("repo")}));
        assert_eq!(msg(store.resolve(Some("g"))), "GenerationError: Generation files are incomplete: g");
        assert_eq!(msg(store.activate("g")), "GenerationError: Generation files are incomplete: g");
        assert_eq!(store.current_id().unwrap(), "builtin");

        let pointer = t.0.join("gens/active-generation.json");
        for bad in ["{", "[]", "{\"format\":1}", "{\"format\":1,\"generation_id\":\"g\",\"x\":1}", "{\"format\":true,\"generation_id\":\"g\"}"] {
            fs::write(&pointer, bad).unwrap();
            assert_eq!(store.current_id().unwrap_err().message, "Active generation pointer is malformed");
        }
        fs::write(&pointer, "{\"format\":1,\"generation_id\":\"-x\"}").unwrap();
        assert_eq!(store.current_id().unwrap_err().message, "Invalid generation_id");

        let missing = GenerationStore::new(&t.0.join("gens"), &t.0.join("repo"), &t.0.join("no-host"));
        assert_eq!(msg(missing.resolve(Some("builtin"))), format!("GenerationError: Builtin host is missing: {}", t.0.join("no-host").display()));
    }
}
