//! `palette-host serve`: prepare and launch a server-owned Palette runtime
//! over MCP stdio, from a profile that fixes the server's paths. The profile
//! keeps configuration, source, packages and saved worlds out of every
//! worker-writable mount, and the launcher passes the runtime only the
//! environment the profile names.

use crate::{cli, sandbox, util};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str = "\
usage: palette-host serve init|inspect|prepare|launch|client --profile P [options]

  init        write a new profile (never replacing one), then report its layout
  inspect     report the profile's layout without starting a worker
  prepare     instantiate Julia, build the host and prewarm the depot (an
              administrator action, run with package networking on)
  launch      run palette-host mcp start under the profile's environment
  client      print an MCP client entry that launches this server over SSH

  --profile P       the server profile (required)
  --repo R          Palette source (init; default /opt/palette)
  --data D          workspaces, state and packages (init; default /var/lib/palette)
  --depot D         prepared Julia depot (init; default /var/cache/palette/depot)
  --ceiling C       host-owned ceiling file (init; default /etc/palette/ceiling.json)
  --runtime-root R  installed capability manifests (init; default /opt/palette-capabilities)
  --ssh-target T    user@host the client connects to (client)
";

const VALUE_FLAGS: &[&str] = &["profile", "repo", "data", "depot", "ceiling", "runtime-root", "ssh-target"];
const FIELDS: &[&str] = &[
    "format", "repo", "data", "depot", "ceiling", "runtime_root", "host_commands", "read_roots", "repair_config",
    "patch_roots", "turn_timeout", "max_turn_timeout",
];
const HOST: &str = "runtime/host/target/release/palette-host";

#[derive(Debug)]
struct Profile {
    profile: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    depot: PathBuf,
    ceiling: PathBuf,
    runtime_root: PathBuf,
    host_commands: Option<PathBuf>,
    repair_config: Option<PathBuf>,
    patch_roots: Map<String, Value>,
    read_roots: Vec<PathBuf>,
    turn_limits: (f64, f64),
}

fn text(p: &Path) -> Result<&str, String> {
    p.to_str().ok_or_else(|| format!("{} is not UTF-8", p.display()))
}

fn absolute(value: Option<&Value>, name: &str) -> Result<PathBuf, String> {
    match value.and_then(Value::as_str).map(Path::new) {
        Some(p) if p.is_absolute() => Ok(util::resolve(p)),
        _ => Err(format!("{name} must be an absolute server path")),
    }
}

/// An optional path field: absent, null and "" all leave it unset.
fn optional(profile: &Map<String, Value>, key: &str) -> Result<Option<PathBuf>, String> {
    match profile.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        value => absolute(value, key).map(Some),
    }
}

fn disjoint(writable: &[(&str, &Path)], protected: &[(&str, &Path)]) -> Result<(), String> {
    fn resolved<'a>(mounts: &[(&'a str, &Path)]) -> Vec<(&'a str, PathBuf)> {
        mounts.iter().map(|(n, p)| (*n, util::resolve(p))).collect()
    }
    fn refs<'a>(mounts: &'a [(&'a str, PathBuf)]) -> Vec<(&'a str, &'a Path)> {
        mounts.iter().map(|(n, p)| (*n, p.as_path())).collect()
    }
    let (writable, protected) = (resolved(writable), resolved(protected));
    sandbox::check_writable_mounts_disjoint(&refs(&writable), &refs(&protected))
}

/// A call's default time limit and the longest one it may ask for.
fn turn_limits(profile: &Map<String, Value>) -> Result<(f64, f64), String> {
    let limit = |key, default| match profile.get(key) {
        None => Some(default),
        Some(Value::Number(n)) => n.as_f64(),
        Some(_) => None,
    };
    match (limit("turn_timeout", 60.0), limit("max_turn_timeout", 1800.0)) {
        (Some(turn), Some(max)) if 0.0 < turn && turn <= max && max <= cli::LONGEST_TURN_TIMEOUT => Ok((turn, max)),
        _ => Err(format!(
            "turn_timeout must be a positive number of seconds, no more than max_turn_timeout, \
             which is at most {} (7 days)",
            cli::LONGEST_TURN_TIMEOUT
        )),
    }
}

