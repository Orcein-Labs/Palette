//! TeamLab: a local shop of Julia tools, and a mailbox, shared by one user's
//! agents (Claude, Codex, Gemini, ...). Any agent lists a tool it wrote; the
//! others browse the shop, try a listing and pin the ones they want. The
//! broker keeps it in a host directory no kernel can write (the ceiling's
//! `teamlab.dir`), so a kernel reaches it only through these actions, under
//! the name the session host was given for the calling agent.
//!
//! A listing is not a product name. It has an id (a hash prefix), a kind
//! (what it is: a function or a module), a tag (what it does, in 15 words or
//! fewer) and the binding its source defines, which is what a kernel calls.
//! Each listing keeps its versions; each agent pins at most one version of a
//! listing, and at most one listing per binding.
//!
//! A listing's author keeps merge authority over it. A version another agent
//! publishes to it is a proposal: it is kept beside the versions, the author
//! is mailed, and it becomes the newest version only when the author accepts
//! it. The author can decline it, and its proposer can withdraw it.
//!
//! The directory holds `state.json` (the index: listings and their versions,
//! each agent's pins, the messages), `blobs/<sha256>.jl` (each version's
//! source, read-only, checked against its name on every read) and
//! `lab.lock`. Every action holds a flock on the lock file, shared to read
//! and exclusive to change, so sessions of different agents on one machine
//! see one consistent shop. The index is replaced atomically and the one it
//! replaced is kept as `state.prev.json`; an index that does not parse is
//! reported and never overwritten. A format-1 index (tools keyed by name) is
//! read as format 2 and written back as format 2 on the next change.
//!
//! Agent names are not authenticated: they come from the MCP client's own
//! name, so they keep honest agents apart (no approving your own tool, no
//! reading your own mail as new) but do not stop an agent lying about itself.

use crate::broker::Fail::{self, Denied, Internal};
use crate::util;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const FORMAT: u64 = 2;
const MAX_SOURCE_BYTES: usize = 512 * 1024;
const MAX_TOOLS: usize = 256;
const MAX_VERSIONS: usize = 64;
/// Pending proposals on one listing; each agent has at most one.
const MAX_PROPOSALS: usize = 16;
const MAX_MESSAGES: usize = 512;
const MAX_BODY_BYTES: usize = 64 * 1024;
/// All kept message bodies together; the index is rewritten on every change.
const MAX_MAIL_BYTES: usize = 4 * 1024 * 1024;
const MAX_SUBJECT_CHARS: usize = 200;
const MAX_NOTE_CHARS: usize = 2000;
const MAX_TAG_WORDS: usize = 15;
const MAX_TAG_CHARS: usize = 120;
const MAX_SYNC_BYTES: usize = 32 * 1024 * 1024;
/// The serialized index, which every change rewrites whole.
const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;
/// The sources of every version the shop keeps, together.
const MAX_BLOB_BYTES: u64 = 256 * 1024 * 1024;
/// A broadcast someone has read is kept this long before it may be dropped.
const BROADCAST_KEEP_S: f64 = 7.0 * 86400.0;
/// How long another agent's new version shows as new in `status`.
const NEW_FOR_S: f64 = 7.0 * 86400.0;

const ACTIONS: &[&str] = &[
    "status", "browse", "publish", "accept", "decline", "approve", "pin", "unpin", "update", "sync", "fetch", "send",
    "inbox", "ack",
];
const KINDS: &[&str] = &["function", "module"];

/// Words a binding cannot be: `sync` binds each tool as `TeamLab.<binding>`.
const JULIA_RESERVED: &[&str] = &[
    "abstract", "baremodule", "begin", "break", "catch", "const", "continue", "do", "else", "elseif", "end",
    "export", "false", "finally", "for", "function", "global", "if", "import", "in", "isa", "let", "local",
    "macro", "module", "mutable", "outer", "primitive", "public", "quote", "return", "struct", "true", "try",
    "type", "using", "where", "while", "TeamLab", "Palette", "Base", "Core", "Main",
];

/// The lab directory a ceiling grants, when its `teamlab` grant names an
/// absolute `dir`.
pub fn ceiling_dir(ceiling: &Value) -> Option<PathBuf> {
    let dir = ceiling.get("teamlab")?.get("dir")?.as_str()?;
    Path::new(dir).is_absolute().then(|| util::resolve(Path::new(dir)))
}

/// The shop the operator grants this user's sessions, as provisioning.py's
/// `with_teamlab` finds it: `PALETTE_TEAMLAB_DIR` (empty or `off` for none),
/// else `$XDG_DATA_HOME/palette/teamlab`, else `~/.local/share/palette/teamlab`.
pub fn user_dir() -> Option<PathBuf> {
    dir_from_env(|k| std::env::var(k).ok()).ok().flatten()
}

/// The shop `PALETTE_TEAMLAB_DIR` or the XDG default names, or None when it
/// is empty or `off`. `~` and `~/x` expand to HOME; any other path that is
/// not absolute (`~user` too) is refused.
pub(crate) fn dir_from_env(var: impl Fn(&str) -> Option<String>) -> Result<Option<PathBuf>, String> {
    let configured = match var("PALETTE_TEAMLAB_DIR") {
        Some(d) => d,
        None => {
            let data = var("XDG_DATA_HOME").map(PathBuf::from).filter(|d| d.is_absolute())
                .or_else(|| Some(Path::new(&var("HOME")?).join(".local/share")));
            return Ok(data.map(|d| d.join("palette/teamlab")));
        }
    };
    if matches!(configured.trim(), "" | "off") {
        return Ok(None);
    }
    let home = || var("HOME").map(PathBuf::from);
    let path = match configured.strip_prefix('~') {
        Some("") => home(),
        Some(rest) => rest.strip_prefix('/').and_then(|rest| Some(home()?.join(rest))),
        None => Some(PathBuf::from(&configured)),
    };
    match path.filter(|p| p.is_absolute()) {
        Some(p) => Ok(Some(p)),
        None => Err(format!("PALETTE_TEAMLAB_DIR must be an absolute path, not {}", util::py_str(&configured))),
    }
}

/// 1-32 of `[a-z0-9._-]`, starting with a letter or digit, and not "all",
/// which addresses mail to every agent.
pub fn valid_agent(name: &str) -> bool {
    name != "all"
        && (1..=32).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// The agent a session acts as when no call names one: PALETTE_AGENT, else
/// "local".
pub fn default_agent() -> String {
    std::env::var("PALETTE_AGENT").ok().filter(|a| valid_agent(a)).unwrap_or_else(|| "local".into())
}

fn valid_binding(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !JULIA_RESERVED.contains(&name)
}

fn is_hex(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn valid_sha(s: &str) -> bool {
    s.len() == 64 && is_hex(s)
}

fn valid_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && is_hex(s)
}

/// A new listing's id: the shortest prefix (8 hex digits, then 12, ...) of a
/// hash of who listed it, its binding and its first source that no other
/// listing has.
fn new_listing_id(taken: &Map<String, Value>, author: &str, binding: &str, first_sha: &str) -> String {
    let full = util::sha256_hex(format!("{author}\n{binding}\n{first_sha}").as_bytes());
    let mut n = 8;
    while n < 64 && taken.contains_key(&full[..n]) {
        n += 4;
    }
    full[..n].to_string()
}

/// A tag with its whitespace normalized, or why it is not one.
fn tag_of(text: &str) -> Result<String, String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let tag = words.join(" ");
    if words.is_empty() || words.len() > MAX_TAG_WORDS || tag.chars().count() > MAX_TAG_CHARS {
        return Err(format!("a teamlab 'tag' says what the tool does in 1 to {MAX_TAG_WORDS} words \
                            (at most {MAX_TAG_CHARS} characters), not {} words", words.len()));
    }
    Ok(tag)
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
    /// The shop at `dir`, created private if it is not there. An existing
    /// shop directory must be a real directory this user owns that no one
    /// else can write.
    fn open(dir: &Path) -> Result<Lab, Fail> {
        let blobs = dir.join("blobs");
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&blobs).map_err(io(&blobs))?;
        for d in [dir, blobs.as_path()] {
            util::check_private_dir(d).map_err(|e| Internal(format!("teamlab {e}")))?;
        }
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
        let format = state.get("format").and_then(Value::as_u64);
        let well_formed = matches!(format, Some(1 | 2))
            && state.get("tools").is_some_and(Value::is_object)
            && state.get("pins").is_some_and(Value::is_object)
            && state.get("messages").is_some_and(Value::is_array)
            && state.get("next_message_id").is_some_and(Value::is_u64);
        if !well_formed {
            return Err(unreadable(format!("not a format-{FORMAT} index")));
        }
        let state = if format == Some(1) { from_format_1(obj(state)) } else { obj(state) };
        let version_ok = |v: &Value| v["sha256"].as_str().is_some_and(valid_sha);
        let listing_ok = |l: &Value| ["kind", "binding", "tag", "author"].iter().all(|k| l[k].is_string())
            && l["versions"].as_array().is_some_and(|v| !v.is_empty() && v.iter().all(version_ok))
            && l.get("proposals").is_none_or(|p| p.as_array().is_some_and(|p| p.iter().all(|v| version_ok(v) && v["author"].is_string())));
        if let Some(id) = listings(&state).iter().find(|(_, l)| !listing_ok(l)).map(|(id, _)| id) {
            return Err(unreadable(format!("listing {id} is missing its kind, binding, tag, author or versions, \
                                           or has a malformed proposal")));
        }
        Ok(state)
    }

    /// Replaces the index atomically, keeping the one it replaces. An index
    /// past MAX_INDEX_BYTES is refused and nothing is written.
    fn save(&self, state: &Map<String, Value>) -> Result<(), Fail> {
        let path = self.state_path();
        let bytes = serde_json::to_vec(state).map_err(|e| Internal(format!("teamlab index: {e}")))?;
        if bytes.len() > MAX_INDEX_BYTES {
            return Err(Denied(format!("the teamlab index would pass {} MiB; ack mail or drop old versions first",
                                      MAX_INDEX_BYTES >> 20)));
        }
        let tmp = self.dir.join(format!(".state.{}.tmp", util::hex(12)));
        util::write_then(&tmp, &bytes, 0o600, || {
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
        }).map_err(io(&path))
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
        util::write_then(&tmp, source, 0o444, || fs::rename(&tmp, &path)).map_err(io(&path))
    }

    /// A version's source, refused unless its bytes still hash to its name.
    fn read_blob(&self, sha: &str) -> Result<String, Fail> {
        let path = self.blob_path(sha);
        let mut f = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path).map_err(io(&path))?;
        let mut bytes = Vec::new();
        Read::take(&mut f, MAX_SOURCE_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(io(&path))?;
        if bytes.len() > MAX_SOURCE_BYTES || util::sha256_hex(&bytes) != sha {
            return Err(Internal(format!("teamlab blob {} was tampered with: its contents no longer match its hash", path.display())));
        }
        String::from_utf8(bytes).map_err(|_| Internal(format!("teamlab blob {} is not UTF-8", path.display())))
    }

    /// Removes the blobs of dropped versions and proposals that no listing
    /// still keeps. Run after the index is saved, so a crash leaves an unused
    /// blob, never an index entry without its source.
    fn collect(&self, state: &Map<String, Value>, dropped: &[String]) {
        for sha in dropped {
            let used = listings(state).values().flat_map(kept).any(|v| v["sha256"] == sha.as_str());
            if !used {
                let _ = fs::remove_file(self.blob_path(sha));
            }
        }
    }
}

