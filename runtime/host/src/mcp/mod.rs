//! `palette-host mcp`: the MCP front door, ported from the Python router and
//! adapter. `router` serves agents; `child` is one workspace's adapter.

pub(crate) mod casefold;
pub(crate) mod child;
pub(crate) mod client;
pub(crate) mod difflib;
pub(crate) mod generation;
pub(crate) mod lineage;
pub(crate) mod patch;
pub(crate) mod preparation;
pub(crate) mod receipts;
pub(crate) mod registry;
pub(crate) mod repair;
pub(crate) mod router;
pub(crate) mod time_machine;
pub(crate) mod transport;

use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use serde_json::Value;

use crate::util;

/// An error as the Python router reported it, `"{kind}: {message}"`: kind is
/// the Python exception's class name, which agents already know to read.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fault {
    pub kind: &'static str,
    pub message: String,
}

impl Fault {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Fault {
        Fault { kind, message: message.into() }
    }

    pub fn value(message: impl Into<String>) -> Fault {
        Fault::new("ValueError", message)
    }

    pub fn permission(message: impl Into<String>) -> Fault {
        Fault::new("PermissionError", message)
    }

    pub fn io(e: std::io::Error) -> Fault {
        Fault::new("OSError", e.to_string())
    }

    /// The OSError subclass Python raises for `e`, naming `path` as it does.
    pub fn os(e: &std::io::Error, path: &Path) -> Fault {
        let kind = match e.raw_os_error() {
            Some(libc::ENOENT) => "FileNotFoundError",
            Some(libc::EACCES | libc::EPERM) => "PermissionError",
            Some(libc::ENOTDIR) => "NotADirectoryError",
            Some(libc::EEXIST) => "FileExistsError",
            Some(libc::EISDIR) => "IsADirectoryError",
            Some(libc::EAGAIN) => "BlockingIOError",
            Some(libc::ETIMEDOUT) => "TimeoutError",
            Some(libc::ESRCH) => "ProcessLookupError",
            Some(libc::ECHILD) => "ChildProcessError",
            _ => "OSError",
        };
        Fault::new(kind, util::py_os_error(e, path))
    }
}

/// `json.loads(text)`, with the JSONDecodeError wording for the failures a
/// peer usually sends: nothing parseable, or a value followed by more.
pub(crate) fn py_json_loads(text: &str) -> Result<Value, Fault> {
    let at_char = |what: &str, at: usize| {
        let before = &text[..at];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        Fault::new("JSONDecodeError", format!("{what}: line {line} column {column} (char {})", before.chars().count()))
    };
    if text.starts_with('\u{feff}') {
        return Err(at_char("Unexpected UTF-8 BOM (decode using utf-8-sig)", 0));
    }
    serde_json::from_str(text).map_err(|e| {
        let what = e.to_string();
        let eof = what.starts_with("EOF while parsing a value");
        let extra = what.starts_with("trailing characters");
        let label = if extra {
            "Extra data"
        } else if eof || what.starts_with("expected value") || what.starts_with("expected ident") {
            "Expecting value"
        } else {
            return Fault::new("JSONDecodeError", what);
        };
        // serde counts a 1-based byte column at the failing byte (the last
        // byte at the end); Python names the failing character, or the end.
        let line_start: usize = text.split_inclusive('\n').take(e.line() - 1).map(str::len).sum();
        let mut at = if eof { text.len() } else { (line_start + e.column().saturating_sub(1)).min(text.len()) };
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        // A misspelt literal (`nul`, `trux`) fails inside it; Python reports
        // the value where it starts.
        while !extra && at > 0 && text.as_bytes()[at - 1].is_ascii_alphabetic() {
            at -= 1;
        }
        at_char(label, at)
    })
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`: a workspace, project root or
/// generation id.
pub(crate) fn valid_id(s: &str) -> bool {
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    s.len() <= 64 && s.starts_with(|c: char| c.is_ascii_alphanumeric()) && s.chars().all(ok)
}

/// Replaces `path` with `value` as one line of JSON (keys sorted), mode 0600,
/// in one rename; its directory is created 0700.
pub(crate) fn atomic_json(path: &Path, value: &Value) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| std::io::Error::other(format!("{} has no parent directory", path.display())))?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let tmp = parent.join(format!(".{name}.{}", util::hex(16)));
    let text = value.to_string() + "\n";
    util::write_then(&tmp, text.as_bytes(), 0o600, || fs::rename(&tmp, path))
}

/// The output of `argv` and its exit code (minus the signal that ended it),
/// or, once `timeout` passes, the output so far with the process killed.
/// `input`, if any, is written to its stdin, which then closes.
pub(crate) fn run_captured(argv: &[String], input: Option<&[u8]>, timeout: Option<Duration>)
    -> std::io::Result<(i32, String, String, bool)> {
    let stdin = if input.is_some() { Stdio::piped() } else { Stdio::null() };
    let mut child = Command::new(&argv[0]).args(&argv[1..])
        .stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    if let (Some(input), Some(mut pipe)) = (input, child.stdin.take()) {
        let input = input.to_vec();
        // A child that never reads must not block the timeout below.
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    let collect = |mut pipe: Box<dyn Read + Send>| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n @ 1..) = pipe.read(&mut chunk) {
                sink.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        });
        (buf, reader)
    };
    let (out, out_reader) = collect(Box::new(child.stdout.take().expect("piped stdout")));
    let (err, err_reader) = collect(Box::new(child.stderr.take().expect("piped stderr")));
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut status = None;
    let timed_out = loop {
        if status.is_none() {
            status = child.try_wait()?;
        }
        // Its output ends once it and every process it left holding the
        // pipes have exited.
        if status.is_some() && out_reader.is_finished() && err_reader.is_finished() {
            break false;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            if status.is_none() {
                let _ = child.kill();
                status = Some(child.wait()?);
            }
            // The output is what arrived by now.
            break true;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let status = status.expect("the loop ends with a status");
    let code = status.code().unwrap_or_else(|| -status.signal().unwrap_or(0));
    let text = |b: &Arc<Mutex<Vec<u8>>>| String::from_utf8_lossy(&b.lock().unwrap()).into_owned();
    Ok((code, text(&out), text(&err), timed_out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn ids_follow_the_python_pattern() {
        for good in ["a", "A9", "x.y_z-1", &"a".repeat(64)] {
            assert!(valid_id(good), "{good}");
        }
        for bad in ["", ".a", "-a", "_a", "a/b", "a b", "ü", &"a".repeat(65), "a\n"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_descendant_holding_the_pipes_cannot_outlast_the_timeout() {
        let argv: Vec<String> = ["/bin/sh", "-c", "echo started; sleep 30 & exit 0"].map(String::from).into();
        let begun = Instant::now();
        let (code, stdout, _, timed_out) = run_captured(&argv, None, Some(Duration::from_millis(500))).unwrap();
        assert!(begun.elapsed() < Duration::from_secs(10), "{:?}", begun.elapsed());
        assert_eq!((code, stdout.as_str(), timed_out), (0, "started\n", true));
        let (code, stdout, _, timed_out) = run_captured(&["/bin/cat".to_string()], Some(b"fed"), Some(Duration::from_secs(30))).unwrap();
        assert_eq!((code, stdout.as_str(), timed_out), (0, "fed", false));
    }

    #[test]
    fn atomic_json_writes_a_private_sorted_line() {
        let dir = util::mkdtemp("palette-mcp-json-").unwrap();
        let path = dir.join("new/sub/x.json");
        atomic_json(&path, &serde_json::json!({"b": 1, "a": [true]})).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\":[true],\"b\":1}\n");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
