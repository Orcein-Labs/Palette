//! `palette-host setup`: prepare Palette on Linux, register it with the local
//! clients, and check it works.

use crate::util::{shjoin, shquote};
use crate::{broker, cli, sandbox, util};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SYSTEM_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// The capabilities every installation deploys into its runtime root.
const BUNDLED_CAPABILITIES: &[&str] = &["teamlab"];
const CHECK_TIMEOUT: Duration = Duration::from_secs(600);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);
const NOTICE: &str = "Palette client integration\nCopyright (c) 2026 Matt C\nApache-2.0. Core runtime is separately licensed AGPL-3.0-only.\n";
const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";

const USAGE: &str = "\
usage: palette-host setup [options]

Prepare Palette on Linux, register it with local clients, and check it works.

  --repo-dir DIR           the Palette SDK (default: the checkout this binary runs from)
  --output-dir DIR         client files and the palette-mcp launcher (default: $PALETTE_HOME/client)
  --data-dir DIR           workspace, state and packages (default: $PALETTE_HOME/data)
  --depot-dir DIR          Julia depot (default: $PALETTE_HOME/depot)
  --runtime-root DIR       capability registry (PALETTE_RUNTIME_ROOT); TeamLab is deployed into it
                           (default: $PALETTE_HOME/capabilities)
  --ceiling FILE           capability ceiling (PALETTE_CAPABILITY_CEILING); created, or added to
                           (default: $PALETTE_HOME/config/ceiling.json)
  --host-commands FILE     host commands file (PALETTE_HOST_COMMANDS), used when it exists; empty for none
                           (default: ~/.config/palette/host-commands.json)
  --read-roots FILE        host directories the kernel may read (PALETTE_READ_ROOTS), one absolute path
                           a line, used when it exists; empty for none (default: ~/.config/palette/read-roots)
  --connect-only           use a runtime already built and prewarmed; do not prepare again
  --wsl-distribution NAME  generate Windows client configuration targeting this WSL distribution
  --no-register            do not add the server to local agent clients
  --replace                take over a client's palette server that runs something else
  --no-check               do not make test calls at the end

$PALETTE_HOME defaults to $XDG_DATA_HOME/palette, or ~/.local/share/palette.
";

const VALUE_FLAGS: &[&str] = &[
    "repo-dir", "output-dir", "data-dir", "depot-dir", "runtime-root", "ceiling", "host-commands", "read-roots", "wsl-distribution",
];
const BOOLEAN_FLAGS: &[&str] = &["help", "connect-only", "no-register", "replace", "no-check"];

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn palette_home() -> PathBuf {
    env_dir("PALETTE_HOME").unwrap_or_else(|| env_dir("XDG_DATA_HOME").unwrap_or_else(|| util::home().join(".local/share")).join("palette"))
}

fn config_dir() -> PathBuf {
    env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| util::home().join(".config")).join("palette")
}

fn text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn at(p: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{}: {e}", p.display())
}

fn read_json(p: &Path) -> Result<Value, String> {
    serde_json::from_str(&fs::read_to_string(p).map_err(at(p))?).map_err(|e| format!("{}: {e}", p.display()))
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).expect("JSON values serialize") + "\n"
}

/// Runs one preparation step, printing elapsed time while it is quiet. A
/// quiet step's standard output, a machine-readable record, is shown only if
/// the step fails.
fn step(title: &str, argv: &[String], env: &[(String, String)], quiet: bool) -> Result<(), String> {
    println!("==> {title}");
    let _ = std::io::stdout().flush();
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).envs(env.iter().map(|(k, v)| (k, v)));
    if quiet {
        cmd.stdout(Stdio::piped());
    }
    let mut child = cmd.spawn().map_err(|e| format!("{title}: starting {}: {e}", argv[0]))?;
    let reader = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = out.read_to_end(&mut b);
            b
        })
    });
    let started = Instant::now();
    let mut tick = 30;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(200));
        let seconds = started.elapsed().as_secs();
        if seconds >= tick {
            println!("    still working, {seconds} s");
            let _ = std::io::stdout().flush();
            tick += 30;
        }
    };
    let captured = reader.map(|r| r.join().unwrap_or_default()).unwrap_or_default();
    if !status.success() {
        print!("{}", String::from_utf8_lossy(&captured));
        use std::os::unix::process::ExitStatusExt;
        let code = status.code().unwrap_or_else(|| -status.signal().unwrap_or(0));
        return Err(format!("{title} failed (exit {code}); the output above says why"));
    }
    Ok(())
}

fn copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::copy(src, dst)?;
    if let Ok(modified) = fs::metadata(src).and_then(|m| m.modified()) {
        let _ = fs::File::options().write(true).open(dst).and_then(|f| f.set_modified(modified));
    }
    Ok(())
}

/// Copies a tree, keeping the symlinks in it as symlinks.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to)?;
        } else if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            copy_file(&from, &to)?;
        }
    }
    fs::set_permissions(dst, fs::metadata(src)?.permissions())
}

/// A copy of `src`, following it if it is a symlink; nothing when it is missing.
fn clone(src: &Path, dst: &Path) -> std::io::Result<()> {
    match fs::metadata(src) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => copy_tree(src, dst),
        Ok(_) => copy_file(src, dst),
    }
}

/// Copies of some paths, to put each back as it was, or remove it if it did
/// not exist, when a step fails.
struct Snapshot {
    dir: PathBuf,
    kept: Vec<(PathBuf, PathBuf)>,
}

