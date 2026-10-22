//! `pal`: Palette from the shell. One call per invocation against the
//! persistent daemon, so the workspace stays warm between calls the way a
//! shell's environment does. The daemon starts on first use.
//!
//!   pal 'x = 41'          evaluate code
//!   pal x + 1             arguments join with spaces
//!   pal -f script.jl      evaluate a file (`-` reads stdin)
//!   pal --tool palette_workspace '{"action":"list"}'

use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::PathBuf;

use serde_json::{json, Map, Value};

const USAGE: &str = "usage: pal [-w WORKSPACE] [-c CONTEXT] [-t SECONDS] [--full] [--json] [--ephemeral] (CODE... | -f FILE | -)
       pal [--json] --tool NAME [ARGUMENTS_JSON]

Evaluates Julia in the persistent Palette workspace and prints its output and
value. With no CODE and stdin not a terminal, the code is read from stdin.
Exit status is 0 on success, 1 when the call fails, 2 on a usage or
connection error. PAL_WORKSPACE and PAL_CONTEXT set the default workspace and
context.";

#[derive(Debug, PartialEq)]
struct Request {
    tool: String,
    arguments: Value,
    json: bool,
}

fn parse(argv: &[String], stdin: &mut dyn FnMut() -> Result<String, String>) -> Result<Option<Request>, String> {
    let mut args = Map::new();
    let mut tool: Option<String> = None;
    let mut json = false;
    let mut code: Vec<String> = Vec::new();
    let mut file: Option<String> = None;
    let mut it = argv.iter();
    fn value(it: &mut std::slice::Iter<String>, flag: &str) -> Result<String, String> {
        it.next().cloned().ok_or_else(|| format!("{flag} needs a value"))
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(None),
            "-w" | "--workspace" => { args.insert("workspace_id".into(), value(&mut it, a)?.into()); }
            "-c" | "--context" => { args.insert("context_id".into(), value(&mut it, a)?.into()); }
            "-t" | "--timeout" => {
                let v = value(&mut it, a)?;
                let secs: f64 = v.parse().map_err(|_| format!("{a} takes seconds, not {v:?}"))?;
                args.insert("timeout_s".into(), json!(secs));
            }
            "-f" | "--file" => file = Some(value(&mut it, a)?),
            "--full" => { args.insert("view".into(), "full".into()); }
            "--ephemeral" => { args.insert("ephemeral".into(), true.into()); }
            "--json" => json = true,
            "--tool" => tool = Some(value(&mut it, a)?),
            "--" => { code.extend(it.by_ref().cloned()); }
            s if s.starts_with('-') && s != "-" && code.is_empty() => return Err(format!("unknown option {s}")),
            _ => code.push(a.clone()),
        }
    }
    for (key, var) in [("workspace_id", "PAL_WORKSPACE"), ("context_id", "PAL_CONTEXT")] {
        if !args.contains_key(key) {
            if let Some(v) = std::env::var(var).ok().filter(|v| !v.is_empty()) {
                args.insert(key.into(), v.into());
            }
        }
    }
    if let Some(name) = tool {
        let mut arguments = match code.join(" ").trim() {
            "" => Value::Object(Map::new()),
            text => serde_json::from_str(text).map_err(|e| format!("--tool arguments are not JSON: {e}"))?,
        };
        let obj = arguments.as_object_mut().ok_or("--tool arguments must be a JSON object")?;
        for (k, v) in args {
            if k == "workspace_id" || k == "context_id" {
                obj.entry(k).or_insert(v);
            }
        }
        return Ok(Some(Request { tool: name, arguments, json }));
    }
    let source = match (file, code.as_slice()) {
        (Some(_), [_, ..]) => return Err("give code or -f FILE, not both".into()),
        (Some(f), []) if f == "-" => stdin()?,
        (Some(f), []) => std::fs::read_to_string(&f).map_err(|e| format!("{f}: {e}"))?,
        (None, [one]) if one == "-" => stdin()?,
        (None, []) if !std::io::stdin().is_terminal() => stdin()?,
        (None, []) => return Ok(None),
        (None, parts) => parts.join(" "),
    };
    if source.trim().is_empty() {
        return Ok(None);
    }
    args.insert("code".into(), source.into());
    Ok(Some(Request { tool: "palette".into(), arguments: Value::Object(args), json }))
}

/// Sends one tools/call over a fresh connection and returns its result.
fn call(path: &PathBuf, req: &Request) -> Result<Value, String> {
    let stream = super::client::start_service(path)?;
    let mut writer = &stream;
    let lines = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "pal", "version": env!("CARGO_PKG_VERSION")}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": req.tool, "arguments": req.arguments}}),
    ];
    for line in &lines {
        writeln!(writer, "{line}").map_err(|e| format!("Palette daemon closed the connection: {e}"))?;
    }
    for line in BufReader::new(&stream).lines() {
        let line = line.map_err(|e| format!("reading from the Palette daemon: {e}"))?;
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if msg.get("id") != Some(&json!(2)) {
            continue;
        }
        if let Some(err) = msg.get("error") {
            return Err(err.get("message").and_then(Value::as_str).map_or_else(|| err.to_string(), str::to_string));
        }
        return msg.get("result").cloned().ok_or_else(|| "the daemon's reply has no result".into());
    }
    Err("the Palette daemon closed the connection before replying".into())
}

fn text_of(result: &Value) -> String {
    result.get("content").and_then(Value::as_array).into_iter().flatten()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>().join("\n")
}

