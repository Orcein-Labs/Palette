//! Palette's development tasks, run from the checkout root as `cargo xtask <task>`.

mod archive;
mod bundles;
mod timings;
mod verify;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask <task>

tasks:
  verify --project-dir DIR [--shard I/N]
      Run Palette's Julia, Rust and real operator conformance gates. DIR is an
      instantiated environment with this checkout plus JSON, CSV, DataFrames,
      EzXML and IJulia. Shard I of N runs the shared gates on shard 1 and a
      share of the test cases by their recorded seconds.
  record-test-case-seconds LOG...
      Merge the case times in security-suite job logs into
      runtime/security/test-case-seconds.json. Fetch a log with
      `gh api --allow-escape-sequences repos/OWNER/REPO/actions/jobs/ID/logs`;
      without the flag gh writes an empty log.
  bundles --output-dir DIR [--revision REV]
      Build the Claude Desktop extension and the Claude and ChatGPT plugin
      ZIPs from committed revision REV (default HEAD), with SHA256SUMS.txt.
      The same revision always gives the same bytes.";

pub enum Fail {
    /// A bad invocation: printed with the usage, exit 2.
    Usage(String),
    /// A gate or step that failed: exit 1.
    Failed(String),
}

impl From<String> for Fail {
    fn from(e: String) -> Fail {
        Fail::Failed(e)
    }
}

/// `--name value` or `--name=value` flags, each one of `known`; the last of
/// a repeated flag wins.
pub fn flags(args: &[String], known: &[&'static str]) -> Result<HashMap<&'static str, String>, String> {
    let mut found = HashMap::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        let key = *known.iter().find(|k| **k == key).ok_or_else(|| format!("unrecognized argument: {arg}"))?;
        found.insert(key, inline.or_else(|| it.next().cloned()).ok_or_else(|| format!("{key} needs a value"))?);
    }
    Ok(found)
}

/// The Palette checkout these tasks belong to.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("runtime/xtask sits two levels below the checkout").to_path_buf()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("verify") => verify::main(&args[1..]),
        Some("record-test-case-seconds") => timings::record_main(&args[1..]),
        Some("bundles") => bundles::main(&args[1..]),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => Err(Fail::Usage(format!("unknown task: {other}"))),
        None => Err(Fail::Usage("a task is required".into())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fail::Usage(e)) => {
            eprintln!("{USAGE}\n\nxtask: {e}");
            ExitCode::from(2)
        }
        Err(Fail::Failed(e)) => {
            eprintln!("xtask: {e}");
            ExitCode::from(1)
        }
    }
}
