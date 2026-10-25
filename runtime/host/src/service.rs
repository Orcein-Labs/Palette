//! `palette-host service`: install and control Palette's user-owned systemd
//! units, palette.service (the daemon) and the optional
//! palette-tunnel.service (an operator-owned tunnel client in front of it).
//! Units are written only under the user's systemd directory, and only these
//! two names.

use crate::util;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str = "\
usage: palette-host service install|start|stop|restart|status [options]

  install                 write palette.service (and palette-tunnel.service when a
                          tunnel client is given), then reload the user manager
  start|stop|restart      control the unit named by --unit
  status                  print the unit's state as JSON

  --repo-dir R            Palette checkout the daemon runs from
  --workspace-dir W       the daemon's default workspace (install)
  --socket S              daemon socket (default $PALETTE_SOCKET, else
                          $XDG_RUNTIME_DIR/palette/daemon.sock)
  --env-file F            existing operator-owned 0600 daemon configuration file
  --unit U                palette.service (default) or palette-tunnel.service
  --tunnel-client C       existing tunnel-client executable; writes the tunnel unit
  --tunnel-profile P      the operator's existing tunnel profile
  --tunnel-profile-dir D  directory holding the tunnel profile
  --tunnel-env-file F     existing operator-owned 0600 credentials file
";

const VALUE_FLAGS: &[&str] = &[
    "repo-dir", "workspace-dir", "socket", "env-file", "unit",
    "tunnel-client", "tunnel-profile", "tunnel-profile-dir", "tunnel-env-file",
];
const DAEMON: &str = "palette.service";
const TUNNEL: &str = "palette-tunnel.service";
const RESERVED: [&str; 4] = ["PALETTE_REPO", "PALETTE_HOST", "PALETTE_SOCKET", "OPERATOR_WORKSPACE"];

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn text(p: &Path) -> Result<&str, String> {
    p.to_str().ok_or_else(|| format!("{} is not UTF-8", p.display()))
}

fn absolute(s: &str) -> PathBuf {
    util::resolve(&util::expand_user(s))
}

fn unit_dir() -> PathBuf {
    env_nonempty("XDG_CONFIG_HOME").map_or_else(|| util::home().join(".config"), PathBuf::from).join("systemd/user")
}

/// The daemon socket: `flag`, else $PALETTE_SOCKET, else the one in the
/// user's runtime directory.
fn socket_path(flag: Option<&str>) -> Result<PathBuf, String> {
    if let Some(s) = flag.map(str::to_string).or_else(|| env_nonempty("PALETTE_SOCKET")) {
        return Ok(absolute(&s));
    }
    let runtime = env_nonempty("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is required to locate Palette's user runtime socket")?;
    Ok(util::resolve(&Path::new(&runtime).join("palette/daemon.sock")))
}