/// A format-1 index (`tools` keyed by name, pins and messages naming tools)
/// as format 2. Each tool becomes a function listing tagged with the first
/// line of its first note, or its name.
fn from_format_1(mut state: Map<String, Value>) -> Map<String, Value> {
    let mut listings = Map::new();
    let mut ids = BTreeMap::new();
    for (name, vs) in obj(state.remove("tools").unwrap_or_default()) {
        let Some(first) = vs.as_array().and_then(|v| v.first()).cloned() else { continue };
        let author = first["author"].as_str().unwrap_or("local").to_string();
        let id = new_listing_id(&listings, &author, &name, text(&first["sha256"]));
        let note_line = first["note"].as_str().and_then(|n| n.lines().find(|l| !l.trim().is_empty())).unwrap_or("");
        let words: Vec<&str> = note_line.split_whitespace().take(MAX_TAG_WORDS).collect();
        let tag = match util::head(&words.join(" "), MAX_TAG_CHARS) {
            t if t.is_empty() => name.replace('_', " "),
            t => t,
        };
        listings.insert(id.clone(), json!({
            "kind": "function", "binding": name, "tag": tag, "author": author,
            "created_at": first["published_at"], "versions": vs,
        }));
        ids.insert(name, id);
    }
    let pins: Map<String, Value> = obj(state.remove("pins").unwrap_or_default()).into_iter()
        .map(|(agent, held)| {
            let held: Map<String, Value> = obj(held).into_iter()
                .filter_map(|(name, sha)| Some((ids.get(&name)?.clone(), sha))).collect();
            (agent, Value::Object(held))
        })
        .collect();
    for m in state.get_mut("messages").and_then(Value::as_array_mut).into_iter().flatten() {
        if let Some(m) = m.as_object_mut() {
            let tool = m.remove("tool");
            let listing = tool.as_ref().and_then(Value::as_str).and_then(|n| ids.get(n)).map_or(Value::Null, |id| json!(id));
            m.insert("listing".into(), listing);
        }
    }
    state.insert("format".into(), json!(FORMAT));
    state.insert("tools".into(), Value::Object(listings));
    state.insert("pins".into(), Value::Object(pins));
    state
}

/// A string field, or "" (for messages: a Value prints with quotes).
fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn listings(state: &Map<String, Value>) -> &Map<String, Value> {
    state["tools"].as_object().expect("checked when loaded")
}

fn listings_mut(state: &mut Map<String, Value>) -> &mut Map<String, Value> {
    state.get_mut("tools").and_then(Value::as_object_mut).expect("checked when loaded")
}

fn versions(listing: &Value) -> &Vec<Value> {
    listing["versions"].as_array().expect("checked when loaded")
}

fn latest(listing: &Value) -> &Value {
    versions(listing).last().expect("checked when loaded")
}

/// Versions other agents proposed that the author has not accepted or declined.
fn proposals(listing: &Value) -> &[Value] {
    listing["proposals"].as_array().map_or(&[], Vec::as_slice)
}

/// Every source a listing keeps a blob for: its versions and its proposals.
fn kept(listing: &Value) -> impl Iterator<Item = &Value> {
    versions(listing).iter().chain(proposals(listing))
}

fn listing<'a>(state: &'a Map<String, Value>, id: &str) -> Result<&'a Value, Fail> {
    listings(state).get(id).ok_or_else(|| Denied(format!("no listing {id} is in the teamlab shop")))
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

/// Another listing this agent pins under `binding`, which `id` would clash with.
fn clash_with(state: &Map<String, Value>, agent: &str, binding: &str, id: &str) -> Option<String> {
    pins_of(state, agent).keys()
        .find(|other| *other != id && listings(state).get(*other).is_some_and(|l| l["binding"] == binding))
        .cloned()
}

fn messages_mut(state: &mut Map<String, Value>) -> &mut Vec<Value> {
    state.get_mut("messages").and_then(Value::as_array_mut).expect("checked when loaded")
}

fn str_param<'a>(p: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, Fail> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(Denied(format!("teamlab '{key}' must be a string"))),
    }
}

fn id_param<'a>(p: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, Fail> {
    match str_param(p, key)? {
        Some(id) if !valid_id(id) => Err(Denied(format!("teamlab '{key}' {id:?} is not a listing id (8 to 64 lowercase hex digits)"))),
        id => Ok(id),
    }
}

fn required_id<'a>(p: &'a Map<String, Value>) -> Result<&'a str, Fail> {
    id_param(p, "id")?.ok_or_else(|| Denied("teamlab requires a listing 'id' (see browse)".into()))
}

fn note_param(p: &Map<String, Value>) -> Result<Value, Fail> {
    match str_param(p, "note")? {
        Some(n) if n.chars().count() > MAX_NOTE_CHARS => Err(Denied(format!("teamlab 'note' is limited to {MAX_NOTE_CHARS} characters"))),
        Some(n) => Ok(json!(n)),
        None => Ok(Value::Null),
    }
}

fn test_param(p: &Map<String, Value>) -> Result<Value, Fail> {
    let Some(t) = p.get("test").filter(|t| !t.is_null()) else { return Ok(Value::Null) };
    match (t.get("passed").and_then(Value::as_bool), t.get("summary").map_or(Some(""), Value::as_str)) {
        (Some(passed), Some(s)) => Ok(json!({"passed": passed, "summary": util::head(s, 4000)})),
        _ => Err(Denied("teamlab 'test' must be {\"passed\": bool, \"summary\": string}".into())),
    }
}

/// The version `sha256` names, or the newest.
fn pick<'a>(listing: &'a Value, id: &str, sha: Option<&str>) -> Result<&'a Value, Fail> {
    match sha {
        None => Ok(latest(listing)),
        Some(s) if !valid_sha(s) => Err(Denied("teamlab 'sha256' must be 64 lowercase hex digits".into())),
        Some(s) => versions(listing).iter().find(|v| v["sha256"] == s)
            .ok_or_else(|| Denied(format!("listing {id} has no version {s}"))),
    }
}

/// Like `pick`, but `sha256` may also name a pending proposal.
fn pick_kept<'a>(listing: &'a Value, id: &str, sha: Option<&str>) -> Result<&'a Value, Fail> {
    match sha.and_then(|s| proposals(listing).iter().find(|v| v["sha256"] == s)) {
        Some(v) => Ok(v),
        None => pick(listing, id, sha),
    }
}

/// The pending proposal `sha256` names, and where it sits.
fn pending<'a>(listing: &'a Value, id: &str, p: &Map<String, Value>) -> Result<(usize, &'a Value), Fail> {
    let sha = str_param(p, "sha256")?.ok_or_else(|| Denied("teamlab requires the proposal's 'sha256' (see status)".into()))?;
    if !valid_sha(sha) {
        return Err(Denied("teamlab 'sha256' must be 64 lowercase hex digits".into()));
    }
    proposals(listing).iter().enumerate().find(|(_, v)| v["sha256"] == sha)
        .ok_or_else(|| Denied(format!("listing {id} has no pending proposal {sha}")))
}

fn summary(v: &Value) -> Value {
    json!({
        "sha256": v["sha256"], "version": v["version"], "author": v["author"], "published_at": v["published_at"],
        "bytes": v["bytes"], "note": v["note"], "test": v["test"], "approvals": v["approvals"],
    })
}

/// A version with what a kernel needs to load it: the listing's id, kind,
/// binding and tag.
fn loadable(id: &str, listing: &Value, v: &Value) -> Value {
    let mut t = summary(v);
    for k in ["kind", "binding", "tag"] {
        t[k] = listing[k].clone();
    }
    t["id"] = json!(id);
    t
}

/// A listing as `browse` and `status` show it to `agent`.
fn view(state: &Map<String, Value>, agent: &str, id: &str, listing: &Value) -> Value {
    let newest = latest(listing);
    let pinned = pins_of(state, agent).get(id).and_then(|sha| versions(listing).iter().find(|v| &v["sha256"] == sha));
    let pinned_by: Vec<&String> = state["pins"].as_object().into_iter().flatten()
        .filter(|(_, held)| held.get(id).is_some()).map(|(a, _)| a).collect();
    json!({
        "id": id, "kind": listing["kind"], "binding": listing["binding"], "tag": listing["tag"],
        "author": listing["author"], "versions": versions(listing).len(), "latest": summary(newest),
        "pinned_by": pinned_by, "pinned": pinned.map(|v| v["sha256"].clone()),
        "pinned_version": pinned.map(|v| v["version"].clone()),
        "behind": pinned.is_some_and(|v| v["sha256"] != newest["sha256"]),
        "proposals": proposals(listing).iter().map(summary).collect::<Vec<_>>(),
    })
}

fn published_at(listing: &Value) -> f64 {
    latest(listing)["published_at"].as_f64().unwrap_or(0.0)
}

