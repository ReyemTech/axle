//! The usage collector driven directly: stand-in axum servers on `127.0.0.1:0` (a dead one
//! is `http://127.0.0.1:9`), temp homes only, the environment from a map.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axle::bridge::session_file::SessionInfo;
use axle::clock::{Clock, TestClock};
use axle::config::ServerConfig;
use axle::domain::{Billing, SessionId};
use axle::hook::Harness;
use axle::http::{AppState, router};
use axle::service::identity::{IdentityInput, IdentityService};
use axle::store::sqlite::SqliteStore;
use axle::usage::collect::{Collect, Limits, collect, collect_with};
use axle::usage::cursor;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use serde_json::{Value, json};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use time::macros::datetime;

const FIXTURES: &str = "tests/fixtures/usage";

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("axle-collect-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(p.join(".axl")).unwrap();
        std::fs::write(p.join(".axl").join("token"), "tok").unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    /// The main Claude Code transcript.
    fn transcript(&self) -> PathBuf {
        self.0.join("projects").join("p").join("sess.jsonl")
    }
    fn stdin(&self) -> String {
        stdin_for(&self.transcript())
    }
    fn write(&self, path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn append(&self, path: &Path, text: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }
    fn offset(&self, transcript: &Path) -> u64 {
        cursor::load(self.path(), transcript).unwrap().offset
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stdin_for(transcript: &Path) -> String {
    json!({"transcript_path": transcript, "session_id": "hs-1"}).to_string()
}

type Env = Box<dyn Fn(&str) -> Option<String> + Sync>;

fn env(pairs: &[(&str, &str)]) -> Env {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Box::new(move |k| m.get(k).cloned())
}

fn session(url: &str) -> Option<(SessionInfo, SystemTime)> {
    session_at(url, SystemTime::UNIX_EPOCH)
}

fn session_at(url: &str, mtime: SystemTime) -> Option<(SessionInfo, SystemTime)> {
    Some((
        SessionInfo {
            session_id: SessionId::new(),
            project_key: None,
            server_url: url.to_string(),
            harness: "claude-code".into(),
        },
        mtime,
    ))
}

fn collect_args<'a>(
    harness: Harness,
    home: &'a Path,
    stdin: &'a str,
    session: Option<(SessionInfo, SystemTime)>,
    env: &'a (dyn Fn(&str) -> Option<String> + Sync),
) -> Collect<'a> {
    Collect {
        harness,
        home,
        stdin,
        session,
        env,
        session_end: false,
    }
}

async fn run(
    home: &TempHome,
    stdin: &str,
    session: Option<(SessionInfo, SystemTime)>,
    env: &Env,
) -> Option<String> {
    collect(collect_args(
        Harness::ClaudeCode,
        home.path(),
        stdin,
        session,
        env.as_ref(),
    ))
    .await
}

/// One synthetic assistant line with usage (free text is `…`).
fn assistant(key: &str, at: &str) -> String {
    json!({
        "type": "assistant", "requestId": key, "timestamp": at,
        "message": {"id": format!("msg_{key}"), "model": "claude-opus-5-5",
            "content": [{"type": "text", "text": "…"}],
            "usage": {"input_tokens": 3, "output_tokens": 40, "cache_read_input_tokens": 100}},
    })
    .to_string()
        + "\n"
}

fn ts(minute: u32) -> String {
    format!("2026-11-28T10:{minute:02}:00Z")
}

fn lines(keys: &[&str]) -> String {
    keys.iter()
        .enumerate()
        .map(|(i, k)| assistant(k, &ts(i as u32)))
        .collect()
}

// --- stand-in servers -----------------------------------------------------------------------

type Bodies = Arc<Mutex<Vec<Value>>>;

#[derive(Clone)]
struct Fake {
    status: u16,
    rejected: Value,
    bodies: Bodies,
}

async fn usage_handler(
    State(f): State<Fake>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, Json<Value>) {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer tok") {
        return (StatusCode::UNAUTHORIZED, Json(json!({})));
    }
    let body: Value = serde_json::from_str(&body).unwrap();
    let n = body["records"].as_array().map_or(0, Vec::len);
    f.bodies.lock().unwrap().push(body);
    let rejected = f.rejected.as_array().map_or(0, Vec::len);
    (
        StatusCode::from_u16(f.status).unwrap(),
        Json(json!({"accepted": n - rejected, "duplicate": 0, "rejected": f.rejected})),
    )
}

