//! Enrollment and device-login endpoints, per-IP limits and `/v1/whoami`, against in-memory
//! SQLite.

use std::net::SocketAddr;
use std::sync::Arc;

use axle::clock::TestClock;
use axle::config::ServerConfig;
use axle::http::{AppState, router};
use axle::store::sqlite::SqliteStore;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use time::Duration;
use time::macros::datetime;
use tower::ServiceExt;

struct App {
    state: AppState,
    clock: Arc<TestClock>,
    router: Router,
}

async fn app() -> App {
    app_with(&[]).await
}

async fn app_with(extra: &[(&'static str, &'static str)]) -> App {
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
    let config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some("test-secret-0123456789".into()),
        _ => extra
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string()),
    })
    .unwrap();
    let state = AppState::new(store, clock.clone(), config);
    App {
        router: router(state.clone()),
        state,
        clock,
    }
}

struct Reply {
    status: StatusCode,
    headers: header::HeaderMap,
    bytes: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
}

impl App {
    async fn send(&self, req: Request<Body>) -> Reply {
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        Reply {
            status,
            headers,
            bytes,
        }
    }

    async fn post(&self, path: &str, body: Value) -> Reply {
        self.post_from(path, body, None, &[]).await
    }

    async fn post_from(
        &self,
        path: &str,
        body: Value,
        ip: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Reply {
        let mut req = Request::post(path).header(header::CONTENT_TYPE, "application/json");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let mut req = req.body(Body::from(body.to_string())).unwrap();
        if let Some(ip) = ip {
            let addr: SocketAddr = format!("{ip}:5555").parse().unwrap();
            req.extensions_mut().insert(ConnectInfo(addr));
        }
        self.send(req).await
    }

    async fn get(&self, path: &str, token: Option<&str>) -> Reply {
        let mut req = Request::get(path);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }

    async fn poll(&self, device_code: &str) -> Reply {
        self.post("/v1/login/poll", json!({"device_code": device_code}))
            .await
    }

    async fn enroll(&self, code: &str) -> Reply {
        self.post("/v1/enroll", json!({"code": code, "host": "laptop"}))
            .await
    }
}

#[tokio::test]
async fn enroll_with_code_returns_token_once() {
    let app = app().await;
    let issued = app.state.enroll.issue_code("work laptop").await.unwrap();
    let res = app.enroll(&issued.code).await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.json());
    let body = res.json();
    assert!(body["token"].as_str().unwrap().starts_with("axl_"));
    assert_eq!(body["server_url"], "http://127.0.0.1:7420");
    assert_eq!(res.headers[header::CACHE_CONTROL], "no-store");

    let again = app.enroll(&issued.code).await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    assert_eq!(again.json(), json!({"error": "invalid or expired code"}));
}

