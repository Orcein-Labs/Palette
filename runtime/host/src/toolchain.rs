//! `palette-host toolchain config|run`: one generic fixed toolchain command.
//! `config` prints a host-command entry for the operator ceiling; the entry
//! runs `toolchain run`, which executes the configured program with a clean
//! environment, an empty private home and at most one checked text input.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::util;

const USAGE: &str = "\
usage: palette-host toolchain config --name NAME --executable FILE --cwd DIR [--library-dir DIR]
                                     [--fixed-arg V]... [--artifact FILE]... [--input-mode none|text]
                                     [--input-prefix V]... [--timeout S]

Emit one generic fixed toolchain command for the operator ceiling.

  --input-prefix V   fixed backend arguments placed before the one text input
";

#[derive(Debug, Default)]
struct Args {
    name: Option<String>,
    executable: Option<String>,
    cwd: Option<String>,
    library_dir: Option<String>,
    fixed_arg: Vec<String>,
    artifact: Vec<String>,
    text_mode: bool,
    input_prefix: Vec<String>,
    timeout: f64,
    rest: Vec<String>,
}

fn existing(value: &str, dir: bool) -> Result<String, String> {
    let path = util::resolve(&util::expand_user(value));
    match (dir, path.is_dir(), path.is_file()) {
        (true, true, _) | (false, _, true) => Ok(path.display().to_string()),
        (true, ..) => Err(format!("Directory does not exist: {}", path.display())),
        (false, ..) => Err(format!("File does not exist: {}", path.display())),
    }
}

fn parse(argv: &[String], run: bool) -> Result<Args, String> {
    let mut a = Args { timeout: 600.0, ..Args::default() };
    let mut i = 0;
    while i < argv.len() {
        let item = &argv[i];
        if run && (item == "--" || !item.starts_with("--")) {
            a.rest = argv[i..].to_vec();
            break;
        }
        let (key, inline) = match item.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (item.as_str(), None),
        };
        let known = ["--executable", "--cwd", "--library-dir", "--fixed-arg", "--artifact", "--input-mode", "--input-prefix", "--timeout"];
        if !(known.contains(&key) || (!run && key == "--name")) {
            return Err(format!("unrecognized argument: {item}\n{USAGE}"));
        }
        let value = match inline {
            Some(v) => v,
            None => {
                i += 1;
                argv.get(i).cloned().ok_or_else(|| format!("argument {key}: expected one argument"))?
            }
        };
        let bad = |e: String| format!("argument {key}: {e}");
        match key {
            "--name" => a.name = Some(value),
            "--executable" => {
                let file = existing(&value, false).map_err(bad)?;
                if !util::executable(Path::new(&file)) {
                    return Err(bad(format!("File is not executable: {file}")));
                }
                a.executable = Some(file);
            }
            "--cwd" => a.cwd = Some(existing(&value, true).map_err(bad)?),
            "--library-dir" => a.library_dir = Some(existing(&value, true).map_err(bad)?),
            "--fixed-arg" => a.fixed_arg.push(value),
            "--artifact" => a.artifact.push(existing(&value, false).map_err(bad)?),
            "--input-mode" => {
                a.text_mode = match value.as_str() {
                    "none" => false,
                    "text" => true,
                    _ => return Err(bad(format!("invalid choice: '{value}' (choose from none, text)"))),
                }
            }
            "--input-prefix" => a.input_prefix.push(value),
            _ => a.timeout = value.trim().parse().map_err(|_| bad(format!("invalid float value: '{value}'")))?,
        }
        i += 1;
    }
    for (flag, set) in [("--executable", a.executable.is_some()), ("--cwd", a.cwd.is_some())] {
        if !set {
            return Err(format!("the following arguments are required: {flag}"));
        }
    }
    if !(a.timeout > 0.0 && a.timeout <= 14400.0) {
        return Err("--timeout must be in (0, 14400]".into());
    }
    Ok(a)
}

fn config(argv: &[String]) -> Result<i32, String> {
    let a = parse(argv, false)?;
    let name = a.name.ok_or("the following arguments are required: --name")?;
    if name.is_empty() || name.contains(['\0', '/', '\\']) {
        return Err("--name must be a non-empty command name without path separators".into());
    }
    if a.text_mode && a.input_prefix.is_empty() {
        return Err("--input-mode text requires a fixed --input-prefix".into());
    }
    if !a.text_mode && !a.input_prefix.is_empty() {
        return Err("--input-prefix requires --input-mode text".into());
    }
    let host = std::env::current_exe().map_err(|e| format!("cannot locate palette-host: {e}"))?;
    let (executable, cwd) = (a.executable.expect("required"), a.cwd.expect("required"));
    let mut run = vec![util::resolve(&host).display().to_string(), "toolchain".into(), "run".into(),
        "--executable".into(), executable, "--cwd".into(), cwd.clone(), "--timeout".into(), util::py_repr_f64(a.timeout)];
    if let Some(dir) = a.library_dir {
        run.extend(["--library-dir".into(), dir]);
    }
    run.extend(a.fixed_arg.iter().map(|v| format!("--fixed-arg={v}")));
    for path in a.artifact {
        run.extend(["--artifact".into(), path]);
    }
    run.extend(["--input-mode".into(), if a.text_mode { "text" } else { "none" }.into()]);
    run.extend(a.input_prefix.iter().map(|v| format!("--input-prefix={v}")));
    run.push("--".into());
    let entry = json!({name: {"argv": run, "cwd": cwd, "timeout_s": a.timeout, "extra_args": a.text_mode}});
    println!("{}", util::py_dumps(&entry, Some(2)));
    Ok(0)
}