/// One systemd token, quoted, with specifier and environment expansion
/// disabled.
fn unit_quote(value: &str) -> Result<String, String> {
    if value.contains(['\n', '\r', '\0']) {
        return Err("unit values cannot contain newlines or NUL bytes".into());
    }
    let mut out = String::from('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("\\x24"),
            '`' => out.push_str("\\x60"),
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

fn quote_all(words: &[&str]) -> Result<String, String> {
    Ok(words.iter().map(|w| unit_quote(w)).collect::<Result<Vec<_>, _>>()?.join(" "))
}

/// A path escaped for directives, like EnvironmentFile=, that take no quoted
/// words: whitespace, controls and `\ " # % ;` become `\xNN` bytes.
fn unit_path(p: &Path) -> Result<String, String> {
    let value = text(p)?;
    if !value.starts_with('/') || value.contains(['\n', '\r', '\0']) {
        return Err("systemd file paths must be absolute and contain no newlines or NUL bytes".into());
    }
    let mut out = String::new();
    for c in value.chars() {
        if c.is_whitespace() || c.is_control() || matches!(c, '\\' | '"' | '#' | '%' | ';') {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("\\x{b:02x}"));
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// An environment file the operator owns, private, that does not redefine
/// the variables that say which Palette the service is.
fn operator_env_file(path: &str) -> Result<PathBuf, String> {
    let path = absolute(path);
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
    let meta = fs::metadata(&path).map_err(at)?;
    // SAFETY: getuid has no preconditions and cannot fail.
    if meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o777 != 0o600 {
        return Err("operator environment file must be owned by this user with mode 0600".into());
    }
    for line in fs::read_to_string(&path).map_err(at)?.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let key = line.strip_prefix("export ").unwrap_or(line).split('=').next().unwrap_or_default().trim();
        if RESERVED.contains(&key) {
            return Err(format!("operator environment file cannot override service identity: {key}"));
        }
    }
    Ok(path)
}

fn env_file_line(path: Option<&str>) -> Result<String, String> {
    path.map_or(Ok(String::new()), |p| Ok(format!("EnvironmentFile={}\n", unit_path(&operator_env_file(p)?)?)))
}

/// Behind a tunnel, remote chats reach the daemon, so it runs without the
/// operator's host commands; local agents alone keep them.
fn render_palette_unit(repo: &Path, workspace: Option<&str>, sock: &Path, env_file: Option<&str>, behind_tunnel: bool) -> Result<String, String> {
    let (repo, sock) = (util::resolve(repo), util::resolve(sock));
    let workspace = workspace.ok_or("--workspace-dir is required because the daemon needs its default workspace")?;
    let host = repo.join("runtime/host/target/release/palette-host");
    if !util::executable(&host) {
        return Err(format!("missing or non-executable Palette host: {}", host.display()));
    }
    let workspace = absolute(workspace);
    let values = [("PALETTE_REPO", &repo), ("PALETTE_HOST", &host), ("PALETTE_SOCKET", &sock), ("OPERATOR_WORKSPACE", &workspace)];
    let mut env = env_file_line(env_file)?;
    for (i, (key, value)) in values.iter().enumerate() {
        env.push_str(&format!("{}Environment={key}={}", if i > 0 { "\n" } else { "" }, unit_quote(text(value)?)?));
    }
    let command = quote_all(&[text(&host)?, "mcp", "--socket", text(&sock)?])?;
    let unset = if behind_tunnel { " PALETTE_HOST_COMMANDS" } else { "" };
    // The broker and operator workers need the user's host /tmp. Sandbox
    // workers establish their own private temporary filesystem.
    Ok(format!(
        "[Unit]
Description=Palette Julia operator daemon
After=default.target

[Service]
Type=simple
ExecStart={command}
Restart=on-failure
RestartSec=2s
RuntimeDirectory=palette
RuntimeDirectoryMode=0700
UMask=0077
PrivateTmp=no
NoNewPrivileges=yes
{env}
UnsetEnvironment=CONTROL_PLANE_API_KEY OPENAI_API_KEY OPENAI_ADMIN_KEY{unset}
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
"
    ))
}

fn render_tunnel_unit(client: &str, profile: &str, env_file: Option<&str>, profile_dir: Option<&str>, sock: Option<&Path>) -> Result<String, String> {
    let client = absolute(client);
    if !util::executable(&client) {
        return Err(format!("tunnel client must be an existing executable: {}", client.display()));
    }
    if profile.is_empty() || profile.contains('\n') {
        return Err("an existing tunnel profile name is required".into());
    }
    let mut words = vec![text(&client)?.to_string(), "run".into(), "--profile".into(), profile.into()];
    if let Some(dir) = profile_dir {
        let dir = absolute(dir);
        if !dir.is_dir() || ![".yaml", ".yml"].iter().any(|suffix| dir.join(format!("{profile}{suffix}")).is_file()) {
            return Err(format!("tunnel profile must exist in the configured profile directory: {}", dir.display()));
        }
        words.extend(["--profile-dir".into(), text(&dir)?.to_string()]);
    }
    let mut env = env_file_line(env_file)?;
    if let Some(sock) = sock {
        env.push_str(&format!("Environment=PALETTE_SOCKET={}\n", unit_quote(text(&util::resolve(sock))?)?));
    }
    let command = quote_all(&words.iter().map(String::as_str).collect::<Vec<_>>())?;
    Ok(format!(
        "[Unit]
Description=Palette operator-owned Chat tunnel client
After=palette.service
Wants=palette.service

[Service]
Type=simple
ExecStart={command}
Restart=always
RestartSec=2s
UMask=0077
PrivateTmp=yes
NoNewPrivileges=yes
{env}StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
"
    ))
}

fn systemctl_bin() -> Result<PathBuf, String> {
    env_nonempty("PALETTE_SYSTEMCTL").map(PathBuf::from).or_else(|| util::which("systemctl")).ok_or_else(|| "systemctl is not available".into())
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let bin = systemctl_bin()?;
    let status = Command::new(&bin).arg("--user").args(args).status().map_err(|e| format!("{}: {e}", bin.display()))?;
    if status.success() { Ok(()) } else { Err(format!("systemctl --user {} failed: {status}", args.join(" "))) }
}

/// Field 22 of /proc/PID/stat, the process's start time in clock ticks: with
/// the PID it names one process even after the PID is reused. The command
/// name may hold ") ", so the fields start after the last one.
fn process_start_ticks(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?.1.split_whitespace().nth(19).map(str::to_string)
}

fn install(f: &HashMap<String, String>) -> Result<i32, String> {
    let get = |k: &str| f.get(k).map(String::as_str);
    let sock = socket_path(get("socket"))?;
    let repo = PathBuf::from(get("repo-dir").map(str::to_string).map_or_else(crate::cli::default_repo_dir, Ok)?);
    // Both units are checked before either is written.
    let tunnel = match (get("tunnel-client"), get("tunnel-profile")) {
        (Some(client), Some(profile)) => Some(render_tunnel_unit(client, profile, get("tunnel-env-file"), get("tunnel-profile-dir"), Some(&sock))?),
        (None, None) if get("tunnel-env-file").is_none() && get("tunnel-profile-dir").is_none() => None,
        _ => return Err("--tunnel-client and --tunnel-profile must be supplied together".into()),
    };
    let dir = unit_dir();
    let tunnel_enabled = dir.join("default.target.wants").join(TUNNEL).exists();
    let daemon = render_palette_unit(&repo, get("workspace-dir"), &sock, get("env-file"), tunnel.is_some() || tunnel_enabled)?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (daemon_path, tunnel_path) = (dir.join(DAEMON), dir.join(TUNNEL));
    util::write_atomic(&daemon_path, &daemon, 0o600)?;
    if let Some(unit) = &tunnel {
        util::write_atomic(&tunnel_path, unit, 0o600)?;
    }
    systemctl(&["daemon-reload"])?;
    println!(
        "{}",
        json!({
            "installed": [daemon_path.to_string_lossy()],
            "socket": sock.to_string_lossy(),
            "tunnel_unit": tunnel.map(|_| tunnel_path.to_string_lossy().into_owned()),
        })
    );
    Ok(0)
}

fn status(unit: &str, socket: Option<&str>) -> Result<i32, String> {
    let bin = systemctl_bin()?;
    let out = Command::new(&bin)
        .args(["--user", "show", unit, "--no-page", "--property=ActiveState,SubState,MainPID,ExecMainStartTimestamp"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!("systemctl --user show {unit} failed: {}", out.status));
    }
    let mut record = Map::new();
    record.insert("unit".into(), json!(unit));
    for (key, value) in String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.split_once('=')) {
        record.insert(key.into(), json!(value));
    }
    let daemon = unit == DAEMON;
    let pid = record.get("MainPID").and_then(Value::as_str).and_then(|p| p.parse::<u32>().ok()).filter(|&p| p > 0);
    record.insert("process_start_ticks".into(), json!(pid.filter(|_| daemon).and_then(process_start_ticks)));
    let sock = if daemon { Some(socket_path(socket)?.to_string_lossy().into_owned()) } else { None };
    record.insert("socket".into(), json!(sock));
    println!("{}", Value::Object(record));
    Ok(0)
}

pub fn main(argv: &[String]) -> Result<i32, String> {
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(0);
    }
    let usage = || "usage: palette-host service install|start|stop|restart|status [options]; see palette-host service --help".to_string();
    let (action, rest) = argv.split_first().ok_or_else(usage)?;
    let mut f = crate::flags(rest, &[])?;
    if let Some(unknown) = f.keys().find(|k| !VALUE_FLAGS.contains(&k.as_str())) {
        return Err(format!("service: unknown option --{unknown}; see palette-host service --help"));
    }
    // An empty value is an absent one, as for the environment variables.
    f.retain(|_, v| !v.is_empty());
    let unit = f.get("unit").map_or(DAEMON, String::as_str);
    if unit != DAEMON && unit != TUNNEL {
        return Err(format!("--unit must be {DAEMON} or {TUNNEL}"));
    }
    match action.as_str() {
        "install" => install(&f),
        "status" => status(unit, f.get("socket").map(String::as_str)),
        "start" | "stop" | "restart" => systemctl(&[action, unit]).map(|()| 0),
        _ => Err(usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A repo with the files the daemon unit names, and a workspace path with
    /// an apostrophe, under a root with spaces in its paths.
    struct Fixture {
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        fn new() -> Fixture {
            let f = Fixture { root: util::resolve(&util::mkdtemp("palette-service-test-").unwrap()) };
            write(&f.repo().join("runtime/host/target/release/palette-host"), "#!/bin/sh\n", 0o700);
            f
        }

        fn repo(&self) -> PathBuf {
            self.root.join("repo with space")
        }

        fn workspace(&self) -> PathBuf {
            self.root.join("workspace's name")
        }

        fn sock(&self) -> PathBuf {
            self.root.join("runtime with space/palette/daemon.sock")
        }

        fn daemon(&self, env_file: Option<&str>) -> Result<String, String> {
            render_palette_unit(&self.repo(), self.workspace().to_str(), &self.sock(), env_file, true)
        }
    }

    fn write(path: &Path, contents: &str, mode: u32) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn s(p: &Path) -> &str {
        p.to_str().unwrap()
    }

    #[test]
    fn daemon_unit_quotes_paths_and_owns_runtime_socket() {
        let f = Fixture::new();
        let unit = f.daemon(None).unwrap();
        let repo = s(&f.repo()).to_string();
        let q = |p: &Path| unit_quote(s(p)).unwrap();
        let expected = format!(
            "[Unit]
Description=Palette Julia operator daemon
After=default.target

[Service]
Type=simple
ExecStart={host} \"mcp\" \"--socket\" {sock}
Restart=on-failure
RestartSec=2s
RuntimeDirectory=palette
RuntimeDirectoryMode=0700
UMask=0077
PrivateTmp=no
NoNewPrivileges=yes
Environment=PALETTE_REPO={repo}
Environment=PALETTE_HOST={host}
Environment=PALETTE_SOCKET={sock}
Environment=OPERATOR_WORKSPACE={workspace}
UnsetEnvironment=CONTROL_PLANE_API_KEY OPENAI_API_KEY OPENAI_ADMIN_KEY PALETTE_HOST_COMMANDS
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
",
            sock = q(&f.sock()),
            repo = unit_quote(&repo).unwrap(),
            host = q(&f.repo().join("runtime/host/target/release/palette-host")),
            workspace = q(&f.workspace()),
        );
        assert_eq!(unit, expected);
        assert!(unit.contains("workspace's name"));
        assert!(!unit.replace("\\x24", "").contains('$'));
        assert_eq!(unit_quote("100% $HOME `x` \\ \"").unwrap(), "\"100%% \\x24HOME \\x60x\\x60 \\\\ \\\"\"");
        assert!(unit_quote("bad\nvalue").is_err());
        assert!(unit_quote("bad\0value").is_err());
    }

    #[test]
    fn daemon_unit_keeps_host_commands_without_a_tunnel() {
        let f = Fixture::new();
        let local = render_palette_unit(&f.repo(), f.workspace().to_str(), &f.sock(), None, false).unwrap();
        assert!(local.contains("UnsetEnvironment=CONTROL_PLANE_API_KEY OPENAI_API_KEY OPENAI_ADMIN_KEY\n"));
        assert!(!local.contains("PALETTE_HOST_COMMANDS"));
        assert!(f.daemon(None).unwrap().contains("OPENAI_ADMIN_KEY PALETTE_HOST_COMMANDS\n"));
    }

    #[test]
    fn daemon_unit_requires_workspace_and_host() {
        let f = Fixture::new();
        let render = |workspace: Option<&str>| render_palette_unit(&f.repo(), workspace, &f.sock(), None, true);
        assert!(render(None).unwrap_err().contains("--workspace-dir is required"));
        let host = f.repo().join("runtime/host/target/release/palette-host");
        fs::set_permissions(&host, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(render(Some("w")).unwrap_err().contains("missing or non-executable Palette host"));
    }

    #[test]
    fn unit_path_escapes_words_and_rejects_relative_paths() {
        assert_eq!(unit_path(Path::new("/a b/c#d%e;f\\g\"h\tz")).unwrap(), "/a\\x20b/c\\x23d\\x25e\\x3bf\\x5cg\\x22h\\x09z");
        assert_eq!(unit_path(Path::new("/x\u{a0}y")).unwrap(), "/x\\xc2\\xa0y");
        assert!(unit_path(Path::new("relative")).is_err());
        assert!(unit_path(Path::new("/a\nb")).is_err());
    }

    #[test]
    fn tunnel_unit_requires_existing_operator_client_and_private_env() {
        let f = Fixture::new();
        let client = f.root.join("tunnel client");
        write(&client, "#!/bin/sh\n", 0o700);
        let credentials = f.root.join("operator credentials.env");
        write(&credentials, "# comment\n\nTOKEN=operator-owned\nexport OTHER=1\n", 0o600);
        let profile_dir = f.root.join("profiles with spaces");
        write(&profile_dir.join("existing profile.yaml"), "config_version: 1\n", 0o644);
        let unit = render_tunnel_unit(s(&client), "existing profile", Some(s(&credentials)), Some(s(&profile_dir)), Some(&f.sock())).unwrap();
        assert!(unit.contains("\"run\" \"--profile\" \"existing profile\" \"--profile-dir\""));
        assert!(unit.contains(&unit_quote(s(&profile_dir)).unwrap()));
        assert!(unit.contains(&format!("Environment=PALETTE_SOCKET={}\n", unit_quote(s(&f.sock())).unwrap())));
        assert!(unit.contains(&format!("EnvironmentFile={}\n", unit_path(&credentials).unwrap())));
        assert!(!unit.contains("EnvironmentFile=\""));
        assert!(unit.contains("Restart=always"));
        // A .yml profile counts too, and no profile dir means no flag.
        write(&profile_dir.join("other.yml"), "", 0o644);
        assert!(render_tunnel_unit(s(&client), "other", None, Some(s(&profile_dir)), None).is_ok());
        let bare = render_tunnel_unit(s(&client), "existing", None, None, None).unwrap();
        assert!(!bare.contains("--profile-dir") && !bare.contains("Environment"));

        fs::set_permissions(&credentials, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(render_tunnel_unit(s(&client), "existing", Some(s(&credentials)), None, None).unwrap_err().contains("mode 0600"));
        write(&credentials, "export PALETTE_SOCKET=/tmp/override.sock\n", 0o600);
        assert!(f.daemon(Some(s(&credentials))).unwrap_err().contains("cannot override service identity: PALETTE_SOCKET"));
        assert!(render_tunnel_unit(s(&f.root.join("missing")), "existing", None, None, None).unwrap_err().contains("existing executable"));
        assert!(render_tunnel_unit(s(&client), "", None, None, None).unwrap_err().contains("profile name is required"));
        assert!(render_tunnel_unit(s(&client), "missing", None, Some(s(&profile_dir)), None).unwrap_err().contains("profile must exist"));
    }

    #[test]
    fn process_start_ticks_reads_this_process() {
        let ticks = process_start_ticks(std::process::id()).unwrap();
        assert!(ticks.parse::<u64>().is_ok());
        assert_eq!(process_start_ticks(0), None);
    }
}