impl Snapshot {
    fn take(paths: &[PathBuf]) -> Result<Snapshot, String> {
        let dir = util::mkdtemp("palette-setup-").map_err(|e| format!("creating a temporary directory: {e}"))?;
        let snapshot = Snapshot { kept: paths.iter().enumerate().map(|(i, p)| (p.clone(), dir.join(i.to_string()))).collect(), dir };
        for (path, copy) in &snapshot.kept {
            clone(path, copy).map_err(at(path))?;
        }
        Ok(snapshot)
    }

    fn restore(&self) {
        for (path, copy) in &self.kept {
            let real_dir = fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
            let _ = if real_dir { fs::remove_dir_all(path) } else { fs::remove_file(path) };
            if let Err(e) = clone(copy, path) {
                eprintln!("palette-host: could not restore {}: {e}", path.display());
            }
        }
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Copies the bundled capabilities into the runtime root, leaving any other
/// capability there as it is.
fn deploy_capabilities(repo: &Path, runtime_root: &Path) -> Result<(), String> {
    let pid = std::process::id();
    for name in BUNDLED_CAPABILITIES {
        let source = repo.join("runtime/integrations").join(name);
        let manifest_path = source.join("capability.json");
        let manifest = read_json(&manifest_path)?;
        let file = manifest.get("source").and_then(Value::as_str).ok_or_else(|| format!("{}: no source", manifest_path.display()))?;
        let staged = runtime_root.join(format!(".{name}.{pid}"));
        let _ = fs::remove_dir_all(&staged);
        fs::create_dir_all(&staged).map_err(at(&staged))?;
        for f in ["capability.json", file] {
            fs::copy(source.join(f), staged.join(f)).map_err(at(&source.join(f)))?;
        }
        let (target, retired) = (runtime_root.join(name), runtime_root.join(format!(".{name}.retired.{pid}")));
        if fs::symlink_metadata(&target).is_ok() {
            fs::rename(&target, &retired).map_err(at(&target))?;
        }
        fs::rename(&staged, &target).map_err(at(&target))?;
        let _ = fs::remove_dir_all(&retired);
    }
    Ok(())
}

const CATEGORIES: &[&str] = &[
    "external_fs_write", "fs_digest", "host_command", "host_request", "network_access", "package_management", "spawn_child_worker", "teamlab",
];

fn string_list<'a>(value: Option<&'a Value>, field: &str, absolute: bool) -> Result<Vec<&'a str>, String> {
    let items = value
        .and_then(Value::as_array)
        .and_then(|a| a.iter().map(|v| v.as_str().filter(|s| !s.is_empty())).collect::<Option<Vec<_>>>())
        .ok_or_else(|| format!("{field} must be a list of non-empty strings"))?;
    if absolute && items.iter().any(|s| !s.starts_with('/')) {
        return Err(format!("{field} entries must be absolute paths"));
    }
    Ok(items)
}

fn has_duplicates(items: &[&str]) -> bool {
    let mut seen = HashSet::new();
    !items.iter().all(|i| seen.insert(i))
}

fn julia_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn validate_host_commands(value: Option<&Value>) -> Result<(), String> {
    let commands = value.and_then(Value::as_object).ok_or("host_command.commands must be an object")?;
    for (name, spec) in commands {
        let spec = spec.as_object().filter(|_| !name.is_empty()).ok_or("host_command entries must map non-empty names to objects")?;
        let argv = string_list(spec.get("argv"), &format!("host_command.commands.{name}.argv"), false)?;
        if !argv.first().is_some_and(|a| a.starts_with('/')) {
            return Err(format!("host_command.commands.{name}.argv[0] must be an absolute executable path"));
        }
        if !spec.get("cwd").and_then(Value::as_str).is_some_and(|c| c.starts_with('/')) {
            return Err(format!("host_command.commands.{name}.cwd must be an absolute path"));
        }
        if let Some(timeout) = spec.get("timeout_s") {
            if !timeout.as_f64().is_some_and(|t| t > 0.0 && t <= 14400.0) {
                return Err(format!("host_command.commands.{name}.timeout_s must be in (0, 14400]"));
            }
        }
        if spec.get("extra_args").is_some_and(|v| !v.is_boolean()) {
            return Err(format!("host_command.commands.{name}.extra_args must be boolean"));
        }
    }
    Ok(())
}

