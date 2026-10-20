//! TeamLab: a shelf of Julia tools and a mailbox shared by one user's agents
//! (Claude, Codex, Gemini, ...). The broker keeps it in a host directory no
//! kernel can write (the ceiling's `teamlab.dir`), so a kernel reaches it
//! only through these actions, under the name the session host was given for
//! the calling agent.
//!
//! The directory holds `state.json` (the index: tools and their versions,
//! each agent's pins, the messages), `blobs/<sha256>.jl` (each version's
//! source, read-only, checked against its name on every read) and
//! `lab.lock`. Every action holds a flock on the lock file, shared to read
//! and exclusive to change, so sessions of different agents on one machine
//! see one consistent shelf. The index is replaced atomically and the one it
//! replaced is kept as `state.prev.json`; an index that does not parse is
//! reported and never overwritten.
//!
//! Agent names are not authenticated: they come from the MCP client's own
//! name, so they keep honest agents apart (no approving your own tool, no
//! reading your own mail as new) but do not stop an agent lying about itself.

use crate::broker::Fail::{self, Denied, Internal};
use crate::util;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const FORMAT: u64 = 1;
const MAX_SOURCE_BYTES: usize = 512 * 1024;
const MAX_TOOLS: usize = 256;
const MAX_VERSIONS: usize = 64;
const MAX_MESSAGES: usize = 512;
const MAX_BODY_BYTES: usize = 64 * 1024;
/// All kept message bodies together; the index is rewritten on every change.
const MAX_MAIL_BYTES: usize = 4 * 1024 * 1024;
const MAX_SUBJECT_CHARS: usize = 200;
const MAX_NOTE_CHARS: usize = 2000;
const MAX_SYNC_BYTES: usize = 32 * 1024 * 1024;
/// A broadcast someone has read is kept this long before it may be dropped.
const BROADCAST_KEEP_S: f64 = 7.0 * 86400.0;

const ACTIONS: &[&str] = &["status", "publish", "approve", "pin", "unpin", "update", "sync", "fetch", "send", "inbox", "ack"];

/// Words a tool name cannot be: `sync` binds each tool as `const <name>`.
const JULIA_RESERVED: &[&str] = &[
    "abstract", "baremodule", "begin", "break", "catch", "const", "continue", "do", "else", "elseif", "end",
    "export", "false", "finally", "for", "function", "global", "if", "import", "in", "isa", "let", "local",
    "macro", "module", "mutable", "outer", "primitive", "public", "quote", "return", "struct", "true", "try",
    "type", "using", "where", "while",
];

/// The lab directory a ceiling grants, when its `teamlab` grant names an
/// absolute `dir`.
pub fn ceiling_dir(ceiling: &Value) -> Option<PathBuf> {
    let dir = ceiling.get("teamlab")?.get("dir")?.as_str()?;
    Path::new(dir).is_absolute().then(|| util::resolve(Path::new(dir)))
}

