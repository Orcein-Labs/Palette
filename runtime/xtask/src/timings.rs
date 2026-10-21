//! Recorded test-case seconds: dealing the suites' cases across CI shards by
//! them, and recording them from CI job logs.
//!
//! Cases are keyed `suite:Class.method` (`test_session:TestSession.test_x`),
//! or `target:case` for a conformance target (`xray:unknown_call_is_an_error`).

use crate::Fail;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const WEIGHTS: &str = "runtime/security/test-case-seconds.json";
/// Shard 1 also runs the shared gates, which took about this long in CI.
const SHARED_GATE_SECONDS: f64 = 90.0;

pub type Weights = BTreeMap<String, f64>;

pub fn load(path: &Path) -> Result<Weights, String> {
    let at = |e: String| format!("{}: {e}", path.display());
    let text = fs::read_to_string(path).map_err(|e| at(e.to_string()))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| at(e.to_string()))?;
    let object = value.as_object().ok_or_else(|| at("expected an object of case seconds".into()))?;
    object.iter().map(|(k, v)| v.as_f64().map(|s| (k.clone(), s)).ok_or_else(|| at(format!("{k} has no number of seconds")))).collect()
}

/// The weights as the file holds them: sorted, one case a line.
pub fn format(weights: &Weights) -> String {
    if weights.is_empty() {
        return "{}\n".into();
    }
    let lines: Vec<String> = weights.iter().map(|(k, s)| format!(" {}: {}", serde_json::Value::from(k.as_str()), number(*s))).collect();
    format!("{{\n{}\n}}\n", lines.join(",\n"))
}

fn number(s: f64) -> String {
    if s.fract() == 0.0 && s.abs() < 1e15 {
        format!("{}", s as i64)
    } else {
        format!("{s}")
    }
}

pub struct Shard {
    pub index: usize,
    pub count: usize,
}

impl Shard {
    pub fn parse(spec: &str) -> Result<Shard, String> {
        let bad = || format!("shard {spec} is not of the form I/N with 1 <= I <= N");
        let (index, count) = spec.split_once('/').ok_or_else(bad)?;
        let (index, count) = (index.parse().map_err(|_| bad())?, count.parse().map_err(|_| bad())?);
        if !(1..=count).contains(&index) {
            return Err(bad());
        }
        Ok(Shard { index, count })
    }

    /// Which of `keys` (the cases, in listing order) are this shard's. The
    /// cases are dealt longest first, each to the least-loaded shard, with
    /// shard 1 starting loaded by the shared gates. A case with no record
    /// costs its suite's mean, and a suite with none the mean of all.
    pub fn deal(&self, keys: &[String], weights: &Weights) -> Vec<bool> {
        let mut suites: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
        for (key, s) in weights {
            let suite = suites.entry(suite_of(key)).or_default();
            suite.0 += s;
            suite.1 += 1.0;
        }
        let fallback = weights.values().sum::<f64>() / weights.len().max(1) as f64;
        let seconds: Vec<f64> = keys
            .iter()
            .map(|k| weights.get(k).copied().unwrap_or_else(|| suites.get(suite_of(k)).map_or(fallback, |(sum, n)| sum / n)))
            .collect();
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_by(|&a, &b| seconds[b].total_cmp(&seconds[a]).then_with(|| keys[a].cmp(&keys[b])));
        let mut loads = vec![0.0; self.count];
        loads[0] = SHARED_GATE_SECONDS;
        let mut mine = vec![false; keys.len()];
        for case in order {
            let job = (0..self.count).min_by(|&a, &b| loads[a].total_cmp(&loads[b]).then(a.cmp(&b))).expect("at least one shard");
            loads[job] += seconds[case];
            mine[case] = job == self.index - 1;
        }
        mine
    }
}

fn suite_of(key: &str) -> &str {
    key.split(':').next().unwrap_or(key)
}

