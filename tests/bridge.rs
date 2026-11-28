//! `axl bridge`: project resolution, and the real binary driven over stdio by an rmcp
//! client, both without a server (fail-open) and against an in-process server.

#[macro_use]
mod common;

use std::future::IntoFuture;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axle::bridge::project::{Env, normalize_origin, resolve_project};
use axle::clock::SystemClock;
use axle::config::ServerConfig;
use axle::http::{AppState, router};
use axle::mcp::AxleMcp;
use axle::store::Store;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

/// A temp directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("axle-bridge-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).unwrap();
        // Canonical, so paths compare equal to what child processes see (/var vs /private/var).
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn normalize_variants() {
    for (url, want) in [
        (
            "git@github.com:Acme/Platform-Infra.git",
            Some("github.com/acme/platform-infra"),
        ),
        (
            "https://github.com/acme/platform-infra",
            Some("github.com/acme/platform-infra"),
        ),
        (
            "ssh://git@git.example.com:2222/team/sub/repo.git",
            Some("git.example.com/team/sub/repo"),
        ),
        ("https://gitlab.com/a/b/c", Some("gitlab.com/a/b/c")),
        ("not a url", None),
        // Extra shapes: credentials, trailing slash, no path, local paths.
        (
            "https://user:pw@GitHub.com:443/Acme/Platform-Infra.git/",
            Some("github.com/acme/platform-infra"),
        ),
        ("https://github.com/A/B.GIT", Some("github.com/a/b")),
        (
            "git@GitHub.com:Acme/Platform-Infra.Git",
            Some("github.com/acme/platform-infra"),
        ),
        ("https://github.com", None),
        ("file:///srv/repo.git", None),
        ("/srv/repo.git", None),
        ("", None),
    ] {
        assert_eq!(normalize_origin(url).as_deref(), want, "{url:?}");
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn resolution_order() {
    let root = TempDir::new();
    let repo = root.0.join("repo");
    let nested = repo.join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:Acme/Platform-Infra.git",
        ],
    );

    let none = Env::default();
    let env = Env::from_pairs([("AXL_PROJECT", "env/project")]);

    // git origin, from a nested directory
    assert_eq!(
        resolve_project(&nested, &none).as_deref(),
        Some("github.com/acme/platform-infra")
    );
    // nearest .axl beats git
    std::fs::write(
        repo.join(".axl"),
        "project = \"acme/platform-infra-file\"\ndisplay_name = \"Platform-Infra\"\n",
    )
    .unwrap();
    assert_eq!(
        resolve_project(&nested, &none).as_deref(),
        Some("acme/platform-infra-file")
    );
    std::fs::write(nested.join(".axl"), "project = \"nearest\"\n").unwrap();
    assert_eq!(resolve_project(&nested, &none).as_deref(), Some("nearest"));
    // env beats the file
    assert_eq!(
        resolve_project(&nested, &env).as_deref(),
        Some("env/project")
    );
    // a value outside the safe charset is no project, never an error
    let bad = Env::from_pairs([("AXL_PROJECT", "has spaces")]);
    assert_eq!(resolve_project(&nested, &bad), None);

    // no git, no file
    let bare = root.0.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(resolve_project(&bare, &none), None);
}

// ---------------------------------------------------------------- the binary

type Client = RunningService<RoleClient, ClientConfig>;

struct Bridge {
    child: Child,
    client: Client,
}

/// Spawns `axl bridge --harness test-harness` in `cwd` with only `env` (plus PATH) set,
/// and connects an MCP client to its stdio.
async fn spawn_bridge(cwd: &Path, env: &[(&str, &str)]) -> Bridge {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_axl"));
    cmd.args(["bridge", "--harness", "test-harness"])
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take().unwrap();
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("axle-bridge-test", "0"),
    )
    .serve((stdout, stdin))
    .await
    .expect("initialize the bridge over stdio");
    Bridge { child, client }
}

impl Bridge {
    async fn call(&self, name: &'static str) -> (bool, String) {
        let r = self
            .client
            .call_tool(CallToolRequestParams::new(name))
            .await
            .unwrap();
        let text = r
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("");
        (r.is_error == Some(true), text)
    }