async fn fake_with(status: u16, rejected: Value) -> (String, Bodies) {
    let bodies = Bodies::default();
    let app = Router::new()
        .route("/v1/usage", post(usage_handler))
        .with_state(Fake {
            status,
            rejected,
            bodies: bodies.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app).into_future());
    (url, bodies)
}

async fn fake_usage(status: u16) -> (String, Bodies) {
    fake_with(status, json!([])).await
}

/// Accepts connections and never answers.
async fn hanging() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    url
}

fn keys(body: &Value) -> Vec<String> {
    body["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap().to_string())
        .collect()
}

fn count(bodies: &Bodies) -> usize {
    bodies.lock().unwrap().len()
}

fn last(bodies: &Bodies) -> Value {
    bodies.lock().unwrap().last().cloned().unwrap()
}

// --- tests ----------------------------------------------------------------------------------

#[tokio::test]
async fn sends_once_and_advances_on_2xx() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["r1", "r2", "r3"]));
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    let s = session(&url);
    let sid = s.as_ref().unwrap().0.session_id;

    assert_eq!(run(&h, &h.stdin(), s.clone(), &e).await, None);
    assert_eq!(count(&bodies), 1);
    let body = last(&bodies);
    assert_eq!(keys(&body), ["r1", "r2", "r3"]);
    assert_eq!(body["parser"], "claude-code/1");
    assert_eq!(body["harness_session_id"], "hs-1");
    assert_eq!(body["session_id"], json!(sid));
    let size = std::fs::metadata(h.transcript()).unwrap().len();
    assert_eq!(h.offset(&h.transcript()), size);
    let status = cursor::load_status(h.path());
    assert!(status.last_success_at.is_some());
    assert_eq!(status.billing.get("claude-code"), Some(&Billing::Unknown));

    assert_eq!(run(&h, &h.stdin(), s, &e).await, None);
    assert_eq!(count(&bodies), 1, "a cursor at EOF sends nothing");
}

#[tokio::test]
async fn keeps_cursor_on_5xx_and_timeout() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["r1", "r2"]));
    let (url, bodies) = fake_usage(503).await;
    let e = env(&[]);

    let line = run(&h, &h.stdin(), session(&url), &e).await.unwrap();
    assert_eq!(line, "usage report not sent (HTTP 503); will retry");
    assert!(cursor::load(h.path(), &h.transcript()).is_none());
    run(&h, &h.stdin(), session(&url), &e).await;
    let sent = bodies.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(keys(&sent[0]), keys(&sent[1]));

    // A 2xx first, so there is a cursor whose offset must not move.
    let (ok, _) = fake_usage(200).await;
    run(&h, &h.stdin(), session(&ok), &e).await;
    let before = h.offset(&h.transcript());
    h.append(&h.transcript(), &assistant("r3", &ts(9)));

    let slow = hanging().await;
    let start = Instant::now();
    let line = tokio::time::timeout(
        Duration::from_secs(3),
        run(&h, &h.stdin(), session(&slow), &e),
    )
    .await
    .expect("collect returns before the 3 s guard")
    .unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(2500),
        "{:?}",
        start.elapsed()
    );
    assert!(line.starts_with("usage report not sent ("), "{line}");
    assert!(line.ends_with("; will retry"), "{line}");
    assert!(!line.contains("127.0.0.1"), "{line}");
    assert_eq!(h.offset(&h.transcript()), before);

    let line = run(&h, &h.stdin(), session("http://127.0.0.1:9"), &e)
        .await
        .unwrap();
    assert_eq!(
        line,
        "usage report not sent (connection failed); will retry"
    );
    assert_eq!(h.offset(&h.transcript()), before);
}

#[tokio::test]
async fn advances_on_403_404_400() {
    for code in [403, 404, 400] {
        let h = TempHome::new();
        h.write(&h.transcript(), &lines(&["r1"]));
        let (url, bodies) = fake_usage(code).await;
        let e = env(&[]);
        let line = run(&h, &h.stdin(), session(&url), &e).await;
        assert_eq!(
            line.as_deref(),
            Some(format!("usage report refused by the server (HTTP {code}); skipped").as_str())
        );
        assert_eq!(run(&h, &h.stdin(), session(&url), &e).await, None);
        assert_eq!(count(&bodies), 1, "HTTP {code}");
    }
}

