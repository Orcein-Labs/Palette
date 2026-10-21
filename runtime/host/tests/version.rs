//! Every part of Palette reports one version: the Julia package, the host and
//! the plugin manifests the release bundles take theirs from.

use serde_json::Value;
use std::fs;
use std::path::Path;

#[test]
fn every_part_reports_one_version() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).unwrap();
    let project = fs::read_to_string(repo.join("Project.toml")).unwrap();
    let version = project.lines().find_map(|l| l.strip_prefix("version = ")).unwrap().trim_matches('"');
    assert_eq!(env!("CARGO_PKG_VERSION"), version);
    for manifest in ["palette/plugin.json", "claude/manifest.json"] {
        let path = repo.join("runtime/plugins").join(manifest);
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["version"], version, "{}", path.display());
    }
}