/// What a shell user wants from a palette result: its printed output, then
/// its value, or the error on stderr.
fn render(result: &Value, raw: bool, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let failed = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let text = text_of(result);
    let payload = serde_json::from_str::<Value>(&text).ok().filter(Value::is_object);
    let Some(p) = payload.filter(|_| !raw) else {
        let _ = writeln!(out, "{}", text.trim_end());
        return i32::from(failed);
    };
    if let Some(o) = p.get("output").and_then(Value::as_str) {
        let _ = write!(out, "{o}");
    }
    let success = p.get("success").and_then(Value::as_bool).unwrap_or(!failed);
    if success {
        match p.get("data") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) => { let _ = writeln!(out, "{s}"); }
            Some(v) => { let _ = writeln!(out, "{v}"); }
        }
        0
    } else {
        let msg = p.get("error").map_or_else(|| text.clone(), |e| e.as_str().map_or_else(|| e.to_string(), str::to_string));
        let _ = writeln!(err, "{}", msg.trim_end());
        1
    }
}

pub(crate) fn main(argv: &[String]) -> i32 {
    let mut stdin = || {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map(|_| s).map_err(|e| format!("reading stdin: {e}"))
    };
    let req = match parse(argv, &mut stdin) {
        Ok(Some(r)) => r,
        Ok(None) => {
            println!("{USAGE}");
            return if argv.iter().any(|a| a == "-h" || a == "--help") { 0 } else { 2 };
        }
        Err(e) => {
            eprintln!("pal: {e}\n{USAGE}");
            return 2;
        }
    };
    match super::client::socket_path().and_then(|p| call(&p, &req)) {
        Ok(result) => render(&result, req.json || req.tool != "palette", &mut std::io::stdout().lock(), &mut std::io::stderr().lock()),
        Err(e) => {
            eprintln!("pal: {e}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util;
    use std::os::unix::net::UnixListener;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    fn parsed(s: &[&str]) -> Request {
        parse(&args(s), &mut || Ok("from stdin".into())).unwrap().unwrap()
    }

    #[test]
    fn parses_code_flags_and_tool_calls() {
        let r = parsed(&["-w", "ws", "-t", "30", "--full", "x", "+", "1"]);
        assert_eq!(r.tool, "palette");
        assert_eq!(r.arguments, json!({"workspace_id": "ws", "timeout_s": 30.0, "view": "full", "code": "x + 1"}));
        assert_eq!(parsed(&["-"]).arguments["code"], "from stdin");
        assert_eq!(parsed(&["-f", "-"]).arguments["code"], "from stdin");
        let t = parsed(&["--json", "-c", "ctx", "--tool", "palette_workspace", r#"{"action":"list"}"#]);
        assert_eq!(t, Request { tool: "palette_workspace".into(), arguments: json!({"action": "list", "context_id": "ctx"}), json: true });
        assert!(parse(&args(&["--bogus"]), &mut || Ok(String::new())).is_err());
        assert!(parse(&args(&["-f", "a.jl", "x"]), &mut || Ok(String::new())).is_err());
        assert!(parse(&args(&["--tool", "palette_workspace", "[1]"]), &mut || Ok(String::new())).is_err());
        // Code may itself start with a dash once code has begun, or after --.
        assert_eq!(parsed(&["--", "-x"]).arguments["code"], "-x");
    }

    #[test]
    fn renders_output_value_and_errors() {
        let ok = json!({"content": [{"type": "text", "text": r#"{"success":true,"output":"hi\n","data":[1,2]}"#}]});
        let (mut o, mut e) = (Vec::new(), Vec::new());
        assert_eq!(render(&ok, false, &mut o, &mut e), 0);
        assert_eq!(String::from_utf8(o).unwrap(), "hi\n[1,2]\n");
        let bad = json!({"isError": true, "content": [{"type": "text", "text": r#"{"success":false,"output":"","error":"boom\n"}"#}]});
        let (mut o, mut e) = (Vec::new(), Vec::new());
        assert_eq!(render(&bad, false, &mut o, &mut e), 1);
        assert_eq!(String::from_utf8(e).unwrap(), "boom\n");
        let (mut o, mut e) = (Vec::new(), Vec::new());
        assert_eq!(render(&bad, true, &mut o, &mut e), 1);
        assert!(String::from_utf8(o).unwrap().starts_with("{\"success\":false"));
    }

    #[test]
    fn calls_a_daemon_and_skips_unrelated_lines() {
        let dir = util::mkdtemp("palette-pal-test-").unwrap();
        let path = dir.join("daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut w = &stream;
            for line in BufReader::new(&stream).lines() {
                let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
                seen.push(msg["method"].as_str().unwrap().to_string());
                match msg["id"].as_i64() {
                    Some(1) => writeln!(w, "{}", json!({"jsonrpc": "2.0", "id": 1, "result": {}})).unwrap(),
                    Some(2) => {
                        writeln!(w, "{}", json!({"jsonrpc": "2.0", "method": "notifications/progress"})).unwrap();
                        let echo = msg["params"]["arguments"]["code"].clone();
                        writeln!(w, "{}", json!({"jsonrpc": "2.0", "id": 2, "result": {"content": [{"type": "text", "text": echo}]}})).unwrap();
                        break;
                    }
                    _ => {}
                }
            }
            seen
        });
        let req = Request { tool: "palette".into(), arguments: json!({"code": "1 + 1"}), json: false };
        let result = call(&path, &req).unwrap();
        assert_eq!(text_of(&result), "1 + 1");
        assert_eq!(server.join().unwrap(), ["initialize", "notifications/initialized", "tools/call"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