#[tokio::test]
async fn rejected_records_still_advance() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["r1", "r2", "r3"]));
    let (url, bodies) = fake_with(
        200,
        json!([{"key": "r1", "reason": "timestamp more than 30 days old"}]),
    )
    .await;
    let e = env(&[]);
    let line = run(&h, &h.stdin(), session(&url), &e).await;
    assert_eq!(
        line.as_deref(),
        Some("usage report: 1 of 3 records rejected (timestamp more than 30 days old)")
    );
    assert_eq!(run(&h, &h.stdin(), session(&url), &e).await, None);
    assert_eq!(count(&bodies), 1);
}

#[tokio::test]
async fn partial_last_line_waits() {
    let h = TempHome::new();
    let second = assistant("r2", &ts(2));
    let (head, tail) = second.split_at(second.len() / 2);
    h.write(&h.transcript(), &(assistant("r1", &ts(1)) + head));
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);

    run(&h, &h.stdin(), session(&url), &e).await;
    assert_eq!(keys(&last(&bodies)), ["r1"]);
    h.append(&h.transcript(), tail);
    run(&h, &h.stdin(), session(&url), &e).await;
    assert_eq!(count(&bodies), 2);
    assert_eq!(keys(&last(&bodies)), ["r2"]);
}

#[tokio::test]
async fn shrunk_or_replaced_file_restarts() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["r1", "r2", "r3"]));
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    run(&h, &h.stdin(), session(&url), &e).await;

    // Truncated and rewritten shorter.
    h.write(&h.transcript(), &lines(&["r9"]));
    run(&h, &h.stdin(), session(&url), &e).await;
    assert_eq!(count(&bodies), 2);
    assert_eq!(keys(&last(&bodies)), ["r9"]);

    // A new file renamed over it (new inode), longer than the old offset.
    let other = h.transcript().with_extension("new");
    h.write(&other, &lines(&["r9", "r10", "r11", "r12"]));
    std::fs::rename(&other, h.transcript()).unwrap();
    run(&h, &h.stdin(), session(&url), &e).await;
    assert_eq!(count(&bodies), 3);
    assert_eq!(keys(&last(&bodies)), ["r9", "r10", "r11", "r12"]);
}

#[tokio::test]
async fn new_cursor_drops_history_before_the_session_file() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["r1", "r2", "r3", "r4"]));
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    // Records are at 10:00, 10:01, 10:02, 10:03; the session file is from 10:01:30.
    let mtime: SystemTime = datetime!(2026-11-28 10:01:30 UTC).into();
    run(&h, &h.stdin(), session_at(&url, mtime), &e).await;
    assert_eq!(keys(&last(&bodies)), ["r3", "r4"]);
}

#[tokio::test]
async fn caps_continue_next_run() {
    let h = TempHome::new();
    let many: String = (0..600)
        .map(|i| assistant(&format!("r{i}"), &ts(i % 60)))
        .collect();
    h.write(&h.transcript(), &many);
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    run(&h, &h.stdin(), session(&url), &e).await;
    run(&h, &h.stdin(), session(&url), &e).await;
    let sent = bodies.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(keys(&sent[0]).len(), 500);
    assert_eq!(keys(&sent[1]).len(), 100);
    assert_eq!(keys(&sent[1])[0], "r500");

    // Over 8 MB of padding, then one record: the first run reads 8 MB and sends nothing, the
    // second reads the rest. The parse budget is raised so only the byte cap cuts (a debug
    // build under load can take more than 300 ms to parse 8 MB).
    let h = TempHome::new();
    let pad = json!({"type": "user", "message": "…".repeat(1300)}).to_string() + "\n";
    let mut text = pad.repeat(9 * 1024 * 1024 / pad.len());
    text.push_str(&assistant("late", &ts(1)));
    h.write(&h.transcript(), &text);
    let (url, bodies) = fake_usage(200).await;
    let stdin = h.stdin();
    let bytes_only = Limits {
        parse_budget: Duration::from_secs(60),
        ..Limits::default()
    };
    let c = collect_args(
        Harness::ClaudeCode,
        h.path(),
        &stdin,
        session(&url),
        e.as_ref(),
    );
    collect_with(c, bytes_only).await;
    assert_eq!(count(&bodies), 0);
    let offset = h.offset(&h.transcript());
    assert!(
        offset > 7 * 1024 * 1024 && offset <= 8 * 1024 * 1024,
        "{offset}"
    );
    let c = collect_args(
        Harness::ClaudeCode,
        h.path(),
        &stdin,
        session(&url),
        e.as_ref(),
    );
    collect_with(c, bytes_only).await;
    assert_eq!(count(&bodies), 1, "the second run reads the rest");
    assert_eq!(keys(&last(&bodies)), ["late"]);
}

