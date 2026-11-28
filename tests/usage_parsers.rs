//! Usage parsers against synthetic fixtures, and the check that the fixtures stay synthetic.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axle::domain::Billing;
use axle::usage::parsers::{self, Line, ParsedRecord, ParserState, parse_all};
use axle::usage::report::WireRecord;
use serde_json::Value;

/// Keys whose string values may hold something other than `"…"`: ids, timestamps, enums
/// and versions, never free text.
const ALLOWED_TEXT_KEYS: &[&str] = &[
    "type",
    "timestamp",
    "requestId",
    "id",
    "uuid",
    "parentUuid",
    "sessionId",
    "session_id",
    "response_id",
    "turn_id",
    "agentId",
    "model",
    "role",
    "speed",
    "stop_reason",
    "service_tier",
    "inference_geo",
    "version",
    "cli_version",
    "userType",
    "auth_mode",
];

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/usage")
}

fn read(rel: &str) -> Vec<u8> {
    std::fs::read(fixtures_root().join(rel)).unwrap()
}

fn expected(rel: &str) -> Vec<WireRecord> {
    serde_json::from_slice(&read(rel)).unwrap()
}

/// The collector's rule (Task 9): the first record per key wins, each count the max seen.
fn dedupe_by_key(records: Vec<ParsedRecord>) -> Vec<ParsedRecord> {
    let mut out: Vec<ParsedRecord> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for r in records {
        match at.get(&r.key) {
            Some(&i) => {
                let c = &mut out[i].counts;
                c.input = c.input.max(r.counts.input);
                c.output = c.output.max(r.counts.output);
                c.cache_write_5m = c.cache_write_5m.max(r.counts.cache_write_5m);
                c.cache_write_1h = c.cache_write_1h.max(r.counts.cache_write_1h);
                c.cache_read = c.cache_read.max(r.counts.cache_read);
                c.reasoning = c.reasoning.max(r.counts.reasoning);
            }
            None => {
                at.insert(r.key.clone(), out.len());
                out.push(r);
            }
        }
    }
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Every string under a key not in `ALLOWED_TEXT_KEYS` must be `"…"`.
fn check_strings(v: &Value, key: Option<&str>, where_: &str) {
    match v {
        Value::String(s) => {
            let allowed = key.is_some_and(|k| ALLOWED_TEXT_KEYS.contains(&k));
            assert!(
                allowed || s == "…",
                "{where_}: key {key:?} holds free text {s:?}; use \"…\""
            );
        }
        Value::Array(items) => items.iter().for_each(|i| check_strings(i, key, where_)),
        Value::Object(map) => map
            .iter()
            .for_each(|(k, v)| check_strings(v, Some(k), where_)),
        _ => {}
    }
}

/// An `@` followed by something shaped like a domain (`x.y`).
fn has_email_domain(text: &str) -> bool {
    text.match_indices('@').any(|(i, _)| {
        let rest: String = text[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
            .collect();
        match rest.split_once('.') {
            Some((host, tld)) => !host.is_empty() && tld.chars().any(|c| c.is_ascii_alphabetic()),
            None => false,
        }
    })
}

#[test]
fn fixtures_are_synthetic() {
    let mut files = Vec::new();
    walk(&fixtures_root(), &mut files);
    let jsonl: Vec<_> = files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    assert!(!jsonl.is_empty(), "no fixtures found");
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        let name = path.display().to_string();
        for bad in ["/Users/", "/home/", "sk-"] {
            assert!(!text.contains(bad), "{name} contains {bad:?}");
        }
        assert!(!has_email_domain(&text), "{name} contains an email address");
    }
    for path in jsonl {
        let text = std::fs::read_to_string(path).unwrap();
        for (n, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let where_ = format!("{}:{}", path.display(), n + 1);
            let v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{where_}: {e}"));
            check_strings(&v, None, &where_);
        }
    }
}

#[test]
fn the_privacy_check_catches_text() {
    let v: Value = serde_json::from_str(r#"{"message":{"content":[{"text":"hi"}]}}"#).unwrap();
    let caught = std::panic::catch_unwind(|| check_strings(&v, None, "t")).is_err();
    assert!(caught);
    assert!(has_email_domain("mail me at a@example.com"));
    assert!(!has_email_domain("pkg@1.2 and @types"));
}

fn parse_fixture(rel: &str) -> Vec<ParsedRecord> {
    let p = parsers::by_id("claude-code/1").unwrap();
    parse_all(p, &read(rel), &mut ParserState::default())
}

#[test]
fn claude_main_fixture() {
    let got = parse_fixture("claude-code/2.1.329/main.jsonl");
    assert_eq!(
        dedupe_by_key(got)
            .iter()
            .map(|r| r.to_wire(Billing::Unknown))
            .collect::<Vec<_>>(),
        expected("claude-code/2.1.329/main.expected.json")
    );
}

#[test]
fn claude_skips_synthetic_and_strips_suffix() {
    let got = parse_fixture("claude-code/2.1.329/main.jsonl");
    assert!(got.iter().all(|r| r.model != "<synthetic>"));
    assert!(got.iter().all(|r| !r.model.contains('[')));
    assert!(got.iter().all(|r| !r.fallback));
    let long = got.iter().find(|r| r.key == "req_011CVd4Long").unwrap();
    assert_eq!(long.model, "claude-opus-5-5");
}

#[test]
fn claude_subagent_fixture() {
    let got = parse_fixture("claude-code/2.1.329/subagent.jsonl");
    assert_eq!(
        dedupe_by_key(got)
            .iter()
            .map(|r| r.to_wire(Billing::Unknown))
            .collect::<Vec<_>>(),
        expected("claude-code/2.1.329/subagent.expected.json")
    );
}

#[test]
fn claude_files_include_subagents() {
    let dir = std::env::temp_dir().join(format!("axl-parsers-{}", uuid::Uuid::now_v7()));
    let proj = dir.join("proj");
    let subs = proj.join("abc/subagents");
    std::fs::create_dir_all(&subs).unwrap();
    let main = proj.join("abc.jsonl");
    for f in [
        main.clone(),
        subs.join("agent-b.jsonl"),
        subs.join("agent-a.jsonl"),
        subs.join("other.txt"),
    ] {
        std::fs::write(f, b"").unwrap();
    }
    let p = parsers::by_id("claude-code/1").unwrap();
    assert_eq!(
        p.files(&main),
        vec![
            main.clone(),
            subs.join("agent-a.jsonl"),
            subs.join("agent-b.jsonl")
        ]
    );
    // No subagent directory: the transcript alone.
    let lone = proj.join("lone.jsonl");
    assert_eq!(p.files(&lone), vec![lone.clone()]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn claude_unrecognised_usage_line() {
    let p = parsers::by_id("claude-code/1").unwrap();
    let mut s = ParserState::default();
    let shaped = r#"{"type":"assistant","timestamp":"…","message":{"usage":{"input_tokens":"x"}}}"#;
    assert_eq!(p.line(shaped.as_bytes(), &mut s), Line::UsageShaped);
    // Valid usage, but no key or a bad timestamp.
    let no_key = br#"{"type":"assistant","timestamp":"2026-11-28T10:00:00Z","message":{"model":"claude-opus-5-5","usage":{"input_tokens":1}}}"#;
    assert_eq!(p.line(no_key, &mut s), Line::UsageShaped);
    let bad_ts = r#"{"type":"assistant","timestamp":"…","requestId":"r","message":{"model":"claude-opus-5-5","usage":{"input_tokens":1}}}"#;
    assert_eq!(p.line(bad_ts.as_bytes(), &mut s), Line::UsageShaped);
    assert_eq!(p.line(br#"{"type":"user"}"#, &mut s), Line::Other);
    assert_eq!(p.line(b"not json", &mut s), Line::Other);
    // A deliberate skip is not a format problem.
    let zero = br#"{"type":"assistant","timestamp":"2026-11-28T10:00:00Z","requestId":"r","message":{"model":"claude-opus-5-5","usage":{"input_tokens":0}}}"#;
    assert_eq!(p.line(zero, &mut s), Line::Other);
}

#[test]
fn ids_and_harness_names() {
    use axle::hook::Harness;
    assert_eq!(Harness::ClaudeCode.name(), "claude-code");
    assert_eq!(Harness::Codex.name(), "codex");
    assert_eq!(
        parsers::for_harness(Harness::ClaudeCode).id(),
        "claude-code/1"
    );
    assert!(parsers::by_id("claude-code/9").is_none());
}

fn wire(records: &[ParsedRecord]) -> Vec<WireRecord> {
    records
        .iter()
        .map(|r| r.to_wire(Billing::Unknown))
        .collect()
}

fn parse_codex(rel: &str) -> Vec<ParsedRecord> {
    let p = parsers::by_id("codex/1").unwrap();
    parse_all(p, &read(rel), &mut ParserState::default())
}

#[test]
fn codex_rollout_fixture() {
    let got = parse_codex("codex/0.172.0/rollout.jsonl");
    assert_eq!(wire(&got), expected("codex/0.172.0/rollout.expected.json"));
    assert!(got.iter().all(|r| !r.fallback));
    // The model changes mid-session; each record carries the latest turn_context model.
    assert_eq!(got[0].model, "gpt-5.3-codex");
    assert_eq!(got[1].model, "gpt-5.2");
}

#[test]
fn codex_legacy_token_count_fixture() {
    let got = parse_codex("codex/legacy-token-count/rollout.jsonl");
    assert_eq!(
        wire(&got),
        expected("codex/legacy-token-count/rollout.expected.json")
    );
    let keys: Vec<_> = got.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "0199a8f2-6c1e-7d30-9a41-0000legacy01:1",
            "0199a8f2-6c1e-7d30-9a41-0000legacy01:2"
        ]
    );
    assert!(got.iter().all(|r| r.fallback));
}

#[test]
fn codex_mixed_rollout_counts_once() {
    let got = parse_codex("codex/0.172.0/mixed.jsonl");
    assert_eq!(wire(&got), expected("codex/0.172.0/mixed.expected.json"));
    assert!(got.iter().all(|r| !r.fallback));
}

#[test]
fn codex_drop_fallback_only_when_the_file_has_records() {
    let p = parsers::by_id("codex/1").unwrap();
    let bytes = read("codex/0.172.0/mixed.jsonl");
    let mut state = ParserState::default();
    let mut all: Vec<ParsedRecord> = bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| match p.line(l, &mut state) {
            Line::Record(r) => Some(r),
            Line::UsageShaped | Line::Other => None,
        })
        .collect();
    assert!(all.iter().any(|r| r.fallback));
    parsers::drop_fallback(&mut all, &state);
    assert!(!all.is_empty());
    assert!(all.iter().all(|r| !r.fallback));

    let mut fallback_only = parse_codex("codex/legacy-token-count/rollout.jsonl");
    let before = fallback_only.len();
    parsers::drop_fallback(&mut fallback_only, &ParserState::default());
    assert_eq!(fallback_only.len(), before);
}

/// Two reads of one file, the state carried between them, as two hook runs would.
fn split_parse(rel: &str) -> Vec<ParsedRecord> {
    let p = parsers::by_id("codex/1").unwrap();
    let bytes = read(rel);
    let lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    let mid = lines.len() / 2;
    let mut state = ParserState::default();
    let mut got = parse_all(p, &lines[..mid].join(&b'\n'), &mut state);
    got.extend(parse_all(p, &lines[mid..].join(&b'\n'), &mut state));
    got
}

#[test]
fn codex_state_survives_a_split_read() {
    for rel in [
        "codex/0.172.0/rollout.jsonl",
        "codex/legacy-token-count/rollout.jsonl",
    ] {
        assert_eq!(split_parse(rel), parse_codex(rel), "{rel}");
    }
}

#[test]
fn codex_input_excludes_cached() {
    let p = parsers::by_id("codex/1").unwrap();
    let mut s = ParserState {
        model: Some("gpt-5.3-codex".into()),
        ..ParserState::default()
    };
    let line = |input: i64, cached: i64| {
        format!(
            r#"{{"timestamp":"2026-11-28T10:00:00Z","type":"token_usage_record","payload":{{"response_id":"resp_1","usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":5,"output_tokens":7,"reasoning_output_tokens":3}}}}}}"#
        )
    };
    let Line::Record(r) = p.line(line(100, 80).as_bytes(), &mut s) else {
        panic!("no record");
    };
    assert_eq!((r.counts.input, r.counts.cache_read), (20, 80));
    assert_eq!((r.counts.cache_write_5m, r.counts.cache_write_1h), (5, 0));
    assert_eq!((r.counts.output, r.counts.reasoning), (7, 3));
    let Line::Record(r) = p.line(line(50, 80).as_bytes(), &mut s) else {
        panic!("no record");
    };
    assert_eq!((r.counts.input, r.counts.cache_read), (0, 80));
    assert!(s.saw_records);
}

#[test]
fn codex_unrecognised_lines() {
    let p = parsers::by_id("codex/1").unwrap();
    let mut s = ParserState::default();
    let record = br#"{"timestamp":"2026-11-28T10:00:00Z","type":"token_usage_record","payload":{"response_id":"resp_1","usage":{"input_tokens":1}}}"#;
    // No turn_context yet: no model.
    assert_eq!(p.line(record, &mut s), Line::UsageShaped);
    s.model = Some("gpt-5.3-codex".into());
    let count = br#"{"timestamp":"2026-11-28T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1}}}}"#;
    // No session_meta yet: no key.
    assert_eq!(p.line(count, &mut s), Line::UsageShaped);
    let null_info = br#"{"timestamp":"2026-11-28T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#;
    assert_eq!(p.line(null_info, &mut s), Line::Other);
    let bad = br#"{"timestamp":"2026-11-28T10:00:00Z","type":"token_usage_record","payload":{"response_id":"r","usage":{"input_tokens":"x"}}}"#;
    assert_eq!(p.line(bad, &mut s), Line::UsageShaped);
    let bad_ts = r#"{"timestamp":"…","type":"token_usage_record","payload":{"response_id":"r","usage":{"input_tokens":1}}}"#;
    assert_eq!(p.line(bad_ts.as_bytes(), &mut s), Line::UsageShaped);
    let other_event = br#"{"timestamp":"2026-11-28T10:00:00Z","type":"event_msg","payload":{"type":"agent_message"}}"#;
    assert_eq!(p.line(other_event, &mut s), Line::Other);
    assert_eq!(
        p.line(br#"{"type":"response_item","payload":{}}"#, &mut s),
        Line::Other
    );
    assert_eq!(p.line(b"not json", &mut s), Line::Other);
    assert!(!s.saw_records);
    assert_eq!(s.ordinal, 0);
}

#[test]
fn codex_files_is_the_rollout() {
    use axle::hook::Harness;
    let p = parsers::for_harness(Harness::Codex);
    assert_eq!(p.id(), "codex/1");
    let path = Path::new("rollout.jsonl");
    assert_eq!(p.files(path), vec![path.to_path_buf()]);
}