/// 1-32 of `[a-z0-9._-]`, starting with a letter or digit.
pub fn valid_agent(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// The agent a session acts as when no call names one: PALETTE_AGENT, else
/// "local".
pub fn default_agent() -> String {
    std::env::var("PALETTE_AGENT").ok().filter(|a| valid_agent(a)).unwrap_or_else(|| "local".into())
}

fn valid_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !JULIA_RESERVED.contains(&name)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn valid_sha(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn io(what: &Path) -> impl Fn(std::io::Error) -> Fail + '_ {
    move |e| Internal(format!("teamlab {}: {e}", what.display()))
}

struct Lab {
    dir: PathBuf,
}

/// Held for the length of one action; dropping it releases the flock.
struct LabLock(#[allow(dead_code)] File);

impl Lab {
    fn open(dir: &Path) -> Result<Lab, Fail> {
        let blobs = dir.join("blobs");
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&blobs).map_err(io(&blobs))?;
        Ok(Lab { dir: dir.to_path_buf() })
    }

    fn lock(&self, exclusive: bool) -> Result<LabLock, Fail> {
        let path = self.dir.join("lab.lock");
        let f = OpenOptions::new().read(true).write(true).create(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path).map_err(io(&path))?;
        let op = if exclusive { libc::LOCK_EX } else { libc::LOCK_SH };
        loop {
            if unsafe { libc::flock(f.as_raw_fd(), op) } == 0 {
                return Ok(LabLock(f));
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != ErrorKind::Interrupted {
                return Err(io(&path)(e));
            }
        }
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    fn load(&self) -> Result<Map<String, Value>, Fail> {
        let path = self.state_path();
        let text = match fs::read(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(obj(json!({"format": FORMAT, "tools": {}, "pins": {}, "messages": [], "next_message_id": 1})));
            }
            Err(e) => return Err(io(&path)(e)),
        };
        let unreadable = |why: String| Internal(format!(
            "teamlab index {} is unreadable ({why}); it is left as it is and nothing is written until it is \
             repaired or restored from state.prev.json beside it", path.display()));
        let state: Value = serde_json::from_slice(&text).map_err(|e| unreadable(e.to_string()))?;
        let well_formed = state.get("format").and_then(Value::as_u64) == Some(FORMAT)
            && state.get("tools").is_some_and(Value::is_object)
            && state.get("pins").is_some_and(Value::is_object)
            && state.get("messages").is_some_and(Value::is_array)
            && state.get("next_message_id").is_some_and(Value::is_u64);
        if !well_formed {
            return Err(unreadable(format!("not a format-{FORMAT} index")));
        }
        Ok(obj(state))
    }

    /// Replaces the index atomically, keeping the one it replaces.
    fn save(&self, state: &Map<String, Value>) -> Result<(), Fail> {
        let path = self.state_path();
        let tmp = self.dir.join(format!(".state.{}.tmp", util::hex(12)));
        let written = (|| {
            let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&tmp)?;
            f.write_all(&serde_json::to_vec(state).map_err(std::io::Error::other)?)?;
            f.sync_all()?;
            let prev = self.dir.join("state.prev.json");
            match fs::remove_file(&prev) {
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            match fs::hard_link(&path, &prev) {
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            fs::rename(&tmp, &path)?;
            File::open(&self.dir)?.sync_all()
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written.map_err(io(&path))
    }

    fn blob_path(&self, sha: &str) -> PathBuf {
        self.dir.join("blobs").join(format!("{sha}.jl"))
    }

    /// Stores a version's source under its hash. An existing blob is kept
    /// only if it still matches its name.
    fn put_blob(&self, sha: &str, source: &[u8]) -> Result<(), Fail> {
        let path = self.blob_path(sha);
        if self.read_blob(sha).is_ok() {
            return Ok(());
        }
        let _ = fs::remove_file(&path);
        let tmp = self.dir.join("blobs").join(format!(".{sha}.{}.tmp", util::hex(8)));
        let written = (|| {
            let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&tmp)?;
            f.write_all(source)?;
            f.sync_all()?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444))?;
            fs::rename(&tmp, &path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written.map_err(io(&path))
    }

    /// A version's source, refused unless its bytes still hash to its name.
    fn read_blob(&self, sha: &str) -> Result<String, Fail> {
        let path = self.blob_path(sha);
        let mut f = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path).map_err(io(&path))?;
        let mut bytes = Vec::new();
        Read::take(&mut f, MAX_SOURCE_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(io(&path))?;
        if bytes.len() > MAX_SOURCE_BYTES || sha256_hex(&bytes) != sha {
            return Err(Internal(format!("teamlab blob {} was tampered with: its contents no longer match its hash", path.display())));
        }
        String::from_utf8(bytes).map_err(|_| Internal(format!("teamlab blob {} is not UTF-8", path.display())))
    }

    /// Removes the blobs of dropped versions that no tool still uses. Run
    /// after the index is saved, so a crash leaves an unused blob, never an
    /// index entry without its source.
    fn collect(&self, state: &Map<String, Value>, dropped: &[String]) {
        for sha in dropped {
            let used = tools(state).values().filter_map(Value::as_array).flatten()
                .any(|v| v.get("sha256").and_then(Value::as_str) == Some(sha));
            if !used {
                let _ = fs::remove_file(self.blob_path(sha));
            }
        }
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn tools(state: &Map<String, Value>) -> &Map<String, Value> {
    state["tools"].as_object().expect("checked when loaded")
}

fn tools_mut(state: &mut Map<String, Value>) -> &mut Map<String, Value> {
    state.get_mut("tools").and_then(Value::as_object_mut).expect("checked when loaded")
}

fn pins_of(state: &Map<String, Value>, agent: &str) -> Map<String, Value> {
    state["pins"].get(agent).and_then(Value::as_object).cloned().unwrap_or_default()
}

fn pins_mut<'a>(state: &'a mut Map<String, Value>, agent: &str) -> &'a mut Map<String, Value> {
    let pins = state.get_mut("pins").and_then(Value::as_object_mut).expect("checked when loaded");
    let entry = pins.entry(agent).or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    entry.as_object_mut().unwrap()
}

fn messages_mut(state: &mut Map<String, Value>) -> &mut Vec<Value> {
    state.get_mut("messages").and_then(Value::as_array_mut).expect("checked when loaded")
}

fn versions<'a>(state: &'a Map<String, Value>, name: &str) -> Result<&'a Vec<Value>, Fail> {
    tools(state).get(name).and_then(Value::as_array).filter(|v| !v.is_empty())
        .ok_or_else(|| Denied(format!("no tool named {name:?} is on the teamlab shelf")))
}

fn str_param<'a>(p: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, Fail> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(Denied(format!("teamlab '{key}' must be a string"))),
    }
}

fn tool_param<'a>(p: &'a Map<String, Value>) -> Result<&'a str, Fail> {
    let name = str_param(p, "name")?.ok_or_else(|| Denied("teamlab requires a tool 'name'".into()))?;
    if !valid_tool_name(name) {
        return Err(Denied(format!("teamlab tool name {name:?} must be a Julia identifier (ASCII letters, digits and _, at most 64, not a keyword)")));
    }
    Ok(name)
}

fn note_param(p: &Map<String, Value>) -> Result<Value, Fail> {
    match str_param(p, "note")? {
        Some(n) if n.chars().count() > MAX_NOTE_CHARS => Err(Denied(format!("teamlab 'note' is limited to {MAX_NOTE_CHARS} characters"))),
        Some(n) => Ok(json!(n)),
        None => Ok(Value::Null),
    }
}

