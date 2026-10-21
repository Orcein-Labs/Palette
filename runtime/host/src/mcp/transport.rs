//! Palette execution arguments at the host transport boundary.
//!
//! `source` is opaque program text carried by the MCP/JSON transport. It is
//! never embedded in another Julia string literal: it becomes the worker's
//! `code` field only after JSON decoding. `code` stays supported; exactly one
//! of the two is sent.
//!
//! `context` and `budget` choose the call's execution context; neither
//! applies to an ephemeral call. They are refused here, before the call
//! starts, because a rejected request must not stop the kernel. Same wording
//! as the host (cli.rs) and the kernel (contexts.jl).

use serde_json::{json, Map, Value};

use crate::{cli, util};

pub(crate) fn normalize_palette_call(args: &Value) -> Result<Map<String, Value>, String> {
    let mut value = args.as_object().ok_or("Palette arguments must be an object")?.clone();
    let has_code = value.contains_key("code");
    let has_source = value.contains_key("source");
    if has_code == has_source {
        return Err("exactly one of code or source is required".into());
    }
    let field = if has_source { "source" } else { "code" };
    if !value[field].is_string() {
        return Err(format!("{field} must be a string"));
    }
    if let Some(source) = value.remove("source") {
        value.insert("code".into(), source);
    }
    check_context(&value)?;
    Ok(value)
}

pub(crate) const CONTEXT_NAMES: [&str; 5] = ["normal", "traced", "reference", "memoized", "budgeted"];

pub(crate) fn context_schema() -> Map<String, Value> {
    let schema = json!({
        "context": {"type": "string", "enum": CONTEXT_NAMES},
        "budget": {"type": "object", "properties": {"seconds": {"type": "number", "exclusiveMinimum": 0},
                                                    "bytes": {"type": "integer", "minimum": 1}},
                   "additionalProperties": false},
    });
    let Value::Object(map) = schema else { unreachable!() };
    map
}

/// A value as Python's `str` prints it in a message.
fn py_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        other => other.to_string(),
    }
}

fn present<'a>(args: &'a Map<String, Value>, field: &str) -> Option<&'a Value> {
    args.get(field).filter(|v| !v.is_null())
}

pub(crate) fn check_context(args: &Map<String, Value>) -> Result<(), String> {
    let ephemeral = args.get("ephemeral") == Some(&Value::Bool(true));
    for field in ["context", "budget"] {
        if ephemeral && present(args, field).is_some() {
            return Err(format!(
                "request '{field}' does not apply to an ephemeral call: a scratch worker always runs in the normal context"
            ));
        }
    }
    let context = present(args, "context");
    if present(args, "budget").is_some() && context != Some(&json!("budgeted")) {
        return Err(match context {
            None => "a budget applies only to the budgeted context: send context \"budgeted\" with it".into(),
            Some(c) => format!("a budget applies only to the budgeted context, not \"{}\"", py_text(c)),
        });
    }
    Ok(())
}

/// A call's time limit when it names none, and the longest it may name:
/// PALETTE_TURN_TIMEOUT (default 60) and PALETTE_MAX_TURN_TIMEOUT (default
/// 1800), passed to the host as --turn-timeout and --max-turn-timeout.
pub(crate) fn turn_limits(var: impl Fn(&str) -> Option<String>) -> Result<(f64, f64), String> {
    let read = |key: &str, default: &str| {
        let text = var(key).unwrap_or_else(|| default.into());
        text.trim().parse::<f64>().map_err(|_| format!("could not convert string to float: '{text}'"))
    };
    let (default, longest) = (read("PALETTE_TURN_TIMEOUT", "60")?, read("PALETTE_MAX_TURN_TIMEOUT", "1800")?);
    if !(0.0 < default && default <= longest && longest <= cli::LONGEST_TURN_TIMEOUT) {
        return Err(format!(
            "PALETTE_TURN_TIMEOUT must be positive and no more than PALETTE_MAX_TURN_TIMEOUT, which is at most {} (7 days)",
            cli::LONGEST_TURN_TIMEOUT
        ));
    }
    Ok((default, longest))
}

/// A limit as short text that parses back to the same number.
pub(crate) fn seconds_text(t: f64) -> String {
    let short = util::py_g(t);
    if short.parse::<f64>() == Ok(t) { short } else { util::py_repr_f64(t) }
}