/// Each case's seconds in a security-suite job log. A case runs from the end
/// of the case before it, or from the start of its suite, so the first case
/// of a suite carries the suite's start-up.
pub fn case_seconds(log: &str) -> Result<BTreeMap<String, f64>, String> {
    let (mut seconds, mut suite, mut case, mut previous) = (BTreeMap::new(), None::<String>, None::<String>, None::<i64>);
    for raw in log.strip_prefix('\u{feff}').unwrap_or(log).split(['\n', '\r']) {
        let line: Vec<char> = strip_colour(raw).chars().collect();
        if line.len() < 28 || line[27] != 'Z' {
            continue;
        }
        let stamp: String = line[..26].iter().collect();
        let at = micros(&stamp).ok_or_else(|| format!("unreadable timestamp {stamp:?}"))?;
        let text: String = line[28..].iter().collect();
        let text = text.trim();
        if let Some(command) = text.strip_prefix("verification: ") {
            (suite, case, previous) = (suite_in(command), None, Some(at));
            continue;
        }
        if let Some(found) = case_in(text) {
            case = Some(found);
        }
        if let (Some(s), Some(c), Some(p)) = (&suite, &case, previous) {
            if ["... ok", "... FAIL", "... ERROR", "... skipped", "... ignored"].iter().any(|end| text.contains(end)) {
                seconds.insert(format!("{s}:{c}"), ((at - p) as f64 / 1e6).round_ties_even());
                (case, previous) = (None, Some(at));
            }
        }
    }
    Ok(seconds)
}

/// Merges a log's times into the record, as the mean of the record and the
/// new time: a single run's times vary too much to deal shards on.
pub fn merge(weights: &mut Weights, seconds: BTreeMap<String, f64>) {
    for (key, s) in seconds {
        let merged = weights.get(&key).map_or(s, |old| ((old + s) / 2.0).round_ties_even());
        weights.insert(key, merged);
    }
}

pub fn record(weights_path: &Path, logs: &[PathBuf]) -> Result<usize, String> {
    let mut weights = if weights_path.exists() { load(weights_path)? } else { Weights::new() };
    for log in logs {
        let bytes = fs::read(log).map_err(|e| format!("{}: {e}", log.display()))?;
        merge(&mut weights, case_seconds(&String::from_utf8_lossy(&bytes)).map_err(|e| format!("{}: {e}", log.display()))?);
    }
    let tmp = weights_path.with_extension("json.tmp");
    fs::write(&tmp, format(&weights)).and_then(|()| fs::rename(&tmp, weights_path)).map_err(|e| format!("{}: {e}", weights_path.display()))?;
    Ok(weights.len())
}

pub fn record_main(args: &[String]) -> Result<(), Fail> {
    if args.is_empty() || args.iter().any(|a| a.starts_with('-')) {
        return Err(Fail::Usage("record-test-case-seconds takes one or more job log paths".into()));
    }
    let logs: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
    let count = record(&crate::root().join(WEIGHTS), &logs)?;
    println!("{count} cases in test-case-seconds.json");
    Ok(())
}