/// The version `sha256` names, or the newest.
fn pick<'a>(versions: &'a [Value], name: &str, sha: Option<&str>) -> Result<&'a Value, Fail> {
    match sha {
        None => Ok(versions.last().expect("versions are never empty")),
        Some(s) if !valid_sha(s) => Err(Denied("teamlab 'sha256' must be 64 lowercase hex digits".into())),
        Some(s) => versions.iter().find(|v| v["sha256"] == s)
            .ok_or_else(|| Denied(format!("tool {name:?} has no version {s}"))),
    }
}

fn summary(v: &Value) -> Value {
    json!({
        "sha256": v["sha256"], "version": v["version"], "author": v["author"], "published_at": v["published_at"],
        "bytes": v["bytes"], "note": v["note"], "test": v["test"], "approvals": v["approvals"],
    })
}

pub fn handle(dir: &Path, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let action = str_param(p, "action")?.unwrap_or("");
    if !ACTIONS.contains(&action) {
        return Err(Denied(format!("teamlab 'action' must be one of {}", ACTIONS.join(", "))));
    }
    let lab = Lab::open(dir)?;
    let reading = matches!(action, "status" | "sync" | "fetch" | "inbox");
    let _lock = lab.lock(!reading)?;
    let mut state = lab.load()?;
    if reading {
        return match action {
            "status" => Ok(status(&lab, &state, agent)),
            "sync" => sync(&lab, &state, agent),
            "fetch" => fetch(&lab, &state, p),
            _ => inbox(&state, agent, p),
        };
    }
    let mut dropped = Vec::new();
    let result = match action {
        "publish" => publish(&lab, &mut state, agent, p, &mut dropped)?,
        "approve" => approve(&mut state, agent, p)?,
        "pin" => pin(&mut state, agent, p)?,
        "unpin" => unpin(&mut state, agent, p)?,
        "update" => update(&mut state, agent, p)?,
        "send" => send(&mut state, agent, p)?,
        _ => ack(&mut state, agent, p)?,
    };
    lab.save(&state)?;
    lab.collect(&state, &dropped);
    Ok(result)
}

fn unread(state: &Map<String, Value>, agent: &str) -> usize {
    state["messages"].as_array().into_iter().flatten().filter(|m| addressed_to(m, agent) && !acked_by(m, agent)).count()
}

fn addressed_to(m: &Value, agent: &str) -> bool {
    m["from"] != agent && (m["to"] == agent || m["to"] == "all")
}

fn acked_by(m: &Value, agent: &str) -> bool {
    m["acked_by"].as_array().is_some_and(|a| a.iter().any(|x| x == agent))
}

fn status(lab: &Lab, state: &Map<String, Value>, agent: &str) -> Value {
    let pins = pins_of(state, agent);
    let listed: Vec<Value> = tools(state).iter().filter_map(|(name, vs)| {
        let vs = vs.as_array().filter(|v| !v.is_empty())?;
        let latest = vs.last().unwrap();
        let pinned = pins.get(name).and_then(|sha| vs.iter().find(|v| &v["sha256"] == sha));
        Some(json!({
            "name": name, "versions": vs.len(), "latest": summary(latest),
            "pinned": pinned.map(|v| v["sha256"].clone()), "pinned_version": pinned.map(|v| v["version"].clone()),
            "behind": pinned.is_some_and(|v| v["sha256"] != latest["sha256"]),
        }))
    }).collect();
    json!({"agent": agent, "dir": lab.dir, "tools": listed, "unread": unread(state, agent)})
}

fn publish(lab: &Lab, state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>, dropped: &mut Vec<String>)
    -> Result<Value, Fail> {
    let name = tool_param(p)?;
    let source = str_param(p, "source")?.ok_or_else(|| Denied("teamlab publish requires the tool's 'source'".into()))?;
    if source.trim().is_empty() || source.len() > MAX_SOURCE_BYTES {
        return Err(Denied(format!("teamlab source must be non-empty and at most {} KiB", MAX_SOURCE_BYTES / 1024)));
    }
    let note = note_param(p)?;
    let test = match p.get("test") {
        None | Some(Value::Null) => Value::Null,
        Some(t) => {
            let passed = t.get("passed").and_then(Value::as_bool);
            let summary = t.get("summary").map_or(Some(""), Value::as_str);
            match (passed, summary) {
                (Some(passed), Some(s)) => json!({"passed": passed, "summary": util::head(s, 4000)}),
                _ => return Err(Denied("teamlab 'test' must be {\"passed\": bool, \"summary\": string}".into())),
            }
        }
    };
    let sha = sha256_hex(source.as_bytes());
    let existing = tools(state).get(name).and_then(Value::as_array).cloned().unwrap_or_default();
    if let Some(v) = existing.iter().find(|v| v["sha256"] == sha.as_str()) {
        // Republishing a version restores its blob if it was damaged.
        lab.put_blob(&sha, source.as_bytes())?;
        pins_mut(state, agent).insert(name.into(), json!(sha));
        return Ok(json!({"name": name, "sha256": sha, "version": v["version"], "new": false, "pinned": true}));
    }
    if existing.is_empty() && tools(state).len() >= MAX_TOOLS {
        return Err(Denied(format!("the teamlab shelf holds at most {MAX_TOOLS} tools")));
    }
    let mut vs = existing;
    if vs.len() >= MAX_VERSIONS {
        // The oldest version no agent has pinned makes room.
        let pinned: Vec<Value> = state["pins"].as_object().into_iter().flat_map(|m| m.values())
            .filter_map(|tools| tools.get(name).cloned()).collect();
        let Some(i) = vs.iter().position(|v| !pinned.contains(&v["sha256"])) else {
            return Err(Denied(format!("tool {name:?} has {MAX_VERSIONS} versions and every one is pinned; unpin one first")));
        };
        dropped.push(vs.remove(i)["sha256"].as_str().unwrap_or_default().to_string());
    }
    lab.put_blob(&sha, source.as_bytes())?;
    let version = vs.last().and_then(|v| v["version"].as_u64()).unwrap_or(0) + 1;
    vs.push(json!({
        "sha256": sha, "version": version, "author": agent, "published_at": util::now_secs(),
        "bytes": source.len(), "note": note, "test": test, "approvals": [],
    }));
    tools_mut(state).insert(name.into(), Value::Array(vs));
    pins_mut(state, agent).insert(name.into(), json!(sha));
    Ok(json!({"name": name, "sha256": sha, "version": version, "new": true, "pinned": true,
              "dropped_versions": dropped.len()}))
}

