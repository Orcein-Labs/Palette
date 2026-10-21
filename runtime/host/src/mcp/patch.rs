//! Host-side, one-attempt file patch grants, ported from `patch_broker.py`.
//! Approval is never a tool argument: a patch is prepared, then approved by
//! the local administrator command (or a client's form confirmation), then
//! applied once by a Landlock-confined helper process.
//!
//!   palette-host patch --storage S review|approve ID [--digest D]
//!   palette-host patch apply-helper     (the record on stdin)

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::{casefold::casefold, difflib, py_json_loads, run_captured, Fault};
use crate::{provisioning, sandbox, util};

pub(crate) const MAX_BYTES: usize = 65536;
const REQUEST_FIELDS: [&str; 10] = ["request_id", "workspace_id", "target", "root", "root_identity", "changes",
                                    "preview", "created_at", "expiry", "allowed_operations"];
const AUTO_APPLY_CLASSES: [&str; 4] = ["runtime", "tools", "recipes", "adapters"];
const IMMUTABLE_CREDENTIAL_COMPONENTS: [&str; 5] = [".ssh", ".aws", ".gnupg", "credentials", "secrets"];
const CREDENTIAL_DIRS: [&str; 3] = [".ssh", ".aws", ".gnupg"];
/// The directory of human-approved kernel tools (PALETTE_TRUSTED_TOOLS). Only
/// a human approval writes there: no auto-apply class or repair can reach it.
pub(crate) const TRUSTED_TOOLS_TARGET: &str = "trusted_tools";
const APPROVAL_MODES: [&str; 2] = ["client", "administrator"];
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
const APPROVAL_WAIT: Duration = Duration::from_secs(120);

/// `[A-Za-z_][A-Za-z0-9_!]*\.(jl|json)`, matched whole.
fn trusted_tool_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jl").or_else(|| name.strip_suffix(".json")) else { return false };
    let mut chars = stem.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '!')
}

/// PALETTE_PATCH_ROOTS, plus the trusted tool directory when one is set.
pub(crate) fn patch_roots_from_env(env: impl Fn(&str) -> Option<String>) -> Result<Map<String, Value>, Fault> {
    let text = env("PALETTE_PATCH_ROOTS").unwrap_or_else(|| "{}".into());
    let Ok(Value::Object(mut roots)) = serde_json::from_str::<Value>(&text) else {
        return Err(Fault::value("PALETTE_PATCH_ROOTS must be a JSON object"));
    };
    if roots.contains_key(TRUSTED_TOOLS_TARGET) {
        return Err(Fault::value(format!("{} is a reserved patch target; set PALETTE_TRUSTED_TOOLS instead",
                                        util::py_repr(&TRUSTED_TOOLS_TARGET.into()))));
    }
    if let Some(dir) = sandbox::trusted_tools_dir(env("PALETTE_TRUSTED_TOOLS").as_deref()).map_err(Fault::value)? {
        roots.insert(TRUSTED_TOOLS_TARGET.into(), dir.to_string_lossy().into_owned().into());
    }
    Ok(roots)
}

/// PALETTE_PATCH_APPROVAL. "client" (the default) lets a client that shows
/// MCP form elicitation confirm a patch; "administrator" accepts only the
/// local administrator command, so no MCP client can confirm anything.
pub(crate) fn approval_mode(value: Option<&str>) -> Result<&'static str, Fault> {
    let mode = value.unwrap_or("client");
    APPROVAL_MODES.into_iter().find(|m| *m == mode)
        .ok_or_else(|| Fault::value(format!("PALETTE_PATCH_APPROVAL must be one of: {}", APPROVAL_MODES.join(", "))))
}

/// `Path(path).parts`.
fn path_parts(path: &Path) -> Vec<String> {
    path.components().filter_map(|c| match c {
        Component::RootDir => Some("/".to_string()),
        Component::CurDir => None,
        other => Some(other.as_os_str().to_string_lossy().into_owned()),
    }).collect()
}

/// Catch secrets even when the configured patch root is narrowed inside one.
pub(crate) fn credential_location(path: &Path) -> bool {
    let parts: Vec<String> = path_parts(path).iter().map(|p| casefold(p)).collect();
    let basename = parts.last().map_or("", String::as_str);
    let parents = &parts[..parts.len().saturating_sub(1)];
    parents.iter().any(|p| IMMUTABLE_CREDENTIAL_COMPONENTS.contains(&p.as_str()))
        || CREDENTIAL_DIRS.contains(&basename) || basename.starts_with(".env")
        || ["credential", "secret"].iter().any(|t| basename.contains(t))
}

/// How a patch path may be changed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Class {
    pub class: String,
    pub self_apply: bool,
    pub reason: &'static str,
}

fn class(class: &str, self_apply: bool, reason: &'static str) -> Class {
    Class { class: class.into(), self_apply, reason }
}

/// Classify a path using host-owned prefixes and an immutable authority
/// denylist. Prefixes come from the operator at broker construction, never
/// from a patch request. The denylist wins over every prefix.
pub(crate) fn classify_patch_path(relative: &str, prefixes: &[(String, String)]) -> Result<Class, Fault> {
    const PROTECTED_DIRS: [&str; 17] = ["runtime/security", "runtime/host", "runtime/capabilities", "runtime/plugins",
                                        "src/security", "src/host", "host/security", ".github", ".buildkite",
                                        "config", "configs", "deployment", "deploy", "infra", ".ssh", ".aws", ".gnupg"];
    const PROTECTED_NAMES: [&str; 20] = [
        "operator_mcp.py", "operator_workspace_router.py", "runtime_registry.py",
        "patch_broker.py", "repair_broker.py", "broker.py", "sandbox.py",
        "launch_worker.py", "install_operator_plugin.py", "serve_palette.py",
        "capability.json",
        "project.toml", "manifest.toml", "startup.jl", "localpreferences.toml",
        "pyproject.toml", "setup.py", "setup.cfg", "tox.ini", "pytest.ini",
    ];
    const AUTHORITY_TOKENS: [&str; 7] = ["credential", "secret", "privilege", "ceiling", "authority", "sandbox", "broker"];
    const PROTECTED_COMPONENTS: [&str; 13] = ["security", "host", "capabilities", "plugins", "scripts",
                                              "config", "configs", "deployment", "deploy", "infra",
                                              ".ssh", ".aws", ".gnupg"];
    let parts = split_parts(relative)?;
    let lowered: Vec<String> = parts.iter().map(|p| casefold(p)).collect();
    let joined = lowered.join("/");
    let basename = lowered.last().expect("a relative path has a part").as_str();
    let parents = &lowered[..lowered.len() - 1];
    let parent_in = |set: &[&str]| parents.iter().any(|p| set.contains(&p.as_str()));
    if parent_in(&IMMUTABLE_CREDENTIAL_COMPONENTS) || CREDENTIAL_DIRS.contains(&basename)
        || ["credential", "secret"].iter().any(|t| basename.contains(t)) || basename.starts_with(".env") {
        return Ok(class("forbidden", false, "credential or secret path"));
    }
    if PROTECTED_DIRS.iter().any(|item| joined == *item || joined.starts_with(&format!("{item}/"))) {
        return Ok(class("protected", false, "authority path"));
    }
    if parent_in(&PROTECTED_COMPONENTS) {
        return Ok(class("protected", false, "authority directory"));
    }
    if parent_in(&["test", "tests", "verification", "fixtures"])
        || ["test_", "test-", "verify_", "verify-"].iter().any(|p| basename.starts_with(p))
        || ["conftest.py", "pytest.ini", "tox.ini"].contains(&basename) {
        return Ok(class("protected", false, "test or verification path"));
    }
    if PROTECTED_NAMES.contains(&basename) || [".env", "config", "settings"].iter().any(|p| basename.starts_with(p))
        || [".pth", ".jllpath"].iter().any(|s| basename.ends_with(s))
        || AUTHORITY_TOKENS.iter().any(|t| basename.contains(t)) {
        return Ok(class("protected", false, "authority or load-path file"));
    }
    for (class_name, prefix) in prefixes {
        if parts.starts_with(&split_parts(prefix)?) {
            return Ok(Class { class: class_name.clone(), self_apply: true, reason: "operator-configured ordinary path" });
        }
    }
    Ok(class("unknown", false, "no operator-configured ordinary path class"))
}

/// The parts of a patch path: relative, with no traversal or Git metadata.
pub(crate) fn parent_path_parts(relative: &Value) -> Result<Vec<String>, Fault> {
    match relative.as_str() {
        Some(s) => split_parts(s),
        None => Err(Fault::value("File path must be relative")),
    }
}

fn split_parts(relative: &str) -> Result<Vec<String>, Fault> {
    if relative.is_empty() {
        return Err(Fault::value("File path must be relative"));
    }
    let parts: Vec<String> = relative.split('/').map(String::from).collect();
    if parts.iter().any(|p| matches!(p.as_str(), "" | "." | ".." | ".git")) || relative.starts_with('/') {
        return Err(Fault::value("Traversal and Git metadata are forbidden"));
    }
    Ok(parts)
}

/// The digest binding every field a reviewer saw, as of its pending state.
pub(crate) fn request_digest(record: &Map<String, Value>) -> Result<String, Fault> {
    let mut immutable = Map::new();
    for key in REQUEST_FIELDS {
        let value = record.get(key).ok_or_else(|| Fault::new("KeyError", util::py_str(key)))?;
        immutable.insert(key.into(), value.clone());
    }
    immutable.insert("status".into(), "pending".into());
    Ok(util::sha256_hex(util::py_dumps(&Value::Object(immutable), None).as_bytes()))
}

fn write_json(path: &Path, value: &Map<String, Value>) -> Result<(), Fault> {
    let tmp = path.with_file_name(format!("tmp{}", util::hex(8)));
    let text = util::py_dumps(&Value::Object(value.clone()), None);
    util::write_then(&tmp, text.as_bytes(), 0o600, || fs::rename(&tmp, path)).map_err(|e| Fault::os(&e, &tmp))
}

// -- Python's view of JSON values --------------------------------------------

fn unhashable(v: &Value, what: &str) -> Result<(), Fault> {
    match v {
        Value::Array(_) | Value::Object(_) => {
            let t = util::py_type_name(v);
            Err(Fault::new("TypeError", format!("cannot use '{t}' as a {what} (unhashable type: '{t}')")))
        }
        _ => Ok(()),
    }
}

/// `roots.get(key)`: only a string can be a configured target.
fn root_of<'a>(roots: &'a BTreeMap<String, String>, key: &Value) -> Result<Option<&'a String>, Fault> {
    unhashable(key, "dict key")?;
    Ok(key.as_str().and_then(|k| roots.get(k)))
}