    /// Closes stdin, waits for exit; returns the exit status success and stderr.
    async fn close(mut self) -> (bool, String) {
        self.client.cancel().await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .expect("bridge exits after stdin closes")
            .unwrap();
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .await
            .unwrap();
        (status.success(), stderr)
    }
}

fn session_file(home: &Path) -> PathBuf {
    home.join(".axl")
        .join("sessions")
        .join(format!("{}.json", std::process::id()))
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn bridge_without_token_fails_open() {
    let home = TempDir::new();
    let work = TempDir::new();
    let bridge = spawn_bridge(
        &work.0,
        &[
            ("HOME", home.0.to_str().unwrap()),
            ("AXL_SERVER_URL", "http://127.0.0.1:9"),
        ],
    )
    .await;

    let tools = bridge.client.list_all_tools().await.unwrap();
    let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
    let mut want: Vec<_> = AxleMcp::tool_definitions()
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    want.sort();
    assert_eq!(names, want);
    assert!(names.iter().any(|n| n == "whoami"));

    let (is_error, text) = bridge.call("whoami").await;
    assert!(is_error);
    assert!(text.starts_with("axle unavailable:"), "{text}");

    // The session file exists even without a server, so hooks still find the session.
    let file = session_file(&home.0);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["harness"], "test-harness");
    assert_eq!(saved["project_key"], Value::Null);
    assert_eq!(saved["server_url"], "http://127.0.0.1:9");
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(&home.0.join(".axl").join("sessions")), 0o700);

    let (ok, _stderr) = bridge.close().await;
    assert!(ok, "exit code 0 on stdin close");
    assert!(!file.exists(), "session file removed on exit");
}

store_matrix!(bridge_end_to_end, |store: Arc<dyn Store>| async move {
    let config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some("test-secret-0123456789".into()),
        _ => None,
    })
    .unwrap();
    let state = AppState::new(store.clone(), Arc::new(SystemClock), config);
    let token = state.auth.create("bridge").await.unwrap().1;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, router(state)).into_future());

    // Token from ~/.axl/token, project from the nearest .axl file.
    let home = TempDir::new();
    let axl_dir = home.0.join(".axl");
    std::fs::create_dir_all(&axl_dir).unwrap();
    let token_file = axl_dir.join("token");
    std::fs::write(&token_file, format!("{token}\n")).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let work = TempDir::new();
    std::fs::write(
        work.0.join(".axl"),
        "project = \"github.com/reyemtech/bridge-e2e\"\n",
    )
    .unwrap();

    let bridge = spawn_bridge(
        &work.0,
        &[
            ("HOME", home.0.to_str().unwrap()),
            ("AXL_SERVER_URL", server.as_str()),
        ],
    )
    .await;

    let file = session_file(&home.0);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["project_key"], "github.com/reyemtech/bridge-e2e");
    assert_eq!(saved["server_url"], server.as_str());
    let session_id = saved["session_id"].as_str().unwrap().to_string();
    assert_eq!(
        uuid::Uuid::parse_str(&session_id)
            .unwrap()
            .get_version_num(),
        7
    );

    // Registered through /v1/register before any MCP call (/mcp would also create it).
    let sid: axle::domain::SessionId = session_id.parse().unwrap();
    let mut registered = false;
    for _ in 0..100 {
        if store.get_session(sid).await.unwrap().is_some() {
            registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(registered, "bridge registers its session at startup");

    let tools = bridge.client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), AxleMcp::tool_definitions().len());

    let (is_error, text) = bridge.call("whoami").await;
    assert!(!is_error, "{text}");
    let me: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(me["project"]["key"], "github.com/reyemtech/bridge-e2e");
    assert_eq!(me["harness"], "test-harness");
    assert_eq!(me["session"], session_id.as_str());
    let host = me["host"].as_str().unwrap();
    assert!(
        !host.is_empty() && !host.contains('.'),
        "short host: {host}"
    );

    // Bad calls are the server's answer, not an outage, and keep the connection: bad
    // arguments, and an unknown tool (a JSON-RPC error from the server).
    for name in ["send_message", "no_such_tool"] {
        let (is_error, text) = bridge.call(name).await;
        assert!(is_error, "{name}: {text}");
        assert!(!text.starts_with("axle unavailable"), "{name}: {text}");
        assert!(!text.is_empty(), "{name}");
    }
    let (is_error, text) = bridge.call("whoami").await;
    assert!(!is_error, "{text}");

    let (ok, stderr) = bridge.close().await;
    assert!(ok, "exit code 0 on stdin close");
    assert!(!file.exists(), "session file removed on exit");
    assert!(!stderr.contains(&token), "token must never be logged");
    assert!(!stderr.contains("axle unavailable"), "{stderr}");
});

