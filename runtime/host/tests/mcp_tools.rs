//! The tool schemas `palette-host mcp` and `palette-host mcp child` list. A
//! repair config gives the child its repair tools but leaves the router's four
//! schemas unchanged.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::io::Write;

fn list_tools(args: &[&str], env: &[(String, String)]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_palette-host"))
        .arg("mcp").args(args).env_clear().envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let replies: Vec<Value> = String::from_utf8(out.stdout).unwrap().lines()
        .filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replies.len(), 1, "{replies:?}");
    replies[0]["result"]["tools"].as_array().unwrap().clone()
}

fn names(tools: &[Value]) -> Vec<&str> {
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    names
}

#[test]
fn a_repair_config_reaches_the_child_and_leaves_the_router_schemas_alone() {
    let tmp = fs::canonicalize(std::env::temp_dir()).unwrap();
    let root = tmp.join(format!("palette-mcp-tools-it-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for dir in ["workspace", "state", "router-state", "capabilities", "fixture", "home"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::write(root.join("fixture/main.jl"), "fixture() = 1\n").unwrap();
    let config = root.join("repair-config.json");
    fs::write(&config, json!({
        "test_recipes": {"fixture": {"id": "fixed-fixture-v1", "argv": ["/usr/bin/true"], "read_roots": []}},
        "auto_apply_prefixes": {"fixture": [["adapters", "main.jl"]]},
    }).to_string()).unwrap();
    fs::write(root.join("ceiling.json"), "{}").unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).unwrap();
    let at = |p: &str| root.join(p).to_string_lossy().into_owned();
    let env: Vec<(String, String)> = [
        ("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())),
        ("HOME", at("home")), ("LANG", "C.UTF-8".into()),
        ("PALETTE_REPO", repo.to_string_lossy().into_owned()),
        ("PALETTE_HOST", env!("CARGO_BIN_EXE_palette-host").into()),
        ("OPERATOR_WORKSPACE", at("workspace")), ("PALETTE_STATE_DIR", at("state")),
        ("PALETTE_WORKSPACE_STATE_ROOT", at("router-state")), ("PALETTE_RUNTIME_ROOT", at("capabilities")),
        ("PALETTE_PATCH_ROOTS", json!({"fixture": at("fixture")}).to_string()),
        ("PALETTE_CAPABILITY_CEILING", at("ceiling.json")),
    ].into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let mut with_config = env.clone();
    with_config.push(("PALETTE_REPAIR_CONFIG".into(), config.to_string_lossy().into_owned()));

    assert_eq!(names(&list_tools(&["child"], &with_config)), ["palette", "palette_control"]);
    let before = list_tools(&[], &env);
    let after = list_tools(&[], &with_config);
    assert_eq!(names(&after), ["palette", "palette_control", "palette_patch", "palette_workspace"]);
    assert_eq!(after, before);
    fs::remove_dir_all(&root).unwrap();
}