#[tokio::test]
async fn line_longer_than_cap_is_skipped() {
    let h = TempHome::new();
    let mut text = String::from("{\"type\":\"assistant\",\"message\":\"");
    text.push_str(&"a".repeat(9 * 1024 * 1024));
    text.push('\n');
    text.push_str(&assistant("after", &ts(1)));
    h.write(&h.transcript(), &text);
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    run(&h, &h.stdin(), session(&url), &e).await;
    run(&h, &h.stdin(), session(&url), &e).await;
    let sent = bodies.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(keys(&sent[0]), ["after"]);
    let c = cursor::load(h.path(), &h.transcript()).unwrap();
    assert!(!c.skip_partial);
    assert_eq!(c.offset, text.len() as u64);
}

#[tokio::test]
async fn subagent_files_are_read() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["main1"]));
    let sub = h
        .transcript()
        .with_extension("")
        .join("subagents")
        .join("agent-a.jsonl");
    h.write(&sub, &lines(&["sub1", "sub2"]));
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    run(&h, &h.stdin(), session(&url), &e).await;
    assert_eq!(count(&bodies), 1);
    assert_eq!(keys(&last(&bodies)), ["main1", "sub1", "sub2"]);
    assert!(cursor::load(h.path(), &sub).is_some());
}

#[tokio::test]
async fn off_sends_nothing() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["r1"]));
    let (url, bodies) = fake_usage(200).await;
    let off = env(&[("AXL_USAGE", "off")]);
    assert_eq!(run(&h, &h.stdin(), session(&url), &off).await, None);
    h.write(
        &h.path().join(".axl").join("config.toml"),
        "[usage]\nenabled = false\n",
    );
    assert_eq!(run(&h, &h.stdin(), session(&url), &env(&[])).await, None);
    assert_eq!(count(&bodies), 0);
    assert!(cursor::load(h.path(), &h.transcript()).is_none());
}

#[tokio::test]
async fn a_bad_billing_value_keeps_the_off_switch() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["r1"]));
    let (url, bodies) = fake_usage(200).await;
    h.write(
        &h.path().join(".axl").join("config.toml"),
        "[usage]\nenabled = false\n[usage.billing]\ncodex = \"apii\"\n",
    );
    assert_eq!(run(&h, &h.stdin(), session(&url), &env(&[])).await, None);
    assert_eq!(count(&bodies), 0);
}

#[tokio::test]
async fn an_unreadable_config_turns_usage_off() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["r1"]));
    let (url, bodies) = fake_usage(200).await;
    h.write(
        &h.path().join(".axl").join("config.toml"),
        "server_url = [not toml\n",
    );
    let line = run(&h, &h.stdin(), session(&url), &env(&[])).await;
    assert_eq!(line.as_deref(), Some("config.toml unreadable; usage off"));
    assert_eq!(count(&bodies), 0);
    assert!(cursor::load(h.path(), &h.transcript()).is_none());
}

#[tokio::test]
async fn no_session_or_token_is_silent() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["r1"]));
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    assert_eq!(run(&h, &h.stdin(), None, &e).await, None);
    assert_eq!(
        run(&h, "{}", session(&url), &e).await,
        None,
        "no transcript_path"
    );
    std::fs::remove_file(h.path().join(".axl").join("token")).unwrap();
    assert_eq!(run(&h, &h.stdin(), session(&url), &e).await, None);
    assert_eq!(count(&bodies), 0);
}

#[tokio::test]
async fn falls_back_to_the_cursor_session() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["r1"]));
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    let s = session(&url);
    let sid = s.as_ref().unwrap().0.session_id;
    run(&h, &h.stdin(), s, &e).await;
    h.append(&h.transcript(), &assistant("r2", &ts(5)));
    assert_eq!(run(&h, &h.stdin(), None, &e).await, None);
    assert_eq!(count(&bodies), 2);
    let body = last(&bodies);
    assert_eq!(keys(&body), ["r2"]);
    assert_eq!(body["session_id"], json!(sid));
}

