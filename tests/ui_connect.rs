//! Connect a machine (onboarding spec §2, §9): the one-time code panel, the pending list,
//! device approval at `/ui/connect`, sign-in with `next`, and the overview empty state.

use std::sync::Arc;

use axle::clock::TestClock;
use axle::config::ServerConfig;
use axle::http::{AppState, router};
use axle::store::sqlite::SqliteStore;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use scraper::{Html, Selector};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use time::macros::datetime;
use tower::ServiceExt;

const SECRET: &str = "test-secret-0123456789";

struct App {
    state: AppState,
    router: Router,
}

async fn app_with(vars: &'static [(&'static str, &'static str)]) -> App {
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
        "AXL_ADMIN_SECRET" => Some(SECRET.into()),
        _ => vars
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string()),
    })
    .unwrap();
    let state = AppState::new(store, clock, config);
    App {
        router: router(state.clone()),
        state,
    }
}

async fn app() -> App {
    app_with(&[]).await
}

struct Page {
    status: StatusCode,
    location: Option<String>,
    set_cookie: Option<String>,
    cache_control: Option<String>,
    body: String,
}

impl Page {
    fn html(&self) -> Html {
        Html::parse_document(&self.body)
    }

    fn text(&self) -> String {
        self.html()
            .root_element()
            .text()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The text of each element matching `css`, trimmed.
    fn texts(&self, css: &str) -> Vec<String> {
        let sel = Selector::parse(css).unwrap();
        self.html()
            .select(&sel)
            .map(|e| e.text().collect::<String>().trim().to_string())
            .collect()
    }

    /// The `attr` of each element matching `css`.
    fn attrs(&self, css: &str, attr: &str) -> Vec<String> {
        let sel = Selector::parse(css).unwrap();
        self.html()
            .select(&sel)
            .filter_map(|e| e.value().attr(attr).map(str::to_string))
            .collect()
    }

    fn csrf(&self) -> String {
        self.attrs("meta[name=csrf-token]", "content")
            .pop()
            .expect("csrf meta tag")
    }

    /// The one-time code shown on the page: `XXXXX-XXXXX`.
    fn code(&self) -> String {
        let chars: Vec<char> = self.text().chars().collect();
        chars
            .windows(11)
            .find(|w| {
                w.iter().enumerate().all(|(i, c)| {
                    if i == 5 {
                        *c == '-'
                    } else {
                        c.is_ascii_digit() || c.is_ascii_uppercase()
                    }
                })
            })
            .expect("a one-time code on the page")
            .iter()
            .collect()
    }
}

impl App {
    async fn send(&self, req: Request<Body>) -> Page {
        let res = self.router.clone().oneshot(req).await.unwrap();
        let header = |name| {
            res.headers()
                .get(name)
                .map(|v: &header::HeaderValue| v.to_str().unwrap().to_string())
        };
        let location = header(header::LOCATION);
        let set_cookie = header(header::SET_COOKIE);
        let cache_control = header(header::CACHE_CONTROL);
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Page {
            status,
            location,
            set_cookie,
            cache_control,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn get(&self, path: &str, cookie: Option<&str>) -> Page {
        let mut req = Request::get(path);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }

    async fn post_form(&self, path: &str, cookie: Option<&str>, form: &str) -> Page {
        let mut req =
            Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        self.send(req.body(Body::from(form.to_string())).unwrap())
            .await
    }

    async fn login(&self) -> String {
        let page = self
            .post_form("/ui/login", None, &format!("secret={SECRET}"))
            .await;
        assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
        cookie_pair(page.set_cookie.as_deref().expect("session cookie"))
    }

    /// Posts the Connect a machine form; returns the response page.
    async fn connect(&self, cookie: &str, csrf: &str, label: &str) -> Page {
        self.post_form(
            "/ui/tokens/connect",
            Some(cookie),
            &format!("csrf={csrf}&label={}", label.replace(' ', "+")),
        )
        .await
    }

    async fn enroll(&self, code: &str) -> (StatusCode, String) {
        let res = self
            .router
            .clone()
            .oneshot(
                Request::post("/v1/enroll")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "code": code, "host": "box" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }
}

fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().trim().to_string()
}

#[tokio::test]
async fn connect_form_shows_code_and_both_commands_once() {
    let app = app().await;
    let cookie = app.login().await;
    let tokens = app.get("/ui/tokens", Some(&cookie)).await;
    let csrf = tokens.csrf();

    let page = app.connect(&cookie, &csrf, "work laptop").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert_eq!(page.cache_control.as_deref(), Some("no-store"));
    let code = page.code();
    let text = page.text();
    assert!(
        text.contains(&format!(
            "curl -fsSL http://127.0.0.1:7420/install.sh | sh -s -- {code}"
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("axl install --server http://127.0.0.1:7420 --code {code}")),
        "{text}"
    );
    assert!(page.text().contains("15 minutes"), "{text}");
    // Copy buttons are labelled and wired through app.js (no inline script).
    assert!(page.texts("button[data-copy]").iter().all(|t| t == "Copy"));
    assert!(!page.body.contains("<script>"));

    // The code is shown once: a later visit lists the pending row, never the code.
    let again = app.get("/ui/tokens", Some(&cookie)).await;
    assert!(!again.body.contains(&code), "{}", again.body);
    assert!(
        again
            .texts("#pending .row-title")
            .contains(&"work laptop".to_string()),
        "{}",
        again.text()
    );
    assert!(
        again
            .texts("#pending button.danger")
            .contains(&"Revoke".to_string())
    );
    assert!(
        again.text().contains("expires in 15:00"),
        "{}",
        again.text()
    );
}

#[tokio::test]
async fn connect_label_is_validated() {
    let app = app().await;
    let cookie = app.login().await;
    let csrf = app.get("/ui/tokens", Some(&cookie)).await.csrf();
    for bad in ["", "+++", &"x".repeat(65)] {
        let page = app.connect(&cookie, &csrf, bad).await;
        assert_eq!(page.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}");
        assert!(
            page.texts("p.error")
                .iter()
                .any(|t| t.contains("1 to 64 characters")),
            "{}",
            page.text()
        );
    }
    assert!(app.state.enroll.pending().await.unwrap().is_empty());
    let ok = app.connect(&cookie, &csrf, &"x".repeat(64)).await;
    assert_eq!(ok.status, StatusCode::OK);
}

#[tokio::test]
async fn connect_without_csrf_is_rejected() {
    let app = app().await;
    let cookie = app.login().await;
    let page = app
        .post_form("/ui/tokens/connect", Some(&cookie), "label=laptop")
        .await;
    assert_eq!(page.status, StatusCode::FORBIDDEN);
    assert!(app.state.enroll.pending().await.unwrap().is_empty());
}

#[tokio::test]
async fn connect_panel_hints_public_url_when_missing() {
    let app = app_with(&[("AXL_BIND", "0.0.0.0:7420")]).await;
    let cookie = app.login().await;
    let page = app.get("/ui/tokens", Some(&cookie)).await;
    let text = page.text();
    assert!(text.contains("AXL_PUBLIC_URL"), "{text}");
    assert!(!text.contains("install.sh"), "{text}");
    assert!(
        page.attrs("form[action='/ui/tokens/connect']", "action")
            .is_empty()
    );

    // A forged POST issues nothing either.
    let posted = app.connect(&cookie, &page.csrf(), "laptop").await;
    assert!(posted.text().contains("AXL_PUBLIC_URL"));
    assert!(!posted.text().contains("install.sh"));
    assert!(app.state.enroll.pending().await.unwrap().is_empty());
}

#[tokio::test]
async fn revoke_pending_enrollment_invalidates_code() {
    let app = app().await;
    let cookie = app.login().await;
    let csrf = app.get("/ui/tokens", Some(&cookie)).await.csrf();
    let code = app.connect(&cookie, &csrf, "work laptop").await.code();

    let tokens = app.get("/ui/tokens", Some(&cookie)).await;
    let action = tokens
        .attrs("#pending form", "action")
        .pop()
        .expect("a revoke form");
    assert!(action.starts_with("/ui/enrollments/") && action.ends_with("/revoke"));
    let res = app
        .post_form(&action, Some(&cookie), &format!("csrf={csrf}"))
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);

    let (status, body) = app.enroll(&code).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid or expired code"));
    let after = app.get("/ui/tokens", Some(&cookie)).await;
    assert!(after.texts("#pending .row-title").is_empty());

    // The pending list refreshes on the tokens SSE frame.
    let fragment = after.attrs("#pending", "hx-get");
    assert_eq!(fragment, vec!["/ui/enrollments/pending".to_string()]);
    assert!(
        app.get(&fragment[0], Some(&cookie))
            .await
            .status
            .is_success()
    );
}

#[tokio::test]
async fn device_approval_shows_host_ip_time_then_approves() {
    let app = app().await;
    let cookie = app.login().await;
    let csrf = app.get("/ui/connect", Some(&cookie)).await.csrf();
    let start = app
        .state
        .enroll
        .start_device("build-02", Some("10.0.0.7"))
        .await
        .unwrap();

    let card = app
        .post_form(
            "/ui/connect/lookup",
            Some(&cookie),
            &format!("csrf={csrf}&user_code={}", start.user_code.to_lowercase()),
        )
        .await;
    assert_eq!(card.status, StatusCode::OK, "{}", card.body);
    let text = card.text();
    for want in [
        "Connect this machine?",
        "build-02",
        "10.0.0.7",
        "2026-11-27 09:00 UTC",
    ] {
        assert!(text.contains(want), "{want} missing from {text}");
    }
    assert_eq!(card.texts("main button"), vec!["Approve", "Deny"]);
    let approve = card
        .attrs("form[action$='/approve']", "action")
        .pop()
        .expect("approve form");
    let done = app
        .post_form(&approve, Some(&cookie), &format!("csrf={csrf}"))
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert!(done.text().contains("Approved"), "{}", done.text());

    match app.state.enroll.poll(&start.device_code).await.unwrap() {
        axle::service::enroll::PollReply::Token { token, host } => {
            assert!(token.starts_with("axl_"));
            assert_eq!(host, "build-02");
        }
        other => panic!("expected a token, got {other:?}"),
    }
}

#[tokio::test]
async fn device_deny() {
    let app = app().await;
    let cookie = app.login().await;
    let csrf = app.get("/ui/connect", Some(&cookie)).await.csrf();
    let start = app.state.enroll.start_device("mac", None).await.unwrap();
    let card = app
        .post_form(
            "/ui/connect/lookup",
            Some(&cookie),
            &format!("csrf={csrf}&user_code={}", start.user_code),
        )
        .await;
    let deny = card
        .attrs("form[action$='/deny']", "action")
        .pop()
        .expect("deny form");
    let done = app
        .post_form(&deny, Some(&cookie), &format!("csrf={csrf}"))
        .await;
    assert_eq!(done.status, StatusCode::OK);
    assert!(done.text().contains("Denied"), "{}", done.text());
    assert_eq!(
        app.state.enroll.poll(&start.device_code).await.unwrap(),
        axle::service::enroll::PollReply::Denied
    );
    // Deciding again says so instead of pretending.
    let again = app
        .post_form(&deny, Some(&cookie), &format!("csrf={csrf}"))
        .await;
    assert!(
        again.text().contains("no longer pending"),
        "{}",
        again.text()
    );
}

#[tokio::test]
async fn connect_page_redirects_to_login_with_next_and_back() {
    let app = app().await;
    let res = app.get("/ui/connect", None).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.location.as_deref(), Some("/ui/login?next=/ui/connect"));
    // Other pages carry no `next`.
    let board = app.get("/ui/board", None).await;
    assert_eq!(board.location.as_deref(), Some("/ui/login"));

    let login = app.get("/ui/login?next=/ui/connect", None).await;
    assert_eq!(
        login.attrs("input[name=next]", "value"),
        vec!["/ui/connect".to_string()]
    );
    let back = app
        .post_form(
            "/ui/login",
            None,
            &format!("secret={SECRET}&next=/ui/connect"),
        )
        .await;
    assert_eq!(back.location.as_deref(), Some("/ui/connect"));

    // A wrong secret keeps `next` on the re-rendered form.
    let wrong = app
        .post_form("/ui/login", None, "secret=nope&next=/ui/connect")
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        wrong.attrs("input[name=next]", "value"),
        vec!["/ui/connect".to_string()]
    );

