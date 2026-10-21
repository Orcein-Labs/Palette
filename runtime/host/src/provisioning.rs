//! The operator's trusted configuration: the
//! capability ceiling, the TeamLab grant, the paths a writable root must
//! not overlap, and the MCP client's agent name.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::{broker, sandbox, teamlab, util};

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

pub(crate) fn julia_identifier(name: &str) -> bool {
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
pub(crate) fn validate_ceiling(value: &Value) -> Result<(), String> {
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

fn read_json(path: &Path, var: &str, shown: &str) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {var} {shown}: {}", util::py_os_error(&e, path)))?;
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON in {var} {shown}: {e}"))
}

/// The host-owned ceiling: the JSON file `PALETTE_CAPABILITY_CEILING` names
/// (unset or blank grants nothing), with the legacy `PALETTE_HOST_COMMANDS`
/// file merged into its host commands, validated.
pub(crate) fn operator_ceiling(var: impl Fn(&str) -> Option<String>) -> Result<Value, String> {
    let mut parsed = match var("PALETTE_CAPABILITY_CEILING").filter(|s| !s.trim().is_empty()) {
        None => json!({}),
        Some(selected) => {
            let path = util::expand_user(&selected);
            read_json(&path, "PALETTE_CAPABILITY_CEILING", &path.display().to_string())?
        }
    };
    let object = parsed.as_object_mut().ok_or("capability ceiling must be a JSON object")?;
    if let Some(legacy_path) = var("PALETTE_HOST_COMMANDS").filter(|s| !s.is_empty()) {
        let legacy = read_json(&util::expand_user(&legacy_path), "PALETTE_HOST_COMMANDS", &legacy_path)?;
        let legacy = legacy.as_object().ok_or("PALETTE_HOST_COMMANDS must contain an object")?;
        let host = object.entry("host_command").or_insert_with(|| json!({}));
        let host = host.as_object_mut().ok_or("host_command grant must be an object")?;
        let commands = host.entry("commands").or_insert_with(|| json!({}));
        let commands = commands.as_object_mut().ok_or("host_command.commands must be an object")?;
        for (name, spec) in legacy {
            if commands.get(name).is_some_and(|old| old != spec) {
                return Err(format!("PALETTE_HOST_COMMANDS conflicts with host_command.commands entry {}", util::py_str(name)));
            }
            commands.insert(name.clone(), spec.clone());
        }
    }
    validate_ceiling(&parsed)?;
    Ok(parsed)
}

/// `ceiling` with the user's TeamLab shop granted, unless it names its own
/// grant or the shop is off (see `teamlab::dir_from_env`).
pub(crate) fn with_teamlab(mut ceiling: Value, var: impl Fn(&str) -> Option<String>) -> Result<Value, String> {
    if ceiling.get("teamlab").is_some() {
        return Ok(ceiling);
    }
    let Some(dir) = teamlab::dir_from_env(var)? else { return Ok(ceiling) };
    let object = ceiling.as_object_mut().ok_or("capability ceiling must be a JSON object")?;
    // As Python's `str(Path(...))`: no trailing or doubled slashes.
    let dir: PathBuf = dir.components().collect();
    object.insert("teamlab".into(), json!({"dir": dir.display().to_string()}));
    validate_ceiling(&ceiling)?;
    Ok(ceiling)
}

const AGENT_MARKS: &[(&str, &[&str])] =
    &[("claude", &["claude"]), ("codex", &["codex"]), ("grok", &["grok"]), ("gemini", &["gemini", "antigravity"])];

/// The TeamLab agent name for an MCP client's `clientInfo.name`. Known
/// clients map to one name each (`claude-code` is `claude`); any other name
/// is reduced to `[a-z0-9._-]`. It labels who published or sent something;
/// it authenticates no one.
pub(crate) fn agent_name(client_name: Option<&Value>) -> Option<String> {
    let name = client_name?.as_str()?.trim().to_lowercase();
    if name == "palette-router" {
        return None;
    }
    if let Some((agent, _)) = AGENT_MARKS.iter().find(|(_, marks)| marks.iter().any(|m| name.contains(m))) {
        return Some(agent.to_string());
    }
    let mut reduced = String::new();
    for c in name.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
            reduced.push(c);
        } else if !reduced.ends_with('\u{1}') {
            // A run of other characters becomes one '-'; the marker keeps
            // a run apart from a '-' the name already had.
            reduced.push('\u{1}');
        }
    }
    let reduced = reduced.replace('\u{1}', "-");
    let trim = |c: char| matches!(c, '-' | '.' | '_');
    let cut: String = reduced.trim_matches(trim).chars().take(32).collect();
    let cut = cut.trim_end_matches(trim);
    (!cut.is_empty() && cut != "all").then(|| cut.to_string())
}