#[tokio::test]
async fn unrecognised_log_warns_once() {
    let h = TempHome::new();
    // Usage-shaped: an assistant line with usage but no timestamp.
    let shaped = json!({"type": "assistant", "requestId": "x",
        "message": {"model": "claude-opus-5-5", "usage": {"input_tokens": 5, "output_tokens": 1}}})
    .to_string()
        + "\n";
    h.write(&h.transcript(), &shaped);
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    assert_eq!(
        run(&h, &h.stdin(), session(&url), &e).await.as_deref(),
        Some("usage parser claude-code/1 does not recognise this log")
    );
    h.append(&h.transcript(), &shaped);
    assert_eq!(run(&h, &h.stdin(), session(&url), &e).await, None);
    assert_eq!(count(&bodies), 0);
}

#[tokio::test]
async fn unreadable_transcript_is_one_line() {
    let h = TempHome::new();
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    let line = run(&h, &h.stdin(), session(&url), &e).await.unwrap();
    assert!(
        line.starts_with("cannot read the transcript for usage ("),
        "{line}"
    );
    assert!(!line.contains(h.path().to_str().unwrap()), "{line}");
    assert_eq!(
        run(&h, &h.stdin(), session(&url), &e).await,
        None,
        "once per session"
    );
    assert_eq!(count(&bodies), 0);
}