/// What a fake server saw on `/v1`: (endpoint, authorization header, body).
type Seen = Arc<Mutex<Vec<(&'static str, String, Value)>>>;

/// A fake `/v1` that records requests and answers register with 200. The first heartbeat
/// gets 404 (a server that forgot the session), the second a 500 whose body is `x` then
/// two-byte characters (so byte 200 is mid-character), later ones 200.
async fn fake_v1(seen: Seen) -> String {
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;

    fn record(seen: &Seen, what: &'static str, headers: &HeaderMap, body: Value) -> usize {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let mut seen = seen.lock().unwrap();
        seen.push((what, auth, body));
        seen.iter().filter(|(w, _, _)| *w == what).count()
    }
    let reg = seen.clone();
    let beat = seen.clone();
    let app = axum::Router::new()
        .route(
            "/v1/register",
            post(
                move |headers: HeaderMap, body: axum::Json<Value>| async move {
                    record(&reg, "register", &headers, body.0);
                    StatusCode::OK
                },
            ),
        )
        .route(
            "/v1/heartbeat",
            post(
                move |headers: HeaderMap, body: axum::Json<Value>| async move {
                    match record(&beat, "heartbeat", &headers, body.0) {
                        1 => (StatusCode::NOT_FOUND, String::new()),
                        2 => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("x{}", "é".repeat(150)),
                        ),
                        _ => (StatusCode::OK, String::new()),
                    }
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app).into_future());
    url
}

#[tokio::test]
async fn bridge_heartbeats_and_reregisters_when_forgotten() {
    let seen: Seen = Arc::default();
    let server = fake_v1(seen.clone()).await;
    let home = TempDir::new();
    let work = TempDir::new();
    let bridge = spawn_bridge(
        &work.0,
        &[
            ("HOME", home.0.to_str().unwrap()),
            ("AXL_SERVER_URL", server.as_str()),
            ("AXL_TOKEN", "tok-123"),
            ("AXL_PROJECT", "github.com/reyemtech/beat"),
            ("AXL_HEARTBEAT_MS", "50"),
        ],
    )
    .await;

    // Expect: register, heartbeat (404), register again, heartbeat (500), and heartbeats
    // that keep coming after the 500.
    let mut log = vec![];
    for _ in 0..200 {
        log = seen.lock().unwrap().clone();
        if log.iter().filter(|(w, _, _)| *w == "heartbeat").count() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let order: Vec<_> = log.iter().map(|(w, _, _)| *w).take(5).collect();
    assert_eq!(
        order,
        [
            "register",
            "heartbeat",
            "register",
            "heartbeat",
            "heartbeat"
        ]
    );
    assert!(log.iter().filter(|(w, _, _)| *w == "heartbeat").count() >= 4);

    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(session_file(&home.0)).unwrap()).unwrap();
    for (what, auth, body) in &log {
        assert_eq!(auth, "Bearer tok-123", "{what}");
        assert_eq!(body["session_id"], saved["session_id"], "{what}");
        if *what == "register" {
            assert_eq!(body["harness"], "test-harness");
            assert_eq!(body["project_key"], "github.com/reyemtech/beat");
            assert!(body["host"].as_str().is_some_and(|h| !h.is_empty()));
        }
    }

    let (ok, stderr) = bridge.close().await;
    assert!(ok, "exit code 0 on stdin close");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(stderr.contains("heartbeat rejected (500"), "{stderr}");
    assert!(!stderr.contains("tok-123"), "token must never be logged");
}

#[tokio::test]
async fn bridge_heartbeat_carries_the_hooks_turn_state() {
    let seen: Seen = Arc::default();
    let server = fake_v1(seen.clone()).await;
    let home = TempDir::new();
    let work = TempDir::new();
    let bridge = spawn_bridge(
        &work.0,
        &[
            ("HOME", home.0.to_str().unwrap()),
            ("AXL_SERVER_URL", server.as_str()),
            ("AXL_TOKEN", "tok-123"),
            ("AXL_HEARTBEAT_MS", "50"),
        ],
    )
    .await;
    let beats = |seen: &Seen| -> Vec<Value> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(w, _, _)| *w == "heartbeat")
            .map(|(_, _, b)| b.clone())
            .collect()
    };
    for _ in 0..200 {
        if !beats(&seen).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // No turn file yet: the old shape.
    assert!(
        beats(&seen)[0].get("turn_state").is_none(),
        "{:?}",
        beats(&seen)
    );

    // A hook records idle; a later heartbeat carries it.
    let turn = axle::bridge::session_file::turn_path(&home.0, std::process::id());
    axle::bridge::session_file::write_turn(
        &turn,
        axle::domain::TurnState::Idle,
        time::OffsetDateTime::now_utc(),
    )
    .unwrap();
    let mut carried = false;
    for _ in 0..200 {
        if beats(&seen).iter().any(|b| b["turn_state"] == "idle") {
            carried = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(carried, "{:?}", beats(&seen));

    let (ok, _) = bridge.close().await;
    assert!(ok);
    assert!(!turn.exists(), "the turn file goes with the session file");
}

// ---------------------------------------------------------------- wake listener

use axle::bridge::wake::{PokeOutcome, WakeAdapter, wake_loop};
use axle::domain::{TurnState, WakeChannel, WakeOutcome, WakeTrigger};
use axle::events::Event;

/// Records every poke and answers with `outcome`.
struct Recording {
    pokes: Mutex<Vec<String>>,
    outcome: PokeOutcome,
}

#[async_trait::async_trait]
impl WakeAdapter for Recording {
    fn channel(&self) -> WakeChannel {
        WakeChannel::ClaudeSocket
    }
    async fn check(&self) -> bool {
        true
    }
    async fn turn_state(&self) -> Option<TurnState> {
        None
    }
    async fn poke(&self, text: &str) -> PokeOutcome {
        self.pokes.lock().unwrap().push(text.to_string());
        self.outcome.clone()
    }
}

fn wake_config(server: &str, token: &str) -> axle::bridge::Config {
    let env = Env::from_pairs([
        ("AXL_SERVER_URL", server),
        ("AXL_TOKEN", token),
        ("AXL_HEARTBEAT_MS", "50"),
        ("AXL_PROJECT", "github.com/reyemtech/wake"),
    ]);
    axle::bridge::Config::resolve("test-harness", Path::new("/"), &env)
}

async fn register(server: &str, token: &str, cfg: &axle::bridge::Config) {
    let res = reqwest::Client::new()
        .post(format!("{server}/v1/register"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "session_id": cfg.session_id, "host": cfg.host, "harness": cfg.harness,
            "project_key": cfg.project_key,
        }))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "{}", res.status());
}