/// The limit a call runs under. A `timeout_s` outside (0, max], or on a
/// scratch call, is refused by the host at once, so it waits the default.
pub(crate) fn call_limit(args: &Map<String, Value>, limits: (f64, f64)) -> f64 {
    let ephemeral = args.get("ephemeral") == Some(&Value::Bool(true));
    match args.get("timeout_s").and_then(Value::as_f64) {
        Some(t) if 0.0 < t && t <= limits.1 && !ephemeral => t,
        _ => limits.0,
    }
}

pub(crate) fn timeout_schema(limits: (f64, f64)) -> Value {
    json!({"type": "number", "exclusiveMinimum": 0, "maximum": limits.1,
           "description": format!("This call's time limit in seconds; default {}, at most {}. \
                                   Not for an ephemeral call, which runs under the default. \
                                   Longer work runs as a job: Palette.host_command(name; wait=false).",
                                  util::py_g(limits.0), util::py_g(limits.1))})
}

/// The palette tool's arguments, as both the adapter and the router list them.
pub(crate) fn palette_properties(limits: (f64, f64)) -> Map<String, Value> {
    let mut props = json!({
        "code": {"type": "string"}, "source": {"type": "string"}, "view": {"enum": ["quiet", "full"]},
        "payload": {"oneOf": [{"type": "string"}, {"type": "object", "additionalProperties": {"type": "string"}}]},
        "ephemeral": {"type": "boolean"}, "timeout_s": timeout_schema(limits),
    });
    let Value::Object(map) = &mut props else { unreachable!() };
    map.extend(context_schema());
    map.clone()
}

/// palette_control's arguments besides action and the receipt query.
pub(crate) fn control_properties() -> Map<String, Value> {
    let props = json!({"restore": {"type": "boolean"}, "prepare": {"type": "boolean"},
                       "checkpoint": {"type": "string", "maxLength": 64}, "fork": {"type": "boolean"}});
    let Value::Object(map) = props else { unreachable!() };
    map
}

// The ends of the two tools' descriptions, the same on both servers.
pub(crate) const PALETTE_GUIDE: &str = "timeout_s sets this call's time limit; Palette.host_commands() lists the host commands allowed; \
Palette.host_command(name; wait=false) runs longer host work as a job. context runs this one call \
traced (records the method each top-level call dispatched to, see Palette.xray), reference \
(registered reference implementations replace the fast ones), memoized, or budgeted \
(budget={seconds, bytes}); Palette.context!(name) sets a session default. Api.promote(:f) keeps a \
helper used in 3 successful calls as a project tool (.palette/tools); Api.tools() lists project and \
trusted tools, which load on first call; Api.trust(:f) asks a human to approve it as trusted. \
Api.definitions(name) and Api.callers(name) query the workspace code graph (Api.codegraph()). Each \
top-level statement sees the methods earlier ones defined; a function body or let block that defines \
methods with eval or include sees them only through Base.invokelatest(f, args...). A struct \
redefined with a new layout is a new type: the reply names the values and methods still using the \
old one.";