#[tokio::test]
async fn session_end_prunes() {
    let h = TempHome::new();
    h.write(&h.transcript(), &lines(&["r1"]));
    let stale = cursor::dir(h.path()).join("0000.json");
    h.write(&stale, "{}");
    let old = SystemTime::now() - Duration::from_secs(40 * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let (url, _) = fake_usage(200).await;
    let e = env(&[]);

    run(&h, &h.stdin(), session(&url), &e).await;
    assert!(stale.exists(), "Stop does not prune");
    let stdin = h.stdin();
    let mut c = collect_args(
        Harness::ClaudeCode,
        h.path(),
        &stdin,
        session(&url),
        e.as_ref(),
    );
    c.session_end = true;
    collect(c).await;
    assert!(!stale.exists());
    assert!(cursor::load(h.path(), &h.transcript()).is_some());
}

/// Every string a record carries, by field name.
fn strings_in(v: &Value, field: Option<&str>, out: &mut Vec<(String, String)>) {
    match v {
        Value::String(s) => out.push((field.unwrap_or("").to_string(), s.clone())),
        Value::Array(a) => a.iter().for_each(|x| strings_in(x, field, out)),
        Value::Object(o) => o.iter().for_each(|(k, x)| strings_in(x, Some(k), out)),
        _ => {}
    }
}

#[tokio::test]
async fn privacy_nothing_but_numbers_and_ids() {
    let h = TempHome::new();
    let fixtures = Path::new(FIXTURES).join("claude-code").join("2.1.329");
    h.write(
        &h.transcript(),
        &std::fs::read_to_string(fixtures.join("main.jsonl")).unwrap(),
    );
    let sub = h
        .transcript()
        .with_extension("")
        .join("subagents")
        .join("agent-a.jsonl");
    h.write(
        &sub,
        &std::fs::read_to_string(fixtures.join("subagent.jsonl")).unwrap(),
    );
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    run(&h, &h.stdin(), session(&url), &e).await;
    let body = last(&bodies);
    assert_eq!(keys(&body).len(), 6);
    let mut found = Vec::new();
    strings_in(&body["records"], None, &mut found);
    for (field, _) in &found {
        assert!(
            ["key", "at", "model", "speed", "billing"].contains(&field.as_str()),
            "{field}"
        );
    }
    let text = body.to_string();
    assert!(!text.contains('…'));
    assert!(!text.contains(h.path().to_str().unwrap()));
    assert!(!text.contains("tok\""));
    assert!(!text.contains("\"tok"));
}

#[tokio::test]
async fn codex_mixed_rollout_split_by_a_cap_counts_once() {
    let h = TempHome::new();
    let fixture =
        std::fs::read_to_string(Path::new(FIXTURES).join("codex/0.172.0/mixed.jsonl")).unwrap();
    // The cap ends the first run right after the first `token_count` line (line 4).
    let first_four: usize = fixture.split_inclusive('\n').take(4).map(str::len).sum();
    let rollout = h
        .path()
        .join(".codex")
        .join("sessions")
        .join("rollout.jsonl");
    h.write(&rollout, &fixture);
    let (url, bodies) = fake_usage(200).await;
    let e = env(&[]);
    let stdin = stdin_for(&rollout);
    let limits = Limits {
        max_file_bytes: first_four as u64,
        ..Limits::default()
    };
    for _ in 0..10 {
        let c = collect_args(Harness::Codex, h.path(), &stdin, session(&url), e.as_ref());
        assert_eq!(collect_with(c, limits).await, None);
        if h.offset(&rollout) == fixture.len() as u64 {
            break;
        }
    }
    assert_eq!(h.offset(&rollout), fixture.len() as u64);
    let sent: Vec<String> = bodies.lock().unwrap().iter().flat_map(keys).collect();
    assert_eq!(sent, ["resp_mixed0001", "resp_mixed0002"]);
}

#[tokio::test]
async fn end_to_end_against_the_server() {
    let h = TempHome::new();
    let fixtures = Path::new(FIXTURES).join("claude-code").join("2.1.329");
    h.write(
        &h.transcript(),
        &std::fs::read_to_string(fixtures.join("main.jsonl")).unwrap(),
    );

    let db = h.path().join("axl.db");
    let opts = SqliteConnectOptions::new()
        .filename(&db)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    let store = Arc::new(SqliteStore::from_pool(pool).await.unwrap());
    let clock = Arc::new(TestClock::new(datetime!(2026-11-28 12:00 UTC)));
    let config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some("test-secret-0123456789".into()),
        _ => None,
    })
    .unwrap();
    let state = AppState::new(store, clock.clone(), config);
    let (token_id, token) = state.auth.create("collect").await.unwrap();
    let verified = state
        .auth
        .verify(&token, clock.now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(verified, token_id);
    let sid = SessionId::new();
    IdentityService::resolve(
        &*state.store,
        &*state.clock,
        IdentityInput {
            project_key: Some("github.com/reyemtech/axle".into()),
            host: "mac".into(),
            harness: "claude-code".into(),
            session: sid,
            token: verified,
        },
    )
    .await
    .unwrap();
    std::fs::write(h.path().join(".axl").join("token"), &token).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, router(state.clone())).into_future());

    let e = env(&[("ANTHROPIC_API_KEY", "x")]);
    let s = Some((
        SessionInfo {
            session_id: sid,
            project_key: Some("github.com/reyemtech/axle".into()),
            server_url: url,
            harness: "claude-code".into(),
        },
        SystemTime::UNIX_EPOCH,
    ));
    assert_eq!(run(&h, &h.stdin(), s, &e).await, None);

    let expected: Vec<Value> = serde_json::from_str(
        &std::fs::read_to_string(fixtures.join("main.expected.json")).unwrap(),
    )
    .unwrap();
    for want in &expected {
        let key = want["key"].as_str().unwrap();
        let got = state
            .store
            .get_usage("claude-code", key)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{key} stored"));
        assert_eq!(got.session, sid);
        assert_eq!(got.billing, Billing::Api);
        assert_eq!(got.model, want["model"].as_str().unwrap());
        assert_eq!(got.counts.input, want["input"].as_i64().unwrap());
        assert_eq!(got.counts.output, want["output"].as_i64().unwrap());
        assert_eq!(
            got.counts.cache_write_5m,
            want["cache_write_5m"].as_i64().unwrap()
        );
        assert_eq!(
            got.counts.cache_write_1h,
            want["cache_write_1h"].as_i64().unwrap()
        );
        assert_eq!(got.counts.cache_read, want["cache_read"].as_i64().unwrap());
        assert_eq!(got.counts.reasoning, want["reasoning"].as_i64().unwrap());
        let at = OffsetDateTime::parse(want["at"].as_str().unwrap(), &Rfc3339).unwrap();
        assert_eq!(got.occurred_at, at);
    }
    assert_eq!(
        cursor::load_status(h.path()).billing.get("claude-code"),
        Some(&Billing::Api)
    );
}