    // Nothing else is ever followed.
    for evil in [
        "https://evil.example",
        "//evil.example",
        "/ui/connect/../tokens",
        "/ui/tokens",
    ] {
        let res = app
            .post_form(
                "/ui/login",
                None,
                &format!("secret={SECRET}&next={}", evil.replace('/', "%2F")),
            )
            .await;
        assert_eq!(res.location.as_deref(), Some("/ui/"), "{evil}");
    }
    let hostile = app.get("/ui/login?next=https://evil.example", None).await;
    assert!(hostile.attrs("input[name=next]", "value").is_empty());
}

#[tokio::test]
async fn unknown_user_code_says_so() {
    let app = app().await;
    let cookie = app.login().await;
    let csrf = app.get("/ui/connect", Some(&cookie)).await.csrf();
    for code in ["ZZZZ-ZZZZ", "nope", ""] {
        let page = app
            .post_form(
                "/ui/connect/lookup",
                Some(&cookie),
                &format!("csrf={csrf}&user_code={code}"),
            )
            .await;
        assert_eq!(page.status, StatusCode::NOT_FOUND, "{code:?}");
        assert!(
            page.text().contains("No pending request with that code"),
            "{}",
            page.text()
        );
        assert!(
            page.text().contains("Check the code"),
            "form is offered again"
        );
    }
}

#[tokio::test]
async fn overview_empty_state_points_at_connect() {
    let app = app().await;
    let cookie = app.login().await;
    let page = app.get("/ui/", Some(&cookie)).await;
    assert!(
        page.text()
            .contains("No agents yet. Connect a machine to get started."),
        "{}",
        page.text()
    );
    assert!(
        page.attrs("#overview a", "href")
            .contains(&"/ui/tokens#connect".to_string())
    );
    let panel = app.get("/ui/overview/panel", Some(&cookie)).await;
    assert!(panel.text().contains("No agents yet."));
}

const XSS: &str = "<script>alert(1)</script>";

async fn pending_device(app: &App, host: &str) -> (String, String) {
    let start = app
        .state
        .enroll
        .start_device(host, Some("10.0.0.7"))
        .await
        .unwrap();
    let id = app
        .state
        .enroll
        .lookup_user_code(&start.user_code)
        .await
        .unwrap()
        .unwrap()
        .id;
    (id.to_string(), start.user_code)
}

#[tokio::test]
async fn result_panel_has_a_countdown_and_distinct_copy_labels() {
    let app = app().await;
    let cookie = app.login().await;
    let csrf = app.get("/ui/tokens", Some(&cookie)).await.csrf();
    let page = app.connect(&cookie, &csrf, "laptop").await;
    assert_eq!(
        page.attrs("#connect-result [data-countdown]", "data-countdown"),
        vec!["900".to_string()]
    );
    assert_eq!(
        page.attrs("#connect-result [data-expires-at]", "data-expires-at"),
        vec!["2026-11-27T09:15:00Z".to_string()]
    );
    assert!(page.text().contains("expires in 15:00"));
    assert_eq!(
        page.attrs("button[data-copy]", "aria-label"),
        vec!["Copy code", "Copy install command", "Copy login command"]
    );
}

#[tokio::test]
async fn mutating_routes_need_csrf_and_change_nothing() {
    let app = app().await;
    let cookie = app.login().await;
    let (id, user_code) = pending_device(&app, "build-02").await;
    let csrf = app.get("/ui/tokens", Some(&cookie)).await.csrf();
    app.connect(&cookie, &csrf, "laptop").await;
    let before = app.state.enroll.pending().await.unwrap().len();
    assert_eq!(before, 2);
    for (path, form) in [
        (
            "/ui/connect/lookup".to_string(),
            format!("user_code={user_code}"),
        ),
        (format!("/ui/connect/{id}/approve"), String::new()),
        (format!("/ui/connect/{id}/deny"), String::new()),
        (format!("/ui/enrollments/{id}/revoke"), String::new()),
        (
            format!("/ui/enrollments/{id}/revoke"),
            "csrf=wrong".to_string(),
        ),
    ] {
        let res = app.post_form(&path, Some(&cookie), &form).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(app.state.enroll.pending().await.unwrap().len(), before);
}

#[tokio::test]
async fn approve_without_a_session_is_refused() {
    let app = app().await;
    let (id, _) = pending_device(&app, "build-02").await;
    for path in [
        format!("/ui/connect/{id}/approve"),
        format!("/ui/connect/{id}/deny"),
        format!("/ui/enrollments/{id}/revoke"),
    ] {
        let res = app.post_form(&path, None, "csrf=anything").await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{path}");
        assert!(res.location.as_deref().unwrap().starts_with("/ui/login"));
    }
    assert_eq!(app.state.enroll.pending().await.unwrap().len(), 1);
}

#[tokio::test]
async fn approve_needs_csrf_even_without_ui_sign_in() {
    // Loopback bind, no admin secret: UI auth is `none`.
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
    let config =
        ServerConfig::from_lookup(|k| (k == "DATABASE_URL").then(|| "sqlite::memory:".to_string()))
            .unwrap();
    assert_eq!(config.ui_auth, axle::config::UiAuth::None);
    let state = AppState::new(store, clock, config);
    let app = App {
        router: router(state.clone()),
        state,
    };
    let (id, _) = pending_device(&app, "mac").await;
    let host = |req: axum::http::request::Builder| req.header(header::HOST, "127.0.0.1:7420");
    let post = |path: String, form: String, cookie: Option<String>| {
        let mut r = host(Request::post(path))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(c) = cookie {
            r = r.header(header::COOKIE, c);
        }
        r.body(Body::from(form)).unwrap()
    };
    // No session at all, no token: refused, nothing approved.
    let res = app
        .send(post(
            format!("/ui/connect/{id}/approve"),
            String::new(),
            None,
        ))
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert_eq!(app.state.enroll.pending().await.unwrap().len(), 1);
    // With a session but no token: still refused.
    let page = app
        .send(
            host(Request::get("/ui/connect"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let cookie = cookie_pair(page.set_cookie.as_deref().unwrap());
    let res = app
        .send(post(
            format!("/ui/connect/{id}/approve"),
            String::new(),
            Some(cookie.clone()),
        ))
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert_eq!(app.state.enroll.pending().await.unwrap().len(), 1);
    // With the token it goes through.
    let res = app
        .send(post(
            format!("/ui/connect/{id}/approve"),
            format!("csrf={}", page.csrf()),
            Some(cookie),
        ))
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

#[tokio::test]
async fn hostile_labels_and_hosts_are_escaped() {
    let app = app().await;
    let cookie = app.login().await;
    let csrf = app.get("/ui/tokens", Some(&cookie)).await.csrf();
    let label = "%3Cscript%3Ealert(1)%3C%2Fscript%3E";
    let issued = app
        .post_form(
            "/ui/tokens/connect",
            Some(&cookie),
            &format!("csrf={csrf}&label={label}"),
        )
        .await;
    let tokens = app.get("/ui/tokens", Some(&cookie)).await;
    // The normal path sanitises the host before it is stored, so the card shows that form.
    let (_, user_code) = pending_device(&app, XSS).await;
    let card = app
        .post_form(
            "/ui/connect/lookup",
            Some(&cookie),
            &format!("csrf={csrf}&user_code={user_code}"),
        )
        .await;
    assert!(
        card.text().contains("-script-alert-1---script-"),
        "{}",
        card.text()
    );

    // A host that reached the store unsanitised must still render escaped.
    let raw = axle::store::NewEnrollment {
        id: axle::domain::EnrollmentId::new(),
        kind: axle::domain::EnrollmentKind::Device,
        secret_hash: &axle::service::auth::hash("axld_raw-host"),
        user_code_hash: Some(&axle::service::auth::hash("ABCD2345")),
        label: XSS,
        host: XSS,
        requester_ip: Some("10.0.0.9"),
        now: app.state.clock.now(),
        expires_at: app.state.clock.now() + time::Duration::minutes(10),
    };
    app.state.store.insert_enrollment(&raw).await.unwrap();
    let raw_card = app
        .post_form(
            "/ui/connect/lookup",
            Some(&cookie),
            &format!("csrf={csrf}&user_code=ABCD-2345"),
        )
        .await;
    assert_eq!(raw_card.status, StatusCode::OK, "{}", raw_card.body);
    assert!(
        raw_card.body.contains("&#60;script&#62;alert(1)"),
        "{}",
        raw_card.body
    );
    assert!(!raw_card.body.contains(XSS));
    assert!(!raw_card.body.contains("<script>alert"));
    let list = app.get("/ui/tokens", Some(&cookie)).await;
    assert!(list.body.contains("&#60;script&#62;alert(1)"));
    assert!(!list.body.contains("<script>alert"));
    for page in [&issued, &tokens] {
        assert!(!page.body.contains("<script>alert"), "{}", page.body);
        assert!(!page.body.contains(XSS));
    }
    assert!(
        issued.body.contains("&#60;script&#62;alert(1)"),
        "{}",
        issued.body
    );
    assert!(tokens.body.contains("&#60;script&#62;alert(1)"));
}
