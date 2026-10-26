//! `palette-host install-plugin`: wires a local Palette plugin directly to the
//! daemon through `palette-host mcp connect`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::{provisioning, util};

const USAGE: &str = "\
usage: palette-host install-plugin --plugin-dir DIR --workspace-dir DIR [--repo-dir DIR]
                                   [--format codex|portable] [--bin-dir DIR]

Wire a local Palette plugin directly to the repository-owned MCP consumer.

  --bin-dir DIR   also write a palette-mcp executable to DIR, for PATH
";
const LAUNCHER_HEADER: &str = "#!/bin/sh\n# Palette-managed launcher\n";
const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";

/// Keep generated plugins on the same instructions as their installed runtime.
pub(crate) fn copy_instructions(repo: &Path, plugin: &Path) -> Result<(), String> {
    let instructions = repo.join("runtime/plugins/palette");
    for relative in ["skills/palette/SKILL.md", "skills/palette-setup/SKILL.md", "hooks/hooks.json", "hooks/session-start.sh"] {
        let source = instructions.join(relative);
        let target = plugin.join(relative);
        if util::resolve(&source) == util::resolve(&target) {
            continue;
        }
        let text = fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
        let parent = target.parent().expect("instruction files have a parent");
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        util::write_atomic(&target, &text, if relative.ends_with(".sh") { 0o755 } else { 0o644 })?;
    }
    Ok(())
}

fn read_json(p: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
}

fn read_or(p: &Path, default: impl FnOnce() -> Result<Value, String>) -> Result<Value, String> {
    if p.exists() { read_json(p) } else { default() }
}

fn write_json(p: &Path, v: &Value) -> Result<(), String> {
    util::write_atomic(p, &(serde_json::to_string_pretty(v).expect("JSON values serialize") + "\n"), 0o600)
}

/// The named object member, created empty when absent.
fn object<'a>(v: &'a mut Value, key: &str, what: &str) -> Result<&'a mut Map<String, Value>, String> {
    v.as_object_mut().ok_or_else(|| format!("{what} must be a JSON object"))?
        .entry(key).or_insert_with(|| json!({}))
        .as_object_mut().ok_or_else(|| format!("{what}: {key} must be an object"))
}

fn only_palette(servers: &Map<String, Value>) -> bool {
    servers.keys().all(|k| k == "palette")
}

/// The daemon socket the client env pins: $PALETTE_SOCKET, else the one in
/// the user's runtime directory, if there is one.
fn socket() -> Option<PathBuf> {
    let env = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
    env("PALETTE_SOCKET").map(|s| util::resolve(&util::expand_user(&s)))
        .or_else(|| env("XDG_RUNTIME_DIR").map(|r| util::resolve(&Path::new(&r).join("palette/daemon.sock"))))
}