/// Prints a refusal the way Python's `SystemExit(message)` does.
fn refuse(message: &str) -> i32 {
    eprintln!("{message}");
    1
}

fn run(argv: &[String]) -> Result<i32, String> {
    let a = parse(argv, true)?;
    let mut forwarded = a.rest;
    if forwarded.first().is_some_and(|f| f == "--") {
        forwarded.remove(0);
    }
    if !a.text_mode && !forwarded.is_empty() {
        return Ok(refuse("this configured toolchain takes no worker-supplied input"));
    }
    if a.text_mode && forwarded.len() != 1 {
        return Ok(refuse("text mode requires exactly one positional input string"));
    }
    if forwarded.iter().any(|f| f.contains('\0') || f.len() > 4096) {
        return Ok(refuse("input must be at most 4096 bytes without NUL"));
    }
    if forwarded.first().is_some_and(|f| f.starts_with('-')) {
        return Ok(refuse("text input may not begin with '-' because that could override configured options"));
    }
    // Give the selected process an empty, private home instead of ambient
    // credentials, model settings, package state, or host service config.
    let home = util::mkdtemp("palette-toolchain-").map_err(|e| e.to_string())?;
    let mut command = Command::new(a.executable.expect("required"));
    command.args(a.fixed_arg).args(a.artifact).args(a.input_prefix).args(forwarded)
        .current_dir(a.cwd.expect("required")).env_clear()
        .env("PATH", "/usr/bin:/bin").env("LANG", "C.UTF-8").env("HOME", &home).env("TMPDIR", &home)
        .stdin(Stdio::null());
    if let Some(dir) = a.library_dir {
        command.env("LD_LIBRARY_PATH", dir);
    }
    let result = wait(command, Duration::from_secs_f64(a.timeout), a.timeout);
    let _ = std::fs::remove_dir_all(&home);
    Ok(result.unwrap_or_else(|message| refuse(&message)))
}

/// The child's exit code, or the refusal when it cannot start or overruns
/// its timeout (it is then killed, as `subprocess.run` does).
fn wait(mut command: Command, timeout: Duration, seconds: f64) -> Result<i32, String> {
    use std::os::unix::process::ExitStatusExt;
    let mut child = command.spawn().map_err(|e| format!("could not start configured toolchain: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok(status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0)));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("configured toolchain exceeded its {}s timeout", util::py_g(seconds)));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn main(argv: &[String]) -> Result<i32, String> {
    match argv.first().map(String::as_str) {
        Some("config") if argv[1..].iter().any(|a| a == "-h" || a == "--help") => {
            print!("{USAGE}");
            Ok(0)
        }
        Some("config") => config(&argv[1..]),
        Some("run") => run(&argv[1..]),
        _ => Err(format!("toolchain takes config or run\n{USAGE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn run_stops_options_at_the_separator_and_checks_the_input() {
        let a = parse(&strings(&["--executable", "/bin/sh", "--cwd", "/", "--input-mode", "text", "--input-prefix=-c", "--", "--model"]), true).unwrap();
        assert_eq!(a.rest, ["--", "--model"]);
        assert_eq!(a.input_prefix, ["-c"]);
        assert_eq!(run(&strings(&["--executable", "/bin/sh", "--cwd", "/", "--input-mode", "text", "--input-prefix=-c", "--", "--model"])).unwrap(), 1);
        assert_eq!(run(&strings(&["--executable", "/bin/true", "--cwd", "/", "--", "input"])).unwrap(), 1);
        assert_eq!(run(&strings(&["--executable", "/bin/sh", "--cwd", "/", "--input-mode", "text", "--input-prefix=-c", "--", "exit 7"])).unwrap(), 7);
    }

    #[test]
    fn run_kills_an_overrunning_toolchain() {
        let start = Instant::now();
        let code = run(&strings(&["--executable", "/bin/sleep", "--cwd", "/", "--timeout", "0.2", "--fixed-arg=5"])).unwrap();
        assert_eq!(code, 1);
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn options_are_checked_like_argparse() {
        assert!(parse(&strings(&["--executable", "/nonexistent", "--cwd", "/"]), false).unwrap_err().contains("File does not exist: /nonexistent"));
        assert!(parse(&strings(&["--executable", "/bin/sh", "--cwd", "/etc/passwd"]), false).unwrap_err().contains("Directory does not exist"));
        assert_eq!(parse(&strings(&["--executable", "/bin/sh", "--cwd", "/", "--timeout", "0"]), false).unwrap_err(), "--timeout must be in (0, 14400]");
        assert_eq!(config(&strings(&["--name", "a/b", "--executable", "/bin/sh", "--cwd", "/"])).unwrap_err(),
            "--name must be a non-empty command name without path separators");
        assert_eq!(config(&strings(&["--name", "a", "--executable", "/bin/sh", "--cwd", "/", "--input-mode", "text"])).unwrap_err(),
            "--input-mode text requires a fixed --input-prefix");
    }
}