/// The line without its terminal colour codes (`ESC [ digits/; m`).
fn strip_colour(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("\x1b[") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let params = after.find(|c: char| !(c.is_ascii_digit() || c == ';')).unwrap_or(after.len());
        if after[params..].starts_with('m') {
            rest = &after[params + 1..];
        } else {
            out.push_str("\x1b[");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_len(s: &str) -> usize {
    s.find(|c: char| !word_char(c)).unwrap_or(s.len())
}

/// The suite a `verification:` line runs: the target of a conformance
/// `cargo test --test <target>`, or else the first `/test_*.py` it names.
fn suite_in(command: &str) -> Option<String> {
    if command.contains("/runtime/conformance/Cargo.toml ") {
        let target = command.split_once(" --test ")?.1;
        let target = &target[..word_len(target)];
        return (!target.is_empty()).then(|| target.to_string());
    }
    command.match_indices('/').find_map(|(at, _)| {
        let rest = &command[at + 1..];
        let word = &rest[..word_len(rest)];
        (word.len() > "test_".len() && word.starts_with("test_") && rest[word.len()..].starts_with(".py")).then(|| word.to_string())
    })
}

/// `Class.method` from a unittest line `test_x (__main__.Class.test_x) ...`,
/// or the case of a libtest line `test case_name ... ok`.
fn case_in(text: &str) -> Option<String> {
    if let Some(rest) = text.strip_prefix("test ") {
        let case = &rest[..word_len(rest)];
        return (!case.is_empty() && rest[case.len()..].starts_with(" ...")).then(|| case.to_string());
    }
    let method = &text[..word_len(text)];
    if method.len() <= "test".len() || !method.starts_with("test") {
        return None;
    }
    let rest = text[method.len()..].strip_prefix(" (__main__.")?;
    let class = &rest[..word_len(rest)];
    (!class.is_empty() && rest[class.len()..].starts_with('.')).then(|| format!("{class}.{method}"))
}

/// Microseconds since the epoch of `YYYY-MM-DDTHH:MM:SS.ffffff`.
fn micros(stamp: &str) -> Option<i64> {
    let b = stamp.as_bytes();
    let field = |from: usize, to: usize| -> Option<i64> {
        let s = stamp.get(from..to)?;
        s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse().ok())?
    };
    if b.len() != 26 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') || b[13] != b':' || b[16] != b':' || b[19] != b'.' {
        return None;
    }
    let (y, m, d) = (field(0, 4)?, field(5, 7)?, field(8, 10)?);
    let (hh, mm, ss, us) = (field(11, 13)?, field(14, 16)?, field(17, 19)?, field(20, 26)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    // Days from 1970-01-01 to the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1_000_000) + us)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weights(pairs: &[(&str, f64)]) -> Weights {
        pairs.iter().map(|(k, s)| (k.to_string(), *s)).collect()
    }

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn shard_specs_must_name_one_of_n() {
        let s = Shard::parse("2/4").unwrap();
        assert_eq!((s.index, s.count), (2, 4));
        for bad in ["0/4", "5/4", "4", "a/4", "1/", "/1", "1/0", "-1/4"] {
            assert!(Shard::parse(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn deals_longest_first_to_the_least_loaded_shard() {
        let w = weights(&[("a:A.t1", 100.0), ("a:A.t2", 50.0), ("b:B.t1", 40.0), ("b:B.t2", 10.0)]);
        // Unrecorded: a:A.t3 costs suite a's mean (75), c:C.t1 the mean of all (50).
        let k = keys(&["a:A.t1", "a:A.t2", "b:B.t1", "b:B.t2", "a:A.t3", "c:C.t1"]);
        // Loads start [90, 0]: t1 100 -> 2 (100); t3 75 -> 1 (165); A.t2 50 -> 2 (150);
        // C.t1 50 -> 2 (200); B.t1 40 -> 1 (205); B.t2 10 -> 2 (210).
        let one = Shard { index: 1, count: 2 }.deal(&k, &w);
        let two = Shard { index: 2, count: 2 }.deal(&k, &w);
        assert_eq!(one, [false, false, true, false, true, false]);
        assert_eq!(two, one.iter().map(|m| !m).collect::<Vec<_>>());
        assert!(Shard { index: 1, count: 1 }.deal(&k, &w).iter().all(|m| *m));
    }

    #[test]
    fn equal_times_deal_by_key_then_lowest_shard() {
        let k = keys(&["s:B.t", "s:A.t", "s:C.t"]);
        let w = weights(&[("s:A.t", 10.0), ("s:B.t", 10.0), ("s:C.t", 10.0)]);
        // [90,0,0]: A.t -> 2, B.t -> 3, C.t -> 2.
        assert_eq!(Shard { index: 2, count: 3 }.deal(&k, &w), [false, true, true]);
        assert_eq!(Shard { index: 3, count: 3 }.deal(&k, &w), [true, false, false]);
        assert_eq!(Shard { index: 1, count: 2 }.deal(&k, &Weights::new()), [false, false, false]);
    }

    const LOG: &str = "\u{feff}2026-10-20T10:00:00.0000000Z \x1b[36;1mverification: python3 /w/runtime/security/test_session.py -v\x1b[0m
2026-10-20T10:00:04.4000000Z test_one (__main__.TestSession.test_one) ... ok
2026-10-20T10:00:04.9000000Z test_two (__main__.TestSession.test_two)
2026-10-20T10:00:07.4000000Z noise from the case
2026-10-20T10:00:07.4000000Z ... FAIL
2026-10-20T10:00:09.0000000Z verification: cargo build --release
2026-10-20T10:00:30.0000000Z test_nope (__main__.TestOther.test_nope) ... ok
2026-10-20T10:01:00.0000000Z verification: python3 /w/runtime/security/test_tools.py -v cases
2026-10-20T10:02:01.5000000Z test_x (__main__.TestTools.test_x) ... skipped 'no'
short line ... ok
2026-10-20T10:03:00.0000000Z verification: cargo test --release --locked --manifest-path /w/runtime/conformance/Cargo.toml --test xray -- --test-threads=1
2026-10-20T10:03:00.1000000Z running 2 tests
2026-10-20T10:03:09.0000000Z test call_numbers_restart_per_kernel ... ok
2026-10-20T10:03:15.0000000Z test unknown_call_is_an_error ... FAILED
2026-10-20T10:03:15.0000000Z test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
";

    #[test]
    fn reads_case_seconds_from_a_job_log() {
        let got = case_seconds(LOG).unwrap();
        // 4.4 rounds to 4; 3.0 from the end of test_one; 61.5 rounds half to even.
        assert_eq!(
            got,
            weights(&[
                ("test_session:TestSession.test_one", 4.0),
                ("test_session:TestSession.test_two", 3.0),
                ("test_tools:TestTools.test_x", 62.0),
                ("xray:call_numbers_restart_per_kernel", 9.0),
                ("xray:unknown_call_is_an_error", 6.0),
            ])
        );
        assert!(case_seconds("2026-13-20T10:00:00.0000000Z x").is_err());
    }

    #[test]
    fn merges_as_the_rounded_mean_and_writes_sorted() {
        let mut w = weights(&[("b:B.t", 5.0), ("a:A.t", 2.0)]);
        merge(&mut w, weights(&[("a:A.t", 5.0), ("c:C.t", 7.0)]));
        // (2 + 5) / 2 = 3.5 rounds half to even.
        assert_eq!(w, weights(&[("a:A.t", 4.0), ("b:B.t", 5.0), ("c:C.t", 7.0)]));
        assert_eq!(format(&w), "{\n \"a:A.t\": 4,\n \"b:B.t\": 5,\n \"c:C.t\": 7\n}\n");
        assert_eq!(format(&Weights::new()), "{}\n");
    }

    #[test]
    fn record_round_trips_the_checked_in_file_and_adds_a_log() {
        let dir = std::env::temp_dir().join(format!("palette-xtask-record-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (file, log) = (dir.join("weights.json"), dir.join("shard1.log"));
        let checked_in = fs::read_to_string(crate::root().join(WEIGHTS)).unwrap();
        fs::write(&file, &checked_in).unwrap();
        assert_eq!(record(&file, &[]).unwrap(), load(&file).unwrap().len());
        assert_eq!(fs::read_to_string(&file).unwrap(), checked_in);
        fs::write(&log, LOG).unwrap();
        record(&file, &[log]).unwrap();
        let mut expected = load(&crate::root().join(WEIGHTS)).unwrap();
        merge(&mut expected, case_seconds(LOG).unwrap());
        assert_eq!(load(&file).unwrap(), expected);
        fs::remove_dir_all(&dir).unwrap();
    }
}