pub fn main(argv: &[String]) -> Result<i32, String> {
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(0);
    }
    let mut f: HashMap<String, String> = HashMap::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let (k, v) = match a.strip_prefix("--").and_then(|k| k.split_once('=')) {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => {
                let k = a.strip_prefix("--").ok_or_else(|| format!("unexpected argument: {a}\n{USAGE}"))?;
                (k.to_string(), it.next().cloned().ok_or_else(|| format!("--{k} needs a value"))?)
            }
        };
        if !["plugin-dir", "workspace-dir", "repo-dir", "format", "bin-dir"].contains(&k.as_str()) {
            return Err(format!("unknown option --{k}\n{USAGE}"));
        }
        f.insert(k, v);
    }
    let get = |k: &str| f.get(k).cloned().ok_or_else(|| format!("--{k} is required\n{USAGE}"));
    let (plugin_arg, workspace_arg) = (get("plugin-dir")?, get("workspace-dir")?);
    let format = f.get("format").map_or("codex", String::as_str);
    if !["codex", "portable"].contains(&format) {
        return Err(format!("--format must be codex or portable, not {format}"));
    }
    let repo = util::resolve(Path::new(&f.get("repo-dir").cloned().map_or_else(crate::cli::default_repo_dir, Ok)?));
    let plugin = util::resolve(Path::new(&plugin_arg));
    let bin_dir = f.get("bin-dir").map(PathBuf::from);
    let mut protected = vec![repo.clone(), plugin.clone()];
    protected.extend(bin_dir.clone());
    provisioning::require_disjoint(&[&workspace_arg], &protected, &[] as &[&Path])?;
    let host = repo.join("runtime/host/target/release/palette-host");
    for path in [repo.join("Project.toml"), repo.join("Manifest.toml"), host.clone()] {
        if !path.is_file() {
            return Err(format!("missing {}; instantiate the local project and build the Rust host before installing", path.display()));
        }
    }
    if !util::executable(&host) {
        return Err(format!("host is not executable: {}", host.display()));
    }

    let mut manifest_path = plugin.join(".codex-plugin/plugin.json");
    let mut manifest = read_or(&manifest_path, || Ok(json!({
        "name": "palette", "version": provisioning::palette_version(&repo)?,
        "description": "Persistent Julia Palette with offline preparation and existing state revival.",
        "interface": {"displayName": "Palette", "shortDescription": "Persistent local Julia workbench"},
    })))?;
    if manifest.get("name").and_then(Value::as_str) != Some("palette") || (format == "codex" && plugin.join("plugin.json").exists()) {
        return Err("target must be a Palette plugin using the Codex compatibility manifest".into());
    }
    let mut config_path = plugin.join(".mcp.json");
    let mut config = read_or(&config_path, || Ok(json!({"mcpServers": {}})))?;
    let servers = object(&mut config, "mcpServers", &config_path.display().to_string())?;
    if !only_palette(servers) {
        return Err("target contains other servers; use a dedicated Palette plugin".into());
    }
    let mut entry = servers.get("palette").cloned().unwrap_or_else(|| json!({}));
    let workspace = util::resolve(Path::new(&workspace_arg));
    let socket = socket();
    let env = object(&mut entry, "env", "the palette server entry")?;
    for (k, v) in [
        ("PALETTE_REPO", repo.display().to_string()),
        ("PALETTE_HOST", host.display().to_string()),
        ("OPERATOR_WORKSPACE", workspace.display().to_string()),
        ("PALETTE_SERVICE", "palette.service".into()),
    ] {
        env.insert(k.into(), json!(v));
    }
    if let Some(s) = &socket {
        env.insert("PALETTE_SOCKET".into(), json!(s.display().to_string()));
    }
    let e = entry.as_object_mut().expect("an object");
    e.insert("command".into(), json!(host.display().to_string()));
    e.insert("args".into(), json!(["mcp", "connect"]));
    e.entry("startup_timeout_sec").or_insert(json!(200));
    e.entry("tool_timeout_sec").or_insert(json!(210));
    servers.insert("palette".into(), entry.clone());
    manifest.as_object_mut().ok_or("plugin.json must be a JSON object")?.insert("mcpServers".into(), json!("./.mcp.json"));

    if format == "portable" {
        manifest_path = plugin.join("plugin.json");
        manifest = read_or(&manifest_path, || read_json(&repo.join("runtime/plugins/palette/plugin.json")))?;
        if manifest.get("name").and_then(Value::as_str) != Some("palette") {
            return Err("target must be a Palette portable plugin".into());
        }
        config_path = plugin.join("mcp.json");
        let mut existing = read_or(&config_path, || Ok(json!({})))?;
        let servers = object(&mut existing, "mcpServers", &config_path.display().to_string())?;
        if !only_palette(servers) {
            return Err("target contains other portable servers".into());
        }
        let prior_env = servers.get("palette").and_then(|p| p.get("env")).and_then(Value::as_object).cloned().unwrap_or_default();
        let e = entry.as_object_mut().expect("an object");
        let mut env = prior_env;
        env.extend(e.get("env").and_then(Value::as_object).cloned().unwrap_or_default());
        e.insert("env".into(), Value::Object(env));
        e.remove("startup_timeout_sec");
        e.remove("tool_timeout_sec");
        let mut server = Map::from_iter([("type".to_string(), json!("stdio"))]);
        server.extend(e.clone());
        servers.insert("palette".into(), Value::Object(server));
        existing.as_object_mut().expect("an object").insert("$schema".into(), json!(MCP_SCHEMA));
        config = existing;
        let assets = plugin.join("assets");
        fs::create_dir_all(&assets).map_err(|e| format!("{}: {e}", assets.display()))?;
        let icon = repo.join("runtime/plugins/palette/assets/palette.svg");
        if util::resolve(&icon) != util::resolve(&assets.join("palette.svg")) {
            fs::copy(&icon, assets.join("palette.svg")).map_err(|e| format!("{}: {e}", icon.display()))?;
        }
    }
    copy_instructions(&repo, &plugin)?;
    write_json(&config_path, &config)?;
    write_json(&manifest_path, &manifest)?;

    if let Some(dir) = &bin_dir {
        let launcher = util::resolve(dir).join("palette-mcp");
        if launcher.exists() && !fs::read_to_string(&launcher).is_ok_and(|t| t.starts_with(LAUNCHER_HEADER)) {
            return Err("refusing to overwrite an unmanaged palette-mcp executable".into());
        }
        let script = format!("{LAUNCHER_HEADER}exec {} mcp connect \"$@\"\n", util::shquote(&host.display().to_string()));
        util::write_atomic(&launcher, &script, 0o755)?;
    }
    println!("{}", json!({
        "plugin": plugin.display().to_string(), "repo": repo.display().to_string(),
        "workspace": workspace.display().to_string(),
        "preparation": "checked on first operator start", "state": "existing state directory wired by operator",
        "socket": socket.map_or_else(|| "${XDG_RUNTIME_DIR}/palette/daemon.sock".to_string(), |s| s.display().to_string()),
        "service": "palette.service",
    }));
    Ok(0)
}
