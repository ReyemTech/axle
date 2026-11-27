//! `/v1` HTTP API, bearer auth, SSE events and health probes, against in-memory SQLite.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use coord::clock::TestClock;
use coord::config::ServerConfig;
use coord::domain::{Actor, Address, AgentId, Importance, ProjectId, SessionId};
use coord::http::{AppState, router};
use coord::service::messages::NewMessage;
use coord::store::sqlite::SqliteStore;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use time::macros::datetime;
use tower::ServiceExt;

struct App {
    state: AppState,
    router: Router,
    pool: SqlitePool,
}

async fn app() -> App {
    let opts: SqliteConnectOptions = "sqlite::memory:".parse().unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts.foreign_keys(true))
        .await
        .unwrap();
    let store = Arc::new(SqliteStore::from_pool(pool.clone()).await.unwrap());
    let clock = Arc::new(TestClock::new(datetime!(2026-11-27 9:00 UTC)));
    let config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "COORD_ADMIN_SECRET" => Some("test-secret".into()),
        _ => None,
    })
    .unwrap();
    let state = AppState::new(store, clock, config);
    App {
        router: router(state.clone()),
        state,
        pool,
    }
}

impl App {
    async fn token(&self) -> String {
        self.state.auth.create("test").await.unwrap().1
    }

    async fn call(&self, req: Request<Body>) -> axum::response::Response {
        self.router.clone().oneshot(req).await.unwrap()
    }