fn approve(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let name = tool_param(p)?;
    let note = note_param(p)?;
    let sha = pick(versions(state, name)?, name, str_param(p, "sha256")?)?["sha256"].as_str().unwrap_or_default().to_string();
    let v = tools_mut(state)[name].as_array_mut().unwrap().iter_mut().find(|v| v["sha256"] == sha.as_str()).unwrap();
    if v["author"] == agent {
        return Err(Denied(format!("{agent} wrote {name} version {}; another agent has to approve it", v["version"])));
    }
    let approvals = v["approvals"].as_array_mut().ok_or_else(|| Internal("teamlab version without approvals".into()))?;
    let new = !approvals.iter().any(|a| a["by"] == agent);
    if new {
        approvals.push(json!({"by": agent, "at": util::now_secs(), "note": note}));
    }
    Ok(json!({"name": name, "sha256": sha, "version": v["version"], "new": new, "approvals": v["approvals"]}))
}

fn pin(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let name = tool_param(p)?;
    let v = pick(versions(state, name)?, name, str_param(p, "sha256")?)?.clone();
    pins_mut(state, agent).insert(name.into(), v["sha256"].clone());
    Ok(json!({"name": name, "sha256": v["sha256"], "version": v["version"]}))
}

fn unpin(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let name = tool_param(p)?;
    let was = pins_mut(state, agent).remove(name);
    Ok(json!({"name": name, "unpinned": was.is_some()}))
}

/// Moves pins to each tool's newest version: the named tools (pinned or
/// not), else every tool this agent has pinned.
fn update(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let names: Vec<String> = match p.get("names") {
        None | Some(Value::Null) => pins_of(state, agent).keys().cloned().collect(),
        Some(Value::Array(a)) => a.iter().map(|n| match n.as_str() {
            Some(n) if valid_tool_name(n) => Ok(n.to_string()),
            _ => Err(Denied("teamlab 'names' must be a list of tool names".into())),
        }).collect::<Result<_, _>>()?,
        Some(_) => return Err(Denied("teamlab 'names' must be a list of tool names".into())),
    };
    let (mut updated, mut current) = (Vec::new(), Vec::new());
    for name in names {
        let Ok(vs) = versions(state, &name) else {
            // A pin on a tool that is gone is dropped rather than kept dangling.
            pins_mut(state, agent).remove(&name);
            continue;
        };
        let latest = vs.last().unwrap().clone();
        let before = pins_of(state, agent).get(&name).cloned();
        if before.as_ref() == Some(&latest["sha256"]) {
            current.push(json!(name));
            continue;
        }
        let from_version = before.as_ref().and_then(|sha| vs.iter().find(|v| &v["sha256"] == sha)).map(|v| v["version"].clone());
        pins_mut(state, agent).insert(name.clone(), latest["sha256"].clone());
        updated.push(json!({"name": name, "from": before, "from_version": from_version,
                            "to": latest["sha256"], "to_version": latest["version"], "author": latest["author"]}));
    }
    Ok(json!({"updated": updated, "current": current}))
}

/// The source of every tool this agent has pinned, each checked against its hash.
fn sync(lab: &Lab, state: &Map<String, Value>, agent: &str) -> Result<Value, Fail> {
    let (mut out, mut missing, mut behind, mut total) = (Vec::new(), Vec::new(), Vec::new(), 0usize);
    for (name, sha) in pins_of(state, agent) {
        let Some(vs) = tools(state).get(&name).and_then(Value::as_array).filter(|v| !v.is_empty()) else {
            missing.push(json!(name));
            continue;
        };
        let Some(v) = vs.iter().find(|v| v["sha256"] == sha) else {
            missing.push(json!(name));
            continue;
        };
        if vs.last().unwrap()["sha256"] != sha {
            behind.push(json!(name));
        }
        let source = lab.read_blob(sha.as_str().unwrap_or_default())?;
        total += source.len();
        if total > MAX_SYNC_BYTES {
            return Err(Denied(format!("the pinned tools come to more than {} MiB; unpin some", MAX_SYNC_BYTES >> 20)));
        }
        let mut t = summary(v);
        t["name"] = json!(name);
        t["source"] = json!(source);
        out.push(t);
    }
    Ok(json!({"agent": agent, "tools": out, "missing": missing, "behind": behind, "unread": unread(state, agent)}))
}

