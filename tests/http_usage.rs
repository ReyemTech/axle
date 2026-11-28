//! `POST /v1/usage` against in-memory SQLite.

use std::sync::Arc;

use axle::clock::{Clock, TestClock};
use axle::config::ServerConfig;
use axle::domain::{Billing, SessionId};
use axle::http::{AppState, router};
use axle::service::identity::{IdentityInput, IdentityService};
use axle::store::sqlite::SqliteStore;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use time::format_description::well_known::Rfc3339;
use time::macros::datetime;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

struct App {
    state: AppState,
    clock: Arc<TestClock>,
    router: Router,
}

async fn app_with(tweak: impl FnOnce(&mut ServerConfig)) -> App {
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
    let clock = Arc::new(TestClock::new(datetime!(2026-11-27 9:00 UTC)));
    let mut config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some("test-secret-0123456789".into()),
        _ => None,
    })
    .unwrap();
    tweak(&mut config);
    let state = AppState::new(store, clock.clone(), config);
    App {
        router: router(state.clone()),
        state,
        clock,
    }
}

async fn app() -> App {
    app_with(|_| {}).await
}

impl App {
    async fn token(&self) -> String {
        self.state.auth.create("test").await.unwrap().1
    }

    /// Registers a session for `token` straight through the identity service, so it does not
    /// spend the token's rate limit.
    async fn register(&self, token: &str) -> SessionId {
        let id = self
            .state
            .auth
            .verify(token, self.clock.now())
            .await
            .unwrap()
            .unwrap();
        let session = SessionId::new();
        IdentityService::resolve(
            &*self.state.store,
            &*self.state.clock,
            IdentityInput {
                project_key: Some("github.com/reyemtech/axle".into()),
                host: "mac".into(),
                harness: "claude-code".into(),
                session,
                token: id,
            },
        )
        .await
        .unwrap();
        session
    }