/// `value[key]`.
fn index<'a>(value: &'a Value, key: &str) -> Result<&'a Value, Fault> {
    match value {
        Value::Object(m) => m.get(key).ok_or_else(|| Fault::new("KeyError", util::py_str(key))),
        Value::Array(_) => Err(Fault::new("TypeError", "list indices must be integers or slices, not str")),
        Value::String(_) => Err(Fault::new("TypeError", "string indices must be integers, not 'str'")),
        _ => Err(Fault::new("TypeError", format!("'{}' object is not subscriptable", util::py_type_name(value)))),
    }
}

fn field<'a>(record: &'a Map<String, Value>, key: &str) -> Result<&'a Value, Fault> {
    record.get(key).ok_or_else(|| Fault::new("KeyError", util::py_str(key)))
}

/// `value.get(key)` on a JSON object, with Python's error for anything else.
fn get<'a>(value: &'a Value, key: &str) -> Result<Option<&'a Value>, Fault> {
    match value {
        Value::Object(m) => Ok(m.get(key)),
        _ => Err(no_attribute(value, "get")),
    }
}

fn no_attribute(value: &Value, name: &str) -> Fault {
    Fault::new("AttributeError", format!("'{}' object has no attribute '{name}'", util::py_type_name(value)))
}

/// `Path(value)`.
fn path_arg(value: &Value) -> Result<&str, Fault> {
    value.as_str().ok_or_else(|| Fault::new("TypeError", format!(
        "argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{}'",
        util::py_type_name(value))))
}

/// `for item in value`.
fn py_iter(value: &Value) -> Result<Vec<Value>, Fault> {
    match value {
        Value::Array(a) => Ok(a.clone()),
        Value::Object(m) => Ok(m.keys().map(|k| Value::String(k.clone())).collect()),
        Value::String(s) => Ok(s.chars().map(|c| Value::String(c.into())).collect()),
        _ => Err(Fault::new("TypeError", format!("'{}' object is not iterable", util::py_type_name(value)))),
    }
}

/// `a, b = value`.
fn unpack_pair(value: &Value) -> Result<(Value, Value), Fault> {
    if !matches!(value, Value::Array(_) | Value::Object(_) | Value::String(_)) {
        return Err(Fault::new("TypeError", format!("cannot unpack non-iterable {} object", util::py_type_name(value))));
    }
    let items = py_iter(value)?;
    match items.len() {
        2 => Ok((items[0].clone(), items[1].clone())),
        n if n > 2 && value.is_string() => Err(Fault::value("too many values to unpack (expected 2)")),
        n if n > 2 => Err(Fault::value(format!("too many values to unpack (expected 2, got {n})"))),
        n => Err(Fault::value(format!("not enough values to unpack (expected 2, got {n})"))),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// `needle in (value or {})`.
fn contains(value: &Value, needle: &str) -> Result<bool, Fault> {
    match value {
        _ if !truthy(value) => Ok(false),
        Value::Object(m) => Ok(m.contains_key(needle)),
        Value::Array(a) => Ok(a.iter().any(|v| v.as_str() == Some(needle))),
        Value::String(s) => Ok(s.contains(needle)),
        _ => Err(Fault::new("TypeError", format!("argument of type '{}' is not iterable", util::py_type_name(value)))),
    }
}

/// `(value or {}).items()`.
fn items(value: &Value) -> Result<Vec<(String, Value)>, Fault> {
    match value {
        _ if !truthy(value) => Ok(Vec::new()),
        Value::Object(m) => Ok(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        _ => Err(no_attribute(value, "items")),
    }
}

/// Python's `a == b` for the hashable values a patch path can be.
fn py_eq(a: &Value, b: &Value) -> bool {
    let num = |v: &Value| match v {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64(),
        _ => None,
    };
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

// -- Descriptor-relative file access -----------------------------------------

/// An OS error as Python's str() of it, with no filename.
fn bare_os(e: &io::Error) -> Fault {
    let kind = Fault::os(e, Path::new("")).kind;
    Fault::new(kind, e.raw_os_error().map_or_else(|| e.to_string(), util::py_errno))
}

fn c_name(name: &str) -> Result<CString, Fault> {
    CString::new(name).map_err(|_| Fault::value("embedded null byte"))
}

fn open_dir_at(dir: RawFd, name: &str) -> Result<OwnedFd, Fault> {
    util::open_at(dir, &c_name(name)?, libc::O_RDONLY | libc::O_DIRECTORY, 0).map_err(|e| Fault::os(&e, Path::new(name)))
}

fn fstat(fd: RawFd) -> Result<libc::stat, Fault> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        return Err(bare_os(&io::Error::last_os_error()));
    }
    Ok(unsafe { st.assume_init() })
}

fn identity(fd: RawFd) -> Result<Value, Fault> {
    let st = fstat(fd)?;
    #[allow(clippy::useless_conversion)]
    Ok(json!([u64::from(st.st_dev), u64::from(st.st_ino)]))
}

/// The directory holding `relative` under `root`, and its last part.
fn parent_fd(root: RawFd, relative: &Value) -> Result<(OwnedFd, String), Fault> {
    let mut parts = parent_path_parts(relative)?;
    let name = parts.pop().expect("a relative path has a part");
    let mut fd = unsafe { BorrowedFd::borrow_raw(root) }.try_clone_to_owned().map_err(|e| bare_os(&e))?;
    for part in &parts {
        fd = open_dir_at(fd.as_raw_fd(), part)?;
    }
    Ok((fd, name))
}

type Snapshot = Option<(String, Vec<u8>, u32)>;

/// The digest, bytes and permission bits of a regular file, or nothing if
/// it does not exist.
fn file_snapshot(dir: RawFd, name: &str) -> Result<Snapshot, Fault> {
    let fd = match util::open_at(dir, &c_name(name)?, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
        Ok(fd) => fd,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
        Err(e) => return Err(Fault::os(&e, Path::new(name))),
    };
    let file = File::from(fd);
    let metadata = file.metadata().map_err(|e| bare_os(&e))?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(Fault::value("Only regular files without hardlinks may be patched"));
    }
    let mut data = Vec::new();
    (&file).take(MAX_BYTES as u64 + 1).read_to_end(&mut data).map_err(|e| bare_os(&e))?;
    if data.len() > MAX_BYTES {
        return Err(Fault::value("Existing file exceeds patch size limit"));
    }
    Ok(Some((util::sha256_hex(&data), data, metadata.mode() & 0o7777)))
}

fn sha_value(snapshot: &Snapshot) -> Value {
    snapshot.as_ref().map_or(Value::Null, |(sha, _, _)| Value::String(sha.clone()))
}

fn decode(data: &[u8]) -> Result<&str, Fault> {
    std::str::from_utf8(data).map_err(|e| Fault::new("UnicodeDecodeError", util::py_utf8_error(data, &e)))
}

/// Mandatory Landlock >=3: this process can write only beneath `root`.
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
fn confine_writes(root: RawFd) -> Result<(), Fault> {
    let errno = || io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u64>(), 0usize, 1u32) };
    if abi < 3 {
        return Err(Fault::new("RuntimeError", "Patch application requires Landlock ABI >=3"));
    }
    // Every access right through ABI 3 except execute, read file and read dir.
    let handled: u64 = ((1 << 15) - 1) & !(1 | 4 | 8);
    let rules = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &handled as *const u64, 8usize, 0u32) };
    if rules < 0 {
        return Err(Fault::new("OSError", format!("[Errno {}] landlock_create_ruleset", errno())));
    }
    let rules = unsafe { OwnedFd::from_raw_fd(rules as RawFd) };
    #[repr(C, packed)]
    struct Rule {
        allowed_access: u64,
        parent_fd: i32,
    }
    // Write file, remove file, make regular file, truncate.
    let rule = Rule { allowed_access: 2 | 32 | 256 | 16384, parent_fd: root };
    let failed = unsafe {
        libc::syscall(libc::SYS_landlock_add_rule, rules.as_raw_fd(), 1u32, &rule as *const Rule, 0u32) != 0
            || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
            || libc::syscall(libc::SYS_landlock_restrict_self, rules.as_raw_fd(), 0u32) != 0
    };
    if failed {
        return Err(Fault::new("OSError", format!("[Errno {}] Landlock enforcement failed", errno())));
    }
    Ok(())
}

#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
fn confine_writes(_root: RawFd) -> Result<(), Fault> {
    Err(Fault::new("RuntimeError", "Patch application requires supported Linux Landlock"))
}

/// Writes an approved record's changes, each atomically, after checking
/// every precondition. Runs in the apply-helper process, which it confines.
fn apply_files(record: &Value) -> Result<Value, Fault> {
    let root = index(record, "root")?;
    let root = c_name(root.as_str().ok_or_else(|| Fault::new("TypeError", format!(
        "open: path should be string, bytes or os.PathLike, not {}", util::py_type_name(root))))?)?;
    let root_fd = util::open_at(libc::AT_FDCWD, &root, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .map_err(|e| Fault::os(&e, Path::new(&*root.to_string_lossy())))?;
    if &identity(root_fd.as_raw_fd())? != index(record, "root_identity")? {
        return Err(Fault::value("Approved root identity changed"));
    }
    confine_writes(root_fd.as_raw_fd())?;
    let mut touched = Vec::new();
    let outcome = (|| -> Result<(), Fault> {
        let mut parents = Vec::new();
        // Check every precondition before the first mutation.
        for change in py_iter(index(record, "changes")?)? {
            let path = index(&change, "path")?;
            let (fd, name) = parent_fd(root_fd.as_raw_fd(), path)?;
            let before = file_snapshot(fd.as_raw_fd(), &name)?;
            if &sha_value(&before) != index(&change, "before_sha256")? {
                return Err(Fault::value(format!("Before hash conflict: {}", path_arg(path)?)));
            }
            parents.push((fd, name, change));
        }
        for (fd, name, change) in &parents {
            let fd = fd.as_raw_fd();
            let path = path_arg(index(change, "path")?)?;
            let before = file_snapshot(fd, name)?;
            if &sha_value(&before) != index(change, "before_sha256")? {
                return Err(Fault::value(format!("Concurrent edit: {path}")));
            }
            let mode = before.as_ref().map_or(0o644, |(_, _, mode)| *mode);
            let tmp = format!(".palette-patch-{}", util::uuid4().replace('-', ""));
            let tmp_c = c_name(&tmp)?;
            let file = File::from(util::open_at(fd, &tmp_c, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)
                .map_err(|e| Fault::os(&e, Path::new(&tmp)))?);
            let written = (|| -> Result<(), Fault> {
                let content = index(change, "content")?;
                let content = content.as_str().ok_or_else(|| no_attribute(content, "encode"))?;
                (&file).write_all(content.as_bytes()).map_err(|e| bare_os(&e))?;
                if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
                    return Err(bare_os(&io::Error::last_os_error()));
                }
                file.sync_all().map_err(|e| bare_os(&e))?;
                drop(file);
                // Keep atomic replacement within this descriptor-relative directory.
                let check = file_snapshot(fd, name)?;
                if sha_value(&check) != sha_value(&before) {
                    return Err(Fault::value(format!("Concurrent edit before commit: {path}")));
                }
                let name_c = c_name(name)?;
                if unsafe { libc::renameat(fd, tmp_c.as_ptr(), fd, name_c.as_ptr()) } != 0 {
                    let e = io::Error::last_os_error();
                    let f = bare_os(&e);
                    return Err(Fault::new(f.kind, format!("{}: {} -> {}", f.message, util::py_str(&tmp), util::py_str(name))));
                }
                touched.push(json!({"path": path, "before_sha256": sha_value(&before),
                                    "after_sha256": util::sha256_hex(content.as_bytes())}));
                if unsafe { libc::fsync(fd) } != 0 {
                    return Err(bare_os(&io::Error::last_os_error()));
                }
                Ok(())
            })();
            if unsafe { libc::unlinkat(fd, tmp_c.as_ptr(), 0) } != 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::ENOENT) {
                    return Err(Fault::os(&e, Path::new(&tmp)));
                }
            }
            written?;
        }
        Ok(())
    })();
    Ok(match outcome {
        Ok(()) => json!({"status": "applied", "files_touched": touched,
                         "atomicity": "per-file; target must be quiescent for a multi-file patch"}),
        Err(e) => json!({"status": if touched.is_empty() { "failed" } else { "partial" },
                         "files_touched": touched, "error": e.message}),
    })
}