fn serve(state: AppState, listener: tokio::net::TcpListener) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).into_future().await;
    })
}

fn test_state(store: Arc<dyn Store>) -> AppState {
    let config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some("test-secret-0123456789".into()),
        _ => None,
    })
    .unwrap();
    AppState::new(store, Arc::new(SystemClock), config)
}

async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

store_matrix!(
    bridge_forwards_wake_to_adapter_and_reports_result,
    |store: Arc<dyn Store>| async move {
        let state = test_state(store);
        let token = state.auth.create("wake").await.unwrap().1;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let _srv = serve(state.clone(), listener);

        let cfg = wake_config(&server, &token);
        register(&server, &token, &cfg).await;
        let session = cfg.session_id;
        let adapter = Arc::new(Recording {
            pokes: Mutex::default(),
            outcome: PokeOutcome::Sent,
        });
        let bridge = axle::bridge::Bridge::new(cfg);
        let looped = tokio::spawn(wake_loop(bridge, adapter.clone()));

        // The results the server publishes (the UI's view of an attempt).
        let mut results = state.bus.subscribe();
        // Let the bridge's stream subscribe, then wake it (and some other session).
        tokio::time::sleep(Duration::from_millis(500)).await;
        let wake = |s, text: &str| Event::WakeRequested {
            session: s,
            task: None,
            trigger: WakeTrigger::HumanPost,
            text: text.into(),
        };
        state
            .bus
            .publish(wake(axle::domain::SessionId::new(), "not for us"))
            .await
            .unwrap();
        state
            .bus
            .publish(wake(session, "[axl from human] hello"))
            .await
            .unwrap();

        wait_for("the poke", || !adapter.pokes.lock().unwrap().is_empty()).await;
        assert_eq!(
            *adapter.pokes.lock().unwrap(),
            ["[axl from human] hello".to_string()]
        );
        let got = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::WakeResult {
                    session: s,
                    outcome,
                    ..
                } = results.recv().await.unwrap()
                {
                    return (s, outcome);
                }
            }
        })
        .await
        .expect("the bridge reports the outcome");
        assert_eq!(got, (session, WakeOutcome::Sent));
        looped.abort();
    }
);

