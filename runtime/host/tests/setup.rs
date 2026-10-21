//! `palette-host setup` end to end, against stub client CLIs and a fake
//! server, under an isolated HOME and PATH: the real registrations and the
//! operator's Palette installation are never touched.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const CLIENTS: [&str; 4] = ["claude", "codex", "grok", "agy"];
const OTHER: &str = "/home/me/rnd/palette/palette-mcp";

/// One stub for every client CLI: it keeps the command its palette server
/// runs in $STUB_STATE/<name> and logs each call. Claude Code and Codex
/// refuse to add over an existing server, as the real ones do.
const CLIENT_STUB: &str = r##"#!/bin/sh
name=$(basename "$0")
echo "$name $*" >> "$STUB_STATE/calls.log"
state="$STUB_STATE/$name"
eval "last=\${$#}"
case "$name $1 $2" in
  "claude mcp get") [ -f "$state" ] || exit 1
    printf 'palette:\n  Scope: User config\n  Type: stdio\n  Command: %s\n  Args: \n' "$(cat "$state")" ;;
  "codex mcp get") [ -f "$state" ] || exit 1
    printf '{"name":"palette","transport":{"type":"stdio","command":"%s","args":[]}}\n' "$(cat "$state")" ;;
  "grok mcp list") printf '[{"name":"other","command":"/bin/other","scope":"user"}'
    if [ -f "$state" ]; then printf ',{"name":"palette","command":"%s","scope":"user"}' "$(cat "$state")"; fi
    printf ']\n' ;;
  "agy mcp list") printf 'NAME     TYPE   STATUS   COMMAND/URL\nother    stdio  enabled  /bin/other\n'
    if [ -f "$state" ]; then printf 'palette  stdio  enabled  %s\n' "$(cat "$state")"; fi ;;
  "claude mcp add"|"codex mcp add") [ -f "$state" ] && exit 1; printf '%s' "$last" > "$state" ;;
  *" mcp add") printf '%s' "$last" > "$state" ;;
  *" mcp remove") [ -f "$state" ] || exit 1; rm "$state" ;;
  *) exit 2 ;;
esac
"##;

/// Prewarming starts sessions under the server's environment, so it needs
/// the deployed capabilities and the ceiling.
/// A palette server that keeps x between calls, unless FAKE_PALETTE=forget.
/// As `prepare`, it checks the capabilities exist and prints a record.
const FAKE_SERVER: &str = r##"#!/bin/sh
if [ "$1" = prepare ]; then
  [ -f "$PALETTE_CAPABILITY_CEILING" ] && [ -d "$PALETTE_RUNTIME_ROOT/teamlab" ] || exit 3
  echo '{"state":"prepared"}'
  exit 0
fi
env > "$FAKE_PALETTE_ENV"
x=
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$1"; }
while IFS= read -r line; do
  case $line in *'"id":'*) ;; *) continue ;; esac
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case $line in
    *'"code":"x = 41"'*) [ "$FAKE_PALETTE" = forget ] || x=41; reply '{\"data\":41}' ;;
    *'"code":"x + 1"'*)
      if [ -z "$x" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"UndefVarError: x not defined"}}\n' "$id"
      else
        reply '{\"data\":42}'
      fi ;;
    *teamlab*) reply '{\"data\":{\"dir\":\"/shop\",\"listed\":0}}' ;;
    *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
"##;

fn source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn write(path: &Path, text: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), to).unwrap();
        }
    }
}

/// A temporary HOME with the stubs on PATH, removed when the test ends.
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
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!("palette-setup-it-{}-{}", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&root);
        let f = Fixture { root };
        for name in CLIENTS {
            write(&f.bin().join(name), CLIENT_STUB, 0o755);
        }
        write(&f.bin().join("python3"), "#!/bin/sh\nexec sh \"$@\"\n", 0o755);
        for name in ["julia", "bwrap"] {
            write(&f.bin().join(name), "#!/bin/sh\nexit 0\n", 0o755);
        }
        fs::create_dir_all(f.state()).unwrap();
        fs::create_dir_all(f.home()).unwrap();
        f
    }

    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn state(&self) -> PathBuf {
        self.root.join("stub-state")
    }

    fn client_dir(&self) -> PathBuf {
        self.home().join(".local/share/palette/client")
    }

    fn launcher(&self) -> String {
        self.client_dir().join("palette-mcp").to_string_lossy().into_owned()
    }

    fn server_env(&self) -> PathBuf {
        self.root.join("server.env")
    }

    /// An SDK with the files setup reads and a fake host that serves MCP and
    /// prepares; it has no .git, so setup does not build a host.
    fn make_repo(&self, name: &str) -> PathBuf {
        let repo = self.root.join(name);
        for dir in ["runtime/integrations/teamlab", "runtime/plugins/palette", "runtime/licenses"] {
            copy_tree(&source().join(dir), &repo.join(dir));
        }
        for file in ["Project.toml", "LICENSE"] {
            fs::copy(source().join(file), repo.join(file)).unwrap();
        }
        write(&repo.join("Manifest.toml"), "", 0o644);
        write(&repo.join("runtime/host/target/release/palette-host"), FAKE_SERVER, 0o755);
        repo
    }

    fn setup(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_palette-host"))
            .arg("setup")
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .env("STUB_STATE", self.state())
            .env("FAKE_PALETTE_ENV", self.server_env())
            .envs(env.iter().copied())
            .current_dir(&self.root)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.setup(args, &[]);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "setup {args:?} failed:\n{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
        stdout
    }

    fn registered(&self, client: &str) -> Option<String> {
        fs::read_to_string(self.state().join(client)).ok()
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.state().join("calls.log")).unwrap_or_default().lines().map(String::from).collect()
    }

    fn adds(&self) -> usize {
        self.calls().iter().filter(|c| c.contains(" mcp add ")).count()
    }
}