    async fn send(&self, token: Option<&str>, body: Vec<u8>) -> (StatusCode, Value) {
        let mut req = Request::post("/v1/usage").header(header::CONTENT_TYPE, "application/json");
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let res = self
            .router
            .clone()
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn post(&self, token: &str, body: Value) -> (StatusCode, Value) {
        self.send(Some(token), body.to_string().into_bytes()).await
    }

    fn at(&self, offset: Duration) -> String {
        (self.clock.now() + offset).format(&Rfc3339).unwrap()
    }

    async fn stored(&self, key: &str) -> Option<axle::store::StoredUsage> {
        self.state
            .store
            .get_usage("claude-code", key)
            .await
            .unwrap()
    }
}

fn record(key: &str, at: &str) -> Value {
    json!({"key": key, "at": at, "model": "claude-opus-5-5",
        "input": 12, "output": 845, "cache_write_5m": 0, "cache_write_1h": 3120, "cache_read": 48211,
        "reasoning": 210, "speed": null, "billing": "subscription"})
}

fn report(session: SessionId, records: Vec<Value>) -> Value {
    json!({"session_id": session, "parser": "claude-code/1", "records": records})
}

#[tokio::test]
async fn accepts_and_dedupes() {
    let app = app().await;
    let token = app.token().await;
    let session = app.register(&token).await;
    let body = report(session, vec![record("k1", &app.at(Duration::ZERO))]);

    let (status, out) = app.post(&token, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out, json!({"accepted": 1, "duplicate": 0, "rejected": []}));

    let (status, out) = app.post(&token, body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out, json!({"accepted": 0, "duplicate": 1, "rejected": []}));

    let stored = app.stored("k1").await.unwrap();
    assert_eq!(stored.cost_micros, Some(51550));
    assert_eq!(stored.billing, Billing::Subscription);
}

#[tokio::test]
async fn auth_and_ownership() {
    let app = app().await;
    let token = app.token().await;
    let other = app.token().await;
    let session = app.register(&token).await;
    let body = report(session, vec![record("k1", &app.at(Duration::ZERO))]);

    let (status, _) = app.send(None, body.to_string().into_bytes()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let unknown = report(SessionId::new(), vec![]);
    let (status, _) = app.post(&token, unknown).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = app.post(&other, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(app.stored("k1").await.is_none());
}

#[tokio::test]
async fn envelope_errors_store_nothing() {
    let app = app().await;
    let token = app.token().await;
    let session = app.register(&token).await;
    let at = app.at(Duration::ZERO);

    let (status, _) = app.send(Some(&token), b"not json".to_vec()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = app
        .post(
            &token,
            json!({"parser": "claude-code/1", "records": [record("e0", &at)]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let many: Vec<Value> = (0..501).map(|i| record(&format!("e{i}"), &at)).collect();
    let (status, out) = app.post(&token, report(session, many)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(out["error"], "more than 500 records");

    let (status, out) = app.send(Some(&token), vec![b' '; 256 * 1024 + 1]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(out["error"], "body over 256 KB");

    for parser in ["Claude Code", "", "claude-code", "claude-code/x", "a/1/2"] {
        let mut body = report(session, vec![record("e0", &at)]);
        body["parser"] = json!(parser);
        let (status, _) = app.post(&token, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{parser:?}");
    }
    let mut body = report(session, vec![record("e0", &at)]);
    body["harness_version"] = json!("v".repeat(65));
    assert_eq!(app.post(&token, body).await.0, StatusCode::BAD_REQUEST);
    let mut body = report(session, vec![record("e0", &at)]);
    body["harness_session_id"] = json!("s".repeat(201));
    assert_eq!(app.post(&token, body).await.0, StatusCode::BAD_REQUEST);

    for i in 0..501 {
        assert!(app.stored(&format!("e{i}")).await.is_none());
    }
}

#[tokio::test]
async fn partial_rejection() {
    let app = app().await;
    let token = app.token().await;
    let session = app.register(&token).await;
    let at = app.at(Duration::ZERO);

    let mut bad_model = record("m", &at);
    bad_model["model"] = json!("bad model!");
    let mut negative = record("n", &at);
    negative["input"] = json!(-1);
    let long_key = "k".repeat(201);
    let mut bad_speed = record("s", &at);
    bad_speed["speed"] = json!("Fast");
    let mut bad_time = record("t", &at);
    bad_time["at"] = json!("yesterday");
    let body = report(
        session,
        vec![
            record("good", &at),
            bad_model,
            negative,
            record(&long_key, &at),
            bad_speed,
            bad_time,
        ],
    );
    let (status, out) = app.post(&token, body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["accepted"], 1);
    assert_eq!(
        out["rejected"],
        json!([
            {"key": "m", "reason": "model empty, over 128 bytes or has characters outside A-Za-z0-9._:/@-"},
            {"key": "n", "reason": "count below 0 or over 1000000000"},
            {"key": long_key, "reason": "key empty or over 200 bytes"},
            {"key": "s", "reason": "speed over 16 bytes or not a-z"},
            {"key": "t", "reason": "timestamp not RFC 3339"},
        ])
    );
    assert!(app.stored("good").await.is_some());
    assert!(app.stored("m").await.is_none());
}

#[tokio::test]
async fn old_and_future_records_rejected() {
    let app = app().await;
    let token = app.token().await;
    let session = app.register(&token).await;
    let body = report(
        session,
        vec![
            record("future", &app.at(Duration::minutes(6))),
            record("old", &app.at(-Duration::days(31))),
            record("soon", &app.at(Duration::minutes(4))),
            record("recent", &app.at(-Duration::days(29))),
        ],
    );
    let (status, out) = app.post(&token, body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["accepted"], 2);
    assert_eq!(
        out["rejected"],
        json!([
            {"key": "future", "reason": "timestamp more than 5 minutes ahead"},
            {"key": "old", "reason": "timestamp more than 30 days old"},
        ])
    );
}

#[tokio::test]
async fn malformed_record_rejected_alone() {
    let app = app().await;
    let token = app.token().await;
    let session = app.register(&token).await;
    let body = report(
        session,
        vec![
            json!({"key": "k", "input": "x"}),
            json!(7),
            record("good", &app.at(Duration::ZERO)),
        ],
    );
    let (status, out) = app.post(&token, body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["accepted"], 1);
    assert_eq!(
        out["rejected"],
        json!([
            {"key": "k", "reason": "malformed record"},
            {"key": "", "reason": "malformed record"},
        ])
    );
}

#[tokio::test]
async fn not_a_heartbeat() {
    let app = app().await;
    let token = app.token().await;
    let session = app.register(&token).await;
    let before = app
        .state
        .store
        .get_session(session)
        .await
        .unwrap()
        .unwrap()
        .last_heartbeat_at;
    app.clock.advance(Duration::minutes(10));
    let (status, out) = app
        .post(
            &token,
            report(session, vec![record("k", &app.at(Duration::ZERO))]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let after = app
        .state
        .store
        .get_session(session)
        .await
        .unwrap()
        .unwrap()
        .last_heartbeat_at;
    assert_eq!(before, after);
    let _: OffsetDateTime = after;
}

#[tokio::test]
async fn counts_against_the_rate_limit() {
    let app = app_with(|c| {
        c.rate_limit_burst = 1;
        c.rate_limit_per_sec = 1;
    })
    .await;
    let token = app.token().await;
    let session = app.register(&token).await;
    let body = report(session, vec![]);
    assert_eq!(app.post(&token, body.clone()).await.0, StatusCode::OK);
    assert_eq!(
        app.post(&token, body).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}
