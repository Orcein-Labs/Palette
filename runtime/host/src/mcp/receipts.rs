//! The host side of the Execution X-Ray receipt query: reads receipts.jsonl and calls.jsonl without a kernel
//! and joins a kernel call to the host receipts it caused. Read-only.

use std::fs;
use std::path::Path;

use serde_json::{json, Map, Value};

use super::Fault;

pub(crate) const NOTE: &str = "Receipts are stamped with the request in flight when the host handled them; \
work a call left running in the background is attributed to the call in flight at the time.";
pub(crate) const LIMIT_MAX: u64 = 500;
const LIMIT_DEFAULT: u64 = 50;

/// palette_control's fields for `{"action": "receipts"}`.
pub(crate) fn query_schema() -> Value {
    json!({"call": {"type": "integer"}, "epoch": {"type": "string", "maxLength": 64},
           "kind": {"type": "string", "maxLength": 64}, "since": {"type": "number"}, "until": {"type": "number"},
           "limit": {"type": "integer", "minimum": 1, "maximum": LIMIT_MAX}})
}

fn is_int(v: &Value) -> bool {
    v.is_i64() || v.is_u64()
}

/// The JSON objects in a JSONL file, and how many non-blank lines were not one.
fn read_lines(path: &Path) -> (Vec<Map<String, Value>>, usize) {
    let Ok(text) = fs::read_to_string(path) else { return (Vec::new(), 0) };
    let (mut objects, mut unreadable) = (Vec::new(), 0);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str(line) {
            Ok(Value::Object(o)) => objects.push(o),
            _ => unreadable += 1,
        }
    }
    (objects, unreadable)
}