pub fn handle(dir: &Path, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let action = str_param(p, "action")?.unwrap_or("");
    if !ACTIONS.contains(&action) {
        return Err(Denied(format!("teamlab 'action' must be one of {}", ACTIONS.join(", "))));
    }
    let lab = Lab::open(dir)?;
    let reading = matches!(action, "status" | "browse" | "sync" | "fetch" | "inbox");
    let _lock = lab.lock(!reading)?;
    let mut state = lab.load()?;
    if reading {
        return match action {
            "status" => Ok(status(&lab, &state, agent)),
            "browse" => browse(&state, agent, p),
            "sync" => sync(&lab, &state, agent),
            "fetch" => fetch(&lab, &state, p),
            _ => inbox(&state, agent, p),
        };
    }
    let mut dropped = Vec::new();
    let result = match action {
        "publish" => publish(&lab, &mut state, agent, p, &mut dropped)?,
        "accept" => accept(&mut state, agent, p, &mut dropped)?,
        "decline" => decline(&mut state, agent, p, &mut dropped)?,
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

/// What this agent pins, what is new in the shop, proposals awaiting this
/// agent's review, and unread mail. A listing is new while its newest version
/// is another agent's, at most a week old, and this agent does not pin it.
fn status(lab: &Lab, state: &Map<String, Value>, agent: &str) -> Value {
    let pins = pins_of(state, agent);
    let now = util::now_secs();
    let pinned: Vec<Value> = pins.keys().filter_map(|id| Some(view(state, agent, id, listings(state).get(id)?))).collect();
    let mut new: Vec<(&String, &Value)> = listings(state).iter()
        .filter(|(id, l)| !pins.contains_key(*id) && latest(l)["author"] != agent && now - published_at(l) <= NEW_FOR_S)
        .collect();
    new.sort_by(|a, b| published_at(b.1).total_cmp(&published_at(a.1)));
    let new: Vec<Value> = new.into_iter().map(|(id, l)| view(state, agent, id, l)).collect();
    let review: Vec<Value> = listings(state).iter().filter(|(_, l)| l["author"] == agent)
        .flat_map(|(id, l)| proposals(l).iter().map(move |v| loadable(id, l, v))).collect();
    json!({"agent": agent, "dir": lab.dir, "pinned": pinned, "new": new, "proposals": review,
           "listed": listings(state).len(), "unread": unread(state, agent)})
}

/// The listings whose kind, binding, tag, author or id hold every word of
/// `query` (all of them without one), newest first.
fn browse(state: &Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let query = str_param(p, "query")?.unwrap_or("");
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut found: Vec<(&String, &Value)> = listings(state).iter().filter(|(id, l)| {
        let hay = format!("{} {} {} {} {id}", text(&l["kind"]), text(&l["binding"]), text(&l["tag"]), text(&l["author"])).to_lowercase();
        words.iter().all(|w| hay.contains(w.as_str()))
    }).collect();
    found.sort_by(|a, b| published_at(b.1).total_cmp(&published_at(a.1)));
    let found: Vec<Value> = found.into_iter().map(|(id, l)| view(state, agent, id, l)).collect();
    Ok(json!({"agent": agent, "query": query, "listings": found, "listed": listings(state).len()}))
}

/// Adds a version to a listing: the one `id` names, else this agent's listing
/// of the same binding, else a new listing, which needs a `kind` and a `tag`.
/// The author pins it, unless they pin another listing with the same binding.
/// A new version of another agent's listing is a proposal instead.
fn publish(lab: &Lab, state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>, dropped: &mut Vec<String>)
    -> Result<Value, Fail> {
    let binding = str_param(p, "binding")?.ok_or_else(|| Denied("teamlab publish requires the 'binding' the source defines".into()))?;
    if !valid_binding(binding) {
        return Err(Denied(format!("teamlab binding {binding:?} must be a Julia identifier (ASCII letters, digits and _, \
                                   at most 64, not a keyword or TeamLab, Palette, Base, Core or Main)")));
    }
    let source = str_param(p, "source")?.ok_or_else(|| Denied("teamlab publish requires the tool's 'source'".into()))?;
    if source.trim().is_empty() || source.len() > MAX_SOURCE_BYTES {
        return Err(Denied(format!("teamlab source must be non-empty and at most {} KiB", MAX_SOURCE_BYTES / 1024)));
    }
    let kind = match str_param(p, "kind")? {
        Some(k) if !KINDS.contains(&k) => return Err(Denied(format!("teamlab 'kind' must be one of {}", KINDS.join(", ")))),
        k => k,
    };
    let tag = str_param(p, "tag")?.map(tag_of).transpose().map_err(Denied)?;
    let (note, test) = (note_param(p)?, test_param(p)?);
    let sha = util::sha256_hex(source.as_bytes());

    let id = match id_param(p, "id")? {
        Some(id) => Some(id.to_string()),
        None => listings(state).iter().find(|(_, l)| l["author"] == agent && l["binding"] == binding).map(|(id, _)| id.clone()),
    };
    let new_listing = id.is_none();
    let id = match id {
        Some(id) => {
            let l = listing(state, &id)?;
            if l["binding"] != binding {
                return Err(Denied(format!("listing {id} binds {}, not {binding}", text(&l["binding"]))));
            }
            if kind.is_some_and(|k| l["kind"] != k) {
                return Err(Denied(format!("listing {id} is a {}, not a {}", text(&l["kind"]), kind.unwrap_or_default())));
            }
            if let Some(tag) = tag.as_ref().filter(|t| l["tag"] != t.as_str()) {
                if l["author"] != agent {
                    return Err(Denied(format!("only {}, who listed {id}, changes its tag", text(&l["author"]))));
                }
                listings_mut(state)[&id]["tag"] = json!(tag);
            }
            id
        }
        None => {
            let (Some(kind), Some(tag)) = (kind, tag.as_ref()) else {
                return Err(Denied(format!("a new teamlab listing needs a 'kind' ({}) and a 'tag': what it does, \
                                           in {MAX_TAG_WORDS} words or fewer", KINDS.join(" or "))));
            };
            if listings(state).len() >= MAX_TOOLS {
                return Err(Denied(format!("the teamlab shop holds at most {MAX_TOOLS} listings")));
            }
            let id = new_listing_id(listings(state), agent, binding, &sha);
            listings_mut(state).insert(id.clone(), json!({
                "kind": kind, "binding": binding, "tag": tag, "author": agent,
                "created_at": util::now_secs(), "versions": [],
            }));
            id
        }
    };

    let mut vs = versions(&listings(state)[&id]).clone();
    let existing = vs.iter().find(|v| v["sha256"] == sha.as_str()).map(|v| v["version"].clone());
    if existing.is_none() && listings(state)[&id]["author"] != agent {
        return propose(lab, state, agent, &id, source, &sha, note, test, dropped);
    }
    let is_new = existing.is_none();
    let version = match existing {
        // Republishing a version restores its blob if it was damaged.
        Some(version) => {
            lab.put_blob(&sha, source.as_bytes())?;
            version
        }
        None => {
            make_room(state, &id, &mut vs, dropped)?;
            check_budget(state, &sha, source.len())?;
            lab.put_blob(&sha, source.as_bytes())?;
            let version = vs.last().and_then(|v| v["version"].as_u64()).unwrap_or(0) + 1;
            vs.push(json!({
                "sha256": sha, "version": version, "author": agent, "published_at": util::now_secs(),
                "bytes": source.len(), "note": note, "test": test, "approvals": [],
            }));
            listings_mut(state)[&id]["versions"] = Value::Array(vs);
            json!(version)
        }
    };
    let clash = clash_with(state, agent, binding, &id);
    if clash.is_none() {
        pins_mut(state, agent).insert(id.clone(), json!(sha));
    }
    let l = &listings(state)[&id];
    Ok(json!({
        "id": id, "kind": l["kind"], "binding": binding, "tag": l["tag"], "sha256": sha, "version": version,
        "new": is_new, "new_listing": new_listing, "proposed": false, "pinned": clash.is_none(), "clash": clash,
        "dropped_versions": dropped.len(),
    }))
}

/// Drops the oldest version no agent pins from a full listing, so one more fits.
fn make_room(state: &Map<String, Value>, id: &str, vs: &mut Vec<Value>, dropped: &mut Vec<String>) -> Result<(), Fail> {
    if vs.len() < MAX_VERSIONS {
        return Ok(());
    }
    let pinned: Vec<&Value> = state["pins"].as_object().into_iter().flat_map(|m| m.values())
        .filter_map(|held| held.get(id)).collect();
    let Some(i) = vs.iter().position(|v| !pinned.contains(&&v["sha256"])) else {
        return Err(Denied(format!("listing {id} has {MAX_VERSIONS} versions and every one is pinned; unpin one first")));
    };
    dropped.push(text(&vs.remove(i)["sha256"]).to_string());
    Ok(())
}

fn check_budget(state: &Map<String, Value>, sha: &str, bytes: usize) -> Result<(), Fail> {
    if blob_bytes_with(state, sha, bytes) > MAX_BLOB_BYTES {
        return Err(Denied(format!("the teamlab shop keeps at most {} MiB of tool source; \
                                   unpin old versions so they can be dropped", MAX_BLOB_BYTES >> 20)));
    }
    Ok(())
}

/// Keeps another agent's version of a listing as a proposal and mails the
/// author, who accepts or declines it. Each agent keeps one proposal per
/// listing: a new one supersedes it.
#[allow(clippy::too_many_arguments)]
fn propose(lab: &Lab, state: &mut Map<String, Value>, agent: &str, id: &str, source: &str, sha: &str, note: Value,
           test: Value, dropped: &mut Vec<String>) -> Result<Value, Fail> {
    let l = &listings(state)[id];
    let (author, binding) = (text(&l["author"]).to_string(), text(&l["binding"]).to_string());
    let mut ps = proposals(l).to_vec();
    let reply = |l: &Value, new: bool, notified: bool, superseded: Value| json!({
        "id": id, "kind": l["kind"], "binding": binding, "tag": l["tag"], "sha256": sha, "version": null,
        "new": new, "new_listing": false, "proposed": true, "author": author, "pinned": false, "clash": null,
        "notified": notified, "superseded": superseded, "dropped_versions": 0,
    });
    if ps.iter().any(|v| v["sha256"] == sha) {
        // Proposing it again restores its blob if it was damaged.
        lab.put_blob(sha, source.as_bytes())?;
        return Ok(reply(l, false, false, Value::Null));
    }
    let superseded = ps.iter().position(|v| v["author"] == agent).map(|i| ps.remove(i)["sha256"].clone());
    if ps.len() >= MAX_PROPOSALS {
        return Err(Denied(format!("listing {id} already has {MAX_PROPOSALS} proposals awaiting {author}")));
    }
    if let Some(old) = superseded.as_ref() {
        dropped.push(text(old).to_string());
    }
    listings_mut(state)[id]["proposals"] = Value::Array(ps);
    check_budget(state, sha, source.len())?;
    lab.put_blob(sha, source.as_bytes())?;
    let proposal = json!({
        "sha256": sha, "version": null, "author": agent, "published_at": util::now_secs(),
        "bytes": source.len(), "note": note, "test": test, "approvals": [],
    });
    listings_mut(state)[id]["proposals"].as_array_mut().expect("just set").push(proposal);
    let mut body = format!("{agent} proposed a new version of {binding}, your listing {id}. Fetch it with this id and \
                            sha256 to review it, then accept or decline it.");
    if let Some(n) = note.as_str() {
        body = format!("{body}\n\n{n}");
    }
    let subject = format!("proposed a new version of {binding} ({id})");
    let notified = post(state, agent, &author, &subject, &body, json!(id), json!(sha)).is_ok();
    Ok(reply(&listings(state)[id], true, notified, superseded.unwrap_or(Value::Null)))
}

/// The author makes a proposal the listing's newest version, credited to the
/// agent that proposed it, and pins it.
fn accept(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>, dropped: &mut Vec<String>)
    -> Result<Value, Fail> {
    let id = required_id(p)?;
    let note = note_param(p)?;
    let l = listing(state, id)?;
    let (i, _) = pending(l, id, p)?;
    if l["author"] != agent {
        return Err(Denied(format!("only {}, who listed {id}, accepts proposals for it", text(&l["author"]))));
    }
    let binding = text(&l["binding"]).to_string();
    let mut ps = proposals(l).to_vec();
    let mut v = ps.remove(i);
    let (sha, proposer) = (text(&v["sha256"]).to_string(), text(&v["author"]).to_string());
    let mut vs = versions(l).clone();
    // The author may have published the same source since; then it is already a version.
    let version = match vs.iter().find(|x| x["sha256"] == sha.as_str()) {
        Some(x) => x["version"].clone(),
        None => {
            make_room(state, id, &mut vs, dropped)?;
            let version = json!(vs.last().and_then(|x| x["version"].as_u64()).unwrap_or(0) + 1);
            v["version"] = version.clone();
            v["proposed_at"] = v["published_at"].take();
            v["published_at"] = json!(util::now_secs());
            vs.push(v);
            version
        }
    };
    let l = &mut listings_mut(state)[id];
    l["versions"] = Value::Array(vs);
    l["proposals"] = Value::Array(ps);
    let clash = clash_with(state, agent, &binding, id);
    if clash.is_none() {
        pins_mut(state, agent).insert(id.into(), json!(sha));
    }
    let mut body = format!("{agent} accepted your proposal for {binding} as version {version} of listing {id}.");
    if let Some(n) = note.as_str() {
        body = format!("{body}\n\n{n}");
    }
    let subject = format!("accepted your version of {binding} ({id})");
    let notified = post(state, agent, &proposer, &subject, &body, json!(id), json!(sha)).is_ok();
    Ok(json!({"id": id, "binding": binding, "sha256": sha, "version": version, "author": proposer,
              "pinned": clash.is_none(), "clash": clash, "notified": notified}))
}

/// Drops a proposal: its listing's author declines it, or the agent that
/// proposed it withdraws it. The other one is mailed.
fn decline(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>, dropped: &mut Vec<String>)
    -> Result<Value, Fail> {
    let id = required_id(p)?;
    let note = note_param(p)?;
    let l = listing(state, id)?;
    let (i, v) = pending(l, id, p)?;
    let (author, proposer) = (text(&l["author"]).to_string(), text(&v["author"]).to_string());
    if agent != author && agent != proposer {
        return Err(Denied(format!("only {author}, who listed {id}, or {proposer}, who proposed it, declines it")));
    }
    let binding = text(&l["binding"]).to_string();
    let sha = text(&v["sha256"]).to_string();
    let withdrawn = agent == proposer;
    let mut ps = proposals(l).to_vec();
    ps.remove(i);
    listings_mut(state)[id]["proposals"] = Value::Array(ps);
    dropped.push(sha.clone());
    let (to, subject, mut body) = if withdrawn {
        (author, format!("withdrew a proposed version of {binding} ({id})"),
         format!("{agent} withdrew the version of {binding} it proposed for your listing {id}."))
    } else {
        (proposer, format!("declined your version of {binding} ({id})"),
         format!("{agent} declined the version of {binding} you proposed for listing {id}. You may still list it \
                  as your own tool under a different binding."))
    };
    if let Some(n) = note.as_str() {
        body = format!("{body}\n\n{n}");
    }
    let notified = post(state, agent, &to, &subject, &body, json!(id), json!(sha)).is_ok();
    Ok(json!({"id": id, "sha256": sha, "declined": !withdrawn, "withdrawn": withdrawn, "notified": notified}))
}

/// The bytes of every distinct source the index keeps, with `sha` added if
/// it is not kept yet.
fn blob_bytes_with(state: &Map<String, Value>, sha: &str, bytes: usize) -> u64 {
    let mut seen = BTreeSet::new();
    let mut total = 0;
    for v in listings(state).values().flat_map(kept) {
        if seen.insert(text(&v["sha256"])) {
            total += v["bytes"].as_u64().unwrap_or(0);
        }
    }
    total + if seen.contains(sha) { 0 } else { bytes as u64 }
}

fn approve(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let id = required_id(p)?;
    let note = note_param(p)?;
    let sha = pick(listing(state, id)?, id, str_param(p, "sha256")?)?["sha256"].clone();
    let v = listings_mut(state)[id]["versions"].as_array_mut().into_iter().flatten().find(|v| v["sha256"] == sha)
        .ok_or_else(|| Internal(format!("teamlab listing {id} lost version {sha}")))?;
    if v["author"] == agent {
        return Err(Denied(format!("{agent} wrote listing {id} version {}; another agent has to approve it", v["version"])));
    }
    let approvals = v["approvals"].as_array_mut().ok_or_else(|| Internal("teamlab version without approvals".into()))?;
    let new = !approvals.iter().any(|a| a["by"] == agent);
    if new {
        approvals.push(json!({"by": agent, "at": util::now_secs(), "note": note}));
    }
    Ok(json!({"id": id, "sha256": sha, "version": v["version"], "new": new, "approvals": v["approvals"]}))
}

fn pin(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let id = required_id(p)?;
    let l = listing(state, id)?;
    let v = pick(l, id, str_param(p, "sha256")?)?.clone();
    let binding = text(&l["binding"]).to_string();
    if let Some(other) = clash_with(state, agent, &binding, id) {
        return Err(Denied(format!("{agent} already pins listing {other} as {binding}; unpin it first")));
    }
    pins_mut(state, agent).insert(id.into(), v["sha256"].clone());
    Ok(json!({"id": id, "binding": binding, "sha256": v["sha256"], "version": v["version"]}))
}

fn unpin(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let id = required_id(p)?;
    let was = pins_mut(state, agent).remove(id);
    Ok(json!({"id": id, "unpinned": was.is_some()}))
}

/// Moves pins to each listing's newest version: the listed `ids` (pinned or
/// not), else every listing this agent pins.
fn update(state: &mut Map<String, Value>, agent: &str, p: &Map<String, Value>) -> Result<Value, Fail> {
    let bad = || Denied("teamlab 'ids' must be a list of listing ids".into());
    let ids: Vec<String> = match p.get("ids") {
        None | Some(Value::Null) => pins_of(state, agent).keys().cloned().collect(),
        Some(Value::Array(a)) => a.iter().map(|i| i.as_str().filter(|i| valid_id(i)).map(String::from).ok_or_else(bad))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(bad()),
    };
    let (mut updated, mut current, mut gone, mut clashes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for id in ids {
        let Some(l) = listings(state).get(&id) else {
            // A pin on a listing that is gone is dropped rather than kept dangling.
            if pins_mut(state, agent).remove(&id).is_some() {
                gone.push(json!(id));
            }
            continue;
        };
        let newest = latest(l).clone();
        let before = pins_of(state, agent).get(&id).cloned();
        if before.as_ref() == Some(&newest["sha256"]) {
            current.push(json!(id));
            continue;
        }
        let binding = l["binding"].clone();
        if let Some(other) = clash_with(state, agent, binding.as_str().unwrap_or_default(), &id) {
            clashes.push(json!({"id": id, "binding": binding, "pinned": other}));
            continue;
        }
        let from_version = before.as_ref().and_then(|sha| versions(l).iter().find(|v| &v["sha256"] == sha)).map(|v| v["version"].clone());
        pins_mut(state, agent).insert(id.clone(), newest["sha256"].clone());
        updated.push(json!({"id": id, "binding": binding, "from": before, "from_version": from_version,
                            "to": newest["sha256"], "to_version": newest["version"], "author": newest["author"]}));
    }
    Ok(json!({"updated": updated, "current": current, "gone": gone, "clashes": clashes}))
}

/// The source of every listing this agent pins, each checked against its
/// hash. A second pinned listing under a binding already sent is left out
/// and named in `clashes`.
fn sync(lab: &Lab, state: &Map<String, Value>, agent: &str) -> Result<Value, Fail> {
    let (mut out, mut missing, mut damaged, mut behind, mut clashes, mut total) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0usize);
    let mut bound = BTreeSet::new();
    for (id, sha) in pins_of(state, agent) {
        let Some((l, v)) = listings(state).get(&id).and_then(|l| Some((l, versions(l).iter().find(|v| v["sha256"] == sha)?))) else {
            missing.push(json!(id));
            continue;
        };
        if !bound.insert(text(&l["binding"])) {
            clashes.push(json!(id));
            continue;
        }
        if latest(l)["sha256"] != sha {
            behind.push(json!(id));
        }
        let source = match lab.read_blob(sha.as_str().unwrap_or_default()) {
            Ok(s) => s,
            Err(Denied(why) | Internal(why)) => {
                damaged.push(json!({"id": id, "error": why}));
                continue;
            }
        };
        total += source.len();
        if total > MAX_SYNC_BYTES {
            return Err(Denied(format!("the pinned tools come to more than {} MiB; unpin some", MAX_SYNC_BYTES >> 20)));
        }
        let mut t = loadable(&id, l, v);
        t["source"] = json!(source);
        out.push(t);
    }
    Ok(json!({"agent": agent, "tools": out, "missing": missing, "damaged": damaged, "behind": behind, "clashes": clashes,
              "unread": unread(state, agent)}))
}

fn fetch(lab: &Lab, state: &Map<String, Value>, p: &Map<String, Value>) -> Result<Value, Fail> {
    let id = required_id(p)?;
    let l = listing(state, id)?;
    let v = pick_kept(l, id, str_param(p, "sha256")?)?;
    let mut t = loadable(id, l, v);
    t["pending"] = json!(v["version"].is_null());
    t["source"] = json!(lab.read_blob(text(&v["sha256"]))?);
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
    let (about, sha) = match id_param(p, "listing")? {
        None => (Value::Null, Value::Null),
        Some(id) => (json!(id), pick_kept(listing(state, id)?, id, str_param(p, "sha256")?)?["sha256"].clone()),
    };
    let id = post(state, agent, to, subject, body, about, sha)?;
    Ok(json!({"id": id, "to": to}))
}

/// Adds a message to the mailbox, dropping read mail first if it is full.
fn post(state: &mut Map<String, Value>, from: &str, to: &str, subject: &str, body: &str, about: Value, sha: Value)
    -> Result<u64, Fail> {
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
        "id": id, "from": from, "to": to, "subject": subject, "body": body,
        "listing": about, "sha256": sha, "sent_at": now, "acked_by": [],
    }));
    Ok(id)
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

