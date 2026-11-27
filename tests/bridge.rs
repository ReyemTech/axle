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
            "git@github.com:ReyemTech/IaC.git",
            Some("github.com/reyemtech/iac"),
        ),
        (
            "https://github.com/reyemtech/iac",
            Some("github.com/reyemtech/iac"),
        ),
        (
            "ssh://git@git.example.com:2222/team/sub/repo.git",
            Some("git.example.com/team/sub/repo"),
        ),
        ("https://gitlab.com/a/b/c", Some("gitlab.com/a/b/c")),
        ("not a url", None),
        // Extra shapes: credentials, trailing slash, no path, local paths.
        (
            "https://user:pw@GitHub.com:443/ReyemTech/IaC.git/",
            Some("github.com/reyemtech/iac"),
        ),
        ("https://github.com/A/B.GIT", Some("github.com/a/b")),
        (
            "git@GitHub.com:ReyemTech/IaC.Git",
            Some("github.com/reyemtech/iac"),
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
            "git@github.com:ReyemTech/IaC.git",
        ],
    );

    let none = Env::default();
    let env = Env::from_pairs([("AXL_PROJECT", "env/project")]);

    // git origin, from a nested directory
    assert_eq!(
        resolve_project(&nested, &none).as_deref(),
        Some("github.com/reyemtech/iac")
    );
    // nearest .axl beats git
    std::fs::write(
        repo.join(".axl"),
        "project = \"reyemtech/iac-file\"\ndisplay_name = \"IaC\"\n",
    )
    .unwrap();
    assert_eq!(
        resolve_project(&nested, &none).as_deref(),
        Some("reyemtech/iac-file")
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
