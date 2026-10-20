//! `cargo xtask bundles` end to end on this checkout's HEAD: what each bundle
//! holds, that its checksums are right, and that a second run is identical.

#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ARTIFACTS: [&str; 3] = ["Palette-Claude.mcpb", "Palette-Claude.zip", "Palette-ChatGPT.zip"];

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().to_path_buf()
}

fn bundles(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask")).arg("bundles").args(args).output().unwrap()
}

fn git(args: &[&str]) -> Output {
    Command::new("git").args(args).current_dir(root()).output().unwrap()
}

fn entries(dir: &Path, name: &str) -> Vec<(String, Vec<u8>)> {
    archive::unzip(&fs::read(dir.join(name)).unwrap()).unwrap()
}

fn find<'a>(entries: &'a [(String, Vec<u8>)], name: &str) -> &'a [u8] {
    &entries.iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("no {name}")).1
}

#[test]
fn builds_the_same_three_bundles_from_a_revision_every_time() {
    let tmp = Dir(Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("bundles-{}", std::process::id())));
    let _ = fs::remove_dir_all(&tmp.0);
    let (a, b) = (tmp.0.join("a"), tmp.0.join("b"));
    let head = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout).unwrap().trim().to_string();

    let out = bundles(&["--output-dir", a.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["revision"], head.as_str());
    assert_eq!(report["artifacts"], serde_json::json!(ARTIFACTS));
    assert_eq!(report["output_dir"], fs::canonicalize(&a).unwrap().to_str().unwrap());

    let sums = Command::new("sha256sum").args(ARTIFACTS).current_dir(&a).output().unwrap();
    assert!(sums.status.success() && fs::read(a.join("SHA256SUMS.txt")).unwrap() == sums.stdout, "SHA256SUMS.txt is what sha256sum writes");

    let out = bundles(&[&format!("--output-dir={}", b.display()), "--revision", &head]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    for name in ARTIFACTS.iter().chain(&["SHA256SUMS.txt"]) {
        assert!(fs::read(a.join(name)).unwrap() == fs::read(b.join(name)).unwrap(), "{name} differs between runs");
    }

    let desktop = entries(&a, "Palette-Claude.mcpb");
    let claude = entries(&a, "Palette-Claude.zip");
    let chatgpt = entries(&a, "Palette-ChatGPT.zip");
    for (n, _) in &desktop {
        assert!(!n.ends_with(".zip"), "{n}");
    }
    find(&desktop, "manifest.json");
    find(&desktop, "server/index.cjs");
    assert!(chatgpt.iter().all(|(n, _)| n.starts_with("palette/")), "the ChatGPT plugin has one root");
    find(&chatgpt, "palette/plugin.json");

    let manifest: serde_json::Map<String, Value> = serde_json::from_slice(find(&claude, ".claude-plugin/plugin.json")).unwrap();
    assert_eq!(manifest.keys().collect::<Vec<_>>(), ["name", "version", "description", "author", "license", "repository", "mcpServers", "skills"]);
    assert_eq!(find(&claude, ".mcp.json"), b"{\n  \"mcpServers\": {\n    \"palette\": {\n      \"command\": \"palette-mcp\"\n    }\n  }\n}\n");
    let source: Value = serde_json::from_slice(find(&claude, "SOURCE.json")).unwrap();
    assert_eq!(source["revision"], head.as_str());
    assert_eq!(source["plugin_version"], manifest["version"]);

    for excluded in [".agents/plugins/marketplace.json", ".claude-plugin/marketplace.json", "runtime/plugins/claude/manifest.json", "runtime/plugins/palette/plugin.json"] {
        assert!(git(&["cat-file", "-e", &format!("{head}:{excluded}")]).status.success(), "{excluded} is not in HEAD");
        for bundle in [&desktop, &claude] {
            assert!(bundle.iter().all(|(n, _)| n != &format!("sdk/{excluded}")), "sdk/{excluded} was bundled");
        }
    }
    let brain_blast = String::from_utf8(find(&claude, "BRAIN_BLAST.md").to_vec()).unwrap();
    assert!(brain_blast.contains("(sdk/runtime/docs/operator.md)") && !brain_blast.contains("(runtime/docs/"));
    let ps1 = find(&claude, "sdk/runtime/scripts/install.ps1");
    assert!(ps1.windows(2).filter(|w| w == b"\r\n").count() == ps1.iter().filter(|&&c| c == b'\n').count(), "install.ps1 keeps its CRLF line endings");
}

#[test]
fn refuses_a_bad_revision_or_a_missing_output_dir() {
    let tmp = Dir(Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("bundles-bad-{}", std::process::id())));
    let out = bundles(&["--output-dir", tmp.0.to_str().unwrap(), "--revision", "no-such-revision"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("git rev-parse no-such-revision^{commit} failed"));
    assert!(!tmp.0.exists(), "nothing is written for a bad revision");
    for args in [&[][..], &["--revision", "HEAD"], &["--output-dir"], &["--output-dir", "x", "--zip"]] {
        assert_eq!(bundles(args).status.code(), Some(2), "{args:?}");
    }
}