fn fetch(lab: &Lab, state: &Map<String, Value>, p: &Map<String, Value>) -> Result<Value, Fail> {
    let name = tool_param(p)?;
    let v = pick(versions(state, name)?, name, str_param(p, "sha256")?)?;
    let mut t = summary(v);
    t["name"] = json!(name);
    t["source"] = json!(lab.read_blob(v["sha256"].as_str().unwrap_or_default())?);
    Ok(t)
}

fn droppable(m: &Value, now: f64) -> bool {
    if m["to"] == "all" {
        m["acked_by"].as_array().is_some_and(|a| !a.is_empty())
            && now - m["sent_at"].as_f64().unwrap_or(now) > BROADCAST_KEEP_S
    } else {
        m["to"].as_str().is_some_and(|to| acked_by(m, to))
    }
}

fn send(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let to = str_param(p, "to")?.ok_or_else(|| Denied("teamlab send requires 'to': an agent name or \"all\"".into()))?;
    if to != "all" && !valid_agent(to) {
        return Err(Denied(format!("teamlab 'to' {to:?} is not an agent name ([a-z0-9._-], at most 32) or \"all\"")));
    }
    if to == agent {
        return Err(Denied("an agent cannot send teamlab mail to itself".into()));
    }
    let subject = str_param(p, "subject")?.unwrap_or("");
    let body = str_param(p, "body")?.unwrap_or("");
    if subject.trim().is_empty() || subject.chars().count() > MAX_SUBJECT_CHARS {
        return Err(Denied(format!("teamlab 'subject' must be non-empty and at most {MAX_SUBJECT_CHARS} characters")));
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(Denied(format!("teamlab 'body' is limited to {} KiB", MAX_BODY_BYTES / 1024)));
    }
    let (tool, sha) = match str_param(p, "tool")? {
        None => (Value::Null, Value::Null),
        Some(name) => {
            if !valid_tool_name(name) {
                return Err(Denied("teamlab 'tool' must name a tool on the shelf".into()));
            }
            let v = pick(versions(state, name)?, name, str_param(p, "sha256")?)?;
            (json!(name), v["sha256"].clone())
        }
    };
    let now = util::now_secs();
    let messages = messages_mut(state);
    let body_bytes = |ms: &Vec<Value>| ms.iter().map(|m| m["body"].as_str().map_or(0, str::len)).sum::<usize>();
    while messages.len() >= MAX_MESSAGES || body_bytes(messages) + body.len() > MAX_MAIL_BYTES {
        let Some(i) = messages.iter().position(|m| droppable(m, now)) else {
            return Err(Denied(format!("the teamlab mailbox is full ({MAX_MESSAGES} messages or {} MiB, none of it read \
                                       by its addressee); ack some first", MAX_MAIL_BYTES >> 20)));
        };
        messages.remove(i);
    }
    let id = state["next_message_id"].as_u64().unwrap_or(1);
    state.insert("next_message_id".into(), json!(id + 1));
    messages_mut(state).push(json!({
        "id": id, "from": agent, "to": to, "subject": subject, "body": body,
        "tool": tool, "sha256": sha, "sent_at": now, "acked_by": [],
    }));
    Ok(json!({"id": id, "to": to}))
}

fn inbox(state: &Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let all = match p.get("all") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(Denied("teamlab 'all' must be true or false".into())),
    };
    let messages: Vec<Value> = state["messages"].as_array().into_iter().flatten()
        .filter(|m| addressed_to(m, agent) && (all || !acked_by(m, agent)))
        .map(|m| {
            let mut m = m.clone();
            m["read"] = json!(acked_by(&m, agent));
            m.as_object_mut().unwrap().remove("acked_by");
            m
        })
        .collect();
    Ok(json!({"agent": agent, "messages": messages, "unread": unread(state, agent)}))
}

/// Marks messages read: the listed `ids`, or "all" of this agent's unread.
fn ack(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let ids: Option<Vec<u64>> = match p.get("ids") {
        Some(Value::String(s)) if s == "all" => None,
        Some(Value::Array(a)) => Some(a.iter().map(|i| i.as_u64().ok_or_else(|| Denied("teamlab 'ids' must be message ids".into())))
            .collect::<Result<_, _>>()?),
        _ => return Err(Denied("teamlab ack requires 'ids': a list of message ids, or \"all\"".into())),
    };
    let mut acked = Vec::new();
    for m in messages_mut(state).iter_mut() {
        let id = m["id"].as_u64().unwrap_or(0);
        if !addressed_to(m, agent) || ids.as_ref().is_some_and(|ids| !ids.contains(&id)) {
            continue;
        }
        acked.push(id);
        if !acked_by(m, agent) {
            if let Some(a) = m["acked_by"].as_array_mut() {
                a.push(json!(agent));
            }
        }
    }
    let unknown: Vec<u64> = ids.unwrap_or_default().into_iter().filter(|i| !acked.contains(i)).collect();
    Ok(json!({"acked": acked, "unknown": unknown}))
}

