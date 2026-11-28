//! `axl login` and `axl logout` against a real in-process server and a temp HOME.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axle::bridge::short_hostname;
use axle::cli::{LoginArgs, LoginHooks, login, login_with, logout};
use axle::client::{Api, AxlDir, ClientConfig, load_config, save_config, write_token};
use axle::clock::SystemClock;
use axle::config::ServerConfig;
use axle::http::AppState;
use axle::store::sqlite::SqliteStore;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("axle-login-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    state: AppState,
    base: String,
}

async fn server() -> Server {
    let opts: SqliteConnectOptions = "sqlite::memory:".parse().unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts.foreign_keys(true))
        .await
        .unwrap();
    let store = Arc::new(SqliteStore::from_pool(pool).await.unwrap());
    let config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some("test-secret-0123456789".into()),
        _ => None,
    })
    .unwrap();
    let state = AppState::new(store, Arc::new(SystemClock), config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axle::http::serve(
        listener,
        state.clone(),
        std::future::pending(),
    ));
    Server { state, base }
}

/// Waits for the device login the client just started to show up as pending.
async fn wait_pending(srv: &Server) -> axle::domain::EnrollmentId {
    loop {
        if let Some(e) = srv.state.enroll.pending().await.unwrap().first() {
            return e.id;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn args(url: &str) -> LoginArgs {
    LoginArgs {
        url: Some(url.to_string()),
        code: None,
        no_browser: true,
        force: false,
    }
}

fn text(out: &[u8]) -> String {
    String::from_utf8(out.to_vec()).unwrap()
}

#[tokio::test]
async fn login_with_code_writes_token_and_config() {
    let srv = server().await;
    let home = TempHome::new();
    let issued = srv.state.enroll.issue_code("work laptop").await.unwrap();
    let mut out = Vec::new();
    let outcome = login(
        home.path(),
        LoginArgs {
            code: Some(issued.code.clone()),
            // A trailing slash: the saved URL is the normalised one the user typed.
            ..args(&format!("{}/", srv.base))
        },
        &mut out,
    )
    .await
    .unwrap();
    let host = short_hostname();
    assert_eq!(outcome.server_url, srv.base);
    assert_eq!(outcome.host, host);
    let out = text(&out);
    assert!(
        out.contains(&format!("Connected to {} as {host}", srv.base)),
        "{out}"
    );
    let dir = AxlDir::new(home.path());
    let token = std::fs::read_to_string(dir.token_path()).unwrap();
    assert!(token.starts_with("axl_"), "{token}");
    assert!(!out.contains(token.trim()), "token leaked to output");
    assert_eq!(
        load_config(&dir).unwrap().server_url.as_deref(),
        Some(srv.base.as_str())
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.token_path()), 0o600);
        assert_eq!(mode(dir.config_path()), 0o600);
        assert_eq!(mode(dir.root.clone()), 0o700);
    }
    // The saved token really works.
    let who = Api::new(&srv.base).whoami(token.trim()).await.unwrap();
    assert!(who.label.contains("work laptop"), "{}", who.label);
}

#[tokio::test]
async fn login_refuses_switching_servers_without_force() {
    let srv = server().await;
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    save_config(
        &dir,
        &ClientConfig {
            server_url: Some("http://other.example:7420".into()),
            port: None,
            ..ClientConfig::default()
        },
    )
    .unwrap();
    write_token(&dir, "axl_old").unwrap();
    let issued = srv.state.enroll.issue_code("x").await.unwrap();
    let mut out = Vec::new();
    let err = login(
        home.path(),
        LoginArgs {
            code: Some(issued.code.clone()),
            ..args(&srv.base)
        },
        &mut out,
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("--force"), "{msg}");
    assert!(msg.contains("http://other.example:7420"), "{msg}");
    assert_eq!(
        std::fs::read_to_string(dir.token_path()).unwrap(),
        "axl_old"
    );
    assert_eq!(
        load_config(&dir).unwrap().server_url.as_deref(),
        Some("http://other.example:7420")
    );
    // The refusal came before the code was spent: it still works with --force.
    login(
        home.path(),
        LoginArgs {
            code: Some(issued.code),
            force: true,
            ..args(&srv.base)
        },
        &mut out,
    )
    .await
    .unwrap();
    assert!(
        std::fs::read_to_string(dir.token_path())
            .unwrap()
            .starts_with("axl_")
    );
    assert_ne!(
        std::fs::read_to_string(dir.token_path()).unwrap(),
        "axl_old"
    );
}

#[tokio::test]
async fn device_login_completes_after_ui_approval() {
    let srv = server().await;
    let home = TempHome::new();
    let opened = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorder = opened.clone();
    let hooks = LoginHooks {
        open: Box::new(move |url| recorder.lock().unwrap().push(url.to_string())),
        interval: Some(Duration::from_millis(600)),
    };
    let mut out = Vec::new();
    let approve = async {
        let id = wait_pending(&srv).await;
        assert!(srv.state.enroll.decide(id, true).await.unwrap());
    };
    let (result, ()) = tokio::join!(
        login_with(
            home.path(),
            LoginArgs {
                no_browser: false,
                ..args(&srv.base)
            },
            &mut out,
            hooks,
        ),
        approve
    );
    result.unwrap();
    let out = text(&out);
    assert!(out.contains("/ui/connect"), "{out}");
    assert!(out.contains("Connected to"), "{out}");
    assert!(!out.contains("axld_"), "device code leaked: {out}");
    assert_eq!(
        opened.lock().unwrap().as_slice(),
        [format!("{}/ui/connect", srv.base)]
    );
    let dir = AxlDir::new(home.path());
    assert!(
        std::fs::read_to_string(dir.token_path())
            .unwrap()
            .starts_with("axl_")
    );
}

#[tokio::test]
async fn device_login_denied_writes_nothing() {
    let srv = server().await;
    let home = TempHome::new();
    let hooks = LoginHooks {
        open: Box::new(|_| {}),
        interval: Some(Duration::from_millis(600)),
    };
    let mut out = Vec::new();
    let deny = async {
        let id = wait_pending(&srv).await;
        assert!(srv.state.enroll.decide(id, false).await.unwrap());
    };
    let (result, ()) = tokio::join!(
        login_with(home.path(), args(&srv.base), &mut out, hooks),
        deny
    );
    let err = result.unwrap_err();
    assert!(err.to_string().contains("denied"), "{err}");
    assert!(!AxlDir::new(home.path()).token_path().exists());
}

#[tokio::test]
async fn bad_code_reports_server_message() {
    let srv = server().await;
    let home = TempHome::new();
    let mut out = Vec::new();
    let err = login(
        home.path(),
        LoginArgs {
            code: Some("AAAAA-AAAAA".into()),
            ..args(&srv.base)
        },
        &mut out,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("invalid or expired code"), "{err}");
    assert!(!AxlDir::new(home.path()).token_path().exists());
}

#[tokio::test]
async fn logout_removes_token_and_revoke_revokes() {
    let srv = server().await;
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    let issued = srv.state.enroll.issue_code("a").await.unwrap();
    let mut out = Vec::new();
    let enroll = |code: String| LoginArgs {
        code: Some(code),
        ..args(&srv.base)
    };
    login(home.path(), enroll(issued.code), &mut out)
        .await
        .unwrap();
    let old = std::fs::read_to_string(dir.token_path()).unwrap();

    // Plain logout: the file goes, the token stays valid on the server.
    let mut out = Vec::new();
    logout(home.path(), false, &mut out).await.unwrap();
    assert!(!dir.token_path().exists());
    assert!(text(&out).contains("Logged out"), "{}", text(&out));
    Api::new(&srv.base).whoami(old.trim()).await.unwrap();

    // With --revoke the server stops accepting it.
    let mut out = Vec::new();
    write_token(&dir, old.trim()).unwrap();
    logout(home.path(), true, &mut out).await.unwrap();
    assert!(!dir.token_path().exists());
    let err = Api::new(&srv.base).whoami(old.trim()).await.unwrap_err();
    assert!(err.to_string().contains("unauthorized"), "{err}");
    assert!(!text(&out).contains(old.trim()));

    // Logging out when already logged out is not an error.
    let mut out = Vec::new();
    logout(home.path(), true, &mut out).await.unwrap();
    assert!(text(&out).contains("Not logged in"), "{}", text(&out));
}

#[tokio::test]
async fn poll_rate_limit_waits_retry_after() {
    use axum::Json;
    use axum::http::{HeaderValue, StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let polls = Arc::new(AtomicUsize::new(0));
    let counter = polls.clone();
    let app = axum::Router::new()
        .route("/readyz", get(|| async { "ok" }))
        .route(
            "/v1/login/start",
            post(|| async {
                Json(json!({
                    "device_code": "axld_x", "user_code": "ABCD-EFGH",
                    "verify_url": "http://127.0.0.1/ui/connect",
                    "interval": 0, "expires_in": 600
                }))
            }),
        )
        .route(
            "/v1/login/poll",
            post(move || {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        let mut res = (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(json!({"error": "rate_limited"})),
                        )
                            .into_response();
                        res.headers_mut()
                            .insert(header::RETRY_AFTER, HeaderValue::from(1));
                        res
                    } else {
                        Json(json!({"token": "axl_fake", "server_url": "http://ignored"}))
                            .into_response()
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    let home = TempHome::new();
    let mut out = Vec::new();
    let started = Instant::now();
    let outcome = login_with(
        home.path(),
        args(&base),
        &mut out,
        LoginHooks {
            open: Box::new(|_| {}),
            interval: Some(Duration::from_millis(10)),
        },
    )
    .await
    .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1), "no wait");
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    // The URL the user typed is saved, not the server's reply.
    assert_eq!(outcome.server_url, base);
    assert_eq!(
        load_config(&AxlDir::new(home.path()))
            .unwrap()
            .server_url
            .as_deref(),
        Some(base.as_str())
    );
}

#[tokio::test]
async fn malformed_config_fails_before_redeeming_the_code() {
    let srv = server().await;
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    dir.ensure().unwrap();
    std::fs::write(dir.config_path(), "server_url = [not toml").unwrap();
    let issued = srv.state.enroll.issue_code("x").await.unwrap();
    let mut out = Vec::new();
    let with_code = |code: String| LoginArgs {
        code: Some(code),
        ..args(&srv.base)
    };
    let err = login(home.path(), with_code(issued.code.clone()), &mut out)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("config.toml"), "{err}");
    assert!(!dir.token_path().exists());
    // The code was not consumed: it works once the file is fixed.
    std::fs::remove_file(dir.config_path()).unwrap();
    login(home.path(), with_code(issued.code), &mut out)
        .await
        .unwrap();
    assert!(dir.token_path().exists());
}

#[tokio::test]
async fn redirecting_server_gets_a_clear_error() {
    use axum::http::{StatusCode, header};
    use axum::routing::get;
    let app = axum::Router::new().route(
        "/readyz",
        get(|| async {
            (
                StatusCode::PERMANENT_REDIRECT,
                [(header::LOCATION, "https://axle.example.com/readyz")],
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    let err = Api::new(&base).readyz().await.unwrap_err().to_string();
    assert!(
        err.contains(&format!(
            "{base} redirects to https://axle.example.com; use that URL instead"
        )),
        "{err}"
    );
    assert!(!err.contains("HTTP 308"), "{err}");
}

#[tokio::test]
async fn login_keeps_usage_config() {
    let srv = server().await;
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    let usage = axle::client::UsageConfig {
        enabled: Some(false),
        ..Default::default()
    };
    save_config(
        &dir,
        &ClientConfig {
            usage: Some(usage.clone()),
            ..ClientConfig::default()
        },
    )
    .unwrap();
    let issued = srv.state.enroll.issue_code("x").await.unwrap();
    let mut out = Vec::new();
    login(
        home.path(),
        LoginArgs {
            code: Some(issued.code.clone()),
            ..args(&srv.base)
        },
        &mut out,
    )
    .await
    .unwrap();
    let cfg = load_config(&dir).unwrap();
    assert_eq!(cfg.server_url.as_deref(), Some(srv.base.as_str()));
    assert_eq!(cfg.usage, Some(usage));
}
