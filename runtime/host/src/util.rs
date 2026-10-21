//! Small helpers shared by the sandbox, broker and session.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Random bytes from the kernel.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("reading /dev/urandom");
    buf
}

/// A version-4 UUID in its usual text form.
pub fn uuid4() -> String {
    let mut b = random_bytes::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = to_hex(&b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// `n` random lowercase hex digits (at most 64).
pub fn hex(n: usize) -> String {
    to_hex(&random_bytes::<32>())[..n].to_string()
}

/// Bytes as lowercase hex, two digits each.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

/// Refuses `dir` unless it is a real directory (not a symlink) this user
/// owns that no one else can write.
pub fn check_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("checking {}: {e}", dir.display()))?;
    let uid = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(format!("{} must be a directory (not a symlink) owned by uid {uid} that only its owner can write", dir.display()));
    }
    Ok(())
}

/// Writes `bytes` to the new file `tmp` (never through a symlink), syncs it,
/// gives it `mode`, then runs `place` to move it where it belongs. `tmp` is
/// removed if any step fails.
pub fn write_then(tmp: &Path, bytes: &[u8], mode: u32, place: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    let written = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::set_permissions(tmp, fs::Permissions::from_mode(mode))?;
        place()
    })();
    if written.is_err() {
        let _ = fs::remove_file(tmp);
    }
    written
}

pub fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// A new private directory `<parent>/<prefix>XXXXXXXX`, mode 0700.
pub fn mkdtemp_in(parent: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    for _ in 0..16 {
        let p = parent.join(format!("{prefix}{}", hex(8)));
        match fs::DirBuilder::new().mode(0o700).create(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("could not create a unique temporary directory"))
}

/// The system's temporary directory, as Python's tempfile.gettempdir() picks it.
pub fn temp_dir() -> PathBuf {
    std::env::var_os("TMPDIR").map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn mkdtemp(prefix: &str) -> std::io::Result<PathBuf> {
    mkdtemp_in(&temp_dir(), prefix)
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// The Palette checkout this binary is running from, found at run time by
/// walking up from the executable to the directory that holds
/// `runtime/scripts/session_loop.jl`. None for a binary installed outside a
/// checkout.
pub fn installed_checkout() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok().and_then(|p| fs::canonicalize(p).ok())?;
    exe.ancestors().find(|d| d.join("runtime/scripts/session_loop.jl").is_file()).map(Path::to_path_buf)
}

/// Absolute, with symlinks resolved where the path exists and `..` applied
/// after them, as Python's non-strict `Path.resolve()`. An allowlist check
/// on the result cannot be escaped through a symlink or `..`.
pub fn resolve(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
    let mut out = PathBuf::from("/");
    for c in abs.components() {
        match c {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if fs::symlink_metadata(&out).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                    if let Ok(real) = fs::canonicalize(&out) {
                        out = real;
                    }
                }
            }
        }
    }
    out
}

/// True when `path` is `dir` or inside it.
pub fn within(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir)
}

/// The last `n` characters of `s`.
pub fn tail(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        s.chars().skip(count - n).collect()
    }
}

pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// A number the way Python's `f"{x:g}"` prints it for the values used here.
pub fn fmt_g(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

/// The significant digits and decimal exponent of `s`, a Rust `{:e}`
/// rendering of a finite number: "-1.25e3" is (true, "125", 3).
fn sci_parts(s: &str) -> (bool, String, i32) {
    let (neg, s) = s.strip_prefix('-').map_or((false, s), |rest| (true, rest));
    let (mantissa, exp) = s.split_once('e').unwrap_or((s, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    (neg, if digits.is_empty() { "0".into() } else { digits.into() }, exp.parse().unwrap_or(0))
}

/// `digits` (d.ddd x 10^exp) in positional or scientific notation, Python
/// style: no trailing zeros, an exponent of at least two digits with a sign,
/// and `point` appends ".0" to a whole positional number.
fn py_digits(neg: bool, digits: &str, exp: i32, scientific: bool, point: bool) -> String {
    let sign = if neg { "-" } else { "" };
    if scientific {
        let (head, tail) = digits.split_at(1);
        let frac = if tail.is_empty() { String::new() } else { format!(".{tail}") };
        let esign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{head}{frac}e{esign}{:02}", exp.abs());
    }
    let n = digits.len() as i32;
    let text = if exp < 0 {
        format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
    } else if exp + 1 >= n {
        format!("{digits}{}{}", "0".repeat((exp + 1 - n) as usize), if point { ".0" } else { "" })
    } else {
        let (int, frac) = digits.split_at((exp + 1) as usize);
        format!("{int}.{frac}")
    };
    format!("{sign}{text}")
}

fn py_special(x: f64) -> Option<String> {
    if x.is_nan() {
        Some("nan".into())
    } else if x.is_infinite() {
        Some(if x > 0.0 { "inf" } else { "-inf" }.into())
    } else {
        None
    }
}

/// `x` as Python's `f"{x:g}"`: six significant digits, scientific below
/// 1e-4 and from 1e6. Unlike `fmt_g`, which keeps every digit for values
/// handed to another process, this is for text a person reads.
pub fn py_g(x: f64) -> String {
    if let Some(s) = py_special(x) {
        return s;
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let (neg, digits, exp) = sci_parts(&format!("{x:.5e}"));
    py_digits(neg, &digits, exp, !(-4..6).contains(&exp), false)
}

/// `x` as Python's `repr(float)`: the shortest digits that read back as
/// `x`, scientific below 1e-4 and from 1e16.
pub fn py_repr_f64(x: f64) -> String {
    if let Some(s) = py_special(x) {
        return s;
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let (neg, digits, exp) = sci_parts(&format!("{x:e}"));
    py_digits(neg, &digits, exp, !(-4..16).contains(&exp), true)
}

/// A word the shell reads back as `s`, as Python's `shlex.quote`.
pub fn shquote(s: &str) -> String {
    if s.is_empty() {
        "''".into()
    } else if s.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)) {
        s.into()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

pub fn shjoin<S: AsRef<str>>(words: &[S]) -> String {
    words.iter().map(|w| shquote(w.as_ref())).collect::<Vec<_>>().join(" ")
}

/// `s` with a leading `~` taken as the home directory and `~name` as that
/// user's, as `os.path.expanduser` does: an unknown user leaves `s` as it is.
pub fn expand_user(s: &str) -> PathBuf {
    let Some(rest) = s.strip_prefix('~') else { return PathBuf::from(s) };
    let (name, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let Some(dir) = (if name.is_empty() { Some(home()) } else { user_home(name) }) else { return PathBuf::from(s) };
    let dir = dir.to_string_lossy().trim_end_matches('/').to_string();
    PathBuf::from(if dir.is_empty() && tail.is_empty() { "/".to_string() } else { dir + tail })
}

/// The home directory of the user `name` in the password database.
fn user_home(name: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let cname = std::ffi::CString::new(name).ok()?;
    let mut buf = vec![0 as libc::c_char; 16384];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwnam_r(cname.as_ptr(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut found) };
    if rc != 0 || found.is_null() || pw.pw_dir.is_null() {
        return None;
    }
    let dir = unsafe { std::ffi::CStr::from_ptr(pw.pw_dir) };
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
}

/// True for a file this user may execute.
pub fn executable(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    p.is_file()
        && std::ffi::CString::new(p.as_os_str().as_bytes()).is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0)
}

/// The first executable `name` on PATH, as `shutil.which` finds it, made
/// absolute: callers write it into units and registrations that run from
/// another directory, where a relative PATH entry would name something else.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let found = std::env::split_paths(&path).filter(|d| !d.as_os_str().is_empty()).map(|d| d.join(name)).find(|p| executable(p))?;
    std::path::absolute(&found).ok()
}

/// Replaces a file in one rename, so a reader never sees a half-written one.
pub fn write_atomic(path: &Path, contents: &str, mode: u32) -> Result<(), String> {
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
    let parent = path.parent().ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let name = path.file_name().ok_or_else(|| format!("{} has no file name", path.display()))?;
    let tmp = parent.join(format!(".{}.{}", name.to_string_lossy(), std::process::id()));
    let _ = fs::remove_file(&tmp);
    write_then(&tmp, contents.as_bytes(), mode, || fs::rename(&tmp, path)).map_err(at)
}

/// Python's repr of a string.
pub fn py_str(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', "\\'"))
    }
}

/// The SHA-256 of `bytes` in lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    to_hex(&Sha256::digest(bytes))
}

/// A string as Python's `json.dumps` writes it (`ensure_ascii`).
pub fn py_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in serde_json::Value::String(s.into()).to_string().chars() {
        // Python also escapes DEL, which serde leaves alone.
        if c.is_ascii() && c != '\x7f' {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_as_python_prints_them() {
        for (x, g, r) in [
            (60.0, "60", "60.0"),
            (1800.0, "1800", "1800.0"),
            (604800.0, "604800", "604800.0"),
            (0.5, "0.5", "0.5"),
            (1234567.0, "1.23457e+06", "1234567.0"),
            (1e6, "1e+06", "1000000.0"),
            (123456.7, "123457", "123456.7"),
            (0.0001, "0.0001", "0.0001"),
            (0.00001234, "1.234e-05", "1.234e-05"),
            (1e16, "1e+16", "1e+16"),
            (1e15, "1e+15", "1000000000000000.0"),
            (0.1 + 0.2, "0.3", "0.30000000000000004"),
            (-2.5, "-2.5", "-2.5"),
            (0.0, "0", "0.0"),
            (f64::INFINITY, "inf", "inf"),
            (999999.5, "1e+06", "999999.5"),
        ] {
            assert_eq!((py_g(x), py_repr_f64(x)), (g.to_string(), r.to_string()), "{x}");
        }
    }
}