/// A fake `/v1/events` whose streams end by themselves (a server restart, as the bridge
/// sees it): the first connection ends at once, the second delivers a wake for the
/// session and ends, and `/v1/wake/result` reports are recorded.
async fn fake_restarting_events(connections: Arc<Mutex<u32>>, reports: Seen) -> String {
    use axum::extract::Query;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};

    let app = axum::Router::new()
        .route(
            "/v1/events",
            get(
                move |Query(q): Query<std::collections::HashMap<String, String>>| async move {
                    let n = {
                        let mut c = connections.lock().unwrap();
                        *c += 1;
                        *c
                    };
                    let body = if n == 1 {
                        ": hello\n\n".to_string()
                    } else {
                        let data = serde_json::json!({
                            "session": q["session"], "task": null,
                            "trigger": "queue", "text": "after restart",
                        });
                        format!("event: WakeRequested\ndata: {data}\n\n")
                    };
                    ([("content-type", "text/event-stream")], body)
                },
            ),
        )
        .route(
            "/v1/wake/result",
            post(
                move |headers: HeaderMap, body: axum::Json<Value>| async move {
                    let auth = headers["authorization"].to_str().unwrap().to_string();
                    reports.lock().unwrap().push(("result", auth, body.0));
                    StatusCode::NO_CONTENT
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app).into_future());
    url
}

#[tokio::test]
async fn bridge_survives_server_restart() {
    let connections = Arc::new(Mutex::new(0));
    let reports: Seen = Arc::default();
    let server = fake_restarting_events(connections.clone(), reports.clone()).await;
    let cfg = wake_config(&server, "tok-1");
    let session = cfg.session_id;
    let adapter = Arc::new(Recording {
        pokes: Mutex::default(),
        outcome: PokeOutcome::Failed("boom".into()),
    });
    let looped = tokio::spawn(wake_loop(axle::bridge::Bridge::new(cfg), adapter.clone()));

    wait_for("the poke after the stream dropped and reconnected", || {
        !adapter.pokes.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(
        *adapter.pokes.lock().unwrap(),
        ["after restart".to_string()]
    );
    assert!(*connections.lock().unwrap() >= 2);
    wait_for("the report", || !reports.lock().unwrap().is_empty()).await;
    let (_, auth, body) = reports.lock().unwrap()[0].clone();
    assert_eq!(auth, "Bearer tok-1");
    assert_eq!(body["session_id"], session.to_string());
    assert_eq!(body["outcome"], "failed");
    assert_eq!(body["detail"], "boom");
    looped.abort();
}

#[tokio::test]
async fn wake_result_rejects_other_token() {
    let (store, _file) = common::sqlite_store().await;
    let state = test_state(store);
    let owner = state.auth.create("owner").await.unwrap().1;
    let other = state.auth.create("other").await.unwrap().1;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let _srv = serve(state, listener);
    let cfg = wake_config(&server, &owner);
    register(&server, &owner, &cfg).await;

    let post = |token: String, session: String| {
        let url = format!("{server}/v1/wake/result");
        async move {
            reqwest::Client::new()
                .post(url)
                .bearer_auth(token)
                .json(&serde_json::json!({
                    "session_id": session, "task_id": null, "outcome": "sent", "detail": null,
                }))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(post(other, cfg.session_id.to_string()).await, 403);
    assert_eq!(post(owner.clone(), cfg.session_id.to_string()).await, 204);
    let unknown = axle::domain::SessionId::new().to_string();
    assert_eq!(post(owner, unknown).await, 404);
}