fn command_path_set(checked: &Value) -> BTreeSet<PathBuf> {
    sandbox::protected_command_paths(checked).paths.iter().map(|p| util::resolve(p)).collect()
}

/// The trusted config and store files and the host-command paths, resolved
/// and sorted. A writable path may not equal, contain or lie inside any of
/// them (`require_disjoint`).
pub(crate) fn protected_host_paths(
    ceiling: &Value,
    include_package_depot: bool,
    include_runtime_root: bool,
    var: impl Fn(&str) -> Option<String>,
) -> Result<Vec<PathBuf>, String> {
    validate_ceiling(ceiling)?;
    let mut paths = BTreeSet::new();
    for key in ["PALETTE_CAPABILITY_CEILING", "PALETTE_HOST_COMMANDS", "PALETTE_REPAIR_CONFIG", "PALETTE_PACKAGE_DEPOT", "PALETTE_RUNTIME_ROOT"] {
        if (key == "PALETTE_PACKAGE_DEPOT" && !include_package_depot) || (key == "PALETTE_RUNTIME_ROOT" && !include_runtime_root) {
            continue;
        }
        if let Some(configured) = var(key).filter(|s| !s.is_empty()) {
            paths.insert(util::resolve(&util::expand_user(&configured)));
        }
    }
    paths.extend(sandbox::parse_read_roots(var("PALETTE_READ_ROOTS").as_deref())?);
    if let Some(tools) = var("PALETTE_TRUSTED_TOOLS").filter(|s| !s.is_empty()) {
        paths.insert(util::resolve(&util::expand_user(&tools)));
    }
    if let Some(dir) = ceiling.get("teamlab").and_then(|g| g.get("dir")).and_then(Value::as_str) {
        paths.insert(util::resolve(Path::new(dir)));
    }
    paths.extend(command_path_set(ceiling));
    Ok(sorted_by_text(paths))
}

/// The directories that hold host-command paths. A writable root equal to
/// one could rename the command's binary, artifact or working directory; a
/// root that contains one already contains the command path.
pub(crate) fn command_parents(ceiling: &Value) -> Result<Vec<PathBuf>, String> {
    validate_ceiling(ceiling)?;
    Ok(sorted_by_text(command_path_set(ceiling).iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect()))
}

/// Sorted as Python sorts paths by `str`: byte order of the whole text, not
/// component order.
fn sorted_by_text(paths: BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = paths.into_iter().collect();
    v.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    v
}

/// Refuse a writable path that equals, contains or lies inside a protected
/// one, or that equals one of `parents` (`command_parents`).
pub(crate) fn require_disjoint<W: AsRef<Path>, P: AsRef<Path>, Q: AsRef<Path>>(
    writable: &[W],
    protected: &[P],
    parents: &[Q],
) -> Result<(), String> {
    let parents: HashSet<PathBuf> = parents.iter().map(|p| util::resolve(p.as_ref())).collect();
    for w in writable {
        let left = util::resolve(w.as_ref());
        if parents.contains(&left) {
            return Err(format!("Writable path {} must not be the parent of a host-command path", left.display()));
        }
        for p in protected {
            let right = util::resolve(p.as_ref());
            if left.starts_with(&right) || right.starts_with(&left) {
                return Err(format!("Writable path {} must not overlap protected path {}", left.display(), right.display()));
            }
        }
    }
    Ok(())
}