/// The operator ceiling's checks, as the Python host's `validate_ceiling`
/// makes them.
fn validate_ceiling(value: &Value) -> Result<(), String> {
    let ceiling = value.as_object().ok_or("capability ceiling must be a JSON object")?;
    let mut unknown: Vec<&str> = ceiling.keys().map(String::as_str).filter(|k| !CATEGORIES.contains(k)).collect();
    if !unknown.is_empty() {
        unknown.sort();
        return Err(format!("unknown capability categories: {}", broker::ql(&unknown)));
    }
    for (category, grant) in ceiling {
        let grant = grant.as_object().ok_or_else(|| format!("{category} grant must be an object"))?;
        match category.as_str() {
            "package_management" => {
                let packages = string_list(grant.get("allowed_packages"), "package_management.allowed_packages", false)?;
                let invalid: Vec<&str> = packages.iter().copied().filter(|p| !julia_identifier(p)).collect();
                if !invalid.is_empty() {
                    return Err(format!("package_management.allowed_packages has invalid Julia identifiers: {}", broker::ql(&invalid)));
                }
                if has_duplicates(&packages) {
                    return Err("package_management.allowed_packages must not contain duplicates".into());
                }
                if grant.get("offline").is_some_and(|v| !v.is_boolean()) {
                    return Err("package_management.offline must be boolean when provided".into());
                }
            }
            "host_request" => {
                if has_duplicates(&string_list(grant.get("allowed_types"), "host_request.allowed_types", false)?) {
                    return Err("host_request.allowed_types must not contain duplicates".into());
                }
            }
            "external_fs_write" => {
                string_list(grant.get("allowed_dirs"), "external_fs_write.allowed_dirs", true)?;
            }
            "fs_digest" => {
                string_list(grant.get("allowed_paths"), "fs_digest.allowed_paths", true)?;
            }
            "network_access" => {
                if !grant.get("allowed").is_some_and(Value::is_boolean) {
                    return Err("network_access.allowed must be boolean".into());
                }
                if let Some(hosts) = grant.get("allowed_hosts").filter(|h| !h.is_null()) {
                    string_list(Some(hosts), "network_access.allowed_hosts", false)?;
                }
            }
            "host_command" => validate_host_commands(grant.get("commands"))?,
            "spawn_child_worker" if !grant.is_empty() => return Err("spawn_child_worker grant must be an empty object".into()),
            "teamlab" if grant.len() != 1 || !grant.get("dir").and_then(Value::as_str).is_some_and(|d| d.starts_with('/')) => {
                return Err("teamlab grant must be {\"dir\": <absolute path>}".into());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Writes the capability ceiling, or adds to the operator's own: a kernel
/// reaches the runtime's capabilities (TeamLab among them) through the
/// palette.runtime host request. Nothing else in an existing file changes.
fn merge_ceiling(path: &Path) -> Result<(), String> {
    let invalid = |e: String| format!("The capability ceiling {} is not valid ({e}); fix or move it and run setup again", path.display());
    let mut ceiling = if path.exists() {
        let contents = fs::read_to_string(path).map_err(|e| invalid(e.to_string()))?;
        serde_json::from_str(&contents).map_err(|e| invalid(e.to_string()))?
    } else {
        json!({})
    };
    validate_ceiling(&ceiling).map_err(invalid)?;
    let grant = ceiling.as_object_mut().expect("validated").entry("host_request").or_insert_with(|| json!({"allowed_types": []}));
    let types = grant.get_mut("allowed_types").and_then(Value::as_array_mut).expect("validated");
    if types.iter().any(|t| t == "palette.runtime") {
        return Ok(());
    }
    types.push(json!("palette.runtime"));
    util::write_atomic(path, &pretty(&ceiling), 0o600)
}

/// One agent client's CLI: how to read the command its server named palette
/// runs (None when it has none, "" when the reply is unreadable), how to add
/// one and how to remove one.
struct Client {
    name: &'static str,
    get: &'static [&'static str],
    parse: fn(&str) -> Option<String>,
    add: &'static [&'static str],
    remove: &'static [&'static str],
}

fn claude_command(stdout: &str) -> Option<String> {
    Some(stdout.lines().find(|l| l.trim().starts_with("Command:")).and_then(|l| l.split_once(':')).map_or("", |(_, c)| c.trim()).to_string())
}

fn codex_command(stdout: &str) -> Option<String> {
    let reply: Option<Value> = serde_json::from_str(stdout).ok();
    Some(reply.as_ref().and_then(|r| r.get("transport")?.get("command")?.as_str()).unwrap_or("").to_string())
}

fn grok_command(stdout: &str) -> Option<String> {
    let Ok(Value::Array(servers)) = serde_json::from_str::<Value>(stdout) else { return Some(String::new()) };
    servers
        .iter()
        .find(|s| s.get("name").and_then(Value::as_str) == Some("palette") && s.get("scope").and_then(Value::as_str).unwrap_or("user") == "user")
        .map(|s| s.get("command").and_then(Value::as_str).unwrap_or("").to_string())
}

/// `line` split at runs of whitespace into at most `max + 1` fields, the last
/// holding the rest, as Python's `str.split(None, max)`.
fn split_fields(line: &str, max: usize) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut rest = line.trim_start();
    while !rest.is_empty() {
        if fields.len() == max {
            fields.push(rest);
            break;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        fields.push(&rest[..end]);
        rest = rest[end..].trim_start();
    }
    fields
}

fn agy_command(stdout: &str) -> Option<String> {
    // NAME  TYPE  STATUS  COMMAND/URL
    stdout.lines().skip(1).find_map(|line| {
        let fields = split_fields(line, 3);
        (fields.len() == 4 && fields[0] == "palette").then(|| fields[3].to_string())
    })
}

const CLIENTS: &[Client] = &[
    Client {
        name: "Claude Code",
        get: &["claude", "mcp", "get", "palette"],
        parse: claude_command,
        add: &["claude", "mcp", "add", "--scope", "user", "palette", "--"],
        remove: &["claude", "mcp", "remove", "palette"],
    },
    Client {
        name: "Codex",
        get: &["codex", "mcp", "get", "palette", "--json"],
        parse: codex_command,
        add: &["codex", "mcp", "add", "palette", "--"],
        remove: &["codex", "mcp", "remove", "palette"],
    },
    Client {
        name: "Grok",
        get: &["grok", "mcp", "list", "--json"],
        parse: grok_command,
        add: &["grok", "mcp", "add", "-s", "user", "palette"],
        remove: &["grok", "mcp", "remove", "-s", "user", "palette"],
    },
    Client {
        name: "Antigravity",
        get: &["agy", "mcp", "list"],
        parse: agy_command,
        add: &["agy", "mcp", "add", "palette"],
        remove: &["agy", "mcp", "remove", "palette"],
    },
];

fn run_client<S: AsRef<str>>(argv: &[S]) -> Result<sandbox::WorkerResult, String> {
    sandbox::run_captured(&argv.iter().map(|a| a.as_ref().to_string()).collect::<Vec<_>>(), None, CLIENT_TIMEOUT)
}

/// The command a client's palette server runs, or None when it has none. A
/// client that does not answer counts as having one it could not read.
fn registered_command(client: &Client) -> Option<String> {
    match run_client(client.get) {
        Ok(found) if found.timed_out => Some(String::new()),
        // `claude mcp get` and `codex mcp get` exit nonzero for an unknown name.
        Ok(found) if found.returncode != 0 => None,
        Ok(found) => (client.parse)(&found.stdout),
        Err(_) => Some(String::new()),
    }
}

/// Adds the server to each local client that is installed. A client that
/// already runs this launcher is left alone; one whose palette server runs
/// something else keeps it unless `replace` is set.
fn register(launcher: &str, replace: bool) {
    let wsl = std::env::var_os("WSL_DISTRO_NAME").is_some_and(|v| !v.is_empty());
    for client in CLIENTS {
        // Inside WSL, PATH also lists Windows programs; install.ps1 registers those.
        match util::which(client.add[0]) {
            Some(found) if !(wsl && found.starts_with("/mnt/")) => {}
            _ => continue,
        }
        let add: Vec<&str> = client.add.iter().copied().chain([launcher]).collect();
        let existing = registered_command(client);
        if existing.as_deref() == Some(launcher) {
            println!("{} already uses this installation.", client.name);
            continue;
        }
        if let (Some(other), false) = (&existing, replace) {
            println!(
                "{} already has a server named palette ({}); left as it is. To use this one, run setup again with --replace, or: {}; {}",
                client.name,
                if other.is_empty() { "unreadable" } else { other },
                shjoin(client.remove),
                shjoin(&add)
            );
            continue;
        }
        if existing.is_some() {
            let _ = run_client(client.remove);
        }
        if run_client(&add).is_ok_and(|r| r.returncode == 0) && registered_command(client).as_deref() == Some(launcher) {
            println!("{} {} to this installation.", if existing.is_some() { "Moved" } else { "Registered" }, client.name);
        } else {
            println!("Could not register with {}; run: {}", client.name, shjoin(&add));
        }
    }
}

/// An installed server spoken to over MCP stdio, as a client would.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    log: fs::File,
    ids: u64,
    // Dropping it stops the watchdog that kills a server that hangs.
    _watchdog: mpsc::Sender<()>,
}