fn read_env(path: &Path) -> HashMap<String, String> {
    fs::read_to_string(path).unwrap().lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.into(), v.into())).collect()
}

fn names(dir: &Path) -> Vec<String> {
    let mut n: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    n.sort();
    n
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn registers_with_each_client_once_and_keeps_other_servers() {
    let f = Fixture::new();
    let repo = f.make_repo("sdk-1");
    let args = ["--repo-dir", repo.to_str().unwrap(), "--connect-only", "--no-check"];
    let out = f.ok(&args);
    for client in ["Claude Code", "Codex", "Grok", "Antigravity"] {
        assert!(out.contains(&format!("Registered {client} to this installation.")), "{out}");
    }
    for client in CLIENTS {
        assert_eq!(f.registered(client).as_deref(), Some(f.launcher().as_str()));
    }
    assert_eq!(f.adds(), 4);

    let out = f.ok(&args);
    assert_eq!(out.matches("already uses this installation.").count(), 4, "{out}");
    assert_eq!(f.adds(), 4);

    for client in CLIENTS {
        fs::write(f.state().join(client), OTHER).unwrap();
    }
    let out = f.ok(&args);
    let kept = format!("already has a server named palette ({OTHER}); left as it is.");
    assert_eq!(out.matches(&kept).count(), 4, "{out}");
    assert!(out.contains("run setup again with --replace, or: claude mcp remove palette; claude mcp add --scope user palette -- "), "{out}");
    assert_eq!(f.adds(), 4);
    for client in CLIENTS {
        assert_eq!(f.registered(client).as_deref(), Some(OTHER));
    }

    let out = f.ok(&[&args[..], &["--replace"]].concat());
    for client in ["Claude Code", "Codex", "Grok", "Antigravity"] {
        assert!(out.contains(&format!("Moved {client} to this installation.")), "{out}");
    }
    for client in CLIENTS {
        assert_eq!(f.registered(client).as_deref(), Some(f.launcher().as_str()));
    }
    let calls = f.calls();
    assert!(calls.iter().all(|c| !c.contains("other")), "{calls:?}");
}

#[test]
fn installs_checks_and_keeps_the_working_installation_when_a_check_fails() {
    let f = Fixture::new();
    let sdk1 = f.make_repo("sdk-1");
    let out = f.ok(&["--repo-dir", sdk1.to_str().unwrap()]);
    assert!(out.contains("Preparing the sandbox"), "{out}");
    assert!(out.contains("x + 1 = 42"), "{out}");
    assert!(out.contains("TeamLab: 0 listings in the shop at /shop."), "{out}");
    for client in CLIENTS {
        assert_eq!(f.registered(client).as_deref(), Some(f.launcher().as_str()));
    }
    let launcher = fs::read_to_string(f.launcher()).unwrap();
    assert!(launcher.starts_with("#!/bin/sh\n# Palette-managed launcher\nexec env "), "{launcher}");
    assert!(launcher.contains("sdk-1"), "{launcher}");

    let palette = f.home().join(".local/share/palette");
    let capabilities = palette.join("capabilities");
    let source_file = json(&sdk1.join("runtime/integrations/teamlab/capability.json"))["source"].as_str().unwrap().to_string();
    let deployed = capabilities.join("teamlab").join(&source_file);
    assert_eq!(fs::read(&deployed).unwrap(), fs::read(sdk1.join("runtime/integrations/teamlab").join(&source_file)).unwrap());
    let ceiling = palette.join("config/ceiling.json");
    assert_eq!(json(&ceiling)["host_request"]["allowed_types"], serde_json::json!(["palette.runtime"]));

    let env = read_env(&f.server_env());
    assert_eq!(env["PALETTE_RUNTIME_ROOT"], capabilities.to_string_lossy());
    assert_eq!(env["PALETTE_CAPABILITY_CEILING"], ceiling.to_string_lossy());
    assert_eq!(env["JULIA_PKG_OFFLINE"], "true");
    for absent in ["TEAMLAB_DIR", "PALETTE_HOST_COMMANDS", "PALETTE_READ_ROOTS"] {
        assert!(!env.contains_key(absent), "{absent} is set");
    }
    assert_eq!(names(&f.client_dir()), ["generic-mcp.json", "palette-launch.json", "palette-local-plugin", "palette-mcp"]);
    let plugin = f.client_dir().join("palette-local-plugin");
    assert_eq!(json(&plugin.join("plugin.json"))["license"], "AGPL-3.0-only");
    let entry = &json(&plugin.join("mcp.json"))["mcpServers"]["palette"];
    assert_eq!(entry["type"], "stdio");
    assert_eq!(entry["env"]["PALETTE_REPO"], sdk1.to_str().unwrap());
    for file in ["LICENSE", "NOTICE", "MIT-INHERITED.txt", "assets/palette.svg"] {
        assert!(plugin.join(file).is_file(), "{file}");
    }

    // An update whose check fails leaves the installation that worked.
    let sdk2 = f.make_repo("sdk-2");
    let changed = sdk2.join("runtime/integrations/teamlab").join(&source_file);
    fs::write(&changed, fs::read_to_string(&changed).unwrap() + "\n# sdk-2\n").unwrap();
    let before = fs::read_to_string(&deployed).unwrap();
    let out = f.setup(&["--repo-dir", sdk2.to_str().unwrap()], &[("FAKE_PALETTE", "forget")]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("The check failed, so this installation was not switched on"), "{stderr}");
    assert!(stderr.contains("UndefVarError"), "{stderr}");
    let launcher = fs::read_to_string(f.launcher()).unwrap();
    assert!(launcher.contains("sdk-1") && !launcher.contains("sdk-2"), "{launcher}");
    assert_eq!(fs::read_to_string(&deployed).unwrap(), before);
    assert_eq!(names(&capabilities), ["teamlab"]);
    assert!(!fs::read_to_string(f.client_dir().join("palette-launch.json")).unwrap().contains("sdk-2"));
    assert_eq!(names(&f.client_dir()), ["generic-mcp.json", "palette-launch.json", "palette-local-plugin", "palette-mcp"]);
}

#[test]
fn operator_files_are_passed_to_the_server() {
    let f = Fixture::new();
    let repo = f.make_repo("sdk-1");
    let config = f.home().join(".config/palette");
    let host_commands = config.join("host-commands.json");
    write(&host_commands, "{}\n", 0o600);
    let out = f.ok(&["--repo-dir", repo.to_str().unwrap(), "--no-register"]);
    assert!(out.contains(&format!("Host commands: {}", host_commands.display())), "{out}");
    assert_eq!(read_env(&f.server_env())["PALETTE_HOST_COMMANDS"], host_commands.to_string_lossy());
    assert!(f.calls().is_empty());

    let roots = config.join("read-roots");
    let quick = ["--repo-dir", repo.to_str().unwrap(), "--no-register", "--connect-only", "--no-check"];
    let missing = f.root.join("missing");
    for bad in ["relative/dir\n".to_string(), format!("{}\n", missing.display())] {
        write(&roots, &bad, 0o644);
        let out = f.setup(&quick, &[]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("read root"), "{}", String::from_utf8_lossy(&out.stderr));
    }

    let (one, two) = (f.root.join("project one"), f.root.join("project-two"));
    fs::create_dir_all(&one).unwrap();
    fs::create_dir_all(&two).unwrap();
    write(&roots, &format!("# read roots\n\n  {}\n{}\n", one.display(), two.display()), 0o644);
    let out = f.ok(&["--repo-dir", repo.to_str().unwrap(), "--no-register"]);
    let joined = format!("{}:{}", one.display(), two.display());
    assert!(out.contains(&format!("Read roots: {}, {}", one.display(), two.display())), "{out}");
    assert_eq!(read_env(&f.server_env())["PALETTE_READ_ROOTS"], joined);
    assert!(fs::read_to_string(f.launcher()).unwrap().contains(&format!("'PALETTE_READ_ROOTS={joined}'")));
    assert!(names(&f.client_dir()).iter().all(|n| !n.starts_with('.')));
}