/// Host receipts from `receipts_dir`, filtered by the palette_control
/// arguments and joined to a kernel call:
///
/// - `call`: join to the calls.jsonl record with this call number (in
///   `epoch`, else the last such record), keeping the receipts stamped with
///   its session_request. Call numbers restart in every kernel, so `epoch`
///   defaults to `kernel_epoch` when a call is named.
/// - `kind`: receipts whose category equals it.
/// - `since`, `until`: inclusive bounds on the receipt timestamp.
/// - `limit`: the last `limit` matches, 1 to 500 (default 50).
///
/// A JSON null reads as an absent argument, except `limit`.
pub(crate) fn query(receipts_dir: &Path, args: &Map<String, Value>, kernel_epoch: Option<&Value>) -> Result<Value, Fault> {
    let arg = |name: &str| args.get(name).filter(|v| !v.is_null());
    let call = arg("call");
    let epoch = match args.get("epoch") {
        Some(v) => Some(v).filter(|v| !v.is_null()),
        None if args.contains_key("call") => kernel_epoch.filter(|v| !v.is_null()),
        None => None,
    };
    let (kind, since, until) = (arg("kind"), arg("since"), arg("until"));
    let limit = args.get("limit").cloned().unwrap_or(json!(LIMIT_DEFAULT));

    if call.is_some_and(|v| !is_int(v)) {
        return Err(Fault::value("call must be an integer"));
    }
    if epoch.is_some_and(|v| !v.is_string()) {
        return Err(Fault::value("epoch must be a string"));
    }
    if kind.is_some_and(|v| !v.is_string()) {
        return Err(Fault::value("kind must be a string"));
    }
    for (name, v) in [("since", since), ("until", until)] {
        if v.is_some_and(|v| !v.is_number()) {
            return Err(Fault::value(format!("{name} must be a number (unix seconds)")));
        }
    }
    if !is_int(&limit) {
        return Err(Fault::value("limit must be an integer"));
    }
    let limit = limit.as_u64().filter(|n| (1..=LIMIT_MAX).contains(n))
        .ok_or_else(|| Fault::value(format!("limit must be between 1 and {LIMIT_MAX}")))? as usize;

    let (receipts, unreadable_r) = read_lines(&receipts_dir.join("receipts.jsonl"));
    let (mut call_record, mut unreadable_c) = (None, 0);
    if let Some(call) = call {
        let (calls, unreadable) = read_lines(&receipts_dir.join("calls.jsonl"));
        unreadable_c = unreadable;
        call_record = calls.into_iter().rev().find(|r| {
            r.get("call").is_some_and(|c| is_int(c) && c == call) && epoch.is_none_or(|e| r.get("epoch") == Some(e))
        });
        if call_record.is_none() {
            let place = epoch.and_then(Value::as_str).map(|e| format!(" in epoch {e}")).unwrap_or_default();
            return Err(Fault::value(format!("no record of call {call}{place}")));
        }
    }

    let join = call_record.as_ref().map(|r| r.get("session_request").and_then(Value::as_str));
    let number = |v: Option<&Value>| v.and_then(Value::as_f64);
    let matched: Vec<_> = receipts.into_iter().filter(|r| {
        let ts = number(r.get("timestamp"));
        join.is_none_or(|j| j.is_some() && r.get("session_request").and_then(Value::as_str) == j)
            && kind.is_none_or(|k| r.get("category") == Some(k))
            && number(since).is_none_or(|s| ts.is_some_and(|t| t >= s))
            && number(until).is_none_or(|u| ts.is_some_and(|t| t <= u))
    }).collect();

    let returned: Vec<_> = matched[matched.len().saturating_sub(limit)..].iter().cloned().map(Value::Object).collect();
    Ok(json!({
        "receipts": returned,
        "matched": matched.len(),
        "returned": returned.len(),
        "call": call_record,
        "unreadable_lines": unreadable_r + unreadable_c,
        "note": NOTE,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util;

    struct Tmp(std::path::PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> Tmp {
        let t = Tmp(util::mkdtemp("palette-receipts-").unwrap());
        let receipts = [
            json!({"category": "fs_digest", "timestamp": 10, "session_request": "r1"}),
            json!({"category": "network_access", "timestamp": 20.5, "session_request": "r1"}),
            json!({"category": "fs_digest", "timestamp": 30, "session_request": "r2"}),
            json!({"category": "fs_digest", "timestamp": true, "session_request": "r2"}),
        ];
        let mut text: String = receipts.iter().map(|r| r.to_string() + "\n").collect();
        text.push_str("not json\n\n[1]\n");
        fs::write(t.0.join("receipts.jsonl"), text).unwrap();
        let calls = [json!({"call": 1, "epoch": "e1", "session_request": "r1"}), json!({"call": 1, "epoch": "e2", "session_request": "r2"}),
                     json!({"call": 2.0, "epoch": "e2", "session_request": "r1"}), json!({"call": 3, "epoch": "e2"})];
        fs::write(t.0.join("calls.jsonl"), calls.iter().map(|r| r.to_string() + "\n").collect::<String>()).unwrap();
        t
    }

    fn run(t: &Tmp, args: Value, epoch: Option<Value>) -> Result<Value, Fault> {
        query(&t.0, args.as_object().unwrap(), epoch.as_ref())
    }

    #[test]
    fn filters_and_joins() {
        let t = fixture();
        let all = run(&t, json!({}), None).unwrap();
        assert_eq!((all["matched"].as_u64(), all["unreadable_lines"].as_u64(), all["call"].is_null()), (Some(4), Some(2), true));
        assert_eq!(all["note"], NOTE);
        let last = run(&t, json!({"limit": 1}), None).unwrap();
        assert_eq!((last["returned"].as_u64(), last["receipts"][0]["timestamp"].clone()), (Some(1), json!(true)));
        let digests = run(&t, json!({"kind": "fs_digest", "since": 10, "until": 29.9}), None).unwrap();
        assert_eq!(digests["matched"], 1);

        // Call 1 means the kernel's own unless an epoch is named.
        let mine = run(&t, json!({"call": 1}), Some(json!("e1"))).unwrap();
        assert_eq!((mine["matched"].as_u64(), mine["call"]["epoch"].as_str(), mine["unreadable_lines"].as_u64()), (Some(2), Some("e1"), Some(2)));
        let named = run(&t, json!({"call": 1, "epoch": "e2"}), Some(json!("e1"))).unwrap();
        assert_eq!(named["matched"], 2);
        let latest = run(&t, json!({"call": 1, "epoch": null}), Some(json!("e1"))).unwrap();
        assert_eq!(latest["call"]["epoch"], "e2");
        // A record without a session_request joins nothing.
        assert_eq!(run(&t, json!({"call": 3}), None).unwrap()["matched"], 0);
    }

    #[test]
    fn refuses_bad_arguments() {
        let t = fixture();
        let err = |args: Value, epoch: Option<Value>| run(&t, args, epoch).unwrap_err().to_string();
        assert_eq!(err(json!({"call": true}), None), "ValueError: call must be an integer");
        assert_eq!(err(json!({"call": 1.0}), None), "ValueError: call must be an integer");
        assert_eq!(err(json!({"call": 1}), Some(json!(7))), "ValueError: epoch must be a string");
        assert_eq!(err(json!({"kind": 1}), None), "ValueError: kind must be a string");
        assert_eq!(err(json!({"until": "x"}), None), "ValueError: until must be a number (unix seconds)");
        assert_eq!(err(json!({"limit": null}), None), "ValueError: limit must be an integer");
        assert_eq!(err(json!({"limit": 501}), None), "ValueError: limit must be between 1 and 500");
        assert_eq!(err(json!({"limit": -1}), None), "ValueError: limit must be between 1 and 500");
        assert_eq!(err(json!({"call": 2}), None), "ValueError: no record of call 2");
        assert_eq!(err(json!({"call": 9, "epoch": "e2"}), None), "ValueError: no record of call 9 in epoch e2");
    }

    #[test]
    fn missing_files_read_as_empty_and_nothing_is_written() {
        let t = Tmp(util::mkdtemp("palette-receipts-").unwrap());
        let empty = run(&t, json!({}), None).unwrap();
        assert_eq!((empty["matched"].as_u64(), empty["returned"].as_u64(), empty["unreadable_lines"].as_u64()), (Some(0), Some(0), Some(0)));
        assert!(empty["call"].is_null());
        let gone = query(&t.0.join("does-not-exist"), json!({"kind": "fs_digest"}).as_object().unwrap(), None).unwrap();
        assert_eq!(gone["receipts"], json!([]));
        // A call needs calls.jsonl.
        fs::write(t.0.join("receipts.jsonl"), "{\"session_request\": \"E1:1\", \"timestamp\": 1}\n").unwrap();
        assert_eq!(run(&t, json!({"call": 1}), None).unwrap_err().to_string(), "ValueError: no record of call 1");

        let t = fixture();
        let snapshot = || {
            let mut files: Vec<_> = fs::read_dir(&t.0).unwrap().flatten().map(|e| (e.file_name(), fs::read(e.path()).unwrap())).collect();
            files.sort();
            files
        };
        let before = snapshot();
        run(&t, json!({"call": 1, "limit": 10}), Some(json!("e1"))).unwrap();
        assert_eq!(before, snapshot());
    }

    #[test]
    fn time_bounds_are_inclusive() {
        let t = fixture();
        let stamps = |args: Value| -> Vec<Value> {
            run(&t, args, None).unwrap()["receipts"].as_array().unwrap().iter().map(|r| r["timestamp"].clone()).collect()
        };
        assert_eq!(stamps(json!({"since": 20.5, "until": 30})), [json!(20.5), json!(30)]);
        assert_eq!(stamps(json!({"until": 10})), [json!(10)]);
    }
}