/// The options whose next argv token names a fixed artifact.
const PATH_ARGUMENT_OPTIONS: &[&str] = &[
    "--executable", "--artifact", "--library", "--library-dir", "--model", "--model-path", "--weights", "--weights-file", "--weights-path",
];

/// The artifact paths named in a host command's frozen argv: absolute tokens,
/// absolute `--option=` values and the absolute token after a path option,
/// with the runner's `--fixed-arg` wrappers unwrapped first.
pub(crate) fn configured_path_arguments(argv: &[&str]) -> Vec<String> {
    let (mut direct, mut fixed) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < argv.len() {
        let item = argv[i];
        if item == "--fixed-arg" && i + 1 < argv.len() {
            fixed.push(argv[i + 1]);
            i += 2;
            continue;
        }
        if let Some(value) = item.strip_prefix("--fixed-arg=") {
            fixed.push(value);
        } else {
            direct.push(item);
        }
        i += 1;
    }
    let mut result = Vec::new();
    for tokens in [direct, fixed] {
        let mut i = 0;
        while i < tokens.len() {
            let item = tokens[i];
            if item.starts_with('/') {
                result.push(item.to_string());
            }
            match item.split_once('=') {
                Some((_, value)) if value.starts_with('/') => result.push(value.to_string()),
                Some(_) => {}
                None if PATH_ARGUMENT_OPTIONS.contains(&item) && i + 1 < tokens.len() => {
                    if tokens[i + 1].starts_with('/') {
                        result.push(tokens[i + 1].to_string());
                    }
                    i += 1;
                }
                None => {}
            }
            i += 1;
        }
    }
    result
}