/// An unnamed temporary file: the server's progress goes there, shown only
/// on failure.
fn scratch_file() -> Result<fs::File, String> {
    let path = util::temp_dir().join(format!("palette-setup-{}.log", util::hex(12)));
    let file = fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path).map_err(at(&path))?;
    let _ = fs::remove_file(&path);
    Ok(file)
}

impl Server {
    fn start(launcher: &Path) -> Result<Server, String> {
        let log = scratch_file()?;
        let mut child = Command::new(launcher)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log.try_clone().map_err(|e| e.to_string())?)
            .spawn()
            .map_err(at(launcher))?;
        let (watchdog, cancelled) = mpsc::channel::<()>();
        let pid = child.id() as libc::pid_t;
        std::thread::spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = cancelled.recv_timeout(CHECK_TIMEOUT) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        });
        let stdin = child.stdin.take().expect("piped");
        let stdout = BufReader::new(child.stdout.take().expect("piped"));
        Ok(Server { child, stdin, stdout, log, ids: 0, _watchdog: watchdog })
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        writeln!(self.stdin, "{message}").and_then(|_| self.stdin.flush()).map_err(|e| format!("writing to the server: {e}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.ids += 1;
        let id = self.ids;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let mut line = String::new();
        loop {
            line.clear();
            if self.stdout.read_line(&mut line).map_err(|e| format!("reading from the server: {e}"))? == 0 {
                return Err("the server exited before it answered".into());
            }
            if line.trim().is_empty() {
                continue;
            }
            let reply: Value = serde_json::from_str(&line).map_err(|e| format!("unreadable reply ({e}): {}", util::head(&line, 500)))?;
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(error.get("message").map_or_else(|| error.to_string(), plain));
            }
            return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn initialize(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "palette-setup", "version": "1"}}),
        )?;
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
    }

    /// The value of one palette call.
    fn evaluate(&mut self, code: &str) -> Result<Value, String> {
        let result = self.request("tools/call", json!({"name": "palette", "arguments": {"code": code}}))?;
        let reply_text: String = result
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect())
            .unwrap_or_default();
        let reply: Value = serde_json::from_str(&reply_text).map_err(|_| format!("unexpected reply: {}", util::head(&reply_text, 500)))?;
        reply.get("data").cloned().ok_or_else(|| format!("the call failed: {}", util::head(&reply_text, 500)))
    }

    fn close(mut self, failed: bool) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if failed {
            let mut log = Vec::new();
            let _ = self.log.seek(SeekFrom::Start(0)).and_then(|_| self.log.read_to_end(&mut log));
            eprint!("{}", util::tail(&String::from_utf8_lossy(&log), 4000));
        }
    }
}

/// A value as text: a string without its quotes, anything else as JSON.
fn plain(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), String::from)
}