    async fn post(&self, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let res = self
            .call(
                Request::post(path)
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await;
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Registers a new session in `project` and returns `(session_id, agent_id)`.
    async fn register(&self, token: &str, project: &str) -> (SessionId, AgentId) {
        let session = SessionId::new();
        let (status, body) = self
            .post(
                token,
                "/v1/register",
                json!({
                    "session_id": session,
                    "host": "mac",
                    "harness": "claude-code",
                    "project_key": project,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["session_id"], json!(session));
        (
            session,
            serde_json::from_value(body["agent_id"].clone()).unwrap(),
        )
    }

    async fn project_id(&self, key: &str) -> ProjectId {
        self.state
            .store
            .get_project_by_key(key)
            .await
            .unwrap()
            .unwrap()
            .id
    }

    async fn send_from_human(&self, to: Address, body: &str) {
        self.state
            .messages
            .send(
                &Actor::Human,
                NewMessage {
                    to,
                    subject: Some("hello".into()),
                    body: body.into(),
                    importance: Importance::Normal,
                    reply_to: None,
                },
            )
            .await
            .unwrap();
    }

    /// Opens the SSE stream for `session` and returns its body as a stream of text chunks.
    async fn events(
        &self,
        token: &str,
        session: SessionId,
    ) -> impl futures_util::Stream<Item = String> + Unpin {
        let res = self
            .call(
                Request::get(format!("/v1/events?session={session}"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()[header::CONTENT_TYPE],
            "text/event-stream",
            "SSE content type"
        );
        res.into_body()
            .into_data_stream()
            .map(|chunk| String::from_utf8(chunk.unwrap().to_vec()).unwrap())
    }
}

/// The next `event:` name on the stream within `wait`, skipping keep-alive comments.
async fn next_event(
    stream: &mut (impl futures_util::Stream<Item = String> + Unpin),
    wait: Duration,
) -> Option<(String, Value)> {
    tokio::time::timeout(wait, async {
        while let Some(chunk) = stream.next().await {
            let mut name = None;
            let mut data = None;
            for line in chunk.lines() {
                if let Some(n) = line.strip_prefix("event: ") {
                    name = Some(n.to_string());
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data = Some(serde_json::from_str(d).unwrap());
                }
            }
            if let (Some(n), Some(d)) = (name, data) {
                return Some((n, d));
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

#[tokio::test]
async fn rejects_without_token() {
    let app = app().await;
    for req in [
        Request::post("/v1/register")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
        Request::post("/v1/heartbeat")
            .header(header::AUTHORIZATION, "Bearer coord_not-a-real-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
        Request::get("/v1/events?session=00000000-0000-0000-0000-000000000000")
            .header(header::AUTHORIZATION, "Basic Zm9vOmJhcg==")
            .body(Body::empty())
            .unwrap(),
    ] {
        let res = app.call(req).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body, json!({"error": "unauthorized"}));
    }
}

#[tokio::test]
async fn rejects_revoked_token() {
    let app = app().await;
    let (id, token) = app.state.auth.create("laptop").await.unwrap();
    app.register(&token, "github.com/reyemtech/iac").await;

    app.state.auth.revoke(id).await.unwrap();

    let (status, body) = app
        .post(
            &token,
            "/v1/register",
            json!({"session_id": SessionId::new(), "host": "mac", "harness": "codex"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({"error": "unauthorized"}));
}

#[tokio::test]
async fn register_returns_project_and_rejects_foreign_session() {
    let app = app().await;
    let token = app.token().await;
    let session = SessionId::new();
    let (status, body) = app
        .post(
            &token,
            "/v1/register",
            json!({
                "session_id": session,
                "host": "mac",
                "harness": "claude-code",
                "project_key": "github.com/reyemtech/iac",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["project"],
        json!({"key": "github.com/reyemtech/iac", "display_name": "reyemtech/iac"})
    );

    let (status, body) = app
        .post(
            &token,
            "/v1/register",
            json!({"session_id": SessionId::new(), "host": "mac", "harness": "codex"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["project"], Value::Null, "no project key → no project");

    // The same session id presented with another token is refused.
    let other = app.token().await;
    let (status, _) = app
        .post(
            &other,
            "/v1/register",
            json!({
                "session_id": session,
                "host": "mac",
                "harness": "claude-code",
                "project_key": "github.com/reyemtech/iac",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app
        .post(&other, "/v1/heartbeat", json!({"session_id": session}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn register_then_heartbeat_reports_unread() {
    let app = app().await;
    let token = app.token().await;
    let (session, agent) = app.register(&token, "github.com/reyemtech/iac").await;

    let (status, body) = app
        .post(&token, "/v1/heartbeat", json!({"session_id": session}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"unread": 0, "leases": []}));

    app.send_from_human(Address::Agent(agent), "please look at the flaky test")
        .await;

    let (status, body) = app
        .post(&token, "/v1/heartbeat", json!({"session_id": session}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unread"], 1);

    let (status, _) = app
        .post(
            &token,
            "/v1/heartbeat",
            json!({"session_id": SessionId::new()}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn heartbeat_lists_held_leases() {
    let app = app().await;
    let token = app.token().await;
    let (session, _) = app.register(&token, "github.com/reyemtech/iac").await;
    let project = app.project_id("github.com/reyemtech/iac").await;
    let task = app
        .state
        .tasks
        .create(
            coord::service::tasks::Caller::Human,
            coord::service::tasks::NewTask {
                project_id: project,
                title: "rotate the keys".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let resolved = coord::service::identity::IdentityService::load(&*app.state.store, session)
        .await
        .unwrap()
        .expect("registered session");
    app.state.tasks.claim(&resolved, task.id).await.unwrap();

    let (_, body) = app
        .post(&token, "/v1/heartbeat", json!({"session_id": session}))
        .await;
    assert_eq!(body["leases"], json!([task.id]));
}

#[tokio::test]
async fn events_stream_delivers_message() {
    let app = app().await;
    let token = app.token().await;
    let (session, agent) = app.register(&token, "github.com/reyemtech/iac").await;
    let mut stream = app.events(&token, session).await;

    app.send_from_human(Address::Agent(agent), "wake up").await;

    let (name, data) = next_event(&mut stream, Duration::from_secs(1))
        .await
        .expect("an event within 1 s");
    assert_eq!(name, "MessageCreated");
    assert_eq!(data["to"], json!({"kind": "agent", "id": agent}));
}

#[tokio::test]
async fn events_not_leaked_across_projects() {
    let app = app().await;
    let token = app.token().await;
    let (session_a, _) = app.register(&token, "github.com/reyemtech/a").await;
    app.register(&token, "github.com/reyemtech/b").await;
    let project_a = app.project_id("github.com/reyemtech/a").await;
    let project_b = app.project_id("github.com/reyemtech/b").await;
    let mut stream = app.events(&token, session_a).await;

    app.send_from_human(Address::Project(project_b), "for B only")
        .await;
    app.send_from_human(Address::Human, "to the human").await;
    app.send_from_human(Address::Project(project_a), "for A")
        .await;

    let (name, data) = next_event(&mut stream, Duration::from_secs(1))
        .await
        .expect("A's own project message arrives");
    assert_eq!(name, "MessageCreated");
    assert_eq!(
        data["to"],
        json!({"kind": "project", "id": project_a}),
        "the first event A sees is its own project's, not B's or the human's"
    );
}

#[tokio::test]
async fn events_report_task_changes_in_project() {
    let app = app().await;
    let token = app.token().await;
    let (session, _) = app.register(&token, "github.com/reyemtech/iac").await;
    let project = app.project_id("github.com/reyemtech/iac").await;
    let mut stream = app.events(&token, session).await;

    let task = app
        .state
        .tasks
        .create(
            coord::service::tasks::Caller::Human,
            coord::service::tasks::NewTask {
                project_id: project,
                title: "rotate the keys".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let (name, data) = next_event(&mut stream, Duration::from_secs(1))
        .await
        .expect("task event");
    assert_eq!(name, "TaskChanged");
    assert_eq!(
        data,
        json!({"id": task.id, "project_id": project, "status": "open", "kind": "created"})
    );
    let outbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox WHERE kind = 'TaskChanged'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(outbox, 1, "every published event is written to the outbox");
}

#[tokio::test]
async fn inbox_summary_lists_unacked_with_short_preview() {
    let app = app().await;
    let token = app.token().await;
    let (session, agent) = app.register(&token, "github.com/reyemtech/iac").await;
    app.send_from_human(Address::Agent(agent), &"x".repeat(500))
        .await;

    let res = app
        .call(
            Request::get(format!("/v1/inbox/summary?session={session}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["from"], "human");
    assert_eq!(items[0]["subject"], "hello");
    assert_eq!(items[0]["importance"], "normal");
    assert!(items[0]["preview"].as_str().unwrap().chars().count() <= 120);
}

#[tokio::test]
async fn token_plaintext_never_stored() {
    let app = app().await;
    let (id, plaintext) = app.state.auth.create("mac-claude").await.unwrap();
    assert!(plaintext.starts_with("coord_"), "{plaintext}");

    let hashes: Vec<String> = sqlx::query_scalar("SELECT hash FROM tokens")
        .fetch_all(&app.pool)
        .await
        .unwrap();
    assert_eq!(hashes.len(), 1);
    assert_ne!(hashes[0], plaintext);
    assert!(!hashes[0].contains(plaintext.trim_start_matches("coord_")));

    let listed = app.state.auth.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].label, "mac-claude");
}

#[tokio::test]
async fn health_endpoints_unauthenticated() {
    let app = app().await;
    for path in ["/healthz", "/readyz"] {
        let res = app
            .call(Request::get(path).body(Body::empty()).unwrap())
            .await;
        assert_eq!(res.status(), StatusCode::OK, "{path}");
    }

    // A broken store makes /readyz fail while /healthz stays up.
    app.pool.close().await;
    let res = app
        .call(Request::get("/readyz").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let res = app
        .call(Request::get("/healthz").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status(), StatusCode::OK);
}