fn load_profile(path: &Path) -> Result<Profile, String> {
    let profile_path = fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let raw = fs::read_to_string(&profile_path).map_err(|e| format!("{}: {e}", profile_path.display()))?;
    let profile = match serde_json::from_str(&raw) {
        Ok(Value::Object(m)) if m.keys().all(|k| FIELDS.contains(&k.as_str())) && m.get("format").and_then(Value::as_f64) == Some(1.0) => m,
        _ => return Err("Unsupported server profile".into()),
    };
    let field = |key: &str| absolute(profile.get(key), key);
    let (repo, data, depot, ceiling, runtime_root) = (field("repo")?, field("data")?, field("depot")?, field("ceiling")?, field("runtime_root")?);
    // Configuration, source, packages and saved worlds cannot enter a worker's
    // writable workspace. These checks happen before creating any directories.
    disjoint(
        &[("data", &data)],
        &[("repo", &repo), ("depot", &depot), ("ceiling", &ceiling), ("runtime_root", &runtime_root), ("profile", &profile_path)],
    )?;
    disjoint(
        &[("workspace", &data.join("workspace"))],
        &[("state", &data.join("state")), ("worlds", &data.join("worlds")), ("packages", &data.join("packages"))],
    )?;
    let host_commands = optional(&profile, "host_commands")?;
    if let Some(p) = &host_commands {
        disjoint(&[("data", &data)], &[("host_commands", p)])?;
    }
    let repair_config = optional(&profile, "repair_config")?;
    if let Some(p) = &repair_config {
        disjoint(&[("data", &data)], &[("repair_config", p)])?;
    }
    let mut patch_roots = Map::new();
    match profile.get("patch_roots") {
        None => {}
        Some(Value::Object(m)) if m.keys().all(|k| !k.is_empty()) => {
            for (target, value) in m {
                let root = absolute(Some(value), "patch root")?;
                let mut protected = vec![("data", data.as_path()), ("depot", &depot), ("profile", &profile_path), ("ceiling", &ceiling)];
                if let Some(p) = &repair_config {
                    protected.push(("repair_config", p));
                }
                disjoint(&[("patch root", &root)], &protected)?;
                patch_roots.insert(target.clone(), text(&root)?.into());
            }
        }
        Some(_) => return Err("patch_roots must map target names to absolute paths".into()),
    }
    let read_roots = match profile.get("read_roots") {
        None => vec![],
        Some(Value::Array(roots)) => roots.iter().map(|r| absolute(Some(r), "read root")).collect::<Result<_, _>>()?,
        Some(_) => return Err("read_roots must be a list".into()),
    };
    let turn_limits = turn_limits(&profile)?;
    Ok(Profile { profile: profile_path, repo, data, depot, ceiling, runtime_root, host_commands, repair_config, patch_roots, read_roots, turn_limits })
}

/// The runtime's whole environment. Transport and model credentials, and any
/// ambient Palette setting, are not inherited.
fn environment(p: &Profile) -> Result<Vec<(String, String)>, String> {
    let mut env: Vec<(String, String)> =
        ["HOME", "PATH", "LANG", "LC_ALL"].iter().filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))).collect();
    let mut set = |k: &str, p: &Path| -> Result<(), String> {
        env.push((k.into(), text(p)?.into()));
        Ok(())
    };
    let d = &p.data;
    set("PALETTE_REPO", &p.repo)?;
    set("PALETTE_HOST", &p.repo.join(HOST))?;
    set("PALETTE_DATA_HOME", d)?;
    set("OPERATOR_WORKSPACE", &d.join("workspace"))?;
    set("PALETTE_STATE_DIR", &d.join("state"))?;
    set("PALETTE_WORKSPACE_STATE_ROOT", &d.join("worlds"))?;
    set("PALETTE_SCRATCH_ROOT", &d.join("scratch"))?;
    set("PALETTE_PACKAGE_DEPOT", &d.join("packages"))?;
    set("PALETTE_CAPABILITY_CEILING", &p.ceiling)?;
    set("PALETTE_RUNTIME_ROOT", &p.runtime_root)?;
    set("JULIA_DEPOT_PATH", &p.depot)?;
    if let Some(h) = &p.host_commands {
        set("PALETTE_HOST_COMMANDS", h)?;
    }
    if let Some(r) = &p.repair_config {
        set("PALETTE_REPAIR_CONFIG", r)?;
    }
    if !p.read_roots.is_empty() {
        let roots = p.read_roots.iter().map(|r| text(r)).collect::<Result<Vec<_>, _>>()?;
        env.push(("PALETTE_READ_ROOTS".into(), roots.join(":")));
    }
    if !p.patch_roots.is_empty() {
        env.push(("PALETTE_PATCH_ROOTS".into(), Value::Object(p.patch_roots.clone()).to_string()));
    }
    env.push(("JULIA_PKG_OFFLINE".into(), "true".into()));
    env.push(("PALETTE_TURN_TIMEOUT".into(), util::fmt_g(p.turn_limits.0)));
    env.push(("PALETTE_MAX_TURN_TIMEOUT".into(), util::fmt_g(p.turn_limits.1)));
    Ok(env)
}

