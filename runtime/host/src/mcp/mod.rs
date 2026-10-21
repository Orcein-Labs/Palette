//! `palette-host mcp`: the MCP front door, ported from the Python router and
//! adapter. Wired into the binary in slice C.

pub(crate) mod generation;
pub(crate) mod lineage;
pub(crate) mod preparation;
pub(crate) mod receipts;
pub(crate) mod time_machine;
pub(crate) mod transport;

use std::fmt;
use std::fs;
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