/// Evaluates x = 41, then x + 1 in a second call, and reads the TeamLab shop.
fn check(launcher: &Path) -> Result<(), String> {
    println!("==> Checking the installation: x = 41, then x + 1");
    let _ = std::io::stdout().flush();
    let mut server = Server::start(launcher)?;
    let checked = (|| {
        server.initialize()?;
        server.evaluate("x = 41")?;
        let value = server.evaluate("x + 1")?;
        if value.as_f64() != Some(42.0) {
            return Err(format!("x + 1 returned {value}, not 42: the second call did not see x"));
        }
        println!("    x + 1 = 42: state persists between calls.");
        let status = server.evaluate(r#"Palette.runtime("invoke", Dict("id"=>"teamlab", "arguments"=>Dict{String,Any}("action"=>"status")))["result"]"#)?;
        let (Some(listed), dir) = (status.get("listed"), status.get("dir")) else {
            return Err(format!("TeamLab status returned {status}"));
        };
        println!("    TeamLab: {} listings in the shop at {}.", plain(listed), dir.map(plain).unwrap_or_default());
        Ok(())
    })();
    server.close(checked.is_err());
    checked
}

/// The directories in a read-roots file: one absolute path a line, blank
/// lines and lines starting with # skipped.
fn read_roots_list(path: &Path) -> Result<Vec<String>, String> {
    let mut roots = Vec::new();
    for line in fs::read_to_string(path).map_err(at(path))?.lines() {
        let entry = line.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        if !entry.starts_with('/') || entry.contains(':') {
            return Err(format!("{}: read roots must be absolute paths without \":\": {entry}", path.display()));
        }
        if !Path::new(entry).is_dir() {
            return Err(format!("{}: read root is not a directory: {entry}", path.display()));
        }
        roots.push(text(&util::resolve(Path::new(entry))));
    }
    Ok(roots)
}

/// The portable plugin folder a client loads: its manifest, the server entry,
/// the icon and the licenses.
fn write_plugin(repo: &Path, plugin: &Path, entry: &Value) -> Result<(), String> {
    let assets = plugin.join("assets");
    fs::create_dir_all(&assets).map_err(at(&assets))?;
    let manifest_path = plugin.join("plugin.json");
    if !manifest_path.exists() {
        // A client bundle's SDK leaves out the plugin declaration; its template
        // sits beside the SDK.
        let template = [repo.join("runtime/plugins/palette/plugin.json"), repo.join("../templates/palette-plugin.json")]
            .into_iter()
            .find(|p| p.is_file())
            .ok_or_else(|| format!("no Palette plugin template in {}", repo.display()))?;
        fs::copy(&template, &manifest_path).map_err(at(&template))?;
    }
    let mut manifest = read_json(&manifest_path)?;
    if manifest.get("name").and_then(Value::as_str) != Some("palette") {
        return Err("target must be a Palette portable plugin".into());
    }
    let config_path = plugin.join("mcp.json");
    let mut config = if config_path.exists() { read_json(&config_path)? } else { json!({}) };
    let config_map = config.as_object_mut().ok_or_else(|| format!("{} must be a JSON object", config_path.display()))?;
    let servers = config_map
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| format!("{}: mcpServers must be an object", config_path.display()))?;
    if servers.keys().any(|k| k != "palette") {
        return Err("target contains other portable servers".into());
    }
    let mut server = Map::new();
    server.insert("type".into(), json!("stdio"));
    server.extend(entry.as_object().expect("an object").clone());
    servers.insert("palette".into(), Value::Object(server));
    config_map.insert("$schema".into(), json!(MCP_SCHEMA));
    fs::write(&config_path, pretty(&config)).map_err(at(&config_path))?;
    for (from, to) in [
        ("runtime/plugins/palette/assets/palette.svg", "assets/palette.svg"),
        ("runtime/licenses/Apache-2.0.txt", "LICENSE"),
        ("runtime/licenses/MIT-INHERITED.txt", "MIT-INHERITED.txt"),
    ] {
        fs::copy(repo.join(from), plugin.join(to)).map_err(at(&repo.join(from)))?;
    }
    fs::write(plugin.join("NOTICE"), NOTICE).map_err(at(&plugin.join("NOTICE")))?;
    manifest.as_object_mut().ok_or("plugin.json must be a JSON object")?.insert("license".into(), json!("Apache-2.0"));
    fs::write(&manifest_path, pretty(&manifest)).map_err(at(&manifest_path))
}