/// The version every adapter reports: the top-level `version` in the
/// checkout's Project.toml.
pub(crate) fn palette_version(repo: &Path) -> Result<String, String> {
    let path = repo.join("Project.toml");
    let text = fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    text.lines()
        .map(str::trim)
        .take_while(|l| !l.starts_with('['))
        .find_map(|l| {
            let (key, value) = l.split_once('=')?;
            let value = value.trim();
            (key.trim() == "version" && value.len() >= 2 && value.starts_with('"') && value.ends_with('"'))
                .then(|| value[1..value.len() - 1].to_string())
        })
        .ok_or_else(|| format!("{} has no top-level version", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn the_operator_ceiling_reads_its_file_and_merges_legacy_host_commands() {
        let dir = util::mkdtemp("palette-prov-test-").unwrap();
        let file = |name: &str, body: &str| {
            let p = dir.join(name);
            fs::write(&p, body).unwrap();
            p.display().to_string()
        };
        assert_eq!(operator_ceiling(vars(&[])).unwrap(), json!({}));
        assert_eq!(operator_ceiling(vars(&[("PALETTE_CAPABILITY_CEILING", "  ")])).unwrap(), json!({}));
        let missing = dir.join("missing.json").display().to_string();
        assert_eq!(
            operator_ceiling(vars(&[("PALETTE_CAPABILITY_CEILING", &missing)])).unwrap_err(),
            format!("cannot read PALETTE_CAPABILITY_CEILING {missing}: [Errno 2] No such file or directory: '{missing}'")
        );
        let bad = file("bad.json", "{");
        assert!(operator_ceiling(vars(&[("PALETTE_CAPABILITY_CEILING", &bad)])).unwrap_err().starts_with(&format!("invalid JSON in PALETTE_CAPABILITY_CEILING {bad}: ")));
        let list = file("list.json", "[]");
        assert_eq!(operator_ceiling(vars(&[("PALETTE_CAPABILITY_CEILING", &list)])).unwrap_err(), "capability ceiling must be a JSON object");

        let a = json!({"argv": ["/bin/true"], "cwd": "/"});
        let b = json!({"argv": ["/bin/false"], "cwd": "/"});
        let ceiling = file("ceiling.json", &json!({"host_command": {"commands": {"a": a}}, "spawn_child_worker": {}}).to_string());
        let legacy = file("legacy.json", &json!({"a": a, "b": b}).to_string());
        assert_eq!(
            operator_ceiling(vars(&[("PALETTE_CAPABILITY_CEILING", &ceiling), ("PALETTE_HOST_COMMANDS", &legacy)])).unwrap(),
            json!({"host_command": {"commands": {"a": a, "b": b}}, "spawn_child_worker": {}})
        );
        assert_eq!(
            operator_ceiling(vars(&[("PALETTE_HOST_COMMANDS", &legacy)])).unwrap(),
            json!({"host_command": {"commands": {"a": a, "b": b}}})
        );
        let clash = file("clash.json", &json!({"a": b}).to_string());
        assert_eq!(
            operator_ceiling(vars(&[("PALETTE_CAPABILITY_CEILING", &ceiling), ("PALETTE_HOST_COMMANDS", &clash)])).unwrap_err(),
            "PALETTE_HOST_COMMANDS conflicts with host_command.commands entry 'a'"
        );
        let legacy_list = file("legacy-list.json", "[]");
        assert_eq!(operator_ceiling(vars(&[("PALETTE_HOST_COMMANDS", &legacy_list)])).unwrap_err(), "PALETTE_HOST_COMMANDS must contain an object");
        let odd_grant = file("odd-grant.json", r#"{"host_command": []}"#);
        assert_eq!(
            operator_ceiling(vars(&[("PALETTE_CAPABILITY_CEILING", &odd_grant), ("PALETTE_HOST_COMMANDS", &legacy)])).unwrap_err(),
            "host_command grant must be an object"
        );
        let odd_commands = file("odd-commands.json", r#"{"host_command": {"commands": 1}}"#);
        assert_eq!(
            operator_ceiling(vars(&[("PALETTE_CAPABILITY_CEILING", &odd_commands), ("PALETTE_HOST_COMMANDS", &legacy)])).unwrap_err(),
            "host_command.commands must be an object"
        );
        let unknown = file("unknown.json", r#"{"zz": {}}"#);
        assert_eq!(operator_ceiling(vars(&[("PALETTE_CAPABILITY_CEILING", &unknown)])).unwrap_err(), "unknown capability categories: ['zz']");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn teamlab_is_granted_unless_the_ceiling_names_it_or_it_is_off() {
        let home = [("HOME", "/h")];
        assert_eq!(with_teamlab(json!({}), vars(&home)).unwrap(), json!({"teamlab": {"dir": "/h/.local/share/palette/teamlab"}}));
        assert_eq!(with_teamlab(json!({"teamlab": {"dir": "/mine"}}), vars(&home)).unwrap(), json!({"teamlab": {"dir": "/mine"}}));
        for off in ["off", "", " "] {
            assert_eq!(with_teamlab(json!({}), vars(&[("HOME", "/h"), ("PALETTE_TEAMLAB_DIR", off)])).unwrap(), json!({}));
        }
        assert_eq!(
            with_teamlab(json!({"spawn_child_worker": {}}), vars(&[("PALETTE_TEAMLAB_DIR", "/lab")])).unwrap(),
            json!({"spawn_child_worker": {}, "teamlab": {"dir": "/lab"}})
        );
        let granted = with_teamlab(json!({}), vars(&[("PALETTE_TEAMLAB_DIR", "/lab")])).unwrap();
        assert!(protected_host_paths(&granted, false, false, vars(&[])).unwrap().contains(&PathBuf::from("/lab")));
        assert_eq!(with_teamlab(json!({}), vars(&[("HOME", "/h"), ("PALETTE_TEAMLAB_DIR", "/lab//x/")])).unwrap(), json!({"teamlab": {"dir": "/lab/x"}}));
        assert_eq!(
            with_teamlab(json!({}), vars(&[("PALETTE_TEAMLAB_DIR", "lab")])).unwrap_err(),
            "PALETTE_TEAMLAB_DIR must be an absolute path, not 'lab'"
        );
    }

    #[test]
    fn agent_names_read_as_the_python_router_derived_them() {
        let name = |s: &str| agent_name(Some(&json!(s)));
        assert_eq!(name("claude-code"), Some("claude".into()));
        assert_eq!(name("  Codex-CLI "), Some("codex".into()));
        assert_eq!(name("grok"), Some("grok".into()));
        assert_eq!(name("Antigravity"), Some("gemini".into()));
        assert_eq!(name("gemini-cli"), Some("gemini".into()));
        assert_eq!(name("palette-router"), None);
        assert_eq!(name("My Editor!! v2"), Some("my-editor-v2".into()));
        assert_eq!(name("--a--b__"), Some("a--b".into()));
        assert_eq!(name("ALL"), None);
        assert_eq!(name("  !! "), None);
        assert_eq!(name(&format!("{}-xyz", "a".repeat(31))), Some("a".repeat(31)));
        assert_eq!(name("zé"), Some("z".into()));
        assert_eq!(name("Kilo Code"), Some("kilo-code".into()));
        assert_eq!(name("_odd.name_"), Some("odd.name".into()));
        assert_eq!(name(&"x".repeat(40)), Some("x".repeat(32)));
        assert_eq!(name("All."), None);
        assert_eq!(name("---"), None);
        assert_eq!(agent_name(Some(&json!(1))), None);
        assert_eq!(agent_name(None), None);
    }

    #[test]
    fn disjoint_paths_refuse_overlap_and_command_parents() {
        let none: [&str; 0] = [];
        assert!(require_disjoint(&["/a/b"], &["/a/c"], &none).is_ok());
        assert!(require_disjoint(&["/a"], &["/b"], &["/a/x"]).is_ok());
        assert_eq!(require_disjoint(&["/a/b"], &["/a"], &none).unwrap_err(), "Writable path /a/b must not overlap protected path /a");
        assert_eq!(require_disjoint(&["/a"], &["/a/b"], &none).unwrap_err(), "Writable path /a must not overlap protected path /a/b");
        assert_eq!(require_disjoint(&["/a/./b/../b"], &["/a/b"], &none).unwrap_err(), "Writable path /a/b must not overlap protected path /a/b");
        assert!(require_disjoint(&["/ab"], &["/a"], &none).is_ok());
        assert_eq!(require_disjoint(&["/a"], &none, &["/a"]).unwrap_err(), "Writable path /a must not be the parent of a host-command path");
    }

    #[test]
    fn protected_paths_gather_config_stores_and_command_paths() {
        let ceiling = json!({
            "host_command": {"commands": {"run": {"argv": ["/opt/bin/run", "--model", "/m/w.bin", "--fixed-arg=--tok=/t/tok.json", "rel"], "cwd": "/work", "lock": "/locks/run.lock"}}},
            "teamlab": {"dir": "/shop"},
        });
        let env = vars(&[
            ("HOME", "/h"),
            ("PALETTE_CAPABILITY_CEILING", "/etc/p/ceiling.json"),
            ("PALETTE_PACKAGE_DEPOT", "/depot"),
            ("PALETTE_RUNTIME_ROOT", "/rt"),
            ("PALETTE_TRUSTED_TOOLS", "/tools"),
        ]);
        let text = |v: Vec<PathBuf>| v.iter().map(|p| p.display().to_string()).collect::<Vec<_>>();
        assert_eq!(
            text(protected_host_paths(&ceiling, true, false, &env).unwrap()),
            ["/depot", "/etc/p/ceiling.json", "/locks/run.lock", "/m/w.bin", "/opt/bin/run", "/shop", "/t/tok.json", "/tools", "/work"]
        );
        assert_eq!(
            text(protected_host_paths(&ceiling, false, true, &env).unwrap()),
            ["/etc/p/ceiling.json", "/locks/run.lock", "/m/w.bin", "/opt/bin/run", "/rt", "/shop", "/t/tok.json", "/tools", "/work"]
        );
        assert_eq!(text(command_parents(&ceiling).unwrap()), ["/", "/locks", "/m", "/opt/bin", "/t"]);
        assert!(protected_host_paths(&json!({"zz": {}}), true, true, &env).is_err());
    }

    #[test]
    fn nested_operator_artifacts_are_protected_but_caller_input_is_not() {
        let root = fs::canonicalize(util::mkdtemp("palette-protected-artifacts-").unwrap()).unwrap();
        let p = |rel: &str| root.join(rel);
        let s = |p: &Path| p.display().to_string();
        let (model, weights, artifact, tokenizer, config) =
            (p("models/runner.gguf"), p("weights/fixed.safetensors"), p("assets/tokenizer.json"), p("assets/vocab.model"), p("config/runner.toml"));
        let (library, executable, lock) = (p("lib"), p("bin/runner"), p("locks/gpu.lock"));
        let paths = [&model, &weights, &artifact, &tokenizer, &config, &library, &executable, &lock];
        for path in paths {
            fs::create_dir_all(if *path == library { path.as_path() } else { path.parent().unwrap() }).unwrap();
        }
        let argv = ["/opt/palette-host".to_string(), "toolchain".into(), "run".into(),
            "--executable".into(), s(&executable), format!("--executable={}", s(&executable)), format!("--model={}", s(&model)),
            "--weights".into(), s(&weights), format!("--weights={}", s(&weights)),
            "--artifact".into(), s(&artifact), format!("--artifact={}", s(&artifact)),
            format!("--tokenizer={}", s(&tokenizer)), format!("--config={}", s(&config)),
            "--library-dir".into(), s(&library), format!("--library-dir={}", s(&library)),
            format!("--fixed-arg=--model={}", s(&model)), format!("--fixed-arg=--tokenizer={}", s(&tokenizer)),
            format!("--fixed-arg=--config={}", s(&config)), "--fixed-arg=--weights".into(), format!("--fixed-arg={}", s(&weights)),
            "--fixed-arg".into(), "--library-dir".into(), "--fixed-arg".into(), s(&library)];
        let ceiling = json!({"host_command": {"commands": {
            "inference": {"argv": argv, "cwd": s(&root), "lock": s(&lock), "extra_args": true}}}});
        let protected = protected_host_paths(&ceiling, false, false, vars(&[("HOME", "/h")])).unwrap();
        let parents = command_parents(&ceiling).unwrap();
        for path in paths {
            assert!(protected.contains(path), "{} is not protected", path.display());
            assert!(parents.contains(&path.parent().unwrap().to_path_buf()));
            assert!(require_disjoint(&[path.parent().unwrap()], &protected, &parents).unwrap_err().contains("parent of a host-command path"));
        }
        // A sibling inside a command's parent cannot rename the command path,
        // so it may be writable.
        let sibling = root.parent().unwrap().join(format!("{}-sibling", root.file_name().unwrap().to_string_lossy()));
        assert!(require_disjoint(&[&sibling], &protected, &parents).is_ok());
        assert!(!protected.contains(&root.parent().unwrap().to_path_buf()));
        // A path the caller passes at run time is input, not configuration.
        assert!(!protected.contains(&root.join("caller-selected-model.gguf")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn path_arguments_unwrap_fixed_args_as_provisioning_py_does() {
        let argv = ["/opt/run", "--model", "rel", "--weights", "/w.bin", "--x=/cfg", "--fixed-arg=--model=/m.bin", "--fixed-arg", "--library",
            "--fixed-arg", "/lib", "/a=/b", "--fixed-arg"];
        assert_eq!(configured_path_arguments(&argv), ["/opt/run", "/w.bin", "/cfg", "/a=/b", "/b", "/m.bin", "/lib"]);
        assert_eq!(configured_path_arguments(&["--model"]), Vec::<String>::new());
    }

    #[test]
    fn the_version_is_the_checkouts_project_version() {
        let dir = util::mkdtemp("palette-prov-version-").unwrap();
        fs::write(dir.join("Project.toml"), "name = \"Palette\"\nversion = \"0.3.1\"\n\n[deps]\nversion = \"no\"\n").unwrap();
        assert_eq!(palette_version(&dir).unwrap(), "0.3.1");
        fs::write(dir.join("Project.toml"), "name = \"Palette\"\n[compat]\nversion = \"no\"\n").unwrap();
        assert!(palette_version(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert!(palette_version(&repo).unwrap().chars().next().is_some_and(|c| c.is_ascii_digit()));
    }
}
