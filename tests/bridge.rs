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

use axle::bridge::project::{
    Detected, Env, describe, detect, folder_hash, is_folder_hash, local_segment, normalize_origin,
    pin, resolve_project,
};
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
        resolve_project(&nested, &none, "mac").as_deref(),
        Some("github.com/acme/platform-infra")
    );
    // nearest .axl beats git
    std::fs::write(
        repo.join(".axl"),
        "project = \"acme/platform-infra-file\"\ndisplay_name = \"Platform-Infra\"\n",
    )
    .unwrap();
    assert_eq!(
        resolve_project(&nested, &none, "mac").as_deref(),
        Some("acme/platform-infra-file")
    );
    std::fs::write(nested.join(".axl"), "project = \"nearest\"\n").unwrap();
    assert_eq!(
        resolve_project(&nested, &none, "mac").as_deref(),
        Some("nearest")
    );
    // env beats the file
    assert_eq!(
        resolve_project(&nested, &env, "mac").as_deref(),
        Some("env/project")
    );
    // a value outside the safe charset is no project, never an error
    let bad = Env::from_pairs([("AXL_PROJECT", "has spaces")]);
    assert_eq!(resolve_project(&nested, &bad, "mac"), None);

    // no git, no file: a local project for the folder
    let bare = root.0.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(
        resolve_project(&bare, &none, "mac").as_deref(),
        Some("local/mac/bare")
    );
}

#[test]
fn local_projects_for_folders_without_a_remote() {
    let root = TempDir::new();
    let none = Env::default();

    // No git at all: the working directory's name, and its hash.
    let plain = root.0.join("Axl SaaS");
    let deep = plain.join("notes");
    std::fs::create_dir_all(&deep).unwrap();
    let d = detect(&plain, &none, "Mac-Mini");
    assert_eq!(d.key.as_deref(), Some("local/mac-mini/axl-saas"));
    assert_eq!(d.folder, Some(folder_hash(&plain)));
    assert!(is_folder_hash(d.folder.as_deref().unwrap()));
    // Outside a repository, each directory is its own folder.
    assert_eq!(
        detect(&deep, &none, "mac").key.as_deref(),
        Some("local/mac/notes")
    );

    // A git repository without a remote: the top level's name, from any subdirectory.
    let repo = root.0.join("brainstorm");
    let sub = repo.join("docs").join("ideas");
    std::fs::create_dir_all(&sub).unwrap();
    git(&repo, &["init", "-q"]);
    let d = detect(&sub, &none, "mac");
    assert_eq!(d.key.as_deref(), Some("local/mac/brainstorm"));
    assert_eq!(d.folder, Some(folder_hash(&repo)));

    // The same repository with a remote: the git key, and still the folder's hash (which is
    // how the server finds the local project to offer for adoption).
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/axle-saas.git",
        ],
    );
    let d = detect(&sub, &none, "mac");
    assert_eq!(d.key.as_deref(), Some("github.com/acme/axle-saas"));
    assert_eq!(d.folder, Some(folder_hash(&repo)));

    // A pin wins over everything and sends no folder.
    std::fs::write(repo.join(".axl"), "project = \"pinned/key\"\n").unwrap();
    assert_eq!(
        detect(&sub, &none, "mac"),
        Detected {
            key: Some("pinned/key".into()),
            folder: None
        }
    );
    let env = Env::from_pairs([("AXL_PROJECT", "env/key")]);
    assert_eq!(detect(&plain, &env, "mac").folder, None);

    // Two same-named folders on one host get the same key from the bridge, but different
    // hashes; the server gives the second one a suffixed key.
    let a = root.0.join("a").join("web");
    let b = root.0.join("b").join("web");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let (da, db) = (detect(&a, &none, "mac"), detect(&b, &none, "mac"));
    assert_eq!(da.key, db.key);
    assert_ne!(da.folder, db.folder);
}

#[test]
fn folder_names_normalise_like_keys() {
    for (raw, want) in [
        ("Axl SaaS", "axl-saas"),
        ("my_project.v2", "my_project.v2"),
        ("--weird!!name--", "weird-name"),
        ("Ünïcødé", "n-c-d"),
        ("...", "folder"),
        ("", "folder"),
        ("a   b", "a-b"),
    ] {
        assert_eq!(local_segment(raw, "folder"), want, "{raw:?}");
    }
    let long = "x".repeat(100);
    assert_eq!(local_segment(&long, "folder").len(), 64);
}

#[test]
fn pin_writes_and_updates_the_axl_file() {
    let root = TempDir::new();
    let repo = root.0.join("repo");
    let sub = repo.join("src");
    std::fs::create_dir_all(&sub).unwrap();
    git(&repo, &["init", "-q"]);
    let none = Env::default();
    assert!(describe(&sub, &none, "mac").contains("local/mac/repo"));

    // Written at the git top level, from a subdirectory.
    let file = pin(&sub, "github.com/acme/web").unwrap();
    assert_eq!(file, repo.join(".axl"));
    assert_eq!(
        resolve_project(&sub, &none, "mac").as_deref(),
        Some("github.com/acme/web")
    );
    assert!(describe(&sub, &none, "mac").contains("pinned by"));

    // Updating keeps the rest of the file.
    std::fs::write(
        &file,
        "# keep me\nproject = \"old\"\ndisplay_name = \"Web\"\n",
    )
    .unwrap();
    pin(&sub, "github.com/acme/web2").unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("# keep me") && text.contains("display_name = \"Web\""));
    assert_eq!(
        resolve_project(&sub, &none, "mac").as_deref(),
        Some("github.com/acme/web2")
    );

    // Outside a repository: the working directory.
    let plain = root.0.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert_eq!(pin(&plain, "x/y").unwrap(), plain.join(".axl"));
    assert!(pin(&plain, "has spaces").is_err());
    assert!(pin(&plain, "  ").is_err());
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
    // The home dir is no repository: a local project for that folder.
    let key = saved["project_key"].as_str().unwrap();
    assert!(key.starts_with("local/"), "{key}");
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