#[tokio::test]
async fn enroll_expired_and_unknown_look_identical() {
    let app = app().await;
    let issued = app.state.enroll.issue_code("work laptop").await.unwrap();
    app.clock.advance(Duration::minutes(15));
    let expired = app.enroll(&issued.code).await;
    let unknown = app.enroll("ZZZZZ-ZZZZZ").await;
    let malformed = app.enroll("nope").await;
    assert_eq!(expired.status, StatusCode::BAD_REQUEST);
    assert_eq!(expired.status, unknown.status);
    assert_eq!(expired.bytes, unknown.bytes);
    assert_eq!(expired.bytes, malformed.bytes);
    assert_eq!(expired.headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn token_from_enroll_authenticates_whoami() {
    let app = app().await;
    let issued = app.state.enroll.issue_code("work laptop").await.unwrap();
    let token = app.enroll(&issued.code).await.json()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let res = app.get("/v1/whoami", Some(&token)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.json(),
        json!({"label": "work laptop (laptop)", "version": env!("CARGO_PKG_VERSION")})
    );
    assert_eq!(res.headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn whoami_requires_token() {
    let app = app().await;
    assert_eq!(
        app.get("/v1/whoami", None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.get("/v1/whoami", Some("axl_nope")).await.status,
        StatusCode::UNAUTHORIZED
    );
}

fn is_user_code(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 9
        && b[4] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || c.is_ascii_digit() || c.is_ascii_uppercase())
}

#[tokio::test]
async fn device_flow_start_poll_approve() {
    let app = app().await;
    let start = app.post("/v1/login/start", json!({"host": "laptop"})).await;
    assert_eq!(start.status, StatusCode::OK, "{:?}", start.json());
    let b = start.json();
    assert!(b["verify_url"].as_str().unwrap().ends_with("/ui/connect"));
    assert_eq!(b["verify_url"], "http://127.0.0.1:7420/ui/connect");
    assert_eq!(b["interval"], 7);
    assert_eq!(b["expires_in"], 600);
    let user_code = b["user_code"].as_str().unwrap();
    assert!(is_user_code(user_code), "{user_code}");
    assert_eq!(start.headers[header::CACHE_CONTROL], "no-store");
    let device_code = b["device_code"].as_str().unwrap().to_string();

    let pending = app.poll(&device_code).await;
    assert_eq!(pending.status, StatusCode::BAD_REQUEST);
    assert_eq!(pending.json(), json!({"error": "authorization_pending"}));
    assert_eq!(pending.headers[header::CACHE_CONTROL], "no-store");

    let id = app
        .state
        .enroll
        .lookup_user_code(user_code)
        .await
        .unwrap()
        .unwrap()
        .id;
    assert!(app.state.enroll.decide(id, true).await.unwrap());
    app.clock.advance(Duration::seconds(7));
    let ok = app.poll(&device_code).await;
    assert_eq!(ok.status, StatusCode::OK, "{:?}", ok.json());
    let body = ok.json();
    assert!(body["token"].as_str().unwrap().starts_with("axl_"));
    assert_eq!(body["server_url"], "http://127.0.0.1:7420");
    assert_eq!(ok.headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn device_flow_slow_down_when_polling_too_fast() {
    let app = app().await;
    let start = app.post("/v1/login/start", json!({"host": "laptop"})).await;
    let code = start.json()["device_code"].as_str().unwrap().to_string();
    let first = app
        .post("/v1/login/poll", json!({"device_code": code}))
        .await;
    assert_eq!(first.json()["error"], "authorization_pending");
    let second = app
        .post("/v1/login/poll", json!({"device_code": code}))
        .await;
    assert_eq!(second.status, StatusCode::BAD_REQUEST);
    assert_eq!(second.json(), json!({"error": "slow_down"}));
}

#[tokio::test]
async fn device_flow_deny_and_expiry() {
    let app = app().await;
    let start = app.post("/v1/login/start", json!({"host": "laptop"})).await;
    let b = start.json();
    let id = app
        .state
        .enroll
        .lookup_user_code(b["user_code"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap()
        .id;
    assert!(app.state.enroll.decide(id, false).await.unwrap());
    let denied = app
        .post("/v1/login/poll", json!({"device_code": b["device_code"]}))
        .await;
    assert_eq!(denied.status, StatusCode::BAD_REQUEST);
    assert_eq!(denied.json(), json!({"error": "denied"}));

    let fresh = app.post("/v1/login/start", json!({"host": "laptop"})).await;
    app.clock.advance(Duration::minutes(10));
    let expired = app
        .post(
            "/v1/login/poll",
            json!({"device_code": fresh.json()["device_code"]}),
        )
        .await;
    assert_eq!(expired.status, StatusCode::BAD_REQUEST);
    assert_eq!(expired.json(), json!({"error": "expired"}));

    let unknown = app
        .post("/v1/login/poll", json!({"device_code": "axld_nope"}))
        .await;
    assert_eq!(unknown.json(), json!({"error": "expired"}));
}

#[tokio::test]
async fn per_ip_limit_is_10_per_minute() {
    let app = app().await;
    let token = app.state.auth.create("test").await.unwrap().1;
    for i in 0..10 {
        let res = app
            .post_from(
                "/v1/enroll",
                json!({"code": "ZZZZZ-ZZZZZ", "host": "h"}),
                Some("203.0.113.5"),
                &[],
            )
            .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "request {i}");
    }
    let limited = app
        .post_from(
            "/v1/enroll",
            json!({"code": "ZZZZZ-ZZZZZ", "host": "h"}),
            Some("203.0.113.5"),
            &[],
        )
        .await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.json(), json!({"error": "rate_limited"}));
    assert!(limited.headers.contains_key(header::RETRY_AFTER));
    assert_eq!(limited.headers[header::CACHE_CONTROL], "no-store");
    // Login endpoints share the same per-IP bucket.
    let login = app
        .post_from(
            "/v1/login/start",
            json!({"host": "h"}),
            Some("203.0.113.5"),
            &[],
        )
        .await;
    assert_eq!(login.status, StatusCode::TOO_MANY_REQUESTS);
    // Another IP is unaffected, and bearer-authenticated requests are not IP limited.
    let other = app
        .post_from(
            "/v1/enroll",
            json!({"code": "ZZZZZ-ZZZZZ", "host": "h"}),
            Some("203.0.113.6"),
            &[],
        )
        .await;
    assert_eq!(other.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        app.get("/v1/whoami", Some(&token)).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn forwarded_for_ignored_unless_trusted() {
    let app = app().await;
    for i in 0..10 {
        let xff = format!("198.51.100.{i}");
        let res = app
            .post_from(
                "/v1/enroll",
                json!({"code": "ZZZZZ-ZZZZZ", "host": "h"}),
                None,
                &[("X-Forwarded-For", &xff)],
            )
            .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST);
    }
    let res = app
        .post_from(
            "/v1/enroll",
            json!({"code": "ZZZZZ-ZZZZZ", "host": "h"}),
            None,
            &[("X-Forwarded-For", "198.51.100.99")],
        )
        .await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS);

    let app = app_with(&[("AXL_TRUSTED_PROXY_HEADER", "X-Forwarded-For")]).await;
    let send = |xff: &'static str| {
        let app = &app;
        async move {
            app.post_from(
                "/v1/enroll",
                json!({"code": "ZZZZZ-ZZZZZ", "host": "h"}),
                Some("192.0.2.1"),
                &[("X-Forwarded-For", xff)],
            )
            .await
            .status
        }
    };
    for _ in 0..10 {
        assert_eq!(send("1.1.1.1, 10.0.0.9").await, StatusCode::BAD_REQUEST);
    }
    // Same right-most entry, different spoofed left side: same bucket.
    assert_eq!(
        send("2.2.2.2, 10.0.0.9").await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // A different right-most entry is its own bucket.
    assert_eq!(send("1.1.1.1, 10.0.0.8").await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn non_loopback_without_public_url_start_is_503() {
    let app = app_with(&[("AXL_BIND", "0.0.0.0:7420")]).await;
    let start = app.post("/v1/login/start", json!({"host": "laptop"})).await;
    assert_eq!(start.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        start.json()["error"]
            .as_str()
            .unwrap()
            .contains("AXL_PUBLIC_URL")
    );

    // A code is never consumed when the server URL can't be answered.
    let issued = app.state.enroll.issue_code("work laptop").await.unwrap();
    let res = app.enroll(&issued.code).await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        res.json()["error"]
            .as_str()
            .unwrap()
            .contains("AXL_PUBLIC_URL")
    );
    assert!(
        app.state
            .enroll
            .exchange(&issued.code, "laptop", None)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn public_url_is_advertised() {
    let app = app_with(&[
        ("AXL_BIND", "0.0.0.0:7420"),
        ("AXL_PUBLIC_URL", "https://axle.example.com/"),
    ])
    .await;
    let issued = app.state.enroll.issue_code("work laptop").await.unwrap();
    let res = app.enroll(&issued.code).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.json()["server_url"], "https://axle.example.com");
    let start = app.post("/v1/login/start", json!({"host": "laptop"})).await;
    assert_eq!(
        start.json()["verify_url"],
        "https://axle.example.com/ui/connect"
    );
}

#[tokio::test]
async fn unparseable_forwarded_value_is_keyed_by_peer_address() {
    let app = app_with(&[("AXL_TRUSTED_PROXY_HEADER", "X-Forwarded-For")]).await;
    let send = |xff: String| {
        let app = &app;
        async move {
            app.post_from(
                "/v1/enroll",
                json!({"code": "ZZZZZ-ZZZZZ", "host": "h"}),
                Some("192.0.2.1"),
                &[("X-Forwarded-For", &xff)],
            )
            .await
            .status
        }
    };
    for i in 0..10 {
        assert_eq!(send(format!("garbage-{i}")).await, StatusCode::BAD_REQUEST);
    }
    // Every junk value shares the peer's bucket, however it is spelled.
    assert_eq!(
        send("1.1.1.1:99".into()).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // A real forwarded address is still its own bucket.
    assert_eq!(send("10.0.0.9".into()).await, StatusCode::BAD_REQUEST);
}