pub fn main(argv: &[String]) -> Result<i32, String> {
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(0);
    }
    let f = crate::flags(argv, BOOLEAN_FLAGS)?;
    if let Some(unknown) = f.keys().find(|k| !VALUE_FLAGS.contains(&k.as_str()) && !BOOLEAN_FLAGS.contains(&k.as_str())) {
        return Err(format!("setup: unknown option --{unknown}; see palette-host setup --help"));
    }
    if !cfg!(target_os = "linux") {
        return Err("Run setup in Linux or WSL2; the supervised host requires bubblewrap".into());
    }
    let home = palette_home();
    let config = config_dir();
    let dir = |key: &str, default: PathBuf| util::resolve(&f.get(key).map_or(default, PathBuf::from));
    let repo = util::resolve(Path::new(&f.get("repo-dir").cloned().map_or_else(cli::default_repo_dir, Ok)?));
    let output = dir("output-dir", home.join("client"));
    let data = dir("data-dir", home.join("data"));
    let depot = dir("depot-dir", home.join("depot"));
    let runtime_root = dir("runtime-root", home.join("capabilities"));
    let ceiling = dir("ceiling", home.join("config/ceiling.json"));
    // An operator file, used when it exists; an empty value means none.
    let operator_file = |key: &str, default: &str| match f.get(key) {
        Some(s) if s.is_empty() => None,
        Some(s) => Some(util::expand_user(s)),
        None => Some(config.join(default)),
    };
    let host_commands = operator_file("host-commands", "host-commands.json").map(|p| util::resolve(&p)).filter(|p| p.is_file());
    let read_roots = match operator_file("read-roots", "read-roots").filter(|p| p.is_file()) {
        Some(p) => read_roots_list(&p)?,
        None => Vec::new(),
    };
    let mut tools = Vec::new();
    for name in ["julia", "bwrap", "python3"] {
        tools.push(util::which(name).ok_or_else(|| format!("missing prerequisite on PATH: {name}"))?);
    }
    let (julia, python) = (&tools[0], text(&tools[2]));
    if !repo.join("Project.toml").is_file() {
        return Err(format!("not a Palette SDK: {}", repo.display()));
    }
    if [&data, &depot, &output, &runtime_root, &ceiling].iter().any(|p| util::within(p, &repo)) {
        return Err("Keep data, depot, capabilities and generated configuration outside the SDK tree".into());
    }
    let workspace = util::resolve(&data.join("workspace"));
    let host_state = [util::resolve(&data.join("state")), util::resolve(&data.join("worlds"))];
    if [&repo, &depot, &output, &runtime_root, &ceiling, &host_state[0], &host_state[1]]
        .iter()
        .any(|p| util::within(&workspace, p) || util::within(p, &workspace))
    {
        return Err("Workspace must not overlap source, depot, capabilities, configuration, or host state".into());
    }
    let host = repo.join("runtime/host/target/release/palette-host");
    let serve = text(&repo.join("runtime/security/serve_palette.py"));
    // The client starts the server with this environment, not the shell's, so
    // it names the Julia found now and keeps packages beside the other data.
    let mut launch_env: Vec<(String, String)> = [
        ("PALETTE_REPO", text(&repo)),
        ("PALETTE_HOST", text(&host)),
        ("PALETTE_DATA_HOME", text(&data)),
        ("PALETTE_STATE_DIR", text(&data.join("state"))),
        ("PALETTE_WORKSPACE_STATE_ROOT", text(&data.join("worlds"))),
        ("OPERATOR_WORKSPACE", text(&data.join("workspace"))),
        ("PALETTE_PACKAGE_DEPOT", text(&data.join("packages"))),
        ("PALETTE_RUNTIME_ROOT", text(&runtime_root)),
        ("PALETTE_CAPABILITY_CEILING", text(&ceiling)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    if let Some(p) = &host_commands {
        launch_env.push(("PALETTE_HOST_COMMANDS".into(), text(p)));
    }
    if !read_roots.is_empty() {
        launch_env.push(("PALETTE_READ_ROOTS".into(), read_roots.join(":")));
    }
    launch_env.push(("JULIA_DEPOT_PATH".into(), text(&depot)));
    launch_env.push(("JULIA_PKG_OFFLINE".into(), "true".into()));
    launch_env.push(("PATH".into(), format!("{}:{SYSTEM_PATH}", text(julia.parent().expect("a file on PATH")))));
    let launcher = output.join("palette-mcp");
    let script = format!(
        "#!/bin/sh\n# Palette-managed launcher\nexec env {} {} {} \"$@\"\n",
        launch_env.iter().map(|(k, v)| shquote(&format!("{k}={v}"))).collect::<Vec<_>>().join(" "),
        shquote(&python),
        shquote(&serve)
    );

    // A failed install, build, prewarm or check puts the capabilities and
    // ceiling back, so the installation that worked keeps a runtime it matches.
    let kept: Vec<PathBuf> = BUNDLED_CAPABILITIES.iter().map(|n| runtime_root.join(n)).chain([ceiling.clone()]).collect();
    let snapshot = Snapshot::take(&kept)?;
    let prepared = (|| {
        // Preparing starts sessions under the environment the server gets, so
        // the capabilities and ceiling it names exist first.
        deploy_capabilities(&repo, &runtime_root)?;
        merge_ceiling(&ceiling)?;
        if !f.contains_key("connect-only") {
            let env: Vec<(String, String)> = launch_env.iter().filter(|(k, _)| k != "JULIA_PKG_OFFLINE" && k != "PATH").cloned().collect();
            let args = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
            step("Installing Julia packages", &args(&["julia", &format!("--project={}", repo.display()), "-e", "using Pkg; Pkg.instantiate()"]), &env, false)?;
            // A release archive carries a prebuilt host; a checkout builds its own.
            let cargo = util::which("cargo");
            if (repo.join(".git").exists() && cargo.is_some()) || !host.is_file() {
                if cargo.is_none() {
                    return Err("This source has no prebuilt host: install Rust (https://rustup.rs) or use the release installer".into());
                }
                let manifest = text(&repo.join("runtime/host/Cargo.toml"));
                step("Building the sandbox host", &args(&["cargo", "build", "--manifest-path", &manifest, "--release", "--locked"]), &env, false)?;
            }
            let mut offline = env.clone();
            offline.push(("JULIA_PKG_OFFLINE".into(), "true".into()));
            let prewarm = text(&repo.join("runtime/security/prewarm_depot.py"));
            step(
                "Preparing the sandbox (the first run takes several minutes)",
                &args(&[&python, &prewarm, "--project-dir", &text(&repo), "--host-bin", &text(&host)]),
                &offline,
                true,
            )?;
        }
        for path in [repo.join("Manifest.toml"), host.clone()] {
            if !path.is_file() {
                return Err(format!("missing prepared runtime file: {}", path.display()));
            }
        }
        if !util::executable(&host) {
            return Err(format!("host is not executable: {}", host.display()));
        }
        fs::create_dir_all(&workspace).map_err(at(&workspace))?;
        fs::create_dir_all(&output).map_err(at(&output))?;
        if !f.contains_key("no-check") {
            // Check the new launcher before it replaces the old one, so a
            // failed update leaves the installation that worked.
            let trial = output.join(format!(".palette-mcp.trial.{}", std::process::id()));
            util::write_atomic(&trial, &script, 0o755)?;
            let checked = check(&trial);
            let _ = fs::remove_file(&trial);
            checked.map_err(|e| format!("The check failed, so this installation was not switched on: {e}"))?;
        }
        Ok(())
    })();
    if prepared.is_err() {
        snapshot.restore();
    }
    drop(snapshot);
    prepared?;

    util::write_atomic(&launcher, &script, 0o755)?;
    let entry = json!({
        "command": python,
        "args": [serve],
        "env": launch_env.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<Map<_, _>>(),
    });
    write_plugin(&repo, &output.join("palette-local-plugin"), &entry)?;
    // Windows clients launch WSL explicitly; the launcher carries the environment.
    let desktop = match f.get("wsl-distribution") {
        Some(d) => json!({"command": "wsl.exe", "args": ["--distribution", d, "--exec", text(&launcher)]}),
        None => entry,
    };
    for (name, value) in [("palette-launch.json", desktop.clone()), ("generic-mcp.json", json!({"mcpServers": {"palette": desktop}}))] {
        fs::write(output.join(name), pretty(&value)).map_err(at(&output.join(name)))?;
    }
    if !f.contains_key("no-register") {
        register(&text(&launcher), f.contains_key("replace"));
    }
    if let Some(p) = &host_commands {
        println!("Host commands: {}", p.display());
    }
    if !read_roots.is_empty() {
        println!("Read roots: {}", read_roots.join(", "));
    }
    println!("Client files: {}\nstdio MCP command: {}", output.display(), launcher.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        util::mkdtemp("palette-setup-test-").unwrap()
    }

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn ceiling_is_created_or_added_to() {
        let dir = tmp();
        let ceiling = dir.join("config/ceiling.json");
        merge_ceiling(&ceiling).unwrap();
        assert_eq!(read_json(&ceiling).unwrap(), json!({"host_request": {"allowed_types": ["palette.runtime"]}}));
        assert_eq!(mode(&ceiling), 0o600);
        let own = json!({"host_request": {"allowed_types": ["other.type"]}, "network_access": {"allowed": true, "allowed_hosts": ["example.org"]}});
        fs::write(&ceiling, own.to_string()).unwrap();
        merge_ceiling(&ceiling).unwrap();
        let merged = read_json(&ceiling).unwrap();
        assert_eq!(merged["host_request"]["allowed_types"], json!(["other.type", "palette.runtime"]));
        assert_eq!(merged["network_access"], own["network_access"]);
        // A ceiling that already grants it is not rewritten.
        let before = fs::metadata(&ceiling).unwrap().modified().unwrap();
        merge_ceiling(&ceiling).unwrap();
        assert_eq!(fs::metadata(&ceiling).unwrap().modified().unwrap(), before);
        for broken in ["{not json", r#"{"no_such_category": {}}"#] {
            fs::write(&ceiling, broken).unwrap();
            let e = merge_ceiling(&ceiling).unwrap_err();
            assert!(e.contains("is not valid") && e.contains("run setup again"), "{e}");
            assert_eq!(fs::read_to_string(&ceiling).unwrap(), broken);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ceiling_checks_read_as_the_python_host_wrote_them() {
        let refused = |v: Value| validate_ceiling(&v).unwrap_err();
        assert_eq!(refused(json!([])), "capability ceiling must be a JSON object");
        assert_eq!(refused(json!({"zz": {}, "aa": {}})), "unknown capability categories: ['aa', 'zz']");
        assert_eq!(refused(json!({"fs_digest": []})), "fs_digest grant must be an object");
        assert_eq!(refused(json!({"host_request": {}})), "host_request.allowed_types must be a list of non-empty strings");
        assert_eq!(refused(json!({"host_request": {"allowed_types": ["a", ""]}})), "host_request.allowed_types must be a list of non-empty strings");
        assert_eq!(refused(json!({"host_request": {"allowed_types": ["a", "a"]}})), "host_request.allowed_types must not contain duplicates");
        assert_eq!(
            refused(json!({"package_management": {"allowed_packages": ["JSON", "1x", "a-b"]}})),
            "package_management.allowed_packages has invalid Julia identifiers: ['1x', 'a-b']"
        );
        assert_eq!(
            refused(json!({"package_management": {"allowed_packages": ["JSON", "JSON"]}})),
            "package_management.allowed_packages must not contain duplicates"
        );
        assert_eq!(
            refused(json!({"package_management": {"allowed_packages": [], "offline": "yes"}})),
            "package_management.offline must be boolean when provided"
        );
        assert_eq!(refused(json!({"external_fs_write": {"allowed_dirs": ["rel"]}})), "external_fs_write.allowed_dirs entries must be absolute paths");
        assert_eq!(refused(json!({"fs_digest": {"allowed_paths": ["rel"]}})), "fs_digest.allowed_paths entries must be absolute paths");
        assert_eq!(refused(json!({"network_access": {}})), "network_access.allowed must be boolean");
        assert_eq!(
            refused(json!({"network_access": {"allowed": true, "allowed_hosts": "x"}})),
            "network_access.allowed_hosts must be a list of non-empty strings"
        );
        assert_eq!(refused(json!({"host_command": {}})), "host_command.commands must be an object");
        assert_eq!(refused(json!({"host_command": {"commands": {"": {}}}})), "host_command entries must map non-empty names to objects");
        let command = |spec: Value| refused(json!({"host_command": {"commands": {"c": spec}}}));
        assert_eq!(command(json!({"argv": ["rel"], "cwd": "/"})), "host_command.commands.c.argv[0] must be an absolute executable path");
        assert_eq!(command(json!({"argv": [], "cwd": "/"})), "host_command.commands.c.argv[0] must be an absolute executable path");
        assert_eq!(command(json!({"argv": ["/bin/true"], "cwd": "rel"})), "host_command.commands.c.cwd must be an absolute path");
        for timeout in [json!(0), json!(14401), json!(true), json!("5")] {
            assert_eq!(command(json!({"argv": ["/bin/true"], "cwd": "/", "timeout_s": timeout})), "host_command.commands.c.timeout_s must be in (0, 14400]");
        }
        assert_eq!(command(json!({"argv": ["/bin/true"], "cwd": "/", "extra_args": 1})), "host_command.commands.c.extra_args must be boolean");
        assert_eq!(refused(json!({"spawn_child_worker": {"x": 1}})), "spawn_child_worker grant must be an empty object");
        assert_eq!(refused(json!({"teamlab": {"dir": "rel"}})), "teamlab grant must be {\"dir\": <absolute path>}");
        assert_eq!(refused(json!({"teamlab": {"dir": "/a", "x": 1}})), "teamlab grant must be {\"dir\": <absolute path>}");
        validate_ceiling(&json!({
            "package_management": {"allowed_packages": ["JSON", "_x1"], "offline": true},
            "host_request": {"allowed_types": ["palette.runtime"]},
            "external_fs_write": {"allowed_dirs": ["/a"]},
            "fs_digest": {"allowed_paths": ["/b"]},
            "network_access": {"allowed": false, "allowed_hosts": null},
            "host_command": {"commands": {"c": {"argv": ["/bin/true"], "cwd": "/", "timeout_s": 14400, "extra_args": false}}},
            "spawn_child_worker": {},
            "teamlab": {"dir": "/shop"},
        }))
        .unwrap();
    }

    #[test]
    fn capabilities_are_replaced_and_others_kept() {
        let root = tmp();
        fs::create_dir_all(root.join("teamlab")).unwrap();
        fs::write(root.join("teamlab/stale.jl"), "").unwrap();
        fs::create_dir(root.join("gesso")).unwrap();
        fs::write(root.join("gesso/capability.json"), "{}").unwrap();
        deploy_capabilities(&repo(), &root).unwrap();
        let source = repo().join("runtime/integrations/teamlab");
        let file = read_json(&source.join("capability.json")).unwrap()["source"].as_str().unwrap().to_string();
        let names = |d: &Path| {
            let mut n: Vec<String> = fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            n.sort();
            n
        };
        let mut expected = vec!["capability.json".to_string(), file.clone()];
        expected.sort();
        assert_eq!(names(&root.join("teamlab")), expected);
        assert_eq!(fs::read(root.join("teamlab").join(&file)).unwrap(), fs::read(source.join(&file)).unwrap());
        assert!(root.join("gesso/capability.json").exists());
        assert_eq!(names(&root), ["gesso", "teamlab"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn snapshot_puts_back_what_was_there() {
        let dir = tmp();
        let (tree, file, missing) = (dir.join("tree"), dir.join("file.json"), dir.join("missing"));
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("a"), "a").unwrap();
        std::os::unix::fs::symlink("a", tree.join("link")).unwrap();
        fs::write(&file, "kept").unwrap();
        let snapshot = Snapshot::take(&[tree.clone(), file.clone(), missing.clone()]).unwrap();
        fs::remove_dir_all(&tree).unwrap();
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("b"), "b").unwrap();
        fs::write(&file, "changed").unwrap();
        fs::create_dir(&missing).unwrap();
        snapshot.restore();
        drop(snapshot);
        assert_eq!(fs::read_to_string(tree.join("a")).unwrap(), "a");
        assert!(!tree.join("b").exists());
        assert_eq!(fs::read_link(tree.join("link")).unwrap(), Path::new("a"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "kept");
        assert!(!missing.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn client_replies_are_read() {
        assert_eq!(claude_command("palette:\n  Scope: User config\n  Command: /a/palette-mcp\n  Args: \n").as_deref(), Some("/a/palette-mcp"));
        assert_eq!(claude_command("palette:\n").as_deref(), Some(""));
        assert_eq!(codex_command(r#"{"transport": {"command": "/a/p"}}"#).as_deref(), Some("/a/p"));
        assert_eq!(codex_command("not json").as_deref(), Some(""));
        let grok = r#"[{"name": "other", "command": "/bin/other"}, {"name": "palette", "command": "/a/p", "scope": "project"}]"#;
        assert_eq!(grok_command(grok), None);
        assert_eq!(grok_command(r#"[{"name": "palette", "command": "/a/p"}]"#).as_deref(), Some("/a/p"));
        assert_eq!(grok_command("{").as_deref(), Some(""));
        let agy = "NAME     TYPE   STATUS   COMMAND/URL\nother    stdio  enabled  /bin/other\npalette  stdio  enabled  /a/my palette\n";
        assert_eq!(agy_command(agy).as_deref(), Some("/a/my palette"));
        assert_eq!(agy_command("NAME TYPE STATUS COMMAND/URL\npalette stdio\n"), None);
        assert_eq!(split_fields("  a  b c d e ", 3), ["a", "b", "c", "d e "]);
    }

    #[test]
    fn words_are_quoted_for_the_shell() {
        assert_eq!(shquote(""), "''");
        assert_eq!(shquote("PATH=/usr/bin:/bin"), "PATH=/usr/bin:/bin");
        assert_eq!(shquote("/a/project one"), "'/a/project one'");
        assert_eq!(shquote("it's"), "'it'\"'\"'s'");
        assert_eq!(shjoin(&["claude", "mcp", "add", "/a b"]), "claude mcp add '/a b'");
        assert_eq!(util::expand_user("~/x"), util::home().join("x"));
        assert_eq!(util::expand_user("~user/x"), PathBuf::from("~user/x"));
    }
}