/// One line for the start of an agent's session: what is new in the shop,
/// how many pinned listings have newer versions, and unread mail. None when
/// there is no shop yet or nothing to report. It only reads.
pub fn news_line(dir: &Path, agent: &str) -> Option<String> {
    let lab = Lab { dir: dir.to_path_buf() };
    if !lab.state_path().exists() {
        return None;
    }
    let state = match lab.load() {
        Ok(s) => s,
        Err(Denied(why) | Internal(why)) => return Some(format!("TeamLab: {why}")),
    };
    let st = status(&lab, &state, agent);
    let mut parts = Vec::new();
    let new = st["new"].as_array().cloned().unwrap_or_default();
    if !new.is_empty() {
        let shown: Vec<String> = new.iter().take(3).map(|l| format!(
            "a {} from {} that {} (id {})", text(&l["kind"]),
            text(&l["latest"]["author"]), lowercase_first(text(&l["tag"])),
            text(&l["id"]))).collect();
        let more = if new.len() > 3 { format!(", and {} more", new.len() - 3) } else { String::new() };
        parts.push(format!("new in the shop: {}{more}", shown.join("; ")));
    }
    let behind = st["pinned"].as_array().into_iter().flatten().filter(|l| l["behind"] == true).count();
    if behind > 0 {
        parts.push(format!("{behind} of your pinned tools {} a newer version", if behind == 1 { "has" } else { "have" }));
    }
    let review = st["proposals"].as_array().map_or(0, Vec::len);
    if review > 0 {
        parts.push(format!("{review} proposed version{} of your tools {} your review", if review == 1 { "" } else { "s" },
                           if review == 1 { "awaits" } else { "await" }));
    }
    let unread = st["unread"].as_u64().unwrap_or(0);
    if unread > 0 {
        parts.push(format!("{unread} unread message{}", if unread == 1 { "" } else { "s" }));
    }
    (!parts.is_empty()).then(|| format!("TeamLab: {}.", parts.join("; ")))
}