/// First-time provisioning is an administrator action, before the sandbox
/// opens. Interactive workers keep offline package networking afterwards.
fn prepare(p: &Profile) -> Result<(), String> {
    let mut tools = HashMap::new();
    for name in ["julia", "cargo", "bwrap"] {
        tools.insert(name, util::which(name).ok_or_else(|| format!("Missing server prerequisite: {name}"))?);
    }
    for dir in [&p.data, &p.depot, &p.runtime_root] {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut env = environment(p)?;
    env.retain(|(k, _)| k != "JULIA_PKG_OFFLINE");
    env.push(("JULIA_PKG_OFFLINE".into(), "false".into()));
    let repo = text(&p.repo)?;
    let host = p.repo.join(HOST);
    let steps: [(&Path, Vec<String>); 3] = [
        (&tools["julia"], vec!["--startup-file=no".into(), format!("--project={repo}"), "-e".into(), "using Pkg; Pkg.instantiate()".into()]),
        (&tools["cargo"], vec!["build".into(), "--release".into(), "--locked".into(), "--manifest-path".into(), format!("{repo}/runtime/host/Cargo.toml")]),
        (&host, vec!["prepare".into(), "--project-dir".into(), repo.into()]),
    ];
    for (bin, args) in steps {
        let status = Command::new(bin).args(&args).env_clear().envs(env.iter().cloned()).current_dir(&p.repo).stdin(Stdio::null()).status()
            .map_err(|e| format!("{}: {e}", bin.display()))?;
        if !status.success() {
            return Err(format!("{} {} failed: {status}", bin.display(), util::shjoin(&args)));
        }
    }
    Ok(())
}

/// Writes a new profile with the given paths. An administrator's existing
/// profile is never replaced.
fn init(path: &Path, f: &HashMap<String, String>) -> Result<(), String> {
    let get = |k: &str, default: &str| f.get(k).cloned().unwrap_or_else(|| default.into());
    let fields = [
        ("repo", get("repo", "/opt/palette")),
        ("data", get("data", "/var/lib/palette")),
        ("depot", get("depot", "/var/cache/palette/depot")),
        ("ceiling", get("ceiling", "/etc/palette/ceiling.json")),
        ("runtime_root", get("runtime-root", "/opt/palette-capabilities")),
    ];
    let mut body = String::from("{\n  \"format\": 1,\n");
    for (k, v) in &fields {
        body += &format!("  \"{k}\": {},\n", Value::from(v.as_str()));
    }
    body += "  \"read_roots\": []\n}\n";
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(at)?;
    file.write_all(body.as_bytes()).map_err(at)?;
    file.set_permissions(fs::Permissions::from_mode(0o600)).map_err(at)
}

fn valid_ssh_target(t: &str) -> bool {
    let mut chars = t.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric()) && chars.all(|c| c.is_ascii_alphanumeric() || "_.@-".contains(c))
}

