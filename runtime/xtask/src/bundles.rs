//! Reproducible client bundles from an explicit committed Palette revision:
//! the Claude Desktop extension, the Claude plugin and the ChatGPT plugin.

use crate::archive;
use crate::Fail;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

type Files = BTreeMap<String, Vec<u8>>;

/// A client upload must declare exactly one plugin. The SDK's repository
/// marketplace and client manifests are authoring files, not nested plugins.
const EXCLUDED: [&str; 5] = [
    ".agents/plugins/marketplace.json",
    ".claude-plugin/marketplace.json",
    "Palette-Launch-Bundles.zip",
    "runtime/plugins/claude/manifest.json",
    "runtime/plugins/palette/plugin.json",
];

/// Rejects an ambiguous client package before it is handed to an uploader.
pub fn validate(names: &[&str], desktop: bool) -> Result<(), String> {
    if names.iter().any(|n| n.ends_with(".zip")) {
        return Err("Client archive must not contain nested ZIPs".into());
    }
    if desktop {
        if !names.contains(&"manifest.json") || !names.contains(&"server/index.cjs") {
            return Err("Desktop MCPB requires its root manifest and bridge".into());
        }
        return Ok(());
    }
    let manifests: Vec<&Path> = names.iter().map(Path::new).filter(|p| p.file_name().is_some_and(|n| n == "plugin.json")).collect();
    let [manifest] = manifests[..] else {
        return Err("Expected a single plugin archive".into());
    };
    let parent = manifest.parent().unwrap_or(Path::new(""));
    let hidden = parent.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'));
    let root = if hidden { parent.parent().unwrap_or(Path::new("")) } else { parent };
    let prefix = format!("{}/", root.display());
    if root != Path::new("") && (root.components().count() != 1 || names.iter().any(|n| !n.starts_with(&prefix))) {
        return Err("Plugin root must have no sibling files".into());
    }
    Ok(())
}

/// `value` as Python's `json.dumps(value, indent=2)` writes it, plus a newline.
fn json_bytes(value: &Value) -> Vec<u8> {
    let mut text = String::new();
    for c in serde_json::to_string_pretty(value).expect("a JSON value serializes").chars() {
        if c.is_ascii() {
            text.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                write!(text, "\\u{unit:04x}").unwrap();
            }
        }
    }
    text.push('\n');
    text.into_bytes()
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git").args(args).current_dir(root).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {} failed ({}): {}", args.join(" "), out.status, String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

/// Every file under `dir/` in `source`, by its path below `dir/`.
fn under(source: &Files, dir: &str) -> Files {
    source.iter().filter_map(|(n, d)| Some((n.strip_prefix(dir)?.to_string(), d.clone()))).collect()
}

/// The three bundles of `revision`, by file name, in the order they are listed.
fn build(source: &Files, revision: &str) -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    if source.keys().any(|n| Path::new(n).components().any(|c| c.as_os_str() == ".archive" || c.as_os_str() == ".git")) {
        return Err("the archive holds a .git or .archive path".into());
    }
    let file = |name: &str| source.get(name).cloned().ok_or_else(|| format!("revision {revision} has no {name}"));
    let manifest: Value = serde_json::from_slice(&file("runtime/plugins/palette/plugin.json")?).map_err(|e| format!("plugin.json: {e}"))?;
    let field = |key: &str| manifest.get(key).cloned().ok_or_else(|| format!("plugin.json has no {key}"));
    let (inherited, license, notice) = (file("runtime/licenses/MIT-INHERITED.txt")?, file("runtime/licenses/Apache-2.0.txt")?, file("NOTICE")?);
    let brain_blast = String::from_utf8(file("BRAIN_BLAST.md")?).map_err(|e| format!("BRAIN_BLAST.md: {e}"))?;
    let mut shared: Files = source.iter().filter(|(n, _)| !EXCLUDED.contains(&n.as_str())).map(|(n, d)| (format!("sdk/{n}"), d.clone())).collect();
    shared.extend([
        ("templates/palette-plugin.json".into(), file("runtime/plugins/palette/plugin.json")?),
        ("setup.sh".into(), file("runtime/plugins/setup.sh")?),
        ("INSTALL.md".into(), file("runtime/plugins/DISTRIBUTION.md")?),
        ("BRAIN_BLAST.md".into(), brain_blast.replace("(runtime/docs/", "(sdk/runtime/docs/").into_bytes()),
        ("LICENSE".into(), license.clone()),
        ("NOTICE".into(), notice.clone()),
        ("licenses/AGPL-3.0-only.txt".into(), file("LICENSE")?),
        ("licenses/MIT-INHERITED.txt".into(), inherited.clone()),
        ("licenses/BOUNDARIES.md".into(), file("runtime/licenses/README.md")?),
        (
            "SOURCE.json".into(),
            json_bytes(&json!({
                "repository": "https://github.com/latentcollapse/Palette",
                "revision": revision,
                "plugin_version": field("version")?,
                "runtime": "source; prepare on Linux or WSL2",
                "extra_language_toolchains": false,
                "sdk_omitted_authoring_manifests": EXCLUDED,
            })),
        ),
    ]);

    let mut desktop = shared.clone();
    desktop.extend(under(source, "runtime/plugins/claude/"));
    desktop.extend([("LICENSE".into(), license), ("NOTICE".into(), notice), ("MIT-INHERITED.txt".into(), inherited)]);

    let mut portable = under(source, "runtime/plugins/palette/");
    portable.insert("README.md".into(), file("runtime/plugins/DISTRIBUTION.md")?);
    let mut claude_manifest = serde_json::Map::new();
    for key in ["name", "version", "description", "author", "license", "repository"] {
        claude_manifest.insert(key.into(), field(key)?);
    }
    claude_manifest.insert("mcpServers".into(), "./.mcp.json".into());
    claude_manifest.insert("skills".into(), "./skills/".into());
    let mut claude = shared.clone();
    claude.extend(portable.iter().filter(|(n, _)| n.starts_with("skills/") || n.starts_with("assets/")).map(|(n, d)| (n.clone(), d.clone())));
    claude.insert(".claude-plugin/plugin.json".into(), json_bytes(&claude_manifest.into()));
    claude.insert(".mcp.json".into(), json_bytes(&json!({"mcpServers": {"palette": {"command": "palette-mcp"}}})));

    let mut chatgpt = shared;
    chatgpt.extend(portable);
    let chatgpt: Files = chatgpt.into_iter().map(|(n, d)| (format!("palette/{n}"), d)).collect();

    Ok(vec![("Palette-Claude.mcpb", archive::zip(&desktop)), ("Palette-Claude.zip", archive::zip(&claude)), ("Palette-ChatGPT.zip", archive::zip(&chatgpt))])
}

