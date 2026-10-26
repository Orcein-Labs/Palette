//! palette-host: the host side of Palette -- the sandbox launcher, the
//! persistent session bridge and the capability broker -- as one binary.
//!
//!   palette-host session    --project-dir P [--ceiling C] [--workspace-dir W] [--state-dir S] ...
//!   palette-host run-worker --workspace W --project P --repo R --script CODE [--broker-socket-dir D] [--timeout S]
//!   palette-host broker     --socket PATH --ceiling C --receipts FILE [--session-id ID] [--project P --repo R --depot D]
//!   palette-host subset     REQUESTED_JSON PARENT_JSON
//!   palette-host session-depot   (prints a session depot's path, holds it until stdin closes, then removes it)
//!   palette-host prewarm    --project-dir P [--repo-dir R]
//!   palette-host prepare    --project-dir P [--repo-dir R] [--state-dir S] [--host-bin H] [--status] [--force] [--timeout-s T]
//!   palette-host teamlab-news [--agent A]
//!   palette-host setup      [--repo-dir R] [--no-register] [--replace] [--no-check] ... (see setup --help)
//!   palette-host service    install|start|stop|restart|status [--unit U] ... (see service --help)
//!   palette-host serve      init|inspect|prepare|launch|client --profile P ... (see serve --help)
//!   palette-host patch      --storage S review|approve ID [--digest D] | apply-helper
//!   palette-host toolchain config --name N --executable E --cwd C ... | run ...  (one fixed operator toolchain)
//!   palette-host mcp        [--socket PATH] | start | child   (the MCP router, its client entry, or one workspace's adapter)

mod broker;
mod cli;
mod install_plugin;
mod mcp;
mod provisioning;
mod receipt_archive;
mod sandbox;
mod serve;
mod service;
mod session;
mod setup;
mod teamlab;
mod toolchain;
mod util;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn flags(argv: &[String], booleans: &[&str]) -> Result<HashMap<String, String>, String> {
    let mut m = HashMap::new();
    let mut i = 0;
    while i < argv.len() {
        let k = argv[i].strip_prefix("--").ok_or_else(|| format!("unexpected argument: {}", argv[i]))?;
        if booleans.contains(&k) {
            m.insert(k.to_string(), "true".into());
        } else {
            i += 1;
            m.insert(k.to_string(), argv.get(i).cloned().ok_or_else(|| format!("--{k} needs a value"))?);
        }
        i += 1;
    }
    Ok(m)
}

/// A ceiling given as JSON text or as a path to a JSON file, and the file it
/// came from, which the broker protects.
fn ceiling_arg(s: &str) -> Result<(Value, Option<PathBuf>), String> {
    let file = sandbox::ceiling_file(s);
    let text = match &file { Some(path) => std::fs::read_to_string(path).map_err(|e| e.to_string())?, None => s.to_string() };
    Ok((serde_json::from_str(&text).map_err(|e| format!("ceiling: {e}"))?, file))
}

fn run_worker(argv: &[String]) -> Result<i32, String> {
    let f = flags(argv, &["network"])?;
    let get = |k: &str| f.get(k).cloned().ok_or_else(|| format!("--{k} is required"));
    let r = sandbox::run_worker(&sandbox::RunWorker {
        workspace_dir: &get("workspace")?,
        project_dir: &get("project")?,
        repo_dir: &get("repo")?,
        script: &get("script")?,
        broker_socket_dir: f.get("broker-socket-dir").map(String::as_str),
        network_enabled: f.contains_key("network"),
        depot_clone_dir: f.get("depot-clone-dir").map(String::as_str),
        timeout: Duration::from_secs_f64(f.get("timeout").map_or(Ok(60.0), |t| t.parse::<f64>()).map_err(|e| e.to_string())?),
    })?;
    println!("{}", json!({"returncode": r.returncode, "stdout": r.stdout, "stderr": r.stderr, "timed_out": r.timed_out}));
    Ok(0)
}

