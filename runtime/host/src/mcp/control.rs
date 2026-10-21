//! `palette-host control`: palette_control's kernel-free actions, run offline
//! against a session's state or receipts directory. Prints the result as JSON.

use std::path::Path;

use serde_json::{json, Map, Value};

use super::{py_json_loads, receipts, time_machine, Fault};
use crate::util;

const USAGE: &str = "usage: palette-host control checkpoints|restore STATE_DIR [JSON] | receipts RECEIPTS_DIR [JSON]";

fn run(action: &str, dir: &Path, args: &Map<String, Value>) -> Result<Value, Fault> {
    match action {
        "checkpoints" => Ok(time_machine::list_checkpoints(dir)),
        "restore" => time_machine::request_restore(dir, args.get("checkpoint"), args.get("fork")).map(|()| json!({"requested": true})),
        // No kernel runs here, so call n is the latest record of n unless an epoch is named.
        _ => receipts::query(dir, args, None),
    }
}

pub(crate) fn main(rest: &[String]) -> Result<i32, String> {
    let (action, dir, args) = match rest {
        [action, dir] => (action, dir, "{}"),
        [action, dir, args] => (action, dir, args.as_str()),
        _ => return Err(USAGE.into()),
    };
    if !matches!(action.as_str(), "checkpoints" | "restore" | "receipts") {
        return Err(format!("argument action: invalid choice: {} (choose from checkpoints, restore, receipts)", util::py_str(action)));
    }
    let args = match py_json_loads(args).map_err(|e| e.to_string())? {
        Value::Object(map) => map,
        _ => return Err("arguments must be a JSON object".into()),
    };
    let result = run(action, Path::new(dir), &args).map_err(|e| e.to_string())?;
    println!("{}", util::py_dumps(&result, None));
    Ok(0)
}