pub fn main(argv: &[String]) -> Result<i32, String> {
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(0);
    }
    let usage = || "usage: palette-host serve init|inspect|prepare|launch|client --profile P; see palette-host serve --help".to_string();
    let (action, rest) = argv.split_first().ok_or_else(usage)?;
    if !["init", "inspect", "prepare", "launch", "client"].contains(&action.as_str()) {
        return Err(usage());
    }
    let f = crate::flags(rest, &[])?;
    if let Some(unknown) = f.keys().find(|k| !VALUE_FLAGS.contains(&k.as_str())) {
        return Err(format!("serve: unknown option --{unknown}; see palette-host serve --help"));
    }
    let path = PathBuf::from(f.get("profile").ok_or("serve: --profile is required")?);
    if action == "init" {
        init(&path, &f)?;
    }
    let p = match load_profile(&path) {
        Ok(p) => p,
        Err(e) if action == "init" => {
            // A profile that cannot be used is not left behind to be refused later.
            let _ = fs::remove_file(&path);
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    match action.as_str() {
        "init" | "inspect" => println!(
            "{}",
            json!({"profile": text(&p.profile)?, "repo": text(&p.repo)?, "workspace": text(&p.data.join("workspace"))?,
                   "state": text(&p.data.join("state"))?, "compute": "server", "edge_required": false})
        ),
        "prepare" => prepare(&p)?,
        "client" => {
            let target = f.get("ssh-target").filter(|t| valid_ssh_target(t)).ok_or("client requires a valid --ssh-target")?;
            let command = util::shjoin(&[text(&p.repo.join(HOST))?, "serve", "launch", "--profile", text(&p.profile)?]);
            let entry = json!({"mcpServers": {"palette": {"command": "ssh", "args": ["-T", "--", target, command]}}});
            println!("{}", serde_json::to_string_pretty(&entry).expect("JSON values serialize"));
        }
        _ => {
            let host = p.repo.join(HOST);
            let e = Command::new(&host).args(["mcp", "start"]).env_clear().envs(environment(&p)?).exec();
            return Err(format!("{}: {e}", host.display()));
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Fixture { root: util::mkdtemp("palette-serve-").unwrap() }
        }

        fn base(&self) -> Map<String, Value> {
            let r = |s: &str| Value::from(self.root.join(s).to_str().unwrap());
            let v = json!({"format": 1, "repo": r("repo"), "data": r("data"), "depot": r("depot"),
                           "ceiling": r("ceiling.json"), "runtime_root": r("capabilities")});
            v.as_object().unwrap().clone()
        }

        fn load(&self, overrides: Value) -> Result<Profile, String> {
            let mut profile = self.base();
            profile.extend(overrides.as_object().unwrap().clone());
            let path = self.root.join("profile.json");
            fs::write(&path, Value::Object(profile).to_string()).unwrap();
            load_profile(&path)
        }

        fn hidden(&self) -> Value {
            Value::from(self.root.join("data/hidden").to_str().unwrap())
        }
    }

    fn env_of(p: &Profile) -> HashMap<String, String> {
        environment(p).unwrap().into_iter().collect()
    }

    #[test]
    fn profile_rejects_writable_authority_overlap() {
        let f = Fixture::new();
        for field in ["repo", "depot", "ceiling", "runtime_root"] {
            let err = f.load(json!({field: f.hidden()})).err().unwrap_or_else(|| panic!("{field} inside data was accepted"));
            assert!(err.contains("overlaps protected"), "{field}: {err}");
        }
        for field in ["host_commands", "repair_config"] {
            assert!(f.load(json!({field: f.hidden()})).is_err(), "{field} inside data was accepted");
        }
        let root = f.root.join("src");
        assert!(f.load(json!({"patch_roots": {"t": f.root.join("data").to_str().unwrap()}})).is_err());
        assert!(f.load(json!({"patch_roots": {"t": "relative"}})).unwrap_err().contains("absolute"));
        assert!(f.load(json!({"patch_roots": {"": root.to_str().unwrap()}})).unwrap_err().contains("patch_roots must map"));
        assert!(f.load(json!({"read_roots": "/srv"})).unwrap_err().contains("must be a list"));
        assert!(f.load(json!({"repo": "opt/palette"})).unwrap_err().contains("repo must be an absolute server path"));
        assert_eq!(f.load(json!({"extra": 1})).err().unwrap(), "Unsupported server profile");
        assert_eq!(f.load(json!({"format": 2})).err().unwrap(), "Unsupported server profile");
        let mut no_repo = f.base();
        no_repo.remove("repo");
        fs::write(f.root.join("p.json"), Value::Object(no_repo).to_string()).unwrap();
        assert!(load_profile(&f.root.join("p.json")).unwrap_err().contains("repo must be"));

        // A patch root may hold source, and is passed as JSON.
        let p = f.load(json!({"patch_roots": {"t": root.to_str().unwrap()}, "host_commands": "", "read_roots": ["/srv/a", "/srv/b"]})).unwrap();
        let env = env_of(&p);
        assert_eq!(serde_json::from_str::<Value>(&env["PALETTE_PATCH_ROOTS"]).unwrap(), json!({"t": root.to_str().unwrap()}));
        assert_eq!(env["PALETTE_READ_ROOTS"], "/srv/a:/srv/b");
        assert!(!env.contains_key("PALETTE_HOST_COMMANDS"));
    }

    #[test]
    fn environment_names_only_the_profile() {
        let f = Fixture::new();
        let p = f.load(json!({})).unwrap();
        let env = env_of(&p);
        let data = f.root.join("data");
        let at = |s: &str| data.join(s).to_str().unwrap().to_string();
        assert_eq!(env["PALETTE_PACKAGE_DEPOT"], at("packages"));
        assert_eq!(env["OPERATOR_WORKSPACE"], at("workspace"));
        assert_eq!(env["PALETTE_HOST"], f.root.join("repo").join(HOST).to_str().unwrap());
        assert_eq!(env["JULIA_DEPOT_PATH"], f.root.join("depot").to_str().unwrap());
        assert_eq!(env["JULIA_PKG_OFFLINE"], "true");
        let inherited = ["HOME", "PATH", "LANG", "LC_ALL"];
        let named = [
            "PALETTE_REPO", "PALETTE_HOST", "PALETTE_DATA_HOME", "OPERATOR_WORKSPACE", "PALETTE_STATE_DIR",
            "PALETTE_WORKSPACE_STATE_ROOT", "PALETTE_SCRATCH_ROOT", "PALETTE_PACKAGE_DEPOT", "PALETTE_CAPABILITY_CEILING",
            "PALETTE_RUNTIME_ROOT", "JULIA_DEPOT_PATH", "JULIA_PKG_OFFLINE", "PALETTE_TURN_TIMEOUT", "PALETTE_MAX_TURN_TIMEOUT",
        ];
        assert!(env.keys().all(|k| inherited.contains(&k.as_str()) || named.contains(&k.as_str())), "{:?}", env.keys());
        assert!(named.iter().all(|k| env.contains_key(*k)));
    }

    #[test]
    fn profile_sets_call_time_limits() {
        let f = Fixture::new();
        let limits = |overrides: Value| {
            let env = env_of(&f.load(overrides).unwrap());
            (env["PALETTE_TURN_TIMEOUT"].clone(), env["PALETTE_MAX_TURN_TIMEOUT"].clone())
        };
        assert_eq!(limits(json!({})), ("60".into(), "1800".into()));
        assert_eq!(limits(json!({"turn_timeout": 120, "max_turn_timeout": 7200.5})), ("120".into(), "7200.5".into()));
        assert_eq!(limits(json!({"max_turn_timeout": 604799.75})).1, "604799.75");
        for bad in [
            json!({"turn_timeout": 0}), json!({"turn_timeout": true}), json!({"turn_timeout": "90"}),
            json!({"turn_timeout": 120, "max_turn_timeout": 60}), json!({"max_turn_timeout": 30}),
            json!({"max_turn_timeout": 604801}),
        ] {
            let err = f.load(bad.clone()).err().unwrap_or_else(|| panic!("{bad} was accepted"));
            assert!(err.contains("no more than max_turn_timeout"), "{bad}: {err}");
        }
    }

    #[test]
    fn ssh_targets_are_one_plain_word() {
        assert!(valid_ssh_target("palette@server"));
        assert!(valid_ssh_target("srv-1.example"));
        for bad in ["", "-oProxyCommand=x", "a b", "a;b", "@host", "a$(b)"] {
            assert!(!valid_ssh_target(bad), "{bad}");
        }
    }
}