/// A standalone broker, serving until its stdin closes; prints one READY line.
fn run_broker(argv: &[String]) -> Result<i32, String> {
    let f = flags(argv, &[])?;
    let get = |k: &str| f.get(k).cloned().ok_or_else(|| format!("--{k} is required"));
    let (ceiling, ceiling_file) = ceiling_arg(&get("ceiling")?)?;
    let receipt_path = PathBuf::from(get("receipts")?);
    let mut protected = vec![receipt_path.clone(), PathBuf::from(get("socket")?)];
    for key in ["project", "repo"] { if let Some(path) = f.get(key) { protected.push(PathBuf::from(path)); } }
    let package_store = sandbox::package_store_paths(&protected, Some(&ceiling))?;
    let b = Arc::new(broker::Broker::new(broker::BrokerConfig {
        ceiling,
        receipt_log_path: receipt_path,
        session_id: f.get("session-id").cloned().unwrap_or_else(util::uuid4),
        package_depot_dir: Some(package_store.depot.to_string_lossy().into_owned()),
        package_environment_dir: Some(package_store.environment.to_string_lossy().into_owned()),
        project_dir: f.get("project").cloned(),
        repo_dir: f.get("repo").cloned(),
        trusted_files: ceiling_file.into_iter().collect(),
        child_timeout: Duration::from_secs_f64(f.get("child-timeout").map_or(Ok(90.0), |t| t.parse::<f64>()).map_err(|e| e.to_string())?),
    })?);
    let server = broker::serve(std::path::Path::new(&get("socket")?), b)?;
    println!("{}", json!({"kind": "READY"}));
    let mut sink = String::new();
    while std::io::stdin().read_line(&mut sink).map(|n| n > 0).unwrap_or(false) {
        sink.clear();
    }
    server.shutdown();
    Ok(0)
}

/// A session depot for callers outside this process (the Python suites): it
/// lives while this process does, since its owner record names this process
/// and an overlay holds its snapshot here.
fn session_depot(argv: &[String]) -> Result<i32, String> {
    if !argv.is_empty() { return Err("session-depot takes no arguments".into()) }
    let depot = sandbox::create_session_depot(None)?;
    println!("{}", json!({"kind": "READY", "depot": depot}));
    let mut sink = String::new();
    while std::io::stdin().read_line(&mut sink).map(|n| n > 0).unwrap_or(false) {
        sink.clear();
    }
    sandbox::remove_session_depot(&depot);
    Ok(0)
}

fn subset(argv: &[String]) -> Result<i32, String> {
    let [req, parent] = argv else { return Err("subset takes REQUESTED_JSON PARENT_JSON".into()) };
    let parse = |s: &str| serde_json::from_str::<Value>(s).map_err(|e| e.to_string());
    let (ok, reason) = broker::ceiling_is_subset(&parse(req)?, &parse(parent)?);
    println!("{}", json!({"ok": ok, "reason": reason}));
    Ok(0)
}

/// Precompile every stdlib and project dependency into the selected depot. A
/// sandbox preparation publishes a snapshot for session clones; operator
/// preparation uses Julia's normal host environment and keeps the live depot.
pub(crate) const PREWARM: &str = include_str!("../../security/prewarm_workload.jl");