fn lowercase_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        // "Parses TOML" reads on after "that"; an acronym like "TOML" keeps its case.
        Some(f) if c.clone().next().is_some_and(char::is_lowercase) => f.to_lowercase().chain(c).collect(),
        _ => s.to_string(),
    }
}

/// What a receipt keeps of a teamlab call: tool sources and message bodies,
/// sent or returned, by hash and size rather than in full, and the agent the
/// call acted as.
pub fn receipt_view(agent: &str, params: &Value, result: &Value) -> (Value, Value) {
    fn digest(text: &Value) -> Value {
        match text.as_str() {
            Some(s) => json!({"sha256": util::sha256_hex(s.as_bytes()), "bytes": s.len()}),
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
    use std::os::unix::fs::PermissionsExt;

    struct TmpDir(PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.0.join("blobs"), fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn lab() -> TmpDir {
        TmpDir(std::env::temp_dir().join(format!("teamlab-test-{}", util::hex(8))))
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

    /// Lists (or adds a version to) `agent`'s function bound as `binding`.
    fn list(dir: &Path, agent: &str, binding: &str, source: &str) -> Value {
        ok(dir, agent, json!({"action": "publish", "kind": "function", "binding": binding, "source": source,
                              "tag": format!("Does what {binding} does")}))
    }

    fn id(r: &Value) -> &str {
        r["id"].as_str().unwrap()
    }

    const SRC: &str = "hello(x) = \"hello, $x\"\n";

    #[test]
    fn publish_lists_and_pins_for_the_author_and_sync_returns_verified_source() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        assert!(valid_id(id(&r)) && id(&r).len() == 8, "{r}");
        assert_eq!((r["version"].clone(), r["new"].clone(), r["new_listing"].clone(), r["pinned"].clone()),
                   (json!(1), json!(true), json!(true), json!(true)));
        let again = list(&d.0, "codex", "hello", SRC);
        assert_eq!((id(&again), again["new"].clone()), (id(&r), json!(false)), "the same source is idempotent");
        let v2 = ok(&d.0, "codex", json!({"action": "publish", "binding": "hello", "source": "hello(x) = 1\n"}));
        assert_eq!((id(&v2), v2["version"].clone(), v2["new_listing"].clone()), (id(&r), json!(2), json!(false)),
                   "the author's next version needs no kind or tag");
        let s = ok(&d.0, "codex", json!({"action": "sync"}));
        assert_eq!(s["tools"][0]["source"], "hello(x) = 1\n");
        assert_eq!((s["tools"][0]["binding"].clone(), s["tools"][0]["kind"].clone(), s["tools"][0]["id"].clone()),
                   (json!("hello"), json!("function"), json!(id(&r))));
        // Another agent sees the listing in the shop but has not pinned it.
        let b = ok(&d.0, "claude", json!({"action": "browse"}));
        assert_eq!((b["listed"].clone(), b["listings"][0]["tag"].clone()), (json!(1), json!("Does what hello does")));
        assert_eq!(b["listings"][0]["pinned"], Value::Null);
        assert_eq!(b["listings"][0]["pinned_by"], json!(["codex"]));
        assert_eq!(ok(&d.0, "claude", json!({"action": "sync"}))["tools"], json!([]));
        let blob = d.0.join("blobs").join(format!("{}.jl", r["sha256"].as_str().unwrap()));
        assert_eq!(fs::metadata(blob).unwrap().permissions().mode() & 0o777, 0o444);
    }

    #[test]
    fn a_new_listing_needs_a_kind_and_a_short_tag() {
        let d = lab();
        let e = call(&d.0, "codex", json!({"action": "publish", "binding": "f", "source": "f() = 1\n"})).unwrap_err();
        assert!(e.contains("needs a 'kind'"), "{e}");
        let e = call(&d.0, "codex", json!({"action": "publish", "binding": "f", "source": "f() = 1\n", "kind": "function"})).unwrap_err();
        assert!(e.contains("'tag'"), "{e}");
        let long = vec!["word"; MAX_TAG_WORDS + 1].join(" ");
        let e = call(&d.0, "codex", json!({"action": "publish", "binding": "f", "source": "f() = 1\n", "kind": "function", "tag": long})).unwrap_err();
        assert!(e.contains("1 to 15 words"), "{e}");
        let e = call(&d.0, "codex", json!({"action": "publish", "binding": "f", "source": "f() = 1\n", "kind": "macro", "tag": "x"})).unwrap_err();
        assert!(e.contains("function, module"), "{e}");
        let r = ok(&d.0, "codex", json!({"action": "publish", "binding": "F", "source": "module F end\n", "kind": "module",
                                          "tag": "  Parses\nTOML   files "}));
        assert_eq!((r["tag"].clone(), r["kind"].clone()), (json!("Parses TOML files"), json!("module")));
    }

    #[test]
    fn anyone_adds_versions_by_id_but_only_the_author_retags() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        // Another agent's version waits for the author as a proposal; the proposals tests cover the rest.
        let v2 = ok(&d.0, "claude", json!({"action": "publish", "id": id(&r), "binding": "hello", "source": "hello(x) = 2\n"}));
        assert_eq!((v2["version"].clone(), v2["proposed"].clone(), v2["pinned"].clone()), (Value::Null, json!(true), json!(false)));
        assert_eq!(ok(&d.0, "gemini", json!({"action": "fetch", "id": id(&r)}))["author"], "codex");
        let e = call(&d.0, "claude", json!({"action": "publish", "id": id(&r), "binding": "hello", "source": SRC, "tag": "Something else"})).unwrap_err();
        assert!(e.contains("only codex"), "{e}");
        let e = call(&d.0, "claude", json!({"action": "publish", "id": id(&r), "binding": "other", "source": SRC})).unwrap_err();
        assert!(e.contains("binds"), "{e}");
        let e = call(&d.0, "claude", json!({"action": "publish", "id": id(&r), "binding": "hello", "kind": "module", "source": SRC})).unwrap_err();
        assert!(e.contains("is a"), "{e}");
        let t = ok(&d.0, "codex", json!({"action": "publish", "id": id(&r), "binding": "hello", "source": SRC, "tag": "Greets  someone"}));
        assert_eq!(t["tag"], "Greets someone");
    }

    fn blob(dir: &Path, r: &Value) -> PathBuf {
        dir.join("blobs").join(format!("{}.jl", r["sha256"].as_str().unwrap()))
    }

    /// `agent` proposes `source` as a new version of codex's `hello` listing `id`.
    fn propose(dir: &Path, agent: &str, id: &str, source: &str) -> Value {
        ok(dir, agent, json!({"action": "publish", "id": id, "binding": "hello", "source": source, "note": "faster"}))
    }

    #[test]
    fn a_proposal_waits_for_the_author_who_is_mailed() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        ok(&d.0, "claude", json!({"action": "pin", "id": id(&r)}));
        let pr = propose(&d.0, "claude", id(&r), "hello(x) = 2\n");
        assert_eq!((pr["author"].clone(), pr["notified"].clone(), pr["new"].clone()), (json!("codex"), json!(true), json!(true)));
        assert_eq!(propose(&d.0, "claude", id(&r), "hello(x) = 2\n")["new"], false, "proposing it again changes nothing");
        // Not the newest version: update and sync keep the author's.
        assert_eq!(ok(&d.0, "claude", json!({"action": "update"}))["current"], json!([id(&r)]));
        assert_eq!(ok(&d.0, "claude", json!({"action": "sync"}))["tools"][0]["sha256"], r["sha256"]);
        let b = ok(&d.0, "gemini", json!({"action": "browse"}));
        assert_eq!((b["listings"][0]["versions"].clone(), b["listings"][0]["proposals"][0]["author"].clone()), (json!(1), json!("claude")));
        // Anyone can fetch it to review it.
        let f = ok(&d.0, "gemini", json!({"action": "fetch", "id": id(&r), "sha256": pr["sha256"]}));
        assert_eq!((f["pending"].clone(), f["source"].clone(), f["author"].clone()), (json!(true), json!("hello(x) = 2\n"), json!("claude")));
        assert_eq!(ok(&d.0, "gemini", json!({"action": "fetch", "id": id(&r)}))["pending"], false);
        let st = ok(&d.0, "codex", json!({"action": "status"}));
        assert_eq!((st["proposals"][0]["sha256"].clone(), st["proposals"][0]["binding"].clone()), (pr["sha256"].clone(), json!("hello")));
        assert_eq!(ok(&d.0, "claude", json!({"action": "status"}))["proposals"], json!([]));
        let mail = &ok(&d.0, "codex", json!({"action": "inbox"}))["messages"][0];
        assert_eq!((mail["from"].clone(), mail["listing"].clone(), mail["sha256"].clone()), (json!("claude"), json!(id(&r)), pr["sha256"].clone()));
        assert!(mail["subject"].as_str().unwrap().contains("proposed a new version of hello"));
        assert!(mail["body"].as_str().unwrap().ends_with("faster"));
        assert_eq!(news_line(&d.0, "codex").unwrap(), "TeamLab: 1 proposed version of your tools awaits your review; 1 unread message.");
    }

    #[test]
    fn accepting_a_proposal_makes_it_newest_credited_to_its_proposer() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        let pr = propose(&d.0, "claude", id(&r), "hello(x) = 2\n");
        let accept = json!({"action": "accept", "id": id(&r), "sha256": pr["sha256"], "note": "thanks"});
        let e = call(&d.0, "gemini", accept.clone()).unwrap_err();
        assert!(e.contains("only codex, who listed"), "{e}");
        assert!(call(&d.0, "claude", accept.clone()).unwrap_err().contains("only codex"));
        let a = ok(&d.0, "codex", accept.clone());
        assert_eq!((a["version"].clone(), a["author"].clone(), a["pinned"].clone(), a["notified"].clone()),
                   (json!(2), json!("claude"), json!(true), json!(true)));
        let f = ok(&d.0, "gemini", json!({"action": "fetch", "id": id(&r)}));
        assert_eq!((f["version"].clone(), f["author"].clone(), f["pending"].clone()), (json!(2), json!("claude"), json!(false)));
        let b = ok(&d.0, "codex", json!({"action": "browse"}));
        assert_eq!((b["listings"][0]["pinned_version"].clone(), b["listings"][0]["proposals"].clone()), (json!(2), json!([])));
        assert!(call(&d.0, "codex", accept).unwrap_err().contains("no pending proposal"));
        let mail = &ok(&d.0, "claude", json!({"action": "inbox"}))["messages"][0];
        assert!(mail["subject"].as_str().unwrap().starts_with("accepted your version of hello"));
        assert!(mail["body"].as_str().unwrap().contains("as version 2"));
        // The proposer's approval of their own version is still refused.
        assert!(call(&d.0, "claude", json!({"action": "approve", "id": id(&r)})).unwrap_err().contains("another agent"));
    }

    #[test]
    fn declining_or_withdrawing_a_proposal_drops_it_and_its_blob() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        let pc = propose(&d.0, "claude", id(&r), "hello(x) = 2\n");
        let pg = propose(&d.0, "gemini", id(&r), "hello(x) = 3\n");
        assert!(blob(&d.0, &pc).exists() && blob(&d.0, &pg).exists());
        let e = call(&d.0, "gemini", json!({"action": "decline", "id": id(&r), "sha256": pc["sha256"]})).unwrap_err();
        assert!(e.contains("only codex, who listed"), "{e}");
        let x = ok(&d.0, "codex", json!({"action": "decline", "id": id(&r), "sha256": pc["sha256"], "note": "too clever"}));
        assert_eq!((x["declined"].clone(), x["withdrawn"].clone(), x["notified"].clone()), (json!(true), json!(false), json!(true)));
        assert!(!blob(&d.0, &pc).exists());
        let mail = &ok(&d.0, "claude", json!({"action": "inbox"}))["messages"][0];
        assert!(mail["subject"].as_str().unwrap().starts_with("declined your version") && mail["body"].as_str().unwrap().ends_with("too clever"));
        let w = ok(&d.0, "gemini", json!({"action": "decline", "id": id(&r), "sha256": pg["sha256"]}));
        assert_eq!((w["declined"].clone(), w["withdrawn"].clone()), (json!(false), json!(true)));
        assert!(!blob(&d.0, &pg).exists());
        let subjects: Vec<Value> = ok(&d.0, "codex", json!({"action": "inbox"}))["messages"].as_array().unwrap().iter().map(|m| m["subject"].clone()).collect();
        assert!(subjects.last().unwrap().as_str().unwrap().starts_with("withdrew a proposed version"), "{subjects:?}");
        assert_eq!(ok(&d.0, "codex", json!({"action": "status"}))["proposals"], json!([]));
        assert!(blob(&d.0, &r).exists(), "the listed version stays");
    }

    #[test]
    fn a_new_proposal_supersedes_the_agents_last_and_proposals_are_capped() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        let first = propose(&d.0, "claude", id(&r), "hello(x) = 2\n");
        let second = propose(&d.0, "claude", id(&r), "hello(x) = 3\n");
        assert_eq!(second["superseded"], first["sha256"]);
        assert!(!blob(&d.0, &first).exists() && blob(&d.0, &second).exists());
        assert_eq!(ok(&d.0, "codex", json!({"action": "status"}))["proposals"].as_array().unwrap().len(), 1);
        for i in 1..MAX_PROPOSALS {
            propose(&d.0, &format!("agent{i}"), id(&r), &format!("hello(x) = {}\n", 10 + i));
        }
        let e = call(&d.0, "late", json!({"action": "publish", "id": id(&r), "binding": "hello", "source": "hello(x) = 0\n"})).unwrap_err();
        assert!(e.contains(&format!("already has {MAX_PROPOSALS} proposals awaiting codex")), "{e}");
        // An agent replacing its own proposal still fits.
        propose(&d.0, "claude", id(&r), "hello(x) = 4\n");
        // A malformed proposal makes the index unreadable rather than loading half of it.
        let mut state: Value = serde_json::from_slice(&fs::read(d.0.join("state.json")).unwrap()).unwrap();
        state["tools"][id(&r)]["proposals"][0]["sha256"] = json!("nope");
        fs::write(d.0.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(call(&d.0, "codex", json!({"action": "status"})).unwrap_err().contains("malformed proposal"));
    }

    #[test]
    fn two_listings_of_one_binding_never_both_load() {
        let d = lab();
        let a = list(&d.0, "codex", "hello", SRC);
        let b = list(&d.0, "gemini", "hello", "hello(x) = 3\n");
        assert_ne!(id(&a), id(&b));
        ok(&d.0, "claude", json!({"action": "pin", "id": id(&a)}));
        let e = call(&d.0, "claude", json!({"action": "pin", "id": id(&b)})).unwrap_err();
        assert!(e.contains("unpin it first"), "{e}");
        // claude lists its own hello while pinning codex's: it is listed, not pinned.
        let c = list(&d.0, "claude", "hello", "hello(x) = 4\n");
        assert_eq!((c["pinned"].clone(), c["clash"].clone()), (json!(false), json!(id(&a))));
        let u = ok(&d.0, "claude", json!({"action": "update", "ids": [id(&b)]}));
        assert_eq!(u["clashes"][0]["pinned"], id(&a));
        // A hand-edited index with two pins on one binding syncs only one.
        let mut state: Value = serde_json::from_slice(&fs::read(d.0.join("state.json")).unwrap()).unwrap();
        state["pins"]["claude"][id(&b)] = b["sha256"].clone();
        fs::write(d.0.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        let s = ok(&d.0, "claude", json!({"action": "sync"}));
        assert_eq!(s["tools"].as_array().unwrap().len(), 1);
        assert_eq!(s["clashes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn an_author_cannot_approve_their_own_version_but_another_agent_can_once() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        let e = call(&d.0, "codex", json!({"action": "approve", "id": id(&r)})).unwrap_err();
        assert!(e.contains("another agent has to approve"), "{e}");
        assert_eq!(ok(&d.0, "claude", json!({"action": "approve", "id": id(&r), "note": "read it"}))["new"], true);
        assert_eq!(ok(&d.0, "claude", json!({"action": "approve", "id": id(&r)}))["new"], false);
        let b = ok(&d.0, "gemini", json!({"action": "browse"}));
        assert_eq!(b["listings"][0]["latest"]["approvals"][0]["by"], "claude");
        assert_eq!(b["listings"][0]["latest"]["approvals"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn pins_hold_a_version_until_update_moves_them() {
        let d = lab();
        let v1 = list(&d.0, "codex", "hello", SRC);
        ok(&d.0, "claude", json!({"action": "pin", "id": id(&v1)}));
        let v2 = list(&d.0, "codex", "hello", "hello(x) = 2\n");
        let s = ok(&d.0, "claude", json!({"action": "sync"}));
        assert_eq!(s["tools"][0]["sha256"], v1["sha256"], "a pin does not follow new versions");
        assert_eq!(s["behind"], json!([id(&v1)]));
        assert_eq!(ok(&d.0, "claude", json!({"action": "status"}))["pinned"][0]["behind"], true);
        let u = ok(&d.0, "claude", json!({"action": "update"}));
        assert_eq!((u["updated"][0]["to"].clone(), u["updated"][0]["from_version"].clone()), (v2["sha256"].clone(), json!(1)));
        assert_eq!(ok(&d.0, "claude", json!({"action": "sync"}))["tools"][0]["source"], "hello(x) = 2\n");
        ok(&d.0, "claude", json!({"action": "pin", "id": id(&v1), "sha256": v1["sha256"]}));
        assert_eq!(ok(&d.0, "claude", json!({"action": "status"}))["pinned"][0]["pinned_version"], 1);
        assert_eq!(ok(&d.0, "claude", json!({"action": "unpin", "id": id(&v1)}))["unpinned"], true);
        assert_eq!(ok(&d.0, "claude", json!({"action": "sync"}))["tools"], json!([]));
    }

    #[test]
    fn browse_matches_every_query_word_newest_first() {
        let d = lab();
        let toml = ok(&d.0, "codex", json!({"action": "publish", "kind": "function", "binding": "parse_toml",
                                             "source": "parse_toml(s) = s\n", "tag": "Parses TOML text into a Dict"}));
        let csv = ok(&d.0, "gemini", json!({"action": "publish", "kind": "module", "binding": "Csv",
                                            "source": "module Csv end\n", "tag": "Reads CSV files into named tuples"}));
        let all = ok(&d.0, "claude", json!({"action": "browse"}));
        let order: Vec<&str> = all["listings"].as_array().unwrap().iter().map(|l| l["id"].as_str().unwrap()).collect();
        assert_eq!(order, [id(&csv), id(&toml)]);
        for (q, want) in [("toml dict", vec![id(&toml)]), ("MODULE", vec![id(&csv)]), ("gemini files", vec![id(&csv)]),
                          (&id(&toml)[..6], vec![id(&toml)]), ("toml csv", vec![])] {
            let b = ok(&d.0, "claude", json!({"action": "browse", "query": q}));
            let got: Vec<&str> = b["listings"].as_array().unwrap().iter().map(|l| l["id"].as_str().unwrap()).collect();
            assert_eq!(got, want, "{q}");
        }
    }

    #[test]
    fn status_shows_other_agents_new_listings_until_pinned() {
        let d = lab();
        let mine = list(&d.0, "claude", "mine", "mine() = 1\n");
        let theirs = list(&d.0, "codex", "theirs", "theirs() = 1\n");
        let st = ok(&d.0, "claude", json!({"action": "status"}));
        assert_eq!(st["new"].as_array().unwrap().iter().map(|l| l["id"].clone()).collect::<Vec<_>>(), [json!(id(&theirs))]);
        assert_eq!((st["listed"].clone(), st["pinned"][0]["id"].clone()), (json!(2), json!(id(&mine))));
        ok(&d.0, "claude", json!({"action": "pin", "id": id(&theirs)}));
        assert_eq!(ok(&d.0, "claude", json!({"action": "status"}))["new"], json!([]));
        // A week-old listing is no longer new.
        let mut state: Value = serde_json::from_slice(&fs::read(d.0.join("state.json")).unwrap()).unwrap();
        state["tools"][id(&mine)]["versions"][0]["published_at"] = json!(util::now_secs() - NEW_FOR_S - 60.0);
        fs::write(d.0.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(ok(&d.0, "codex", json!({"action": "status"}))["new"], json!([]));
    }

    #[test]
    fn news_line_reports_new_listings_stale_pins_and_mail() {
        let d = lab();
        assert_eq!(news_line(&d.0, "claude"), None, "no shop yet");
        assert!(!d.0.exists(), "news_line creates nothing");
        let a = ok(&d.0, "codex", json!({"action": "publish", "kind": "function", "binding": "parse_toml",
                                          "source": "parse_toml(s) = s\n", "tag": "Parses TOML text"}));
        assert_eq!(news_line(&d.0, "codex"), None, "nothing new for the author");
        let line = news_line(&d.0, "claude").unwrap();
        assert_eq!(line, format!("TeamLab: new in the shop: a function from codex that parses TOML text (id {}).", id(&a)));
        ok(&d.0, "claude", json!({"action": "pin", "id": id(&a)}));
        ok(&d.0, "codex", json!({"action": "publish", "binding": "parse_toml", "source": "parse_toml(s) = 2s\n"}));
        ok(&d.0, "codex", json!({"action": "send", "to": "claude", "subject": "v2 is out"}));
        assert_eq!(news_line(&d.0, "claude").unwrap(), "TeamLab: 1 of your pinned tools has a newer version; 1 unread message.");
        for i in 0..4 {
            list(&d.0, "gemini", &format!("tool{i}"), &format!("tool{i}() = 1\n"));
        }
        assert!(news_line(&d.0, "claude").unwrap().contains(", and 1 more;"));
        fs::write(d.0.join("state.json"), b"{").unwrap();
        assert!(news_line(&d.0, "claude").unwrap().starts_with("TeamLab: teamlab index"));
    }

    #[test]
    fn the_shop_dir_follows_provisioning() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| {
            pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        let dir = |pairs| dir_from_env(env(pairs)).unwrap();
        assert_eq!(dir(&[("HOME", "/h")]), Some(PathBuf::from("/h/.local/share/palette/teamlab")));
        assert_eq!(dir(&[("HOME", "/h"), ("XDG_DATA_HOME", "/x")]), Some(PathBuf::from("/x/palette/teamlab")));
        assert_eq!(dir(&[("HOME", "/h"), ("XDG_DATA_HOME", "rel")]), Some(PathBuf::from("/h/.local/share/palette/teamlab")));
        assert_eq!(dir(&[("HOME", "/h"), ("PALETTE_TEAMLAB_DIR", "/lab")]), Some(PathBuf::from("/lab")));
        assert_eq!(dir(&[("HOME", "/h"), ("PALETTE_TEAMLAB_DIR", "~/lab")]), Some(PathBuf::from("/h/lab")));
        assert_eq!(dir(&[("HOME", "/h"), ("PALETTE_TEAMLAB_DIR", "~")]), Some(PathBuf::from("/h")));
        assert_eq!(dir(&[("HOME", "/h"), ("PALETTE_TEAMLAB_DIR", "")]), None);
        assert_eq!(dir(&[("HOME", "/h"), ("PALETTE_TEAMLAB_DIR", " off ")]), None);
        assert_eq!(dir(&[]), None);
        for relative in ["relative", "~root/lab"] {
            let pairs: &'static [(&str, &str)] = Box::leak(Box::new([("HOME", "/h"), ("PALETTE_TEAMLAB_DIR", relative)]));
            assert_eq!(dir_from_env(env(pairs)).unwrap_err(), format!("PALETTE_TEAMLAB_DIR must be an absolute path, not '{relative}'"));
        }
    }

    #[test]
    fn a_format_1_index_reads_as_listings_and_is_written_back_as_format_2() {
        let d = lab();
        let l = Lab::open(&d.0).unwrap();
        let sha = util::sha256_hex(SRC.as_bytes());
        l.put_blob(&sha, SRC.as_bytes()).unwrap();
        let v = |note: Value| json!({"sha256": sha, "version": 1, "author": "codex", "published_at": 100.0, "bytes": SRC.len(),
                                     "note": note, "test": null, "approvals": []});
        fs::write(d.0.join("state.json"), serde_json::to_vec(&json!({
            "format": 1, "next_message_id": 2,
            "tools": {"hello": [v(json!("\nGreets a name, politely and at length\nmore"))], "two_words": [v(Value::Null)]},
            "pins": {"claude": {"hello": sha, "gone": sha}},
            "messages": [{"id": 1, "from": "codex", "to": "claude", "subject": "s", "body": "", "tool": "hello",
                          "sha256": sha, "sent_at": 100.0, "acked_by": []}],
        })).unwrap()).unwrap();
        let b = ok(&d.0, "claude", json!({"action": "browse"}));
        let by: BTreeMap<&str, &Value> = b["listings"].as_array().unwrap().iter().map(|l| (l["binding"].as_str().unwrap(), l)).collect();
        assert_eq!(by["hello"]["tag"], "Greets a name, politely and at length");
        assert_eq!(by["two_words"]["tag"], "two words");
        assert_eq!((by["hello"]["kind"].clone(), by["hello"]["author"].clone()), (json!("function"), json!("codex")));
        assert_eq!(by["hello"]["pinned"], json!(sha));
        let hello = by["hello"]["id"].as_str().unwrap().to_string();
        assert_eq!(ok(&d.0, "claude", json!({"action": "inbox"}))["messages"][0]["listing"], json!(hello));
        assert_eq!(ok(&d.0, "claude", json!({"action": "sync"}))["tools"][0]["source"], SRC);
        // Reads leave it as format 1; the first change writes format 2.
        assert!(fs::read_to_string(d.0.join("state.json")).unwrap().contains("\"format\":1"));
        ok(&d.0, "claude", json!({"action": "ack", "ids": "all"}));
        let saved: Value = serde_json::from_slice(&fs::read(d.0.join("state.json")).unwrap()).unwrap();
        assert_eq!(saved["format"], 2);
        assert_eq!(saved["pins"]["claude"], json!({hello.clone(): sha}));
        // The author's next publish of the binding lands on the migrated listing.
        let r = ok(&d.0, "codex", json!({"action": "publish", "binding": "hello", "source": "hello(x) = 2\n"}));
        assert_eq!((r["id"].clone(), r["version"].clone()), (json!(hello), json!(2)));
    }

    #[test]
    fn listing_ids_grow_until_unique() {
        let full = util::sha256_hex(b"codex\nhello\nabc");
        let mut taken = Map::new();
        assert_eq!(new_listing_id(&taken, "codex", "hello", "abc"), full[..8]);
        taken.insert(full[..8].to_string(), json!({}));
        assert_eq!(new_listing_id(&taken, "codex", "hello", "abc"), full[..12]);
    }

    #[test]
    fn a_listing_missing_its_fields_makes_the_index_unreadable() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        let mut state: Value = serde_json::from_slice(&fs::read(d.0.join("state.json")).unwrap()).unwrap();
        state["tools"][id(&r)].as_object_mut().unwrap().remove("tag");
        fs::write(d.0.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        let e = call(&d.0, "codex", json!({"action": "status"})).unwrap_err();
        assert!(e.contains("unreadable") && e.contains(id(&r)), "{e}");
        state["tools"][id(&r)]["tag"] = json!("restored");
        state["tools"][id(&r)]["versions"][0]["sha256"] = json!("../../etc/passwd");
        fs::write(d.0.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        let e = call(&d.0, "codex", json!({"action": "status"})).unwrap_err();
        assert!(e.contains("unreadable") && e.contains(id(&r)), "a version's sha256 must be a hash: {e}");
    }

    #[test]
    fn a_tampered_blob_is_refused_not_loaded() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        let blob = d.0.join("blobs").join(format!("{}.jl", r["sha256"].as_str().unwrap()));
        fs::set_permissions(&blob, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&blob, "run(`rm -rf ~`)\n").unwrap();
        let other = list(&d.0, "codex", "other", "other() = 2\n");
        let e = call(&d.0, "codex", json!({"action": "fetch", "id": id(&r)})).unwrap_err();
        assert!(e.contains("tampered"), "{e}");
        let s = ok(&d.0, "codex", json!({"action": "sync"}));
        assert_eq!(s["tools"].as_array().unwrap().iter().map(|t| t["id"].clone()).collect::<Vec<_>>(), [other["id"].clone()],
                   "the other pinned tool still loads");
        assert_eq!(s["damaged"][0]["id"], r["id"]);
        assert!(s["damaged"][0]["error"].as_str().unwrap().contains("tampered"), "{s}");
        list(&d.0, "codex", "hello", SRC);
        assert_eq!(ok(&d.0, "codex", json!({"action": "sync"}))["tools"][0]["source"], SRC);
    }

    #[test]
    fn a_symlinked_blob_is_refused() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        let blob = d.0.join("blobs").join(format!("{}.jl", r["sha256"].as_str().unwrap()));
        let elsewhere = d.0.join("elsewhere.jl");
        fs::write(&elsewhere, SRC).unwrap();
        fs::remove_file(&blob).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &blob).unwrap();
        let s = ok(&d.0, "codex", json!({"action": "sync"}));
        assert_eq!((s["tools"].as_array().unwrap().len(), s["damaged"][0]["id"].clone()), (0, r["id"].clone()), "{s}");
        assert!(call(&d.0, "codex", json!({"action": "fetch", "id": id(&r)})).unwrap_err().starts_with("internal"));
    }

    #[test]
    fn a_shop_dir_that_is_a_symlink_or_writable_by_others_is_refused() {
        let d = lab();
        list(&d.0, "codex", "hello", SRC);
        let link = d.0.with_extension("link");
        std::os::unix::fs::symlink(&d.0, &link).unwrap();
        let e = call(&link, "codex", json!({"action": "status"})).unwrap_err();
        let _ = fs::remove_file(&link);
        assert!(e.contains("not a symlink"), "{e}");
        for dir in [d.0.clone(), d.0.join("blobs")] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o770)).unwrap();
            let e = call(&d.0, "codex", json!({"action": "status"})).unwrap_err();
            assert!(e.contains("only its owner can write"), "{}: {e}", dir.display());
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        ok(&d.0, "codex", json!({"action": "status"}));
    }

    #[test]
    fn the_index_and_the_kept_source_are_capped() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        let l = Lab::open(&d.0).unwrap();
        let mut state = l.load().unwrap();
        listings_mut(&mut state)[id(&r)]["versions"][0]["bytes"] = json!(MAX_BLOB_BYTES);
        l.save(&state).unwrap();
        let e = call(&d.0, "codex", json!({"action": "publish", "binding": "hello", "source": "hello(x) = 2\n"})).unwrap_err();
        assert!(e.contains("MiB of tool source"), "{e}");
        assert_eq!(fs::read_dir(d.0.join("blobs")).unwrap().count(), 1, "no blob is written for a refused version");
        list(&d.0, "codex", "hello", SRC);

        listings_mut(&mut state)[id(&r)]["tag"] = json!("x".repeat(MAX_INDEX_BYTES));
        match l.save(&state) {
            Err(Denied(why)) => assert!(why.contains("index would pass"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(l.load().is_ok(), "the saved index is left as it was");
    }

    #[test]
    fn an_unreadable_index_is_never_overwritten() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        ok(&d.0, "claude", json!({"action": "pin", "id": id(&r)}));
        fs::write(d.0.join("state.json"), b"{\"format\": 2, \"tools\": {\"hel").unwrap();
        for p in [json!({"action": "status"}), json!({"action": "browse"}),
                  json!({"action": "publish", "binding": "other", "source": SRC}),
                  json!({"action": "send", "to": "all", "subject": "hi"})] {
            let e = call(&d.0, "codex", p).unwrap_err();
            assert!(e.contains("unreadable") && e.contains("state.prev.json"), "{e}");
        }
        assert_eq!(fs::read(d.0.join("state.json")).unwrap(), b"{\"format\": 2, \"tools\": {\"hel");
        fs::copy(d.0.join("state.prev.json"), d.0.join("state.json")).unwrap();
        assert_eq!(ok(&d.0, "codex", json!({"action": "browse"}))["listings"][0]["id"], id(&r));
    }

    #[test]
    fn the_oldest_unpinned_version_makes_room_and_its_blob_goes() {
        let d = lab();
        let first = list(&d.0, "codex", "t", "t() = 0\n");
        ok(&d.0, "claude", json!({"action": "pin", "id": id(&first)}));
        let second = list(&d.0, "codex", "t", "t() = 1\n");
        for i in 2..=MAX_VERSIONS {
            list(&d.0, "codex", "t", &format!("t() = {i}\n"));
        }
        let b = ok(&d.0, "codex", json!({"action": "browse"}));
        assert_eq!(b["listings"][0]["versions"], MAX_VERSIONS);
        assert_eq!(b["listings"][0]["latest"]["version"], MAX_VERSIONS as u64 + 1);
        assert_eq!(ok(&d.0, "claude", json!({"action": "sync"}))["tools"][0]["sha256"], first["sha256"]);
        assert!(call(&d.0, "claude", json!({"action": "fetch", "id": id(&first), "sha256": second["sha256"]})).is_err());
        assert!(!d.0.join("blobs").join(format!("{}.jl", second["sha256"].as_str().unwrap())).exists());
    }

    #[test]
    fn mail_reaches_the_addressee_and_broadcasts_reach_everyone_else() {
        let d = lab();
        let r = list(&d.0, "codex", "hello", SRC);
        let m1 = ok(&d.0, "codex", json!({"action": "send", "to": "claude", "subject": "review hello?", "listing": id(&r)}));
        ok(&d.0, "codex", json!({"action": "send", "to": "all", "subject": "new in the shop", "body": "see browse"}));
        let c = ok(&d.0, "claude", json!({"action": "inbox"}));
        assert_eq!(c["unread"], 2);
        assert_eq!(c["messages"][0]["listing"], id(&r));
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
        assert!(call(&d.0, "claude", json!({"action": "send", "to": "codex", "subject": "x", "listing": "deadbeef"})).is_err());
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
    fn bad_bindings_ids_sizes_and_actions_are_denied() {
        let d = lab();
        let new = |binding: &str, source: &str| json!({"action": "publish", "kind": "function", "tag": "t", "binding": binding, "source": source});
        for (p, want) in [
            (json!({"action": "nope"}), "must be one of"),
            (json!({"action": "publish", "source": SRC}), "requires the 'binding'"),
            (new("end", SRC), "Julia identifier"),
            (new("TeamLab", SRC), "Julia identifier"),
            (new("a-b", SRC), "Julia identifier"),
            (new("x", &"x".repeat(MAX_SOURCE_BYTES + 1)), "KiB"),
            (new("x", "  "), "non-empty"),
            (json!({"action": "publish", "kind": "function", "tag": "t", "binding": "x", "source": SRC, "test": {"passed": "yes"}}), "'test'"),
            (json!({"action": "pin", "id": "0123abcd"}), "no listing 0123abcd"),
            (json!({"action": "pin", "id": "hello"}), "not a listing id"),
            (json!({"action": "pin"}), "requires a listing 'id'"),
            (json!({"action": "update", "ids": ["x"]}), "list of listing ids"),
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
                    handle(&dir, &agent, json!({"action": "publish", "kind": "function", "tag": "Counts", "binding": format!("tool{t}"),
                                                "source": format!("f() = {i}\n")}).as_object().unwrap()).unwrap();
                    handle(&dir, &agent, json!({"action": "send", "to": "all", "subject": format!("{t}/{i}")})
                        .as_object().unwrap()).unwrap();
                }
            })
        }).collect();
        for t in threads {
            t.join().unwrap();
        }
        let b = ok(&dir, "observer", json!({"action": "browse"}));
        assert_eq!(b["listings"].as_array().unwrap().len(), 8);
        assert!(b["listings"].as_array().unwrap().iter().all(|l| l["versions"] == 10));
        assert_eq!(ok(&dir, "observer", json!({"action": "status"}))["unread"], 80);
    }

    #[test]
    fn receipts_keep_sources_and_bodies_by_hash() {
        let (p, r) = receipt_view("claude",
            &json!({"action": "publish", "binding": "hello", "source": SRC}),
            &json!({"tools": [{"binding": "hello", "source": SRC}], "messages": [{"body": "long"}]}));
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
        for bad in ["", "all", "Claude", "-x", ".x", "a b", "a/b", &"a".repeat(33)] {
            assert!(!valid_agent(bad), "{bad}");
        }
    }
}
