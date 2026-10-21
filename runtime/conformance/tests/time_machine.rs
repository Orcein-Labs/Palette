//! The Time Machine (src/time_machine.jl, `palette-host control`): named
//! checkpoints of the live world, restored or forked by a new kernel, with a
//! receipt of each identity the checkpoint recorded.
//!
//! As in the revival suite, the adversarial cases check that a restore does
//! NOT claim what is not true: a damaged checkpoint is refused, and code that
//! is not the checkpoint's is never reported as the same.

use palette_conformance::{control, env, read_json, KernelCase, SessionOptions};
use serde_json::{json, Value};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

const MEAN: &str = "summarize(xs) = sum(xs) / length(xs)\n";
const MAX: &str = "summarize(xs) = maximum(xs)\n";

fn checkpoint_names(t: &KernelCase) -> Vec<String> {
    let listed = control("checkpoints", t.state(), json!({})).unwrap();
    listed["checkpoints"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect()
}

fn restore(t: &KernelCase, args: Value) {
    control("restore", t.state(), args).unwrap();
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

/// A world checkpointed as `good`, then changed in code, data and ENV.
fn good_then_broken(t: &KernelCase) -> PathBuf {
    let ops = t.ws.join("ops.jl");
    std::fs::write(&ops, MEAN).unwrap();
    let r = t.session(&[
        r#"include("ops.jl"); data = [1.0, 2.0, 3.0, 6.0]; ENV["TM_MODE"] = "good"; result = summarize(data)"#,
        r#"Palette.checkpoint("good"; note="mean of the frame times")"#,
        "nothing",
    ]);
    assert!(text(&r[2]["output"]).contains("Saved checkpoint `good`"), "{}", r[2]);
    std::fs::write(&ops, MAX).unwrap();
    t.session(&[r#"include("ops.jl"); data = [9.0, 1.0]; ENV["TM_MODE"] = "broken"; result = summarize(data)"#]);
    ops
}

/// `auto-YYYYMMDD-HHMMSS-before-undo`.
fn is_undo_name(name: &str) -> bool {
    let Some(stamp) = name.strip_prefix("auto-").and_then(|n| n.strip_suffix("-before-undo")) else {
        return false;
    };
    let b = stamp.as_bytes();
    b.len() == 15 && b[8] == b'-' && b.iter().enumerate().all(|(i, c)| i == 8 || c.is_ascii_digit())
}

#[test]
fn seven_step_restore_and_undo() {
    let t = KernelCase::new();
    good_then_broken(&t);
    restore(&t, json!({"checkpoint": "good"}));
    let (out, r) = t.revive(r#"(summarize(data), result, ENV["TM_MODE"], Palette.revival()["checkpoint"])"#);
    assert_eq!(r["success"], true, "{r}");
    let d = &r["data"];
    assert_eq!((&d[0], &d[1], &d[2]), (&json!(3.0), &json!(3.0), &json!("good")), "{out}");
    let receipt = &d[3];
    assert!(out.contains("[time machine] Restored checkpoint `good`"), "{out}");
    assert_eq!(receipt["identical"], true, "{receipt}");
    assert_eq!(receipt["environment"]["verdict"], "same");
    let includes = receipt["includes"].as_array().unwrap();
    assert_eq!(includes.iter().map(|i| &i["replayed_from"]).collect::<Vec<_>>(), ["checkpoint copy"]);
    assert_eq!(includes.iter().map(|i| &i["workspace"]).collect::<Vec<_>>(), ["differs"]);
    let identities = receipt["definitions"].as_array().unwrap().iter().chain(receipt["data"].as_array().unwrap());
    assert!(identities.into_iter().all(|d| d["verdict"] == "same"), "{receipt}");
    assert!(out.contains("ops.jl"), "the report names the workspace file that differs: {out}");
    assert_eq!(std::fs::read_to_string(t.ws.join("ops.jl")).unwrap(), MAX, "and leaves it alone");
    // Values computed from the checkpoint's copy are not stale: it is the code the world runs.
    assert!(!out.contains("restored but stale"), "{out}");
    assert!(!out.contains("[changed on disk"), "{out}");
    let (out, r) = t.revive(r#"(Palette.provenance(:result; strong=true)["freshness"], summarize(data))"#);
    assert_eq!(r["data"], json!(["no_observed_staleness", 3.0]), "{out}");
    let world = read_json(&t.state.join("world.json"));
    assert_eq!(world["history"].as_array().unwrap().last().unwrap()["event"], "restore");
    // The world it replaced was kept, so the restore can be undone.
    let auto = text(&receipt["saved_before_restore"]).to_string();
    assert!(checkpoint_names(&t).contains(&auto), "{auto}");
    restore(&t, json!({"checkpoint": auto}));
    let (out, r) = t.revive(r#"c = Palette.revival()["checkpoint"]; (summarize(data), ENV["TM_MODE"], c["identical"], c["saved_before_restore"])"#);
    let d = r["data"].as_array().unwrap();
    assert_eq!(d[..3], [json!(9.0), json!("broken"), json!(true)], "{out}");
    assert!(is_undo_name(text(&d[3])), "undoing an undo does not nest names: {}", d[3]);
}

#[test]
fn restore_survives_a_later_plain_revival() {
    let t = KernelCase::new();
    good_then_broken(&t);
    restore(&t, json!({"checkpoint": "good"}));
    t.revive("nothing");
    let (out, r) = t.revive("summarize(data)");
    assert_eq!(r["data"], json!(3.0), "still the checkpoint's code, not the workspace file's: {out}");
}

#[test]
fn fork_records_its_parent() {
    let t = KernelCase::new();
    good_then_broken(&t);
    let before = read_json(&t.state.join("world.json"))["id"].clone();
    restore(&t, json!({"checkpoint": "good", "fork": true}));
    let (out, r) = t.revive(r#"c = Palette.checkpoints(); w = c.world; (w["id"], w["parent"]["world"], w["parent"]["checkpoint"], c.checkpoints[1]["name"])"#);
    assert_eq!(r["success"], true, "{r}");
    assert!(out.contains("Forked a new world from checkpoint `good`"), "{out}");
    let d = r["data"].as_array().unwrap();
    assert_ne!(d[0], before);
    assert_eq!(d[1..], [before, json!("good"), json!("good")]);
}

#[test]
fn damaged_checkpoint_is_refused() {
    let t = KernelCase::new();
    good_then_broken(&t);
    let cdir = t.state.join("checkpoints/good");
    let record = read_json(&cdir.join("checkpoint.json"));
    let data_file = cdir.join(text(&record["data"][0]["file"]));
    // A hard link: replace it rather than write through to the live snapshot.
    std::fs::remove_file(&data_file).unwrap();
    std::fs::write(&data_file, b"not the saved data").unwrap();
    restore(&t, json!({"checkpoint": "good"}));
    let (out, r) = t.revive(r#"(summarize(data), Palette.revival()["checkpoint"]["event"])"#);
    assert!(out.contains("Restoring checkpoint `good` failed"), "{out}");
    assert!(out.contains("damaged"), "{out}");
    assert_eq!(r["data"], json!([9.0, "restore_failed"]), "revived as it was: {out}");
}

#[test]
fn damaged_copy_of_source_is_refused() {
    let t = KernelCase::new();
    good_then_broken(&t);
    let record = read_json(&t.state.join("checkpoints/good/checkpoint.json"));
    let sha = text(record["includes"][0]["files"].as_object().unwrap().values().next().unwrap()).to_string();
    std::fs::write(t.state.join("blobs").join(sha), "summarize(xs) = 0.0\n").unwrap();
    restore(&t, json!({"checkpoint": "good"}));
    let (out, r) = t.revive("summarize(data)");
    assert!(out.contains("Restoring checkpoint `good` failed"), "{out}");
    assert_eq!(r["data"], json!(9.0), "{out}");
}

#[test]
fn code_changed_before_the_checkpoint_is_not_claimed_same() {
    let t = KernelCase::new();
    let ops = t.ws.join("ops.jl");
    std::fs::write(&ops, MEAN).unwrap();
    t.session(&[r#"include("ops.jl"); data = [1.0, 2.0, 3.0, 6.0]; result = summarize(data)"#]);
    std::fs::write(&ops, MAX).unwrap(); // changed without include(): the world still runs MEAN
    t.session(&[r#"Palette.checkpoint("after-edit")"#, "nothing"]);
    let record = read_json(&t.state.join("checkpoints/after-edit/checkpoint.json"));
    assert_eq!(record["includes"].as_array().unwrap().iter().map(|i| &i["kept"]).collect::<Vec<_>>(), [false]);
    restore(&t, json!({"checkpoint": "after-edit"}));
    let (out, r) = t.revive(r#"Palette.revival()["checkpoint"]"#);
    let receipt = &r["data"];
    assert_eq!(receipt["identical"], false, "{out}");
    assert_eq!(receipt["includes"].as_array().unwrap().iter().map(|i| &i["replayed_from"]).collect::<Vec<_>>(), ["workspace"]);
    let summarize: Vec<&Value> =
        receipt["definitions"].as_array().unwrap().iter().filter(|d| d["name"] == "summarize").map(|d| &d["verdict"]).collect();
    assert_eq!(summarize, ["differs"]);
}

#[test]
fn names_listing_and_forgetting() {
    let t = KernelCase::new();
    std::fs::write(t.ws.join("ops.jl"), MEAN).unwrap();
    let r = t.session_with(
        &[
            r#"include("ops.jl"); Palette.checkpoint("one")"#,
            "nothing",
            r#"Palette.checkpoint("one")"#,
            r#"Palette.checkpoint("auto-x")"#,
            r#"Palette.checkpoint("a/b")"#,
        ],
        SessionOptions { may_fail: true, ..Default::default() },
    );
    assert!(text(&r[1]["output"]).contains("Saved checkpoint `one`"), "{}", r[1]);
    assert!(text(&r[2]["error"]).contains("exists"), "{}", r[2]);
    assert!(text(&r[3]["error"]).contains("auto-"), "{}", r[3]);
    assert!(text(&r[4]["error"]).contains("checkpoint name"), "{}", r[4]);
    assert_eq!(checkpoint_names(&t), ["one"]);
    for bad in ["missing", "../one"] {
        let e = control("restore", t.state(), json!({"checkpoint": bad})).unwrap_err();
        assert!(e.value_error, "{bad}: {e:?}");
    }
    let (out, r) = t.revive(r#"Palette.forget_checkpoint("one")"#);
    assert_eq!(r["success"], true, "{r}");
    assert!(checkpoint_names(&t).is_empty());
    assert_eq!(std::fs::read_dir(t.state.join("blobs")).unwrap().count(), 0, "{out}");
}

#[test]
fn failed_call_still_saves_its_checkpoint() {
    let t = KernelCase::new();
    let r = t.session_with(
        &[r#"Palette.checkpoint("boom"); error("stop here")"#, "nothing"],
        SessionOptions { may_fail: true, ..Default::default() },
    );
    // A call that fails still completes: its state, and the checkpoint, are saved.
    assert!(text(&r[1]["output"]).contains("Saved checkpoint `boom`"), "{}", r[1]);
}

#[test]
fn unchanged_data_shares_one_file_with_checkpoints() {
    let t = KernelCase::new();
    t.session(&["big = collect(1:10_000); small = 1", r#"Palette.checkpoint("a")"#, "small = 2"]);
    let manifest = t.manifest();
    let groups = manifest["data"].as_array().unwrap();
    let big = groups.iter().find(|g| g["names"] == json!(["big"])).unwrap();
    let live = t.state.join(text(&big["file"]));
    let kept = t.state.join("checkpoints/a").join(text(&big["file"]));
    // `big` did not change after the checkpoint, so both name the same file, one inode.
    assert_eq!(live.metadata().unwrap().ino(), kept.metadata().unwrap().ino());
    let mut on_disk: Vec<String> = std::fs::read_dir(t.state())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("data-"))
        .collect();
    let mut named: Vec<String> = groups.iter().map(|g| text(&g["file"]).to_string()).collect();
    on_disk.sort();
    named.sort();
    assert_eq!(on_disk, named);
}

#[test]
fn restores_keep_only_the_newest_automatic_checkpoints() {
    let t = KernelCase::new();
    good_then_broken(&t);
    let keep_two = || env(&[("PALETTE_AUTO_CHECKPOINTS", "2")]);
    let mut out = String::new();
    for _ in 0..3 {
        restore(&t, json!({"checkpoint": "good"}));
        let (o, r) = t.revive_with("nothing", keep_two());
        assert_eq!(r["success"], true, "{r}");
        out = o;
    }
    let names = checkpoint_names(&t);
    let autos: Vec<&String> = names.iter().filter(|n| n.starts_with("auto-")).collect();
    assert_eq!(autos.len(), 2, "{autos:?}");
    assert!(out.contains("Removed the oldest automatic checkpoint"), "{out}");
    assert!(names.contains(&"good".to_string()), "named checkpoints are never pruned");
    // Each kept checkpoint is still whole.
    restore(&t, json!({"checkpoint": autos[0]}));
    let (out, r) = t.revive_with("summarize(data)", keep_two());
    assert_eq!(r["data"], json!(3.0), "{out}");
}