pub(crate) const CONTROL_GUIDE: &str = "action=checkpoints lists the checkpoints Palette.checkpoint(name) saved; action=restore with \
checkpoint=name restarts the world at that checkpoint (fork=true makes it a new world), after saving \
the current world as an auto- checkpoint. action=receipts returns the host receipts of call=n (this \
kernel's, unless epoch is given; Palette.xray(n) shows its costs), or filters them by kind and \
since/until (unix seconds).";

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn source_becomes_code_and_the_two_are_exclusive() {
        assert_eq!(normalize_palette_call(&json!({"source": "1+1", "view": "full"})).unwrap(), obj(json!({"code": "1+1", "view": "full"})));
        assert_eq!(normalize_palette_call(&json!({"code": "x"})).unwrap(), obj(json!({"code": "x"})));
        assert_eq!(normalize_palette_call(&json!([])).unwrap_err(), "Palette arguments must be an object");
        assert_eq!(normalize_palette_call(&json!({})).unwrap_err(), "exactly one of code or source is required");
        assert_eq!(normalize_palette_call(&json!({"code": "a", "source": "b"})).unwrap_err(), "exactly one of code or source is required");
        assert_eq!(normalize_palette_call(&json!({"source": 1})).unwrap_err(), "source must be a string");
        assert_eq!(normalize_palette_call(&json!({"code": null})).unwrap_err(), "code must be a string");
        assert_eq!(
            normalize_palette_call(&json!({"code": "x", "ephemeral": true, "context": "traced"})).unwrap_err(),
            "request 'context' does not apply to an ephemeral call: a scratch worker always runs in the normal context"
        );
    }

    #[test]
    fn context_and_budget_are_checked_before_the_call() {
        let check = |v: Value| check_context(&obj(v));
        assert!(check(json!({"ephemeral": true, "context": null})).is_ok());
        assert!(check(json!({"context": "budgeted", "budget": {"seconds": 1}})).is_ok());
        assert_eq!(
            check(json!({"ephemeral": true, "budget": {}})).unwrap_err(),
            "request 'budget' does not apply to an ephemeral call: a scratch worker always runs in the normal context"
        );
        assert_eq!(check(json!({"budget": {}})).unwrap_err(), "a budget applies only to the budgeted context: send context \"budgeted\" with it");
        assert_eq!(check(json!({"budget": {}, "context": "traced"})).unwrap_err(), "a budget applies only to the budgeted context, not \"traced\"");
        assert_eq!(check(json!({"budget": {}, "context": true})).unwrap_err(), "a budget applies only to the budgeted context, not \"True\"");
    }

    #[test]
    fn turn_limits_and_call_limits() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        assert_eq!(turn_limits(env(&[])).unwrap(), (60.0, 1800.0));
        assert_eq!(turn_limits(env(&[("PALETTE_TURN_TIMEOUT", " 30 "), ("PALETTE_MAX_TURN_TIMEOUT", "604800")])).unwrap(), (30.0, 604800.0));
        let refused = "PALETTE_TURN_TIMEOUT must be positive and no more than PALETTE_MAX_TURN_TIMEOUT, which is at most 604800 (7 days)";
        for bad in [&[("PALETTE_TURN_TIMEOUT", "0")][..], &[("PALETTE_TURN_TIMEOUT", "2000")], &[("PALETTE_MAX_TURN_TIMEOUT", "604801")], &[("PALETTE_TURN_TIMEOUT", "nan")]] {
            assert_eq!(turn_limits(env(bad)).unwrap_err(), refused);
        }
        assert_eq!(turn_limits(env(&[("PALETTE_TURN_TIMEOUT", "soon")])).unwrap_err(), "could not convert string to float: 'soon'");

        let limits = (60.0, 1800.0);
        let limit = |v: Value| call_limit(&obj(v), limits);
        assert_eq!(limit(json!({"timeout_s": 90})), 90.0);
        assert_eq!(limit(json!({"timeout_s": 1800.0})), 1800.0);
        for v in [json!({}), json!({"timeout_s": 0}), json!({"timeout_s": 1801}), json!({"timeout_s": true}), json!({"timeout_s": "9"}),
                  json!({"timeout_s": 90, "ephemeral": true})] {
            assert_eq!(limit(v), 60.0);
        }
        assert_eq!(seconds_text(60.0), "60");
        assert_eq!(seconds_text(1234567.0), "1234567.0");
        assert_eq!(seconds_text(0.1 + 0.2), "0.30000000000000004");
    }

    #[test]
    fn schemas_list_the_arguments_both_servers_take() {
        let props = palette_properties((60.0, 1800.0));
        let keys: Vec<&str> = props.keys().map(String::as_str).collect();
        assert_eq!(keys, ["budget", "code", "context", "ephemeral", "payload", "source", "timeout_s", "view"]);
        assert_eq!(props["timeout_s"]["maximum"], json!(1800.0));
        assert_eq!(
            props["timeout_s"]["description"],
            "This call's time limit in seconds; default 60, at most 1800. Not for an ephemeral call, which runs under the default. \
             Longer work runs as a job: Palette.host_command(name; wait=false)."
        );
        assert_eq!(props["context"]["enum"], json!(CONTEXT_NAMES));
        assert_eq!(control_properties()["checkpoint"], json!({"type": "string", "maxLength": 64}));
    }
}
