//! `/v1` HTTP API, bearer auth, SSE events and health probes, against in-memory SQLite.

use std::sync::Arc;
use std::time::Duration;

use axle::clock::TestClock;
use axle::config::ServerConfig;
use axle::domain::{Actor, Address, AgentId, Importance, ProjectId, SessionId};
use axle::http::{AppState, router};
use axle::service::messages::NewMessage;
use axle::store::sqlite::SqliteStore;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use time::macros::datetime;
use tower::ServiceExt;

struct App {
    state: AppState,
    clock: Arc<TestClock>,
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
        "AXL_ADMIN_SECRET" => Some("test-secret-0123456789".into()),
        _ => None,
    })
    .unwrap();
    let state = AppState::new(store, clock.clone(), config);
    App {
        router: router(state.clone()),
        state,
        clock,
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
async fn revoke_self_revokes_caller_only() {
    let app = app().await;
    let mine = app.token().await;
    let other = app.token().await;
    let (status, _) = app.post(&mine, "/v1/token/revoke", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let whoami = |token: String| {
        let app = &app;
        async move {
            app.call(
                Request::get("/v1/whoami")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .status()
        }
    };
    assert_eq!(whoami(mine.clone()).await, StatusCode::UNAUTHORIZED);
    assert_eq!(whoami(other).await, StatusCode::OK);
    // Revoking again is refused like any request with a dead token.
    let (status, _) = app.post(&mine, "/v1/token/revoke", json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
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
            .header(header::AUTHORIZATION, "Bearer axl_not-a-real-token")
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
    app.register(&token, "github.com/acme/platform-infra").await;

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
                "project_key": "github.com/acme/platform-infra",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["project"],
        json!({"key": "github.com/acme/platform-infra", "display_name": "acme/platform-infra"})
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
                "project_key": "github.com/acme/platform-infra",
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
async fn event_stream_follows_a_merge_to_the_surviving_ids() {
    let app = app().await;
    let token = app.token().await;
    let reg = |key: &'static str, folder: Option<&'static str>| {
        let token = token.clone();
        let app = &app;
        async move {
            let session = SessionId::new();
            let (status, body) = app
                .post(
                    &token,
                    "/v1/register",
                    json!({
                        "session_id": session, "host": "mac", "harness": "claude-code",
                        "project_key": key, "folder": folder,
                    }),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let agent: AgentId = serde_json::from_value(body["agent_id"].clone()).unwrap();
            (session, agent)
        }
    };
    const FOLDER: &str = "0123456789abcdef";
    // The git project exists with a mac/claude-code agent (pinned, no folder).
    let (_, twin) = reg("github.com/acme/axle-saas", None).await;
    let target = app.project_id("github.com/acme/axle-saas").await;
    // A local session of the same host and harness, with its stream open.
    let (session, _) = reg("local/mac/axl-saas", Some(FOLDER)).await;
    let local = app.project_id("local/mac/axl-saas").await;
    reg("github.com/acme/axle-saas", Some(FOLDER)).await;
    let mut stream = app.events(&token, session).await;

    // The owner merges it: the stream says so, then follows the surviving ids.
    let out = app.state.adopt_project(local).await.unwrap();
    assert!(matches!(out, axle::domain::Adoption::Merged(_)), "{out:?}");
    let (name, data) = next_event(&mut stream, Duration::from_secs(2))
        .await
        .expect("a merge event");
    assert_eq!(name, "ProjectMerged");
    assert_eq!(data["into"], json!(target));

    app.send_from_human(Address::Project(target), "to the git project")
        .await;
    let (name, data) = next_event(&mut stream, Duration::from_secs(2))
        .await
        .expect("project broadcast after the merge");
    assert_eq!(name, "MessageCreated");
    assert_eq!(data["to"]["id"], json!(target), "{data}");
    app.send_from_human(Address::Agent(twin), "to the survivor")
        .await;
    let (name, _) = next_event(&mut stream, Duration::from_secs(2))
        .await
        .expect("direct mail to the surviving agent");
    assert_eq!(name, "MessageCreated");
}

#[tokio::test]
async fn session_project_reports_a_remote_waiting_for_adoption() {
    let app = app().await;
    let token = app.token().await;
    let register = |key: &'static str| {
        let token = token.clone();
        let app = &app;
        async move {
            let session = SessionId::new();
            let (status, body) = app
                .post(
                    &token,
                    "/v1/register",
                    json!({
                        "session_id": session,
                        "host": "mac",
                        "harness": "claude-code",
                        "project_key": key,
                        "folder": "0123456789abcdef",
                    }),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            (session, body)
        }
    };
    let get = |session: SessionId| {
        let token = token.clone();
        let app = &app;
        async move {
            let res = app
                .call(
                    Request::get(format!("/v1/project?session={session}"))
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await;
            assert_eq!(res.status(), StatusCode::OK);
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        }
    };
    let (first, body) = register("local/mac/axl-saas").await;
    assert_eq!(body["project"]["key"], "local/mac/axl-saas");
    let p = get(first).await;
    assert_eq!(p["local_host"], "mac");
    assert_eq!(p["adopt_key"], Value::Null);

    // Same folder, now with a remote: still the local project, with the git key noted.
    let (second, body) = register("github.com/acme/axle-saas").await;
    assert_eq!(body["project"]["key"], "local/mac/axl-saas");
    let p = get(second).await;
    assert_eq!(p["key"], "local/mac/axl-saas");
    assert_eq!(p["adopt_key"], "github.com/acme/axle-saas");
}

#[tokio::test]
async fn register_then_heartbeat_reports_unread() {
    let app = app().await;
    let token = app.token().await;
    let (session, agent) = app.register(&token, "github.com/acme/platform-infra").await;

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
    let (session, _) = app.register(&token, "github.com/acme/platform-infra").await;
    let project = app.project_id("github.com/acme/platform-infra").await;
    let task = app
        .state
        .tasks
        .create(
            axle::service::tasks::Caller::Human,
            axle::service::tasks::NewTask {
                project_id: project,
                title: "rotate the keys".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let resolved = axle::service::identity::IdentityService::load(&*app.state.store, session)
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
async fn leases_lists_held_tasks_with_titles_and_renews_nothing() {
    let app = app().await;
    let token = app.token().await;
    let (session, _) = app.register(&token, "github.com/acme/platform-infra").await;
    let get = |path: String| {
        let token = token.clone();
        let app = &app;
        async move {
            let res = app
                .call(
                    Request::get(path)
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await;
            let status = res.status();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    let (status, body) = get(format!("/v1/leases?session={session}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));

    let project = app.project_id("github.com/acme/platform-infra").await;
    let task = app
        .state
        .tasks
        .create(
            axle::service::tasks::Caller::Human,
            axle::service::tasks::NewTask {
                project_id: project,
                title: "rotate the keys".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let resolved = axle::service::identity::IdentityService::load(&*app.state.store, session)
        .await
        .unwrap()
        .expect("registered session");
    app.state.tasks.claim(&resolved, task.id).await.unwrap();
    let before = app
        .state
        .tasks
        .get(task.id)
        .await
        .unwrap()
        .task
        .lease_expires_at;

    app.clock.advance(time::Duration::seconds(30));
    let (status, body) = get(format!("/v1/leases?session={session}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!([{
            "id": task.id,
            "title": "rotate the keys",
            "status": "claimed",
            "created_by": "human",
            "edited_by": null,
            "project": "github.com/acme/platform-infra",
        }])
    );
    // The title's last editor, when not its author, is named so hooks can frame it.
    app.state
        .tasks
        .edit(&resolved, task.id, Some("rotate all keys".into()), None)
        .await
        .unwrap();
    let (_, body) = get(format!("/v1/leases?session={session}")).await;
    assert_eq!(body[0]["title"], "rotate all keys");
    assert_eq!(body[0]["created_by"], "human");
    assert_eq!(
        body[0]["edited_by"],
        "github.com/acme/platform-infra/mac/claude-code"
    );
    // A read: the lease is not renewed (that is what the heartbeat is for).
    let after = app
        .state
        .tasks
        .get(task.id)
        .await
        .unwrap()
        .task
        .lease_expires_at;
    assert_eq!(before, after);

    // Another token cannot read this session's leases.
    let other = app.token().await;
    let res = app
        .call(
            Request::get(format!("/v1/leases?session={session}"))
                .header(header::AUTHORIZATION, format!("Bearer {other}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let (status, _) = get(format!("/v1/leases?session={}", SessionId::new())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn events_stream_delivers_message() {
    let app = app().await;
    let token = app.token().await;
    let (session, agent) = app.register(&token, "github.com/acme/platform-infra").await;
    let mut stream = app.events(&token, session).await;

    app.send_from_human(Address::Agent(agent), "wake up").await;

    let (name, data) = next_event(&mut stream, Duration::from_secs(1))
        .await
        .expect("an event within 1 s");
    assert_eq!(name, "MessageCreated");
    assert_eq!(data["to"], json!({"kind": "agent", "id": agent}));
}

#[tokio::test]
async fn enrollment_events_are_not_sent_to_agents() {
    let app = app().await;
    let token = app.token().await;
    let (session, _) = app.register(&token, "github.com/acme/platform-infra").await;
    let mut stream = app.events(&token, session).await;

    app.state.enroll.issue_code("work laptop").await.unwrap();

    assert!(
        next_event(&mut stream, Duration::from_millis(200))
            .await
            .is_none(),
        "enrollment changes must not reach agent streams"
    );
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
    let (session, _) = app.register(&token, "github.com/acme/platform-infra").await;
    let project = app.project_id("github.com/acme/platform-infra").await;
    let mut stream = app.events(&token, session).await;

    let task = app
        .state
        .tasks
        .create(
            axle::service::tasks::Caller::Human,
            axle::service::tasks::NewTask {
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
    let (session, agent) = app.register(&token, "github.com/acme/platform-infra").await;
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
    assert!(plaintext.starts_with("axl_"), "{plaintext}");

    let hashes: Vec<String> = sqlx::query_scalar("SELECT hash FROM tokens")
        .fetch_all(&app.pool)
        .await
        .unwrap();
    assert_eq!(hashes.len(), 1);
    assert_ne!(hashes[0], plaintext);
    assert!(!hashes[0].contains(plaintext.trim_start_matches("axl_")));

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

/// Status of a heartbeat for an unknown session (404 while under the rate limit).
async fn probe(app: &App, token: &str) -> axum::response::Response {
    app.call(
        Request::post("/v1/heartbeat")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"session_id": SessionId::new()}).to_string(),
            ))
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn rate_limit_is_per_token_with_burst() {
    let app = app().await;
    let (a, b) = (app.token().await, app.token().await);
    let burst = app.state.config.rate_limit_burst;
    assert_eq!(burst, 100);

    // The test clock is frozen, so nothing refills during the burst.
    for _ in 0..burst {
        assert_eq!(probe(&app, &a).await.status(), StatusCode::NOT_FOUND);
    }
    let res = probe(&app, &a).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(res.headers()[header::RETRY_AFTER], "1");
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body, json!({"error": "rate_limited"}));

    // Another token has its own bucket.
    assert_eq!(probe(&app, &b).await.status(), StatusCode::NOT_FOUND);

    // 20/s sustained: one second later 20 more requests pass, the 21st does not.
    app.clock.advance(time::Duration::seconds(1));
    for _ in 0..20 {
        assert_eq!(probe(&app, &a).await.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(
        probe(&app, &a).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );

    // Unauthenticated probes are not limited.
    let res = app
        .call(Request::get("/healthz").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn shutdown_ends_sse_streams_and_returns() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let app = app().await;
    let token = app.token().await;
    let (session, _) = app.register(&token, "github.com/acme/platform-infra").await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(axle::http::serve(listener, app.state.clone(), async {
        let _ = stopped.await;
    }));

    // A connected SSE client.
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    conn.write_all(
        format!(
            "GET /v1/events?session={session} HTTP/1.1\r\nHost: x\r\n\
             Authorization: Bearer {token}\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut head = vec![0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(2), conn.read(&mut head))
        .await
        .expect("response head")
        .unwrap();
    let head = String::from_utf8_lossy(&head[..n]);
    assert!(head.contains("text/event-stream"), "{head}");

    let started = std::time::Instant::now();
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("serve returns promptly with an SSE client connected")
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < axle::http::SHUTDOWN_DRAIN);

    // The stream was ended by the server: the client reads to EOF.
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), conn.read_to_end(&mut rest))
        .await
        .expect("connection closed")
        .unwrap();
}

#[tokio::test]
async fn register_rejects_unsafe_identity_labels() {
    let app = app().await;
    let token = app.token().await;
    let (status, body) = app
        .post(
            &token,
            "/v1/register",
            json!({"session_id": SessionId::new(), "host": "mac] (x", "harness": "codex"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("invalid host"),
        "{body}"
    );
}