fn prewarm(argv: &[String]) -> Result<i32, String> {
    let f = flags(argv, &[])?;
    let project = std::fs::canonicalize(f.get("project-dir").ok_or("--project-dir is required")?).map_err(|e| e.to_string())?;
    let repo = std::fs::canonicalize(f.get("repo-dir").cloned().map_or_else(cli::default_repo_dir, Ok)?).map_err(|e| e.to_string())?;
    let julia = sandbox::resolve_julia()?;
    let workspace = util::mkdtemp("palette-prewarm-").map_err(|e| e.to_string())?;
    let depot = sandbox::default_depot();
    let packages = sandbox::package_store_paths(&[project.clone(), repo.clone(), workspace.clone()], None)?;
    // No depot clone: the real depot is bound writable at its own path.
    let mode = std::env::var("PALETTE_MODE").unwrap_or_else(|_| "operator".into());
    if !matches!(mode.as_str(), "operator" | "sandbox") {
        let _ = std::fs::remove_dir_all(&workspace);
        return Err("PALETTE_MODE must be operator or sandbox".into());
    }
    let status = if mode == "sandbox" {
        let mut argv = sandbox::build_bwrap_argv(&sandbox::SandboxSpec {
            workspace_dir: &workspace.to_string_lossy(),
            broker_socket_dir: None,
            project_dir: &project.to_string_lossy(),
            repo_dir: &repo.to_string_lossy(),
            julia_bin: &julia,
            julia_depot: &depot,
            network_enabled: false,
            depot_clone_dir: None,
            package_store_root: Some(packages.root.to_str().ok_or("package store path is not UTF-8")?),
            package_depot_dir: Some(packages.depot.to_str().ok_or("package depot path is not UTF-8")?),
            package_environment_dir: Some(packages.environment.to_str().ok_or("package environment path is not UTF-8")?),
            state_dir: None,
        })?;
        argv.extend(["--".into(), julia.clone(), "--startup-file=no".into(), "-e".into(), PREWARM.into()]);
        std::process::Command::new(&argv[0]).args(&argv[1..])
            .stdin(std::process::Stdio::null()).status()
    } else {
        // Operator mode keeps the same Julia workload and persistent cache
        // destinations, while running it directly under the host environment.
        let repo_text = repo.to_string_lossy();
        let project_text = project.to_string_lossy();
        let package_depot = packages.depot.to_string_lossy();
        let package_environment = packages.environment.to_string_lossy();
        let mut julia_env = sandbox::operator_julia_env(
            &repo_text,
            &project_text,
            Some(&package_depot),
            Some(&package_environment),
        );
        julia_env.insert("PALETTE_PACKAGE_DEPOT".into(), packages.root.to_string_lossy().into_owned());
        std::process::Command::new(&julia)
            .arg("--startup-file=no")
            .arg("-e").arg(PREWARM)
            .current_dir(&workspace)
            .envs(&julia_env)
            .stdin(std::process::Stdio::null()).status()
    };
    let _ = std::fs::remove_dir_all(&workspace);
    let code = status.map_err(|e| e.to_string())?.code().unwrap_or(1);
    // Sandbox sessions clone the immutable snapshot instead of the live depot.
    // Operator sessions use their normal live depot and do not publish one.
    if code == 0 && mode == "sandbox" {
        if let Err(e) = sandbox::publish_depot_snapshot(std::path::Path::new(&depot)) {
            eprintln!("palette-host: the depot snapshot was not published: {e}");
        }
    }
    Ok(code)
}

/// One line of TeamLab news for a session-start hook, or nothing. It never
/// fails the hook: any problem prints nothing and exits 0.
fn teamlab_news(argv: &[String]) -> Result<i32, String> {
    let agent = flags(argv, &[]).ok().and_then(|f| f.get("agent").cloned())
        .filter(|a| teamlab::valid_agent(a)).unwrap_or_else(teamlab::default_agent);
    if let Some(line) = teamlab::user_dir().and_then(|dir| teamlab::news_line(&dir, &agent)) {
        println!("{line}");
    }
    Ok(0)
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // Installed as a `pal` link, the binary is the shell shim.
    if argv.first().and_then(|a| std::path::Path::new(a).file_name()).is_some_and(|n| n == "pal") {
        std::process::exit(mcp::pal::main(argv.get(1..).unwrap_or(&[])));
    }
    let rest = argv.get(2..).unwrap_or(&[]);
    let code = match argv.get(1).map(String::as_str) {
        Some("session") => Ok(cli::main(rest)),
        Some("run-worker") => run_worker(rest),
        Some("broker") => run_broker(rest),
        Some("subset") => subset(rest),
        Some("session-depot") => session_depot(rest),
        Some("prewarm") => prewarm(rest),
        Some("prepare") => flags(rest, &["status", "force"]).and_then(|f| mcp::preparation::main(&f)),
        Some("teamlab-news") => teamlab_news(rest),
        Some("reload") if rest.is_empty() => Ok(mcp::pal::main(&["--reload".into()])),
        Some("setup") => setup::main(rest),
        Some("install-plugin") => install_plugin::main(rest),
        Some("service") => service::main(rest),
        Some("serve") => serve::main(rest),
        Some("patch") => mcp::patch::main(rest),
        Some("control") => mcp::control::main(rest),
        Some("toolchain") => toolchain::main(rest),
        Some("pal") => Ok(mcp::pal::main(rest)),
        Some("mcp") => match rest.first().map(String::as_str) {
            Some("child") => Ok(mcp::child::main()),
            Some("connect") if rest.len() == 1 => Ok(mcp::client::main()),
            Some("start") if rest.len() == 1 => mcp::router::start(),
            _ => mcp::router::main(rest),
        },
        _ => Err("usage: palette-host session|run-worker|broker|subset|prewarm|prepare|teamlab-news|reload|setup|install-plugin|service|serve|patch|control|toolchain|pal|mcp ...".into()),
    };
    std::process::exit(match code {
        Ok(c) => c,
        Err(e) => {
            eprintln!("palette-host: {e}");
            2
        }
    });
}