pub fn main(args: &[String]) -> Result<(), Fail> {
    let mut flags = crate::flags(args, &["--revision", "--output-dir"]).map_err(Fail::Usage)?;
    let output_dir = PathBuf::from(flags.remove("--output-dir").ok_or_else(|| Fail::Usage("--output-dir is required".into()))?);
    let wanted = flags.remove("--revision").unwrap_or_else(|| "HEAD".into());
    let root = crate::root();
    let revision = String::from_utf8_lossy(&git(&root, &["rev-parse", &format!("{wanted}^{{commit}}")])?).trim().to_string();
    let source = archive::untar(&git(&root, &["archive", "--format=tar", &revision])?)?;
    let outputs = build(&source, &revision)?;
    std::fs::create_dir_all(&output_dir).map_err(|e| format!("{}: {e}", output_dir.display()))?;
    let mut sums = String::new();
    for (name, data) in &outputs {
        let entries = archive::unzip(data).map_err(|e| format!("{name}: {e}"))?;
        validate(&entries.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), name.ends_with(".mcpb")).map_err(|e| format!("{name}: {e}"))?;
        let path = output_dir.join(name);
        std::fs::write(&path, data).map_err(|e| format!("{}: {e}", path.display()))?;
        writeln!(sums, "{}  {name}", Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect::<String>()).unwrap();
    }
    let sums_path = output_dir.join("SHA256SUMS.txt");
    std::fs::write(&sums_path, sums).map_err(|e| format!("{}: {e}", sums_path.display()))?;
    let output_dir = std::fs::canonicalize(&output_dir).map_err(|e| format!("{}: {e}", output_dir.display()))?;
    let artifacts: Vec<&str> = outputs.iter().map(|(n, _)| *n).collect();
    let report = json!({"revision": revision, "artifacts": artifacts, "output_dir": output_dir.to_string_lossy()});
    print!("{}", String::from_utf8(json_bytes(&report)).expect("ASCII"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_refuses_ambiguous_packages() {
        let ok: [(&[&str], bool); 5] = [
            (&["manifest.json", "server/index.cjs", "x"], true),
            (&["plugin.json", "a", "b/c"], false),
            (&[".claude-plugin/plugin.json", "skills/s.md", "LICENSE"], false),
            (&["palette/plugin.json", "palette/templates/palette-plugin.json"], false),
            (&["palette/.codex/plugin.json", "palette/x"], false),
        ];
        for (names, desktop) in ok {
            assert_eq!(validate(names, desktop), Ok(()), "{names:?}");
        }
        let bad: [(&[&str], bool, &str); 6] = [
            (&["manifest.json", "server/index.cjs", "x.zip"], true, "nested ZIPs"),
            (&["manifest.json"], true, "root manifest and bridge"),
            (&["a"], false, "single plugin"),
            (&["plugin.json", "b/plugin.json"], false, "single plugin"),
            (&["palette/plugin.json", "README.md"], false, "sibling files"),
            (&["a/b/plugin.json", "a/b/x"], false, "sibling files"),
        ];
        for (names, desktop, why) in bad {
            assert!(validate(names, desktop).is_err_and(|e| e.contains(why)), "{names:?}");
        }
    }

    #[test]
    fn json_bytes_matches_python_json_dumps() {
        let v = json!({"z": "caf\u{e9} \u{1f600}", "a": [], "m": {"k": [1, true]}});
        let want = "{\n  \"z\": \"caf\\u00e9 \\ud83d\\ude00\",\n  \"a\": [],\n  \"m\": {\n    \"k\": [\n      1,\n      true\n    ]\n  }\n}\n";
        assert_eq!(String::from_utf8(json_bytes(&v)).unwrap(), want);
    }
}
