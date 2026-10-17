//! Small helpers shared by the sandbox, broker and session.

use std::fs;
use std::io::Read;
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
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

pub fn hex(n: usize) -> String {
    let b = random_bytes::<32>();
    b.iter().map(|x| format!("{x:02x}")).collect::<String>()[..n].to_string()
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
