//! `coord bridge`: project resolution, and the real binary driven over stdio by an rmcp
//! client, both without a server (fail-open) and against an in-process server.

#[macro_use]
mod common;

use std::future::IntoFuture;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use coord::bridge::project::{Env, normalize_origin, resolve_project};
use coord::clock::SystemClock;
use coord::config::ServerConfig;
use coord::http::{AppState, router};
use coord::mcp::CoordMcp;
use coord::store::Store;
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
        let path = std::env::temp_dir().join(format!("coord-bridge-{}", uuid::Uuid::now_v7()));
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
    let env = Env::from_pairs([("COORD_PROJECT", "env/project")]);

    // git origin, from a nested directory
    assert_eq!(
        resolve_project(&nested, &none).as_deref(),
        Some("github.com/reyemtech/iac")
    );
    // nearest .coordination beats git
    std::fs::write(
        repo.join(".coordination"),
        "project = \"reyemtech/iac-file\"\ndisplay_name = \"IaC\"\n",
    )
    .unwrap();
    assert_eq!(
        resolve_project(&nested, &none).as_deref(),
        Some("reyemtech/iac-file")
    );
    std::fs::write(nested.join(".coordination"), "project = \"nearest\"\n").unwrap();
    assert_eq!(resolve_project(&nested, &none).as_deref(), Some("nearest"));
    // env beats the file
    assert_eq!(
        resolve_project(&nested, &env).as_deref(),
        Some("env/project")
    );
    // a value outside the safe charset is no project, never an error
    let bad = Env::from_pairs([("COORD_PROJECT", "has spaces")]);
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

/// Spawns `coord bridge --harness test-harness` in `cwd` with only `env` (plus PATH) set,
/// and connects an MCP client to its stdio.
async fn spawn_bridge(cwd: &Path, env: &[(&str, &str)]) -> Bridge {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_coord"));
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
        Implementation::new("coord-bridge-test", "0"),
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
    home.join(".coord")
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
            ("COORD_SERVER_URL", "http://127.0.0.1:9"),
        ],
    )
    .await;

    let tools = bridge.client.list_all_tools().await.unwrap();
    let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
    let mut want: Vec<_> = CoordMcp::tool_definitions()
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    want.sort();
    assert_eq!(names, want);
    assert!(names.iter().any(|n| n == "whoami"));

    let (is_error, text) = bridge.call("whoami").await;
    assert!(is_error);
    assert!(text.starts_with("coord unavailable:"), "{text}");

    // The session file exists even without a server, so hooks still find the session.
    let file = session_file(&home.0);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["harness"], "test-harness");
    assert_eq!(saved["project_key"], Value::Null);
    assert_eq!(saved["server_url"], "http://127.0.0.1:9");
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(&home.0.join(".coord").join("sessions")), 0o700);

    let (ok, _stderr) = bridge.close().await;
    assert!(ok, "exit code 0 on stdin close");
    assert!(!file.exists(), "session file removed on exit");
}

store_matrix!(bridge_end_to_end, |store: Arc<dyn Store>| async move {
    let config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "COORD_ADMIN_SECRET" => Some("test-secret".into()),
        _ => None,
    })
    .unwrap();
    let state = AppState::new(store.clone(), Arc::new(SystemClock), config);
    let token = state.auth.create("bridge").await.unwrap().1;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, router(state)).into_future());

    // Token from ~/.coord/token, project from the nearest .coordination file.
    let home = TempDir::new();
    let coord_dir = home.0.join(".coord");
    std::fs::create_dir_all(&coord_dir).unwrap();
    let token_file = coord_dir.join("token");
    std::fs::write(&token_file, format!("{token}\n")).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let work = TempDir::new();
    std::fs::write(
        work.0.join(".coordination"),
        "project = \"github.com/reyemtech/bridge-e2e\"\n",
    )
    .unwrap();

    let bridge = spawn_bridge(
        &work.0,
        &[
            ("HOME", home.0.to_str().unwrap()),
            ("COORD_SERVER_URL", server.as_str()),
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

    let tools = bridge.client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), CoordMcp::tool_definitions().len());

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

    // The bridge registered its session with /v1/register at startup.
    let sid: coord::domain::SessionId = session_id.parse().unwrap();
    assert!(store.get_session(sid).await.unwrap().is_some());

    let (ok, stderr) = bridge.close().await;
    assert!(ok, "exit code 0 on stdin close");
    assert!(!file.exists(), "session file removed on exit");
    assert!(!stderr.contains(&token), "token must never be logged");
});