/// What a receipt keeps of a teamlab call: tool sources and message bodies,
/// sent or returned, by hash and size rather than in full, and the agent the
/// call acted as.
pub fn receipt_view(agent: &str, params: &Value, result: &Value) -> (Value, Value) {
    fn digest(text: &Value) -> Value {
        match text.as_str() {
            Some(s) => json!({"sha256": sha256_hex(s.as_bytes()), "bytes": s.len()}),
            None => text.clone(),
        }
    }
    let mut params = params.clone();
    if let Some(p) = params.as_object_mut() {
        for field in ["source", "body"] {
            if let Some(s) = p.get_mut(field) {
                *s = digest(s);
            }
        }
        p.insert("agent".into(), json!(agent));
    }
    let mut result = result.clone();
    if let Some(s) = result.get_mut("source") {
        *s = digest(s);
    }
    for (list, field) in [("tools", "source"), ("messages", "body")] {
        for item in result.get_mut(list).and_then(Value::as_array_mut).into_iter().flatten() {
            if let Some(s) = item.get_mut(field) {
                *s = digest(s);
            }
        }
    }
    (params, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TmpDir(PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.0.join("blobs"), fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn lab() -> TmpDir {
        let d = std::env::temp_dir().join(format!("teamlab-test-{}", util::hex(8)));
        TmpDir(d)
    }

    fn call(dir: &Path, agent: &str, p: Value) -> Result<Value, String> {
        handle(dir, agent, p.as_object().unwrap()).map_err(|e| match e {
            Denied(r) => format!("denied: {r}"),
            Internal(r) => format!("internal: {r}"),
        })
    }

    fn ok(dir: &Path, agent: &str, p: Value) -> Value {
        call(dir, agent, p.clone()).unwrap_or_else(|e| panic!("{p}: {e}"))
    }

    const SRC: &str = "hello(x) = \"hello, $x\"\n";

    #[test]
    fn publish_pins_for_the_author_and_sync_returns_verified_source() {
        let d = lab();
        let r = ok(&d.0, "codex", json!({"action": "publish", "name": "hello", "source": SRC, "note": "first"}));
        assert_eq!(r["version"], 1);
        assert_eq!(r["new"], true);
        let again = ok(&d.0, "codex", json!({"action": "publish", "name": "hello", "source": SRC}));
        assert_eq!(again["new"], false, "publishing the same source is idempotent");
        let s = ok(&d.0, "codex", json!({"action": "sync"}));
        assert_eq!(s["tools"][0]["source"], SRC);
        assert_eq!(s["tools"][0]["author"], "codex");
        // Another agent sees the tool but has not pinned it.
        let st = ok(&d.0, "claude", json!({"action": "status"}));
        assert_eq!(st["tools"][0]["name"], "hello");
        assert_eq!(st["tools"][0]["pinned"], Value::Null);
        assert_eq!(ok(&d.0, "claude", json!({"action": "sync"}))["tools"], json!([]));
        let blob = d.0.join("blobs").join(format!("{}.jl", r["sha256"].as_str().unwrap()));
        assert_eq!(fs::metadata(blob).unwrap().permissions().mode() & 0o777, 0o444);
    }

    #[test]
    fn an_author_cannot_approve_their_own_tool_but_another_agent_can_once() {
        let d = lab();
        ok(&d.0, "codex", json!({"action": "publish", "name": "hello", "source": SRC}));
        let e = call(&d.0, "codex", json!({"action": "approve", "name": "hello"})).unwrap_err();
        assert!(e.contains("another agent has to approve"), "{e}");
        assert_eq!(ok(&d.0, "claude", json!({"action": "approve", "name": "hello", "note": "read it"}))["new"], true);
        assert_eq!(ok(&d.0, "claude", json!({"action": "approve", "name": "hello"}))["new"], false);
        let st = ok(&d.0, "gemini", json!({"action": "status"}));
        assert_eq!(st["tools"][0]["latest"]["approvals"][0]["by"], "claude");
        assert_eq!(st["tools"][0]["latest"]["approvals"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn pins_hold_a_version_until_update_moves_them() {
        let d = lab();
        let v1 = ok(&d.0, "codex", json!({"action": "publish", "name": "hello", "source": SRC}));
        ok(&d.0, "claude", json!({"action": "pin", "name": "hello"}));
        let v2 = ok(&d.0, "codex", json!({"action": "publish", "name": "hello", "source": "hello(x) = 2\n"}));
        let s = ok(&d.0, "claude", json!({"action": "sync"}));
        assert_eq!(s["tools"][0]["sha256"], v1["sha256"], "a pin does not follow new versions");
        assert_eq!(s["behind"], json!(["hello"]));
        let u = ok(&d.0, "claude", json!({"action": "update"}));
        assert_eq!(u["updated"][0]["to"], v2["sha256"]);
        assert_eq!(u["updated"][0]["from_version"], 1);
        assert_eq!(ok(&d.0, "claude", json!({"action": "sync"}))["tools"][0]["source"], "hello(x) = 2\n");
        // Pin back to the first version by hash, then unpin.
        ok(&d.0, "claude", json!({"action": "pin", "name": "hello", "sha256": v1["sha256"]}));
        assert_eq!(ok(&d.0, "claude", json!({"action": "status"}))["tools"][0]["pinned_version"], 1);
        assert_eq!(ok(&d.0, "claude", json!({"action": "unpin", "name": "hello"}))["unpinned"], true);
        assert_eq!(ok(&d.0, "claude", json!({"action": "sync"}))["tools"], json!([]));
    }

    #[test]
    fn a_tampered_blob_is_refused_not_loaded() {
        let d = lab();
        let r = ok(&d.0, "codex", json!({"action": "publish", "name": "hello", "source": SRC}));
        let blob = d.0.join("blobs").join(format!("{}.jl", r["sha256"].as_str().unwrap()));
        fs::set_permissions(&blob, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&blob, "run(`rm -rf ~`)\n").unwrap();
        for action in ["sync", "fetch"] {
            let e = call(&d.0, "codex", json!({"action": action, "name": "hello"})).unwrap_err();
            assert!(e.contains("tampered"), "{action}: {e}");
        }
        // Publishing the real source again repairs the blob.
        ok(&d.0, "codex", json!({"action": "publish", "name": "hello", "source": SRC}));
        assert_eq!(ok(&d.0, "codex", json!({"action": "sync"}))["tools"][0]["source"], SRC);
    }

    #[test]
    fn a_symlinked_blob_is_refused() {
        let d = lab();
        let r = ok(&d.0, "codex", json!({"action": "publish", "name": "hello", "source": SRC}));
        let blob = d.0.join("blobs").join(format!("{}.jl", r["sha256"].as_str().unwrap()));
        let elsewhere = d.0.join("elsewhere.jl");
        fs::write(&elsewhere, SRC).unwrap();
        fs::remove_file(&blob).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &blob).unwrap();
        assert!(call(&d.0, "codex", json!({"action": "sync"})).unwrap_err().starts_with("internal"));
    }

    #[test]
    fn an_unreadable_index_is_never_overwritten() {
        let d = lab();
        ok(&d.0, "codex", json!({"action": "publish", "name": "hello", "source": SRC}));
        ok(&d.0, "claude", json!({"action": "pin", "name": "hello"}));
        fs::write(d.0.join("state.json"), b"{\"format\": 1, \"tools\": {\"hel").unwrap();
        for p in [json!({"action": "status"}), json!({"action": "publish", "name": "other", "source": SRC}),
                  json!({"action": "send", "to": "all", "subject": "hi"})] {
            let e = call(&d.0, "codex", p).unwrap_err();
            assert!(e.contains("unreadable") && e.contains("state.prev.json"), "{e}");
        }
        assert_eq!(fs::read(d.0.join("state.json")).unwrap(), b"{\"format\": 1, \"tools\": {\"hel");
        // The previous index is there to restore from.
        fs::copy(d.0.join("state.prev.json"), d.0.join("state.json")).unwrap();
        assert_eq!(ok(&d.0, "codex", json!({"action": "status"}))["tools"][0]["name"], "hello");
    }

    #[test]
    fn the_oldest_unpinned_version_makes_room_and_its_blob_goes() {
        let d = lab();
        let first = ok(&d.0, "codex", json!({"action": "publish", "name": "t", "source": "t() = 0\n"}));
        ok(&d.0, "claude", json!({"action": "pin", "name": "t"}));
        let second = ok(&d.0, "codex", json!({"action": "publish", "name": "t", "source": "t() = 1\n"}));
        for i in 2..=MAX_VERSIONS {
            ok(&d.0, "codex", json!({"action": "publish", "name": "t", "source": format!("t() = {i}\n")}));
        }
        let st = ok(&d.0, "codex", json!({"action": "status"}));
        assert_eq!(st["tools"][0]["versions"], MAX_VERSIONS);
        assert_eq!(st["tools"][0]["latest"]["version"], MAX_VERSIONS as u64 + 1);
        // claude's pin kept version 1; version 2 was the oldest unpinned.
        assert_eq!(ok(&d.0, "claude", json!({"action": "sync"}))["tools"][0]["sha256"], first["sha256"]);
        assert!(call(&d.0, "claude", json!({"action": "fetch", "name": "t", "sha256": second["sha256"]})).is_err());
        assert!(!d.0.join("blobs").join(format!("{}.jl", second["sha256"].as_str().unwrap())).exists());
    }

    #[test]
    fn mail_reaches_the_addressee_and_broadcasts_reach_everyone_else() {
        let d = lab();
        ok(&d.0, "codex", json!({"action": "publish", "name": "hello", "source": SRC}));
        let m1 = ok(&d.0, "codex", json!({"action": "send", "to": "claude", "subject": "review hello?", "tool": "hello"}));
        ok(&d.0, "codex", json!({"action": "send", "to": "all", "subject": "new shelf", "body": "see status"}));
        let c = ok(&d.0, "claude", json!({"action": "inbox"}));
        assert_eq!(c["unread"], 2);
        assert_eq!(c["messages"][0]["tool"], "hello");
        assert!(c["messages"][0]["sha256"].as_str().is_some_and(valid_sha));
        assert_eq!(ok(&d.0, "gemini", json!({"action": "inbox"}))["unread"], 1);
        assert_eq!(ok(&d.0, "codex", json!({"action": "inbox"}))["unread"], 0, "own mail is not news");
        let a = ok(&d.0, "claude", json!({"action": "ack", "ids": [m1["id"], 999]}));
        assert_eq!(a["unknown"], json!([999]));
        assert_eq!(ok(&d.0, "claude", json!({"action": "inbox"}))["unread"], 1);
        ok(&d.0, "claude", json!({"action": "ack", "ids": "all"}));
        assert_eq!(ok(&d.0, "claude", json!({"action": "inbox"}))["messages"], json!([]));
        assert_eq!(ok(&d.0, "claude", json!({"action": "inbox", "all": true}))["messages"].as_array().unwrap().len(), 2);
        assert!(call(&d.0, "claude", json!({"action": "send", "to": "claude", "subject": "x"})).is_err());
        assert!(call(&d.0, "claude", json!({"action": "send", "to": "Bad Name", "subject": "x"})).is_err());
    }

    #[test]
    fn a_full_mailbox_drops_read_mail_and_refuses_when_none_is_read() {
        let d = lab();
        for i in 0..MAX_MESSAGES {
            ok(&d.0, "codex", json!({"action": "send", "to": "claude", "subject": format!("m{i}")}));
        }
        let e = call(&d.0, "codex", json!({"action": "send", "to": "claude", "subject": "one more"})).unwrap_err();
        assert!(e.contains("mailbox is full"), "{e}");
        ok(&d.0, "claude", json!({"action": "ack", "ids": [1]}));
        let r = ok(&d.0, "codex", json!({"action": "send", "to": "claude", "subject": "one more"}));
        assert_eq!(r["id"], MAX_MESSAGES as u64 + 1);
        let inbox = ok(&d.0, "claude", json!({"action": "inbox", "all": true}));
        assert_eq!(inbox["messages"].as_array().unwrap().len(), MAX_MESSAGES);
        assert_eq!(inbox["messages"][0]["subject"], "m1", "the read message was the one dropped");
    }

    #[test]
    fn mail_bodies_together_are_capped() {
        let d = lab();
        let body = "x".repeat(MAX_BODY_BYTES);
        for i in 0..MAX_MAIL_BYTES / MAX_BODY_BYTES {
            ok(&d.0, "codex", json!({"action": "send", "to": "claude", "subject": format!("m{i}"), "body": body}));
        }
        let e = call(&d.0, "codex", json!({"action": "send", "to": "claude", "subject": "more", "body": body})).unwrap_err();
        assert!(e.contains("mailbox is full"), "{e}");
        ok(&d.0, "claude", json!({"action": "ack", "ids": [1, 2]}));
        ok(&d.0, "codex", json!({"action": "send", "to": "claude", "subject": "more", "body": body}));
        assert!(fs::metadata(d.0.join("state.json")).unwrap().len() < (MAX_MAIL_BYTES + 64 * 1024) as u64);
    }

    #[test]
    fn bad_names_sizes_and_actions_are_denied() {
        let d = lab();
        for (p, want) in [
            (json!({"action": "nope"}), "must be one of"),
            (json!({"action": "publish", "name": "end", "source": SRC}), "Julia identifier"),
            (json!({"action": "publish", "name": "a-b", "source": SRC}), "Julia identifier"),
            (json!({"action": "publish", "name": "x", "source": "x".repeat(MAX_SOURCE_BYTES + 1)}), "KiB"),
            (json!({"action": "publish", "name": "x", "source": "  "}), "non-empty"),
            (json!({"action": "publish", "name": "x", "source": SRC, "test": {"passed": "yes"}}), "'test'"),
            (json!({"action": "pin", "name": "missing"}), "no tool named"),
            (json!({"action": "ack"}), "requires 'ids'"),
        ] {
            let e = call(&d.0, "codex", p.clone()).unwrap_err();
            assert!(e.starts_with("denied") && e.contains(want), "{p}: {e}");
        }
    }

    #[test]
    fn concurrent_publishers_on_one_lab_lose_nothing() {
        let d = lab();
        let dir = d.0.clone();
        let threads: Vec<_> = (0..8).map(|t| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                for i in 0..10 {
                    let agent = format!("agent{t}");
                    handle(&dir, &agent, json!({"action": "publish", "name": format!("tool{t}"), "source": format!("f() = {i}\n")})
                        .as_object().unwrap()).unwrap();
                    handle(&dir, &agent, json!({"action": "send", "to": "all", "subject": format!("{t}/{i}")})
                        .as_object().unwrap()).unwrap();
                }
            })
        }).collect();
        for t in threads {
            t.join().unwrap();
        }
        let st = ok(&dir, "observer", json!({"action": "status"}));
        assert_eq!(st["tools"].as_array().unwrap().len(), 8);
        assert!(st["tools"].as_array().unwrap().iter().all(|t| t["versions"] == 10));
        assert_eq!(st["unread"], 80);
    }

    #[test]
    fn receipts_keep_sources_and_bodies_by_hash() {
        let (p, r) = receipt_view("claude",
            &json!({"action": "publish", "name": "hello", "source": SRC}),
            &json!({"tools": [{"name": "hello", "source": SRC}], "messages": [{"body": "long"}]}));
        assert_eq!(p["agent"], "claude");
        assert_eq!(p["source"]["bytes"], SRC.len());
        assert!(p["source"]["sha256"].as_str().is_some_and(valid_sha));
        assert_eq!(r["tools"][0]["source"]["bytes"], SRC.len());
        assert_eq!(r["messages"][0]["body"]["bytes"], 4);
        let (p, _) = receipt_view("claude", &json!({"action": "send", "to": "codex", "subject": "s", "body": "long"}), &json!({}));
        assert_eq!(p["body"]["bytes"], 4);
        assert_eq!(p["subject"], "s");
    }

    #[test]
    fn agent_names_are_checked() {
        for good in ["claude", "codex", "gemini", "grok", "local", "a.b-c_d", "x1"] {
            assert!(valid_agent(good), "{good}");
        }
        for bad in ["", "Claude", "-x", ".x", "a b", "a/b", &"a".repeat(33)] {
            assert!(!valid_agent(bad), "{bad}");
        }
    }
}