// -- The broker -----------------------------------------------------------------

/// What a broker is built with besides its storage and roots.
#[derive(Default)]
pub(crate) struct Options {
    /// `{target: [[class, prefix], ...]}`: paths a tested repair may apply.
    pub auto_apply_prefixes: Value,
    pub protected_paths: Vec<PathBuf>,
    pub registry_root: Option<String>,
    /// `{target: [source.jl, ...]}`: sandboxed capability sources.
    pub sandbox_source_paths: Value,
    pub approval: Option<String>,
}

/// The MCP client side of an approval: requests go out, replies come back.
pub(crate) trait Client {
    fn send(&mut self, message: &Value) -> io::Result<()>;
    /// The next line from the client, or None at end of input or once
    /// `timeout` passes.
    fn recv(&mut self, timeout: Duration) -> Option<String>;
}

pub(crate) struct PatchBroker {
    pub approval: &'static str,
    pub storage: PathBuf,
    pub roots: BTreeMap<String, String>,
    pub protected_paths: Vec<PathBuf>,
    pub registry_root: Option<PathBuf>,
    pub sandbox_source_paths: BTreeMap<String, BTreeSet<String>>,
    pub auto_apply_prefixes: BTreeMap<String, Vec<(String, String)>>,
    /// The deployed `runtime/` whose trust chain no patch may touch.
    pub runtime_dir: Option<PathBuf>,
    /// The palette-host the approval commands and the apply helper run.
    pub host: PathBuf,
    registry_root_has_symlink_ancestor: bool,
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Owned by this user and not group or world writable.
fn operator_owned(m: &fs::Metadata) -> bool {
    m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0
}

/// A capability manifest as JSON, or None if it cannot be read or parsed.
fn read_manifest(path: &Path) -> Result<Option<Value>, Fault> {
    let Ok(bytes) = fs::read(path) else { return Ok(None) };
    Ok(serde_json::from_str(decode(&bytes)?).ok())
}

impl PatchBroker {
    pub fn new(storage: &Path, roots: &Map<String, Value>, options: Options) -> Result<PatchBroker, Fault> {
        if contains(&options.auto_apply_prefixes, TRUSTED_TOOLS_TARGET)?
            || contains(&options.sandbox_source_paths, TRUSTED_TOOLS_TARGET)? {
            return Err(Fault::value("Trusted tools are approved by a human only; they take no auto-apply or sandbox source paths"));
        }
        let approval = approval_mode(options.approval.as_deref())?;
        let storage = util::resolve(storage);
        let mut resolved = BTreeMap::new();
        for (key, path) in roots {
            let path = util::resolve_strict(Path::new(path_arg(path)?)).map_err(|(e, p)| Fault::os(&e, &p))?;
            resolved.insert(key.clone(), path.to_string_lossy().into_owned());
        }
        let writable: Vec<&String> = resolved.values().collect();
        provisioning::require_disjoint(&writable, &[&storage], &[] as &[&Path]).map_err(Fault::permission)?;
        let mut broker = PatchBroker {
            approval,
            storage,
            roots: resolved,
            protected_paths: options.protected_paths.iter().map(|p| util::resolve(p)).collect(),
            registry_root: None,
            sandbox_source_paths: BTreeMap::new(),
            auto_apply_prefixes: BTreeMap::new(),
            runtime_dir: util::installed_checkout().map(|c| util::resolve(&c.join("runtime"))),
            host: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("palette-host")),
            registry_root_has_symlink_ancestor: false,
        };
        if let Some(registry_root) = &options.registry_root {
            let configured = util::expand_user(registry_root);
            if is_symlink(&configured) {
                return Err(Fault::value("Operator runtime registry root must not be a symlink"));
            }
            let resolved = util::resolve(&configured);
            if resolved.exists() && !resolved.is_dir() {
                return Err(Fault::value("Operator runtime registry root must be a directory"));
            }
            broker.registry_root_has_symlink_ancestor = util::absolute(&configured) != resolved;
            broker.registry_root = Some(resolved);
        }
        if truthy(&options.sandbox_source_paths) && broker.registry_root.is_none() {
            return Err(Fault::value("Sandbox source paths require the operator runtime registry root"));
        }
        for (target, entries) in items(&options.auto_apply_prefixes)? {
            if !broker.roots.contains_key(&target) {
                return Err(Fault::value("Auto-apply classes require a configured patch target"));
            }
            let mut normalized = Vec::new();
            for entry in py_iter(&entries)? {
                let (class_name, prefix) = unpack_pair(&entry)?;
                unhashable(&class_name, "set element")?;
                let Some(class_name) = class_name.as_str().filter(|c| AUTO_APPLY_CLASSES.contains(c)) else {
                    return Err(Fault::value("Unknown auto-apply path class"));
                };
                parent_path_parts(&prefix)?;
                normalized.push((class_name.to_string(), prefix.as_str().expect("checked above").to_string()));
            }
            broker.auto_apply_prefixes.insert(target, normalized);
        }
        for (target, entries) in items(&options.sandbox_source_paths)? {
            let filenames = broker.sandbox_sources(roots, &target, &entries)?;
            broker.sandbox_source_paths.insert(target, filenames);
        }
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&broker.storage)
            .map_err(|e| Fault::os(&e, &broker.storage))?;
        Ok(broker)
    }

    /// Validates one target's configured sandbox sources.
    fn sandbox_sources(&self, roots: &Map<String, Value>, target: &str, entries: &Value) -> Result<BTreeSet<String>, Fault> {
        let Some(root) = self.roots.get(target) else {
            return Err(Fault::value("Sandbox source paths require a configured patch target"));
        };
        let root = PathBuf::from(root);
        let raw_root = util::expand_user(path_arg(&roots[target])?);
        if is_symlink(&raw_root) || util::absolute(&raw_root) != root {
            return Err(Fault::value("Sandbox source target must not be a symlink"));
        }
        let registry = self.registry_root.as_ref().filter(|r| r.is_dir() && root.parent() == Some(r.as_path()));
        let Some(registry) = registry else {
            return Err(Fault::value("Sandbox source target must be one immediate capability directory under the runtime registry"));
        };
        if is_symlink(&root) || !root.is_dir() {
            return Err(Fault::value("Sandbox source target must be a real capability directory"));
        }
        if self.registry_root_has_symlink_ancestor {
            return Err(Fault::value("Sandbox source registry path cannot contain symlinked ancestors"));
        }
        if credential_location(&root) {
            return Err(Fault::value("Sandbox source target cannot be inside a credential or secret path"));
        }
        let root_info = fs::metadata(&root).map_err(|e| Fault::os(&e, &root))?;
        let registry_info = fs::metadata(registry).map_err(|e| Fault::os(&e, registry))?;
        if !operator_owned(&root_info) || !operator_owned(&registry_info) {
            return Err(Fault::value("Sandbox source registry must be operator-owned and not group/world writable"));
        }
        let Some(entries) = entries.as_array().filter(|e| !e.is_empty()) else {
            return Err(Fault::value("Sandbox source paths must list exact Julia source filenames"));
        };
        let mut filenames = BTreeSet::new();
        for relative in entries {
            let parts = parent_path_parts(relative)?;
            let relative = relative.as_str().expect("checked above");
            if parts.len() != 1 || !parts[0].ends_with(".jl") {
                return Err(Fault::value("Sandbox source paths must be immediate .jl files under a capability directory"));
            }
            let Ok(metadata) = fs::symlink_metadata(root.join(&parts[0])) else {
                return Err(Fault::value(format!("Configured sandbox source is unavailable: {relative}")));
            };
            if !metadata.file_type().is_file() || metadata.nlink() != 1 || !operator_owned(&metadata) {
                return Err(Fault::value("Configured sandbox source must be a regular file without hardlinks"));
            }
            let manifest_path = root.join("capability.json");
            let manifest_info = fs::symlink_metadata(&manifest_path);
            let (Ok(manifest_info), Some(manifest)) = (manifest_info, read_manifest(&manifest_path)?) else {
                return Err(Fault::value("Sandbox source target requires a readable capability manifest"));
            };
            if !manifest.is_object() || !manifest_info.file_type().is_file() || manifest_info.nlink() != 1
                || !operator_owned(&manifest_info) || manifest.get("source").and_then(Value::as_str) != Some(&parts[0]) {
                return Err(Fault::value("Configured sandbox source must match its immutable capability manifest"));
            }
            if !matches!(self.configured_class(target, relative).as_deref(), Some("adapters" | "tools")) {
                return Err(Fault::value("Sandbox source requires an explicit adapters or tools path class"));
            }
            filenames.insert(relative.to_string());
        }
        Ok(filenames)
    }

    fn configured_class(&self, target: &str, relative: &str) -> Option<String> {
        let parts = split_parts(relative).ok()?;
        self.auto_apply_prefixes.get(target)?.iter()
            .find(|(_, prefix)| split_parts(prefix).is_ok_and(|p| parts.starts_with(&p)))
            .map(|(class_name, _)| class_name.clone())
    }

    fn is_sandbox_source(&self, target: &str, relative: &str) -> Result<bool, Fault> {
        if !self.sandbox_source_paths.get(target).is_some_and(|s| s.contains(relative)) {
            return Ok(false);
        }
        if !matches!(self.configured_class(target, relative).as_deref(), Some("adapters" | "tools")) {
            return Ok(false);
        }
        let (Some(root), Some(registry)) = (self.roots.get(target), &self.registry_root) else { return Ok(false) };
        let root = Path::new(root);
        let manifest_path = root.join("capability.json");
        let infos = [root, registry.as_path(), &root.join(relative), &manifest_path].map(fs::symlink_metadata);
        let [Ok(root_info), Ok(registry_info), Ok(source_info), Ok(manifest_info)] = infos else { return Ok(false) };
        let Some(manifest) = read_manifest(&manifest_path)? else { return Ok(false) };
        let single_file = |m: &fs::Metadata| m.file_type().is_file() && m.nlink() == 1 && operator_owned(m);
        Ok(root_info.file_type().is_dir() && operator_owned(&root_info)
            && registry_info.file_type().is_dir() && operator_owned(&registry_info)
            && single_file(&source_info) && single_file(&manifest_info)
            && manifest.get("source").and_then(Value::as_str) == Some(relative))
    }

    pub fn classify(&self, target: &Value, relative: &Value) -> Result<Class, Fault> {
        let Some(root) = root_of(&self.roots, target)? else { return Err(self.unknown_target(target)) };
        let target = target.as_str().expect("a configured target is a string");
        if target == TRUSTED_TOOLS_TARGET {
            if !relative.as_str().is_some_and(trusted_tool_file) {
                return Err(Fault::permission("The trusted tool directory holds only <name>.jl and <name>.json files"));
            }
            return Ok(class("protected", false, "trusted tool: a human approves it"));
        }
        let candidate = util::absolute(&Path::new(root).join(parent_path_parts(relative)?.join("/")));
        if credential_location(&candidate) {
            return Ok(class("forbidden", false, "credential or secret path"));
        }
        let relative = relative.as_str().expect("checked above");
        let mut result = classify_patch_path(relative, self.auto_apply_prefixes.get(target).map_or(&[], Vec::as_slice))?;
        if result.class == "forbidden" {
            return Ok(result);
        }
        let sandbox_source = self.is_sandbox_source(target, relative)?;
        if sandbox_source && result.class == "protected" {
            if !matches!(result.reason, "authority path" | "authority directory") {
                return Ok(result);
            }
            result = Class { class: self.configured_class(target, relative).unwrap_or_default(), self_apply: true,
                             reason: "operator-configured sandboxed capability source" };
        }
        // The deployed runtime trust chain is protected by canonical location,
        // including when an operator configures a narrower subtree as a target.
        let Some(runtime_dir) = &self.runtime_dir else {
            return Err(Fault::new("RuntimeError", "The deployed Palette runtime directory was not found; patches cannot be classified"));
        };
        let protected = ["security", "host", "plugins", "scripts"].map(|d| runtime_dir.join(d));
        let deployed_registry = runtime_dir.join("capabilities");
        // The canonical deployed registry stays immutable unless this exact
        // registry was explicitly selected and the candidate is an exact,
        // validated sandbox source. A configured registry elsewhere must not
        // make the repository's registry patchable.
        let explicit_registry_source = sandbox_source
            && self.registry_root.as_ref().is_some_and(|r| *r == util::resolve(&deployed_registry));
        if candidate.starts_with(&deployed_registry) && !explicit_registry_source {
            return Ok(class("protected", false, "deployed runtime trust chain"));
        }
        if protected.iter().any(|p| candidate.starts_with(p)) {
            return Ok(class("protected", false, "deployed runtime trust chain"));
        }
        if self.protected_paths.iter().any(|p| candidate.starts_with(p)) {
            return Ok(class("protected", false, "configured protected host path"));
        }
        if self.registry_root.as_ref().is_some_and(|r| candidate.starts_with(r)) && !sandbox_source {
            return Ok(class("protected", false, "operator runtime registry"));
        }
        Ok(result)
    }

    /// The broker-wide lock, held until the returned file drops.
    fn locked(&self) -> Result<File, Fault> {
        let path = self.storage.join("broker.lock");
        let lock = OpenOptions::new().append(true).create(true).read(true).open(&path).map_err(|e| Fault::os(&e, &path))?;
        flock(&lock)?;
        Ok(lock)
    }

    fn path(&self, identifier: &Value) -> Result<PathBuf, Fault> {
        match identifier.as_str() {
            Some(id) if id.len() == 32 && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) => {
                Ok(self.storage.join(format!("{id}.json")))
            }
            _ => Err(Fault::value("Invalid patch request_id")),
        }
    }

    fn receipt(&self, record: &Map<String, Value>) -> Result<(), Fault> {
        let path = self.storage.join("receipts.jsonl");
        let mut stream = OpenOptions::new().append(true).create(true).open(&path).map_err(|e| Fault::os(&e, &path))?;
        let mut shown = record.clone();
        shown.remove("changes");
        shown.remove("preview");
        let line = util::py_dumps(&Value::Object(shown), None) + "\n";
        stream.write_all(line.as_bytes()).and_then(|()| stream.sync_all()).map_err(|e| bare_os(&e))
    }

    fn unknown_target(&self, target: &Value) -> Fault {
        let configured = if self.roots.is_empty() { "none".to_string() } else { self.roots.keys().cloned().collect::<Vec<_>>().join(", ") };
        Fault::permission(format!("{} is not a patch target; the host configures them in PALETTE_PATCH_ROOTS (configured: {configured})",
                                  util::py_repr(target)))
    }

    /// What the local administrator runs; the agent's own calls cannot approve.
    fn approval_steps(&self, record: &Map<String, Value>) -> Map<String, Value> {
        let text = |key: &str| record.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let (id, sha) = (text("request_id"), text("patch_sha256"));
        let command = [self.host.to_string_lossy().into_owned(), "patch".into(), "--storage".into(),
                       self.storage.to_string_lossy().into_owned()];
        let review = [&command[..], &["review".into(), id.clone()]].concat();
        let approve = [&command[..], &["approve".into(), id.clone(), "--digest".into(), sha.clone()]].concat();
        let steps = json!({"status": "approval_required", "request_id": id, "patch_sha256": sha,
                           "review_command": util::shjoin(&review), "approve_command": util::shjoin(&approve),
                           "after_approval": "Call apply again with this request_id within 15 minutes of prepare; it applies the approved patch once."});
        let Value::Object(steps) = steps else { unreachable!() };
        steps
    }

    pub fn prepare(&self, args: &Map<String, Value>, workspace: &str) -> Result<Map<String, Value>, Fault> {
        let target = args.get("target").unwrap_or(&Value::Null);
        let Some(root) = root_of(&self.roots, target)? else { return Err(self.unknown_target(target)) };
        let changes = match args.get("changes") {
            Some(Value::Array(c)) if (1..=16).contains(&c.len()) => c,
            _ => return Err(Fault::value("Expected 1..16 exact file changes")),
        };
        if util::py_dumps(&Value::Array(changes.clone()), None).len() > MAX_BYTES {
            return Err(Fault::value("Patch exceeds 64 KiB"));
        }
        let root_c = c_name(root)?;
        let root_fd = util::open_at(libc::AT_FDCWD, &root_c, libc::O_RDONLY | libc::O_DIRECTORY, 0)
            .map_err(|e| Fault::os(&e, Path::new(root)))?;
        let mut preview = String::new();
        let mut seen: Vec<&Value> = Vec::new();
        let root_identity = identity(root_fd.as_raw_fd())?;
        for change in changes {
            let valid = change.as_object().filter(|c| {
                c.len() == 3 && ["path", "before_sha256", "content"].iter().all(|k| c.contains_key(*k)) && c["content"].is_string()
            });
            let Some(change) = valid else {
                return Err(Fault::value("Each change needs path, before_sha256 and UTF-8 content only"));
            };
            let path = &change["path"];
            unhashable(path, "set element")?;
            if seen.iter().any(|s| py_eq(s, path)) {
                return Err(Fault::value("Duplicate patch path"));
            }
            seen.push(path);
            if self.classify(target, path)?.class == "forbidden" {
                return Err(Fault::permission("Credential and secret files cannot be patched"));
            }
            let (fd, name) = parent_fd(root_fd.as_raw_fd(), path)?;
            let before = file_snapshot(fd.as_raw_fd(), &name)?;
            let path = path.as_str().expect("classified paths are strings");
            if sha_value(&before) != change["before_sha256"] {
                return Err(Fault::value(format!("Before hash conflict: {path}")));
            }
            let old = match &before {
                Some((_, data, _)) => decode(data)?,
                None => "",
            };
            let new = change["content"].as_str().expect("checked above");
            preview += &difflib::unified_diff(&difflib::split_lines_keep(old), &difflib::split_lines_keep(new), path, path);
        }
        if preview.len() > MAX_BYTES {
            return Err(Fault::value("Review diff exceeds 64 KiB; split this patch"));
        }
        let now = util::now_secs();
        let record = json!({"request_id": util::uuid4().replace('-', ""), "workspace_id": workspace, "target": target,
                            "root": root, "root_identity": root_identity, "changes": changes, "preview": preview,
                            "created_at": now, "expiry": now + 900.0, "allowed_operations": ["create", "modify"],
                            "status": "pending"});
        let Value::Object(mut record) = record else { unreachable!() };
        let sha = request_digest(&record)?;
        record.insert("patch_sha256".into(), sha.into());
        let _lock = self.locked()?;
        write_json(&self.path(&record["request_id"])?, &record)?;
        self.receipt(&record)?;
        Ok(record)
    }

    pub fn inspect(&self, identifier: &Value, workspace: Option<&str>) -> Result<Map<String, Value>, Fault> {
        let path = self.path(identifier)?;
        let bytes = fs::read(&path).map_err(|e| Fault::os(&e, &path))?;
        let record = py_json_loads(decode(&bytes)?)?;
        let Value::Object(record) = record else { return Err(no_attribute(&record, "get")) };
        if record.get("patch_sha256").and_then(Value::as_str) != Some(&request_digest(&record)?) {
            return Err(Fault::permission("Stored patch differs from its bound digest"));
        }
        if let Some(workspace) = workspace {
            if field(&record, "workspace_id")?.as_str() != Some(workspace) {
                return Err(Fault::permission("Patch belongs to a different workspace"));
            }
        }
        Ok(record)
    }

    /// Reclassify every bound path at each authority transition. Protected
    /// and unknown files can still be changed through an explicit exact-digest
    /// approval; credential and secret paths never can, even when a request
    /// was created before policy changed.
    fn reject_forbidden_changes(&self, record: &Map<String, Value>) -> Result<(), Fault> {
        let target = field(record, "target")?;
        for change in py_iter(field(record, "changes")?)? {
            if self.classify(target, index(&change, "path")?)?.class == "forbidden" {
                return Err(Fault::permission("Credential and secret files cannot be patched"));
            }
        }
        Ok(())
    }

    pub fn approve(&self, identifier: &Value, expected_digest: &str, authority: &str) -> Result<(), Fault> {
        let _lock = self.locked()?;
        let mut record = self.inspect(identifier, None)?;
        if field(&record, "status")? != "pending" || expired(&record)?
            || record.get("patch_sha256").and_then(Value::as_str) != Some(expected_digest) {
            return Err(Fault::permission("Patch is not pending, expired, or digest differs"));
        }
        record.insert("status".into(), "approved".into());
        record.insert("granted_by".into(), authority.into());
        record.insert("granted_at".into(), util::now_secs().into());
        record.insert("capability_id".into(), util::uuid4().replace('-', "").into());
        write_json(&self.path(identifier)?, &record)?;
        self.receipt(&record)
    }

    /// The per-target lock, shared across registries and held outside the
    /// approved target, until the returned file drops.
    fn target_locked(&self, root: &str) -> Result<File, Fault> {
        let root = Path::new(root);
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let path = root.parent().unwrap_or(Path::new("")).join(format!(".{name}.palette-patch.lock"));
        let c = c_name(&path.to_string_lossy())?;
        let fd = util::open_at(libc::AT_FDCWD, &c, libc::O_RDWR | libc::O_CREAT, 0o600).map_err(|e| Fault::os(&e, &path))?;
        let lock = File::from(fd);
        flock(&lock)?;
        Ok(lock)
    }

    pub fn apply(&self, identifier: &Value, workspace: &str) -> Result<Map<String, Value>, Fault> {
        let root = path_arg(field(&self.inspect(identifier, Some(workspace))?, "root")?)?.to_string();
        let _lock = self.locked()?;
        let _target_lock = self.target_locked(&root)?;
        let mut record = self.inspect(identifier, Some(workspace))?;
        if field(&record, "status")? != "approved" || expired(&record)? {
            return Err(Fault::permission("No valid unused approval"));
        }
        let configured = root_of(&self.roots, field(&record, "target")?)?;
        if configured.map(|r| Value::String(r.clone())).as_ref() != Some(field(&record, "root")?) {
            return Err(Fault::permission("Target configuration changed"));
        }
        self.reject_forbidden_changes(&record)?;
        let granted_by = record.get("granted_by").cloned().unwrap_or_else(|| "".into());
        let granted_by = granted_by.as_str().ok_or_else(|| no_attribute(&granted_by, "startswith"))?;
        if granted_by.starts_with("isolated_tested_repair:") {
            // Auto-approval is only valid while every path still has its
            // frozen neutral classification. Protected changes use the
            // ordinary explicit administrator flow.
            let target = field(&record, "target")?;
            for change in py_iter(field(&record, "changes")?)? {
                if !self.classify(target, index(&change, "path")?)?.self_apply {
                    return Err(Fault::permission("Repair auto-apply classification changed before apply"));
                }
            }
        }
        // Consume before effects. Process failure cannot make this grant reusable.
        record.insert("status".into(), "consumed_completion_unknown".into());
        write_json(&self.path(identifier)?, &record)?;
        self.receipt(&record)?;
        let result = match self.run_helper(&record) {
            Ok(Value::Object(result)) => result,
            Ok(other) => [("status".to_string(), "consumed_completion_unknown".into()),
                          ("error".to_string(), format!("apply helper returned {}", util::py_type_name(&other)).into())].into_iter().collect(),
            Err(error) => [("status".to_string(), "consumed_completion_unknown".into()),
                           ("error".to_string(), error.into())].into_iter().collect(),
        };
        record.extend(result);
        write_json(&self.path(identifier)?, &record)?;
        self.receipt(&record)?;
        record.remove("changes");
        record.remove("preview");
        Ok(record)
    }

    /// The apply helper's result, or the error text that stands in for one.
    fn run_helper(&self, record: &Map<String, Value>) -> Result<Value, String> {
        let argv = vec![self.host.to_string_lossy().into_owned(), "patch".into(), "apply-helper".into()];
        let input = util::py_dumps(&Value::Object(record.clone()), None);
        let (code, stdout, stderr, timed_out) = run_captured(&argv, Some(input.as_bytes()), Some(HELPER_TIMEOUT))
            .map_err(|e| util::py_os_error(&e, &self.host))?;
        if timed_out {
            let shown = Value::Array(argv.into_iter().map(Value::String).collect());
            return Err(format!("Command '{}' timed out after {} seconds", util::py_repr(&shown), HELPER_TIMEOUT.as_secs()));
        }
        if code != 0 {
            return Err(util::tail(&stderr, 2000));
        }
        py_json_loads(&stdout).map_err(|f| f.message)
    }

    /// One patch tool call: read, prepare, status or apply (which asks the
    /// client to confirm a pending request when it can).
    pub fn call(&self, args: &Map<String, Value>, workspace: &str, capabilities: &Map<String, Value>,
                client: &mut dyn Client) -> Result<Value, Fault> {
        let action = field(args, "action")?;
        if action == "read" {
            let target = args.get("target").unwrap_or(&Value::Null);
            let Some(root) = root_of(&self.roots, target)? else { return Err(self.unknown_target(target)) };
            let path = args.get("path").unwrap_or(&Value::Null);
            if self.classify(target, path)?.class == "forbidden" {
                return Err(Fault::permission("Credential and secret files cannot be read"));
            }
            let root_fd = util::open_at(libc::AT_FDCWD, &c_name(root)?, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                .map_err(|e| Fault::os(&e, Path::new(root)))?;
            let (fd, name) = parent_fd(root_fd.as_raw_fd(), path)?;
            let snapshot = file_snapshot(fd.as_raw_fd(), &name)?;
            let content = match &snapshot {
                Some((_, data, _)) => Value::String(decode(data)?.to_string()),
                None => Value::Null,
            };
            return Ok(json!({"target": target, "path": path, "sha256": sha_value(&snapshot), "content": content}));
        }
        if action == "prepare" {
            return self.prepare(args, workspace).map(Value::Object);
        }
        let record = self.inspect(args.get("request_id").unwrap_or(&Value::Null), Some(workspace))?;
        if action == "status" {
            return Ok(Value::Object(record));
        }
        let request_id = field(&record, "request_id")?.clone();
        if field(&record, "status")? != "pending" {
            return self.apply(&request_id, workspace).map(Value::Object);
        }
        self.reject_forbidden_changes(&record)?;
        let declined = |reason: &str| {
            let mut steps = self.approval_steps(&record);
            steps.insert("reason".into(), reason.into());
            Ok(Value::Object(steps))
        };
        if self.approval == "administrator" {
            return declined("This operator accepts patch approval only from the local administrator command; MCP client confirmation is turned off.");
        }
        match capabilities.get("elicitation") {
            Some(Value::Object(e)) if e.is_empty() || e.contains_key("form") => {}
            _ => return declined("Client does not advertise form elicitation. Local administrator must review and approve the exact pending request; Julia and MCP tool arguments cannot grant approval."),
        }
        let text = |key: &str| record.get(key).map(|v| v.as_str().map_or_else(|| util::py_repr(v), String::from)).unwrap_or_default();
        let rid = format!("approve-{}", util::uuid4().replace('-', ""));
        let message = format!("Apply exactly this one-shot patch to {} for workspace {workspace}? Digest {}. Expires in 15 minutes.\n{}",
                              text("root"), text("patch_sha256"), text("preview"));
        let request = json!({"jsonrpc": "2.0", "id": rid, "method": "elicitation/create", "params": {
            "mode": "form", "message": message, "requestedSchema": {"type": "object", "properties": {
                "confirm": {"type": "boolean", "title": "Approve this exact patch once"}}, "required": ["confirm"]}}});
        client.send(&request).map_err(|e| bare_os(&e))?;
        // Bound the client response. Tool requests during confirmation are refused.
        let deadline = Instant::now() + APPROVAL_WAIT;
        while let Some(raw) = client.recv(deadline.saturating_duration_since(Instant::now())) {
            let reply = py_json_loads(&raw)?;
            let Value::Object(fields) = &reply else { return Err(no_attribute(&reply, "get")) };
            if fields.get("id").and_then(Value::as_str) == Some(rid.as_str()) && !fields.contains_key("method") {
                let empty = Value::Object(Map::new());
                let result = fields.get("result").unwrap_or(&empty);
                let accepted = get(result, "action")? == Some(&Value::from("accept"))
                    && get(get(result, "content")?.unwrap_or(&empty), "confirm")? == Some(&Value::Bool(true));
                if accepted {
                    let sha = field(&record, "patch_sha256")?.as_str().unwrap_or_default();
                    self.approve(&request_id, sha, "mcp_client_confirmed_user")?;
                    return self.apply(&request_id, workspace).map(Value::Object);
                }
                let _lock = self.locked()?;
                let mut outcome = record.clone();
                outcome.insert("approval_outcome".into(), "not_approved".into());
                self.receipt(&outcome)?;
                return Ok(json!({"status": "not_approved", "request_id": request_id}));
            }
            if let (Some(id), true) = (fields.get("id"), fields.contains_key("method")) {
                client.send(&json!({"jsonrpc": "2.0", "id": id, "error": {
                    "code": -32000, "message": "Approval is pending; retry this request afterward"}})).map_err(|e| bare_os(&e))?;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        Ok(json!({"status": "approval_timeout", "request_id": request_id}))
    }
}

fn flock(lock: &File) -> Result<(), Fault> {
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(bare_os(&io::Error::last_os_error()));
    }
    Ok(())
}

/// `time.time() >= record["expiry"]`.
fn expired(record: &Map<String, Value>) -> Result<bool, Fault> {
    let expiry = field(record, "expiry")?;
    let at = match expiry {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    };
    at.map(|at| util::now_secs() >= at).ok_or_else(|| Fault::new("TypeError", format!(
        "'>=' not supported between instances of 'float' and '{}'", util::py_type_name(expiry))))
}

/// `palette-host patch ...`: the administrator's review and approval, and
/// the confined apply helper the broker runs.
pub(crate) fn main(rest: &[String]) -> Result<i32, String> {
    if rest == ["apply-helper"] {
        let mut input = Vec::new();
        let result = io::stdin().read_to_end(&mut input).map_err(|e| bare_os(&e))
            .and_then(|_| decode(&input).and_then(py_json_loads))
            .and_then(|record| apply_files(&record))
            .unwrap_or_else(|e| json!({"status": "failed", "files_touched": [], "error": e.message}));
        println!("{}", util::py_dumps(&result, None));
        return Ok(0);
    }
    let (mut storage, mut digest, mut positional) = (None, None, Vec::new());
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        let mut option = |name: &str, slot: &mut Option<String>| -> Result<bool, String> {
            if arg == name {
                *slot = Some(args.next().ok_or(format!("argument {name}: expected one argument"))?.clone());
            } else if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
                *slot = Some(value.to_string());
            } else {
                return Ok(false);
            }
            Ok(true)
        };
        if !option("--storage", &mut storage)? && !option("--digest", &mut digest)? {
            positional.push(arg.clone());
        }
    }
    let usage = "usage: palette-host patch --storage S review|approve ID [--digest D] | apply-helper";
    let (Some(storage), [action, request_id]) = (storage, positional.as_slice()) else { return Err(usage.into()) };
    if !matches!(action.as_str(), "review" | "approve") {
        return Err(format!("argument action: invalid choice: {} (choose from review, approve)", util::py_str(action)));
    }
    let broker = PatchBroker::new(Path::new(&storage), &Map::new(), Options::default()).map_err(|e| e.to_string())?;
    let request_id = Value::String(request_id.clone());
    if action == "approve" {
        let digest = digest.filter(|d| !d.is_empty()).ok_or("Approval requires the reviewed --digest")?;
        broker.approve(&request_id, &digest, "local_administrator").map_err(|e| e.to_string())?;
    }
    let record = broker.inspect(&request_id, None).map_err(|e| e.to_string())?;
    println!("{}", util::py_dumps(&Value::Object(record), Some(2)));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::Command;

    /// The palette-host binary `cargo test` builds beside the test binary.
    fn host() -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let host = exe.parent().and_then(Path::parent).unwrap().join("palette-host");
        assert!(host.is_file(), "build palette-host first ({})", host.display());
        host
    }

    fn sha(text: &str) -> String {
        util::sha256_hex(text.as_bytes())
    }

    fn roots(pairs: &[(&str, &Path)]) -> Map<String, Value> {
        pairs.iter().map(|(k, p)| (k.to_string(), Value::String(p.to_string_lossy().into_owned()))).collect()
    }

    fn broker(storage: &Path, roots: &Map<String, Value>, options: Options) -> PatchBroker {
        let mut broker = PatchBroker::new(storage, roots, options).unwrap();
        broker.host = host();
        broker
    }

    struct Fixture {
        tmp: PathBuf,
        root: PathBuf,
        store: PathBuf,
        broker: PatchBroker,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = util::resolve(&util::mkdtemp("palette-patch-").unwrap());
            let root = tmp.join("project");
            fs::create_dir(&root).unwrap();
            fs::write(root.join("a.txt"), "before\n").unwrap();
            let store = tmp.join("grants");
            let broker = broker(&store, &roots(&[("project", &root)]), Options::default());
            Fixture { tmp, root, store, broker }
        }

        fn prepare(&self, path: &str, before: Option<&str>, content: &str) -> Result<Map<String, Value>, Fault> {
            let args = json!({"target": "project", "changes": [{"path": path, "before_sha256": before.map(sha), "content": content}]});
            self.broker.prepare(args.as_object().unwrap(), "world-a")
        }

        /// What the local administrator runs.
        fn approve(&self, record: &Map<String, Value>) {
            let out = Command::new(host()).args(["patch", "--storage"]).arg(&self.store)
                .args(["approve", record["request_id"].as_str().unwrap(), "--digest", record["patch_sha256"].as_str().unwrap()])
                .output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }

        fn apply(&self, record: &Map<String, Value>, workspace: &str) -> Result<Map<String, Value>, Fault> {
            self.broker.apply(&record["request_id"], workspace)
        }

        fn read(&self, path: &str) -> String {
            fs::read_to_string(self.root.join(path)).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.tmp);
        }
    }

    /// A client that answers nothing and records what it was sent.
    #[derive(Default)]
    struct Silent(Vec<Value>);

    impl Client for Silent {
        fn send(&mut self, message: &Value) -> io::Result<()> {
            self.0.push(message.clone());
            Ok(())
        }
        fn recv(&mut self, _: Duration) -> Option<String> {
            None
        }
    }

    /// A client that confirms (or declines) whatever it is asked.
    struct Confirms(bool, Vec<String>);

    impl Client for Confirms {
        fn send(&mut self, message: &Value) -> io::Result<()> {
            if message["method"] == "elicitation/create" {
                self.1.push(json!({"jsonrpc": "2.0", "id": "tool-1", "method": "tools/call"}).to_string());
                let action = if self.0 { "accept" } else { "decline" };
                self.1.push(json!({"jsonrpc": "2.0", "id": message["id"], "result": {"action": action, "content": {"confirm": true}}}).to_string());
            }
            Ok(())
        }
        fn recv(&mut self, _: Duration) -> Option<String> {
            (!self.1.is_empty()).then(|| self.1.remove(0))
        }
    }

    fn call(broker: &PatchBroker, args: Value, capabilities: Value, client: &mut dyn Client) -> Result<Value, Fault> {
        broker.call(args.as_object().unwrap(), "world-a", capabilities.as_object().unwrap(), client)
    }

    fn fault<T>(result: Result<T, Fault>) -> Fault {
        match result {
            Ok(_) => panic!("expected a fault"),
            Err(fault) => fault,
        }
    }

    fn kind<T>(result: Result<T, Fault>) -> &'static str {
        fault(result).kind
    }

    #[test]
    fn real_apply_and_replay() {
        let f = Fixture::new();
        let read = call(&f.broker, json!({"action": "read", "target": "project", "path": "a.txt"}), json!({}), &mut Silent::default()).unwrap();
        assert_eq!(read["content"], "before\n");
        assert_eq!(read["sha256"], sha("before\n"));
        let request = f.prepare("a.txt", Some("before\n"), "after\n").unwrap();
        assert_eq!(kind(f.apply(&request, "world-a")), "PermissionError");
        f.approve(&request);
        assert_eq!(kind(f.apply(&request, "world-b")), "PermissionError");
        let result = f.apply(&request, "world-a").unwrap();
        assert_eq!(result["status"], "applied", "{result:?}");
        assert_eq!(f.read("a.txt"), "after\n");
        assert_eq!(result["files_touched"][0]["after_sha256"], sha("after\n"));
        assert_eq!(kind(f.apply(&request, "world-a")), "PermissionError");
        assert!(fs::read_to_string(f.store.join("receipts.jsonl")).unwrap().contains("\"status\": \"applied\""));
        let stored = fs::metadata(f.broker.path(&request["request_id"]).unwrap()).unwrap();
        assert_eq!(stored.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn the_target_lock_is_shared_across_registries() {
        let f = Fixture::new();
        let other = broker(&f.tmp.join("other-grants"), &roots(&[("project", &f.root)]), Options::default());
        let lock_path = f.tmp.join(".project.palette-patch.lock");
        let try_lock = || {
            let file = File::open(&lock_path).unwrap();
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
        };
        for broker in [&f.broker, &other] {
            let _held = broker.target_locked(&f.root.to_string_lossy()).unwrap();
            assert!(!try_lock());
        }
        assert!(try_lock());
    }

    #[test]
    fn d02_patch_roots_cannot_cover_private_grants() {
        let f = Fixture::new();
        let alias = f.tmp.join("grants-alias");
        symlink(&f.store, &alias).unwrap();
        for root in [&f.store, &f.tmp, &alias] {
            let result = PatchBroker::new(&f.store, &roots(&[("unsafe", root)]), Options::default());
            assert_eq!(kind(result), "PermissionError", "{}", root.display());
        }
    }

    #[test]
    fn a_conflict_consumes_without_overwrite() {
        let f = Fixture::new();
        let request = f.prepare("a.txt", Some("before\n"), "after\n").unwrap();
        f.approve(&request);
        fs::write(f.root.join("a.txt"), "someone else's work\n").unwrap();
        let result = f.apply(&request, "world-a").unwrap();
        assert_eq!(result["status"], "failed", "{result:?}");
        assert_eq!(result["error"], "Before hash conflict: a.txt");
        assert_eq!(f.read("a.txt"), "someone else's work\n");
        assert_eq!(kind(f.apply(&request, "world-a")), "PermissionError");
    }

    #[test]
    fn create_and_unsafe_paths() {
        let f = Fixture::new();
        let request = f.prepare("new.txt", None, "new\n").unwrap();
        f.approve(&request);
        assert_eq!(f.apply(&request, "world-a").unwrap()["status"], "applied");
        assert_eq!(f.read("new.txt"), "new\n");
        let outside = f.tmp.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("external.txt"), "before\n").unwrap();
        symlink(&outside, f.root.join("link")).unwrap();
        symlink(outside.join("external.txt"), f.root.join("alias.txt")).unwrap();
        fs::hard_link(outside.join("external.txt"), f.root.join("hard.txt")).unwrap();
        for (path, expected) in [("../outside/external.txt", "ValueError"), ("link/external.txt", "NotADirectoryError"),
                                 ("alias.txt", "OSError"), ("hard.txt", "ValueError"), (".git/config", "ValueError"),
                                 ("/tmp/nope", "ValueError")] {
            assert_eq!(kind(f.prepare(path, Some("before\n"), "after\n")), expected, "{path}");
        }
        assert_eq!(fs::read_to_string(outside.join("external.txt")).unwrap(), "before\n");
    }

    #[test]
    fn r08_credentials_are_forbidden_even_under_protected_or_self_apply_roots() {
        let mut f = Fixture::new();
        for relative in ["credentials.json", "secrets/token", ".ssh/id_rsa", ".aws/credentials", ".gnupg/private-keys-v1.d/key"] {
            let path = f.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "credential-before\n").unwrap();
            assert_eq!(kind(f.prepare(relative, Some("credential-before\n"), "credential-after\n")), "PermissionError", "{relative}");
            let read = call(&f.broker, json!({"action": "read", "target": "project", "path": relative}), json!({}), &mut Silent::default());
            assert_eq!(kind(read), "PermissionError", "{relative}");
        }
        f.broker.protected_paths = vec![f.root.clone()];
        assert_eq!(f.broker.classify(&"project".into(), &".ssh/id_rsa".into()).unwrap().class, "forbidden");
        f.broker.auto_apply_prefixes.insert("project".into(), vec![("adapters".into(), ".ssh".into())]);
        assert_eq!(f.broker.classify(&"project".into(), &".ssh/id_rsa".into()).unwrap().class, "forbidden");
    }

    #[test]
    fn r09_an_old_approved_credential_request_is_reclassified_at_apply() {
        let f = Fixture::new();
        fs::write(f.root.join("credentials.json"), "before\n").unwrap();
        // A pending request an older broker wrote before the immutable
        // credential denylist existed.
        let request = f.prepare("a.txt", Some("before\n"), "after\n").unwrap();
        let mut old = request.clone();
        old.insert("changes".into(), json!([{"path": "credentials.json", "before_sha256": sha("before\n"), "content": "after\n"}]));
        old.insert("patch_sha256".into(), request_digest(&old).unwrap().into());
        write_json(&f.broker.path(&old["request_id"]).unwrap(), &old).unwrap();
        f.broker.approve(&old["request_id"], old["patch_sha256"].as_str().unwrap(), "local_administrator").unwrap();
        assert_eq!(kind(f.apply(&old, "world-a")), "PermissionError");
        assert_eq!(f.read("credentials.json"), "before\n");
    }

    #[test]
    fn a_symlink_swapped_in_after_approval_is_refused() {
        let f = Fixture::new();
        let request = f.prepare("a.txt", Some("before\n"), "after\n").unwrap();
        f.approve(&request);
        let outside = f.tmp.join("external.txt");
        fs::write(&outside, "before\n").unwrap();
        fs::remove_file(f.root.join("a.txt")).unwrap();
        symlink(&outside, f.root.join("a.txt")).unwrap();
        let result = f.apply(&request, "world-a").unwrap();
        assert_eq!(result["status"], "failed", "{result:?}");
        assert_eq!(result["error"], "[Errno 40] Too many levels of symbolic links: 'a.txt'");
        assert_eq!(fs::read_to_string(outside).unwrap(), "before\n");
    }

    #[test]
    fn a_client_without_confirmation_cannot_apply() {
        let f = Fixture::new();
        let request = f.prepare("a.txt", Some("before\n"), "after\n").unwrap();
        let mut client = Silent::default();
        let result = call(&f.broker, json!({"action": "apply", "request_id": request["request_id"]}), json!({}), &mut client).unwrap();
        assert_eq!(result["status"], "approval_required");
        assert!(client.0.is_empty());
        assert_eq!(f.read("a.txt"), "before\n");
        assert_eq!(kind(f.broker.approve(&request["request_id"], "wrong digest", "local_administrator")), "PermissionError");
    }

    #[test]
    fn the_printed_approve_command_approves_exactly_this_patch() {
        let f = Fixture::new();
        let request = f.prepare("a.txt", Some("before\n"), "after\n").unwrap();
        let args = json!({"action": "apply", "request_id": request["request_id"]});
        let result = call(&f.broker, args.clone(), json!({}), &mut Silent::default()).unwrap();
        let approve = result["approve_command"].as_str().unwrap();
        assert!(approve.contains(request["patch_sha256"].as_str().unwrap()));
        for command in [result["review_command"].as_str().unwrap(), approve] {
            let out = Command::new("/bin/sh").args(["-c", command]).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(shown["request_id"], request["request_id"]);
        }
        let applied = call(&f.broker, args, json!({}), &mut Silent::default()).unwrap();
        assert_eq!(applied["status"], "applied", "{applied:?}");
    }

    #[test]
    fn the_cli_requires_the_digest() {
        let f = Fixture::new();
        let request = f.prepare("a.txt", Some("before\n"), "after\n").unwrap();
        let out = Command::new(host()).args(["patch", "--storage"]).arg(&f.store)
            .args(["approve", request["request_id"].as_str().unwrap()]).output().unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(String::from_utf8_lossy(&out.stderr), "palette-host: Approval requires the reviewed --digest\n");
        assert_eq!(f.broker.inspect(&request["request_id"], None).unwrap()["status"], "pending");
    }

    #[test]
    fn an_unknown_target_names_the_configured_ones() {
        let f = Fixture::new();
        for action in ["read", "prepare"] {
            let args = json!({"action": action, "target": "projcet", "path": "a.txt", "changes": []});
            let fault = call(&f.broker, args, json!({}), &mut Silent::default()).unwrap_err();
            assert_eq!(fault.kind, "PermissionError");
            assert_eq!(fault.message, "'projcet' is not a patch target; the host configures them in PALETTE_PATCH_ROOTS (configured: project)");
        }
    }

    #[test]
    fn administrator_mode_never_asks_the_client() {
        let f = Fixture::new();
        let admin = broker(&f.store, &roots(&[("project", &f.root)]),
                           Options { approval: Some("administrator".into()), ..Options::default() });
        let request = f.prepare("a.txt", Some("before\n"), "after\n").unwrap();
        let args = json!({"action": "apply", "request_id": request["request_id"]});
        let mut client = Confirms(true, Vec::new());
        let result = call(&admin, args.clone(), json!({"elicitation": {"form": {}}}), &mut client).unwrap();
        assert_eq!(result["status"], "approval_required", "{result:?}");
        assert_eq!(f.read("a.txt"), "before\n");
        f.approve(&request);
        let applied = call(&admin, args, json!({}), &mut client).unwrap();
        assert_eq!(applied["status"], "applied", "{applied:?}");
        assert_eq!(f.read("a.txt"), "after\n");
    }

    #[test]
    fn a_client_confirmation_applies_once_and_a_decline_does_not() {
        let f = Fixture::new();
        let declined = f.prepare("a.txt", Some("before\n"), "declined\n").unwrap();
        let mut client = Confirms(false, Vec::new());
        let args = json!({"action": "apply", "request_id": declined["request_id"]});
        let result = call(&f.broker, args, json!({"elicitation": {}}), &mut client).unwrap();
        assert_eq!(result, json!({"status": "not_approved", "request_id": declined["request_id"]}));
        assert_eq!(f.read("a.txt"), "before\n");
        let request = f.prepare("a.txt", Some("before\n"), "after\n").unwrap();
        let args = json!({"action": "apply", "request_id": request["request_id"]});
        let mut client = Confirms(true, Vec::new());
        let result = call(&f.broker, args.clone(), json!({"elicitation": {"form": {}}}), &mut client).unwrap();
        assert_eq!(result["status"], "applied", "{result:?}");
        assert_eq!(result["granted_by"], "mcp_client_confirmed_user");
        assert_eq!(f.read("a.txt"), "after\n");
        let timed_out = f.prepare("a.txt", Some("after\n"), "again\n").unwrap();
        let args = json!({"action": "apply", "request_id": timed_out["request_id"]});
        let mut client = Silent::default();
        let result = call(&f.broker, args, json!({"elicitation": {}}), &mut client).unwrap();
        assert_eq!(result["status"], "approval_timeout");
        assert_eq!(client.0[0]["params"]["mode"], "form");
    }

    #[test]
    fn the_approval_mode_is_parsed_strictly() {
        assert_eq!(approval_mode(None).unwrap(), "client");
        assert_eq!(approval_mode(Some("administrator")).unwrap(), "administrator");
        for bad in ["", "Administrator", "none", "human"] {
            assert_eq!(kind(approval_mode(Some(bad))), "ValueError", "{bad}");
        }
        let f = Fixture::new();
        let options = Options { approval: Some("auto".into()), ..Options::default() };
        assert_eq!(kind(PatchBroker::new(&f.store, &roots(&[("project", &f.root)]), options)), "ValueError");
    }

    #[test]
    fn expired_and_tampered_requests() {
        let f = Fixture::new();
        let mut request = f.prepare("a.txt", Some("before\n"), "after\n").unwrap();
        request.insert("expiry".into(), 0.into());
        request.insert("patch_sha256".into(), request_digest(&request).unwrap().into());
        write_json(&f.broker.path(&request["request_id"]).unwrap(), &request).unwrap();
        let digest = request["patch_sha256"].as_str().unwrap();
        assert_eq!(kind(f.broker.approve(&request["request_id"], digest, "local_administrator")), "PermissionError");
        let request = f.prepare("a.txt", Some("before\n"), "after\n").unwrap();
        let mut altered = request.clone();
        altered.insert("changes".into(), json!([{"path": "a.txt", "before_sha256": sha("before\n"), "content": "not approved"}]));
        write_json(&f.broker.path(&request["request_id"]).unwrap(), &altered).unwrap();
        let digest = request["patch_sha256"].as_str().unwrap();
        let fault = f.broker.approve(&request["request_id"], digest, "local_administrator").unwrap_err();
        assert_eq!(fault, Fault::permission("Stored patch differs from its bound digest"));
        assert_eq!(f.read("a.txt"), "before\n");
    }

    #[test]
    fn a_partial_failure_is_reported_without_replay() {
        let f = Fixture::new();
        let protected = f.root.join("protected");
        fs::create_dir(&protected).unwrap();
        fs::write(protected.join("b.txt"), "before\n").unwrap();
        let changes: Vec<Value> = ["a.txt", "protected/b.txt"].iter()
            .map(|p| json!({"path": p, "before_sha256": sha("before\n"), "content": "after\n"})).collect();
        let args = json!({"target": "project", "changes": changes});
        let request = f.broker.prepare(args.as_object().unwrap(), "world-a").unwrap();
        f.approve(&request);
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o555)).unwrap();
        let result = f.apply(&request, "world-a");
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o755)).unwrap();
        let result = result.unwrap();
        assert_eq!(result["status"], "partial", "{result:?}");
        assert_eq!(result["files_touched"].as_array().unwrap().len(), 1);
        assert_eq!(result["files_touched"][0]["path"], "a.txt");
        assert_eq!(f.read("a.txt"), "after\n");
        assert_eq!(f.read("protected/b.txt"), "before\n");
        assert_eq!(kind(f.apply(&request, "world-a")), "PermissionError");
    }

    #[test]
    fn r03_protected_unknown_and_credentials_never_self_apply() {
        let mut f = Fixture::new();
        let classes = ["runtime", "tools", "recipes", "adapters"].map(|c| (c.to_string(), c.to_string())).to_vec();
        f.broker.auto_apply_prefixes.insert("project".into(), classes);
        let classify = |path: &str| f.broker.classify(&"project".into(), &path.into()).unwrap();
        assert!(!classify("runtime/security/patch_broker.py").self_apply);
        assert!(!classify("runtime/capabilities/manifest.json").self_apply);
        assert_eq!(classify("settings/credentials.json").class, "forbidden");
        assert!(!classify("unclassified/app.py").self_apply);
        assert!(!classify("app/config.py").self_apply);
        assert!(classify("tools/helper.jl").self_apply);
        let security = f.broker.runtime_dir.clone().unwrap().join("security");
        let nested = broker(&f.tmp.join("nested-grants"), &roots(&[("security-tree", &security)]),
                            Options { auto_apply_prefixes: json!({"security-tree": [["runtime", "new_helpers"]]}), ..Options::default() });
        assert!(!nested.classify(&"security-tree".into(), &"new_helpers/module.py".into()).unwrap().self_apply);
    }

    #[test]
    fn r12_registry_exceptions_never_cover_manifest_security_config_host_or_credentials() {
        let f = Fixture::new();
        let registry = f.tmp.join("runtime-capabilities");
        let capability = registry.join("adapter");
        fs::create_dir_all(&capability).unwrap();
        fs::write(capability.join("adapter.jl"), "answer() = 1\n").unwrap();
        fs::write(capability.join("capability.json"), json!({"id": "adapter", "version": "1", "source": "adapter.jl",
            "entrypoint": "answer", "dependencies": [], "requires": []}).to_string()).unwrap();
        let options = |prefixes: Value, protected: Vec<PathBuf>, sources: Value| Options {
            auto_apply_prefixes: json!({"adapter": prefixes}), protected_paths: protected,
            registry_root: Some(registry.to_string_lossy().into_owned()),
            sandbox_source_paths: json!({"adapter": sources}), approval: None,
        };
        let adapter_roots = roots(&[("adapter", &capability)]);
        let b = broker(&f.tmp.join("registry-grants"), &adapter_roots,
            options(json!([["adapters", "adapter.jl"], ["adapters", "capability.json"], ["adapters", "config.json"], ["adapters", "security"]]),
                    vec![], json!(["adapter.jl"])));
        let classify = |b: &PatchBroker, path: &str| b.classify(&"adapter".into(), &path.into()).unwrap();
        let source = classify(&b, "adapter.jl");
        assert_eq!((source.class.as_str(), source.self_apply), ("adapters", true));
        for relative in ["capability.json", "config.json", "security/logic.jl"] {
            let c = classify(&b, relative);
            assert_eq!((c.class.as_str(), c.self_apply), ("protected", false), "{relative}");
        }
        for relative in [".env", ".ssh/id_rsa", "credentials/token"] {
            assert_eq!(classify(&b, relative).class, "forbidden", "{relative}");
        }
        let protected_host = broker(&f.tmp.join("hostcwd-grants"), &adapter_roots,
            options(json!([["adapters", "adapter.jl"]]), vec![capability.clone()], json!(["adapter.jl"])));
        let c = classify(&protected_host, "adapter.jl");
        assert_eq!((c.class.as_str(), c.self_apply), ("protected", false));
        let manifest_source = PatchBroker::new(&f.tmp.join("manifest-whitelist-grants"), &adapter_roots,
            options(json!([["adapters", "capability.json"]]), vec![], json!(["capability.json"])));
        assert_eq!(kind(manifest_source), "ValueError");
        let ssh_root = f.tmp.join(".ssh");
        fs::create_dir(&ssh_root).unwrap();
        fs::write(ssh_root.join("id_rsa"), "SYNTHETIC_PRIVATE_KEY_FIXTURE\n").unwrap();
        let narrowed = broker(&f.tmp.join("ssh-grants"), &roots(&[("private", &ssh_root)]), Options::default());
        assert_eq!(narrowed.classify(&"private".into(), &"id_rsa".into()).unwrap().class, "forbidden");
        let read = call(&narrowed, json!({"action": "read", "target": "private", "path": "id_rsa"}), json!({}), &mut Silent::default());
        assert_eq!(kind(read), "PermissionError");
        let args = json!({"target": "private", "changes": [{"path": "id_rsa", "before_sha256": sha("SYNTHETIC_PRIVATE_KEY_FIXTURE\n"), "content": "REPLACED\n"}]});
        assert_eq!(kind(narrowed.prepare(args.as_object().unwrap(), "world-a")), "PermissionError");
    }

    #[test]
    fn r13_the_default_deployed_registry_stays_immutable_without_an_explicit_exception() {
        let f = Fixture::new();
        let fake_runtime = f.tmp.join("runtime");
        let capability = fake_runtime.join("capabilities/gesso");
        fs::create_dir_all(fake_runtime.join("security")).unwrap();
        fs::create_dir_all(&capability).unwrap();
        fs::write(capability.join("gesso.jl"), "answer() = 1\n").unwrap();
        fs::write(capability.join("capability.json"), json!({"source": "gesso.jl"}).to_string()).unwrap();
        let mut b = broker(&f.tmp.join("default-registry-grants"), &roots(&[("deployed", &capability)]),
                           Options { auto_apply_prefixes: json!({"deployed": [["adapters", "gesso.jl"]]}), ..Options::default() });
        b.runtime_dir = Some(fake_runtime);
        let c = b.classify(&"deployed".into(), &"gesso.jl".into()).unwrap();
        assert_eq!((c.class.as_str(), c.self_apply), ("protected", false));
        b.runtime_dir = None;
        assert_eq!(kind(b.classify(&"deployed".into(), &"gesso.jl".into())), "RuntimeError");
    }

    #[test]
    fn python_shaped_errors_for_malformed_configuration() {
        let f = Fixture::new();
        let project = roots(&[("project", &f.root)]);
        let config_fault = |prefixes: Value| fault(PatchBroker::new(&f.store, &project,
            Options { auto_apply_prefixes: prefixes, ..Options::default() })).to_string();
        assert_eq!(config_fault(json!({"project": [["runtime", "a", "b"]]})), "ValueError: too many values to unpack (expected 2, got 3)");
        assert_eq!(config_fault(json!({"project": ["abc"]})), "ValueError: too many values to unpack (expected 2)");
        assert_eq!(config_fault(json!({"project": [["runtime"]]})), "ValueError: not enough values to unpack (expected 2, got 1)");
        assert_eq!(config_fault(json!({"project": [3]})), "TypeError: cannot unpack non-iterable int object");
        assert_eq!(config_fault(json!({"project": 3})), "TypeError: 'int' object is not iterable");
        assert_eq!(config_fault(json!({"project": [[["x"], "a"]]})), "TypeError: cannot use 'list' as a set element (unhashable type: 'list')");
        assert_eq!(config_fault(json!({"project": [["other", "a"]]})), "ValueError: Unknown auto-apply path class");
        assert_eq!(config_fault(json!({"elsewhere": []})), "ValueError: Auto-apply classes require a configured patch target");
        assert_eq!(config_fault(json!([1])), "AttributeError: 'list' object has no attribute 'items'");
        assert_eq!(config_fault(json!(5)), "TypeError: argument of type 'int' is not iterable");
        assert!(PatchBroker::new(&f.store, &project, Options { auto_apply_prefixes: json!({"project": ["ra"]}), ..Options::default() }).is_err());
        let args = json!({"target": ["project"], "changes": []});
        assert_eq!(f.broker.prepare(args.as_object().unwrap(), "w").unwrap_err().to_string(),
                   "TypeError: cannot use 'list' as a dict key (unhashable type: 'list')");
        let duplicate = json!({"target": "project", "changes": [
            {"path": "a.txt", "before_sha256": sha("before\n"), "content": "x"},
            {"path": "a.txt", "before_sha256": sha("before\n"), "content": "y"}]});
        assert_eq!(f.broker.prepare(duplicate.as_object().unwrap(), "w").unwrap_err(), Fault::value("Duplicate patch path"));
        assert_eq!(f.broker.inspect(&"nothex".into(), None).unwrap_err(), Fault::value("Invalid patch request_id"));
    }

    #[test]
    fn the_apply_helper_reports_a_bad_record_as_failed() {
        let out = Command::new(host()).args(["patch", "apply-helper"]).stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped()).spawn().and_then(|mut child| {
                child.stdin.take().unwrap().write_all(b"[]")?;
                child.wait_with_output()
            }).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout),
                   "{\"error\": \"list indices must be integers or slices, not str\", \"files_touched\": [], \"status\": \"failed\"}\n");
    }

    #[test]
    fn trusted_tool_names() {
        for good in ["helper.jl", "helper.json", "push_row!.jl", "_x9.json"] {
            assert!(trusted_tool_file(good), "{good}");
        }
        for bad in ["sub/helper.jl", "../helper.jl", "helper.py", ".helper.jl", "9lives.jl", "helper.jl.bak", "", ".jl", "!x.jl"] {
            assert!(!trusted_tool_file(bad), "{bad}");
        }
    }

    #[test]
    fn the_trusted_tool_target_comes_from_palette_trusted_tools_only() {
        let tmp = util::resolve(&util::mkdtemp("palette-trusted-").unwrap());
        let trusted = tmp.join("trusted/tools");
        let env = |pairs: &'static [(&'static str, String)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
        let pairs: &'static [(&str, String)] = Box::leak(Box::new([("PALETTE_PATCH_ROOTS", "{}".to_string()),
                                                                   ("PALETTE_TRUSTED_TOOLS", trusted.to_string_lossy().into_owned())]));
        let found = patch_roots_from_env(env(pairs)).unwrap();
        assert_eq!(found, roots(&[(TRUSTED_TOOLS_TARGET, &trusted)]));
        assert_eq!(fs::metadata(&trusted).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(patch_roots_from_env(env(&[])).unwrap().is_empty());
        let reserved: &'static [(&str, String)] = Box::leak(Box::new([("PALETTE_PATCH_ROOTS", json!({"trusted_tools": "/x"}).to_string())]));
        assert_eq!(patch_roots_from_env(env(reserved)).unwrap_err(),
                   Fault::value("'trusted_tools' is a reserved patch target; set PALETTE_TRUSTED_TOOLS instead"));
        let relative: &'static [(&str, String)] = Box::leak(Box::new([("PALETTE_TRUSTED_TOOLS", "relative/tools".to_string())]));
        assert!(patch_roots_from_env(env(relative)).unwrap_err().message.contains("absolute path"));
        for bad in ["[]", "not json"] {
            let pairs: &'static [(&str, String)] = Box::leak(Box::new([("PALETTE_PATCH_ROOTS", bad.to_string())]));
            assert_eq!(patch_roots_from_env(env(pairs)).unwrap_err(), Fault::value("PALETTE_PATCH_ROOTS must be a JSON object"));
        }
        fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn trusted_tools_take_only_tool_files_and_a_human() {
        let f = Fixture::new();
        let trusted = f.tmp.join("trusted/tools");
        fs::create_dir_all(&trusted).unwrap();
        let trusted_roots = roots(&[(TRUSTED_TOOLS_TARGET, &trusted)]);
        for options in [Options { auto_apply_prefixes: json!({"trusted_tools": [["runtime", ""]]}), ..Options::default() },
                        Options { sandbox_source_paths: json!({"trusted_tools": ["helper.jl"]}), ..Options::default() }] {
            assert!(fault(PatchBroker::new(&f.tmp.join("t-grants"), &trusted_roots, options)).message.contains("human"));
        }
        let b = broker(&f.tmp.join("t-grants"), &trusted_roots, Options::default());
        let target = Value::from(TRUSTED_TOOLS_TARGET);
        for name in ["helper.jl", "helper.json", "push_row!.jl", "_x9.json"] {
            assert_eq!(b.classify(&target, &name.into()).unwrap(), class("protected", false, "trusted tool: a human approves it"));
        }
        for name in [json!("sub/helper.jl"), json!("../helper.jl"), json!("helper.py"), json!(".helper.jl"),
                     json!("9lives.jl"), json!("helper.jl.bak"), json!(""), Value::Null] {
            assert_eq!(kind(b.classify(&target, &name)), "PermissionError", "{name}");
        }
        let args = json!({"target": TRUSTED_TOOLS_TARGET, "changes": [{"path": "sub/helper.jl", "before_sha256": null, "content": "x\n"}]});
        assert_eq!(kind(b.prepare(args.as_object().unwrap(), "world-a")), "PermissionError");
    }
}
