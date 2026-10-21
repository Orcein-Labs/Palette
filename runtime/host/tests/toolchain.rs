//! `palette-host toolchain config` then the configured `toolchain run`, with
//! a shell fixture standing in for an inference binary.

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn a_generic_toolchain_keeps_its_binary_and_artifact_fixed() {
    let tmp = fs::canonicalize(std::env::temp_dir()).unwrap();
    let root = tmp.join(format!("palette-toolchain-it-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let artifact = root.join("selected-model.bin");
    fs::write(&artifact, "operator-selected").unwrap();
    let executable = root.join("fixture-tool");
    fs::write(&executable, "#!/bin/sh\nfor a in \"$@\"; do echo \"arg:$a\"; done\nenv | sed 's/^/env:/'\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let (root_s, artifact_s) = (root.to_str().unwrap(), artifact.to_str().unwrap());

    let config = Command::new(env!("CARGO_BIN_EXE_palette-host"))
        .args(["toolchain", "config", "--name", "fixture", "--executable", executable.to_str().unwrap(),
               "--cwd", root_s, "--artifact", artifact_s, "--fixed-arg=--backend",
               "--input-mode", "text", "--input-prefix=--prompt"])
        .output().unwrap();
    assert!(config.status.success(), "{}", String::from_utf8_lossy(&config.stderr));
    let entry = &serde_json::from_slice::<Value>(&config.stdout).unwrap()["fixture"];
    assert_eq!(entry["extra_args"], true);
    let argv: Vec<&str> = entry["argv"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();

    let result = Command::new(argv[0]).args(&argv[1..]).arg("What is 6 times 7?")
        .env("OPENAI_API_KEY", "fixture-secret").output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let out = String::from_utf8(result.stdout).unwrap();
    let args: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("arg:")).collect();
    assert!(args.contains(&artifact_s));
    let prompt = args.iter().position(|a| *a == "--prompt").unwrap();
    assert_eq!(args[prompt + 1], "What is 6 times 7?");
    assert!(!out.lines().any(|l| l.starts_with("env:OPENAI_API_KEY=")));

    // Caller input is text only: it cannot swap in another model.
    let attacker = root.join("attacker.bin");
    let denied = Command::new(argv[0]).args(&argv[1..]).args(["--model", attacker.to_str().unwrap()]).output().unwrap();
    assert!(!denied.status.success());
    let _ = fs::remove_dir_all(&root);
}
