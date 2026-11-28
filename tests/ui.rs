//! Web UI (spec §12, §7): login, CSRF, overview, board and the live stream, against
//! in-memory SQLite with oneshot requests and HTML assertions.

use std::sync::Arc;
use std::time::Duration;

use axle::clock::TestClock;
use axle::config::{ServerConfig, UiAuth};
use axle::domain::{Actor, Address, Importance, SessionId, Task, TaskId, TaskStatus, WaitingOn};
use axle::http::{AppState, router};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::messages::NewMessage;
use axle::service::tasks::{BlockInput, Caller, ClaimOutcome, Completion, NewTask};
use axle::store::sqlite::SqliteStore;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use scraper::{Html, Selector};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use time::macros::datetime;
use tower::ServiceExt;

const SECRET: &str = "test-secret-0123456789";
const INFRA: &str = "github.com/acme/platform-infra";

struct App {
    pool: sqlx::SqlitePool,
    state: AppState,
    clock: Arc<TestClock>,
    router: Router,
}

async fn app_with(configure: impl FnOnce(&mut ServerConfig)) -> App {
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
    let mut config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some(SECRET.into()),
        _ => None,
    })
    .unwrap();
    configure(&mut config);
    let state = AppState::new(store, clock.clone(), config);
    App {
        pool,
        router: router(state.clone()),
        state,
        clock,
    }
}

async fn app() -> App {
    app_with(|_| {}).await
}

struct Page {
    status: StatusCode,
    location: Option<String>,
    set_cookie: Option<String>,
    body: String,
}

impl Page {
    fn html(&self) -> Html {
        Html::parse_document(&self.body)
    }

    /// Every text node, joined by spaces.
    fn text(&self) -> String {
        self.html()
            .root_element()
            .text()
            .collect::<Vec<_>>()
            .join(" ")
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
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Page {
            status,
            location,
            set_cookie,
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

    /// Logs in with the admin secret; returns the `name=value` cookie pair.
    async fn login(&self) -> String {
        let page = self
            .post_form("/ui/login", None, &format!("secret={SECRET}"))
            .await;
        assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
        assert_eq!(page.location.as_deref(), Some("/ui/"));
        cookie_pair(page.set_cookie.as_deref().expect("session cookie"))
    }

    /// An agent session in `project` that heartbeats at the current clock.
    async fn agent(&self, project: &str, host: &str, harness: &str) -> Resolved {
        let (token, _) = self.state.auth.create("test").await.unwrap();
        IdentityService::resolve(
            &*self.state.store,
            &*self.state.clock,
            IdentityInput {
                project_key: Some(project.into()),
                host: host.into(),
                harness: harness.into(),
                session: SessionId::new(),
                token,
            },
        )
        .await
        .unwrap()
    }

    async fn task(&self, by: &Resolved, title: &str, parent: Option<TaskId>) -> Task {
        self.state
            .tasks
            .create(
                Caller::Agent(by),
                NewTask {
                    project_id: by.agent.project_id.unwrap(),
                    parent_id: parent,
                    title: title.into(),
                    body: "body".into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap()
    }

    async fn claim(&self, by: &Resolved, id: TaskId) {
        let outcome = self.state.tasks.claim(by, id).await.unwrap();
        assert!(matches!(outcome, ClaimOutcome::Claimed(_)), "{outcome:?}");
    }

    async fn complete(&self, by: &Resolved, id: TaskId) {
        self.claim(by, id).await;
        self.state
            .tasks
            .complete(
                by,
                id,
                Completion {
                    summary: "done".into(),
                    ..Completion::default()
                },
            )
            .await
            .unwrap();
    }
}

/// `axl_ui=abc` from a `Set-Cookie` value.
fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().trim().to_string()
}

fn csrf_of(page: &Page) -> String {
    let sel = Selector::parse("meta[name=csrf-token]").unwrap();
    page.html()
        .select(&sel)
        .next()
        .expect("csrf meta tag")
        .value()
        .attr("content")
        .unwrap()
        .to_string()
}

fn select_texts(page: &Page, css: &str) -> Vec<String> {
    let sel = Selector::parse(css).unwrap();
    page.html()
        .select(&sel)
        .map(|e| e.text().collect::<String>().trim().to_string())
        .collect()
}

#[tokio::test]
async fn ui_requires_login() {
    let app = app().await;
    for path in ["/ui/", "/ui/board", "/ui/live", "/ui/overview/panel"] {
        let page = app.get(path, None).await;
        assert_eq!(page.status, StatusCode::SEE_OTHER, "{path}");
        assert_eq!(page.location.as_deref(), Some("/ui/login"), "{path}");
    }
    // A bogus cookie is no better than none.
    let page = app.get("/ui/", Some("axl_ui=forged")).await;
    assert_eq!(page.location.as_deref(), Some("/ui/login"));

    let login = app.get("/ui/login", None).await;
    assert_eq!(login.status, StatusCode::OK);
    assert_eq!(select_texts(&login, "input[name=secret]").len(), 1);

    let wrong = app.post_form("/ui/login", None, "secret=nope").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.set_cookie.is_none());
    assert!(
        wrong.text().contains("Wrong admin secret"),
        "{}",
        wrong.body
    );

    let page = app
        .post_form("/ui/login", None, &format!("secret={SECRET}"))
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    let set_cookie = page.set_cookie.unwrap();
    assert!(set_cookie.starts_with("axl_ui="), "{set_cookie}");
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Strict"), "{set_cookie}");
    assert!(set_cookie.contains("Path=/ui"), "{set_cookie}");
    assert!(!set_cookie.contains("Secure"), "{set_cookie}");
    let cookie = cookie_pair(&set_cookie);
    assert_eq!(app.get("/ui/", Some(&cookie)).await.status, StatusCode::OK);
    assert_eq!(
        app.get("/ui/board", Some(&cookie)).await.status,
        StatusCode::OK
    );
    // `/ui` without the slash goes to the overview.
    let bare = app.get("/ui", None).await;
    assert_eq!(bare.location.as_deref(), Some("/ui/"));

    // Sessions expire after 12 hours.
    app.clock
        .advance(time::Duration::hours(12) + time::Duration::seconds(1));
    let page = app.get("/ui/", Some(&cookie)).await;
    assert_eq!(page.location.as_deref(), Some("/ui/login"));
}

#[tokio::test]
async fn secure_cookie_when_configured() {
    let app = app_with(|c| c.cookie_secure = true).await;
    let page = app
        .post_form("/ui/login", None, &format!("secret={SECRET}"))
        .await;
    assert!(page.set_cookie.unwrap().contains("Secure"));
}

#[tokio::test]
async fn post_without_csrf_rejected() {
    let app = app().await;
    let cookie = app.login().await;
    let overview = app.get("/ui/", Some(&cookie)).await;
    let csrf = csrf_of(&overview);
    assert!(csrf.len() >= 32, "{csrf}");

    for form in ["", "csrf=", "csrf=wrong"] {
        let page = app.post_form("/ui/logout", Some(&cookie), form).await;
        assert_eq!(page.status, StatusCode::FORBIDDEN, "{form:?}");
    }
    // Still signed in.
    assert_eq!(app.get("/ui/", Some(&cookie)).await.status, StatusCode::OK);

    // Another session's token does not work either.
    let other = app.login().await;
    let other_csrf = csrf_of(&app.get("/ui/", Some(&other)).await);
    assert_ne!(other_csrf, csrf);
    let page = app
        .post_form("/ui/logout", Some(&cookie), &format!("csrf={other_csrf}"))
        .await;
    assert_eq!(page.status, StatusCode::FORBIDDEN);

    // The header form is accepted too (what htmx requests send).
    let page = app
        .send(
            Request::post("/ui/logout")
                .header(header::COOKIE, &other)
                .header("x-csrf-token", &other_csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);

    let page = app
        .post_form("/ui/logout", Some(&cookie), &format!("csrf={csrf}"))
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.location.as_deref(), Some("/ui/login"));
    assert!(page.set_cookie.unwrap().contains("Max-Age=0"));
    // The server forgot the session.
    let page = app.get("/ui/", Some(&cookie)).await;
    assert_eq!(page.location.as_deref(), Some("/ui/login"));
}

#[tokio::test]
async fn login_attempts_are_rate_limited() {
    let app = app().await;
    for _ in 0..10 {
        let page = app.post_form("/ui/login", None, "secret=nope").await;
        assert_eq!(page.status, StatusCode::UNAUTHORIZED);
    }
    let page = app
        .post_form("/ui/login", None, &format!("secret={SECRET}"))
        .await;
    assert_eq!(page.status, StatusCode::TOO_MANY_REQUESTS);
    app.clock.advance(time::Duration::seconds(6));
    let page = app
        .post_form("/ui/login", None, &format!("secret={SECRET}"))
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn trusted_header_signs_in_without_secret() {
    let app = app_with(|c| {
        c.ui_auth = UiAuth::TrustedHeader {
            header: "x-forwarded-user".into(),
            secret: Some(SECRET.into()),
        }
    })
    .await;
    assert_eq!(
        app.get("/ui/", None).await.location.as_deref(),
        Some("/ui/login")
    );
    let page = app
        .send(
            Request::get("/ui/")
                .header("x-forwarded-user", "owner")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    // A session is set up so CSRF works as usual.
    let cookie = cookie_pair(page.set_cookie.as_deref().expect("session cookie"));
    let csrf = csrf_of(&page);
    let page = app
        .send(
            Request::post("/ui/logout")
                .header("x-forwarded-user", "owner")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("csrf=bad"))
                .unwrap(),
        )
        .await;
    assert_eq!(page.status, StatusCode::FORBIDDEN);
    let page = app
        .send(
            Request::post("/ui/logout")
                .header("x-forwarded-user", "owner")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf={csrf}")))
                .unwrap(),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
}

/// An app whose UI has no sign-in (`AXL_UI_AUTH=none`), resolved from the environment.
async fn open_app(vars: &'static [(&'static str, &'static str)]) -> App {
    app_with(|c| {
        *c = ServerConfig::from_lookup(|k| {
            if k == "DATABASE_URL" {
                return Some("sqlite::memory:".into());
            }
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        })
        .unwrap();
        assert_eq!(c.ui_auth, UiAuth::None);
    })
    .await
}

fn get_host(path: &str, host: &str, cookie: Option<&str>) -> Request<Body> {
    let mut req = Request::get(path).header(header::HOST, host);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    req.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn no_auth_mode_serves_the_board_without_login_and_keeps_csrf() {
    // Nothing configured on the default loopback bind: no sign-in.
    let app = open_app(&[]).await;
    let page = app
        .send(get_host("/ui/board", "127.0.0.1:7420", None))
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    // An anonymous session carries the CSRF token; there is no log-out button.
    let set_cookie = page.set_cookie.clone().expect("anonymous session cookie");
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
    let cookie = cookie_pair(&set_cookie);
    let csrf = csrf_of(&page);
    assert!(
        select_texts(&page, "form.logout").is_empty(),
        "{}",
        page.body
    );
    // The session is reused, not replaced.
    let again = app
        .send(get_host("/ui/", "localhost:7420", Some(&cookie)))
        .await;
    assert_eq!(again.status, StatusCode::OK);
    assert!(again.set_cookie.is_none());
    assert_eq!(csrf_of(&again), csrf);

    // The login page is gone.
    for page in [
        app.send(get_host("/ui/login", "localhost", None)).await,
        app.send(
            Request::post("/ui/login")
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("secret=x"))
                .unwrap(),
        )
        .await,
    ] {
        assert_eq!(page.status, StatusCode::SEE_OTHER);
        assert_eq!(page.location.as_deref(), Some("/ui/"));
    }

    // POSTs still need the session's CSRF token: none, a wrong one, or no cookie at all.
    let post = |cookie: Option<&str>, form: &str| {
        let mut req = Request::post("/ui/messages")
            .header(header::HOST, "127.0.0.1:7420")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        req.body(Body::from(form.to_string())).unwrap()
    };
    for (cookie, form) in [
        (Some(cookie.as_str()), "to=x&body=hi".to_string()),
        (Some(cookie.as_str()), "csrf=wrong&to=x&body=hi".to_string()),
        (None, format!("csrf={csrf}&to=x&body=hi")),
    ] {
        let page = app.send(post(cookie, &form)).await;
        assert_eq!(page.status, StatusCode::FORBIDDEN, "{cookie:?} {form}");
    }
    // With the token the action runs (whatever it makes of the form, it is not a 403).
    let page = app
        .send(post(Some(&cookie), &format!("csrf={csrf}&to=x&body=hi")))
        .await;
    assert_ne!(page.status, StatusCode::FORBIDDEN, "{}", page.body);

    // The strict CSP stays.
    let res = app
        .router
        .clone()
        .oneshot(get_host("/ui/", "localhost", None))
        .await
        .unwrap();
    let csp = res.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"));
}

#[tokio::test]
async fn no_auth_mode_rejects_unlisted_hosts() {
    let app = open_app(&[
        ("AXL_UI_AUTH", "none"),
        ("AXL_BIND", "0.0.0.0:7420"),
        ("AXL_ALLOWED_HOSTS", "axle.internal"),
    ])
    .await;
    for path in [
        "/ui/",
        "/ui/board",
        "/ui/live",
        "/ui/login",
        "/ui/static/app.css",
    ] {
        for host in ["evil.example", "evil.example:7420", "0.0.0.0:7420"] {
            let page = app.send(get_host(path, host, None)).await;
            assert_eq!(
                page.status,
                StatusCode::MISDIRECTED_REQUEST,
                "{path} {host}"
            );
            assert!(page.set_cookie.is_none(), "no session for {host}");
        }
    }
    // No Host header at all is rejected too.
    let page = app.get("/ui/", None).await;
    assert_eq!(page.status, StatusCode::MISDIRECTED_REQUEST);
    for host in [
        "axle.internal",
        "axle.internal:443",
        "[::1]:7420",
        "localhost",
    ] {
        let page = app.send(get_host("/ui/board", host, None)).await;
        assert_eq!(page.status, StatusCode::OK, "{host}");
    }
}

#[tokio::test]
async fn secret_mode_still_requires_login_for_any_host() {
    let app = app().await;
    assert_eq!(app.state.config.ui_auth, UiAuth::Secret(SECRET.into()));
    // The host allowlist only applies without sign-in; the login wall stays.
    let page = app
        .send(get_host("/ui/board", "axle.example.com", None))
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.location.as_deref(), Some("/ui/login"));
    let cookie = app.login().await;
    let page = app
        .send(get_host("/ui/board", "axle.example.com", Some(&cookie)))
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(select_texts(&page, "form.logout button"), ["Log out"]);
}

#[tokio::test]
async fn board_columns_and_progress() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let codex = app.agent(INFRA, "build-01", "codex").await;
    let parent = app.task(&infra, "Rotate DB creds", None).await;
    let a = app
        .task(&infra, "update Pulumi secret", Some(parent.id))
        .await;
    let b = app.task(&infra, "roll CNPG users", Some(parent.id)).await;
    let _c = app
        .task(&infra, "update ExternalSecret", Some(parent.id))
        .await;
    app.complete(&codex, a.id).await;
    app.claim(&infra, b.id).await;
    app.claim(&infra, parent.id).await;

    let page = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        select_texts(&page, ".column > header h2"),
        ["Open", "Claimed", "Blocked", "Review", "Done"]
    );
    assert_eq!(
        select_texts(&page, ".card .ratio-text"),
        ["1 of 3 subtasks done"]
    );
    assert!(
        select_texts(&page, ".card .badge-blocked").is_empty(),
        "no badge without a blocked child"
    );
    // Children are not cards of their own; the parent sits in Claimed with its owner.
    let claimed = select_texts(&page, ".column[data-status=claimed] .card-title");
    assert_eq!(claimed, ["Rotate DB creds"]);
    assert!(
        select_texts(&page, ".column[data-status=done] .card").is_empty(),
        "a done child is not a top-level card"
    );
    let owners = select_texts(&page, ".card .owners");
    assert!(
        owners.iter().any(|o| o.contains("claude-code@mac")),
        "{owners:?}"
    );

    let children = app
        .get(&format!("/ui/tasks/{}/children", parent.id), Some(&cookie))
        .await;
    assert_eq!(children.status, StatusCode::OK);
    let rows = select_texts(&children, ".child");
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows[0].contains("update Pulumi secret") && rows[0].contains("codex@build-01"));
    assert!(rows[1].contains("roll CNPG users") && rows[1].contains("claude-code@mac"));
    assert!(rows[2].ends_with("open"), "{rows:?}");
    assert_eq!(
        select_texts(&children, ".child-owner")[2],
        "open",
        "an unclaimed subtask names its state once"
    );

    // A blocked child shows on the parent card without expanding it.
    app.state
        .tasks
        .block(&infra, b.id, "waiting")
        .await
        .unwrap();
    let page = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        select_texts(&page, ".column[data-status=claimed] .card .badge-blocked"),
        ["1 blocked"]
    );
    // The parent's children are stations on its line, in order, each naming its state.
    assert_eq!(
        select_texts(&page, ".card .stations .station"),
        ["done", "blocked", "open"]
    );
    // The blocked child also has a card of its own in Blocked, naming its parent, so the
    // board and the top bar's pulse count the same tasks.
    assert_eq!(
        select_texts(&page, ".column[data-status=blocked] .card-title"),
        ["roll CNPG users"]
    );
    assert_eq!(
        select_texts(&page, ".column[data-status=blocked] .card-parent"),
        ["Subtask of Rotate DB creds"]
    );
    assert_eq!(select_texts(&page, "#pulse .pulse-item")[1], "1 blocked");
    // The top bar's pulse fragment counts the blocked child.
    let pulse = app.get("/ui/pulse", Some(&cookie)).await;
    assert_eq!(pulse.status, StatusCode::OK);
    assert!(!pulse.body.contains("<html"));
    assert_eq!(
        select_texts(&pulse, ".pulse-item"),
        ["2 live", "1 blocked", "0 need you"]
    );

    let missing = app
        .get(
            &format!("/ui/tasks/{}/children", SessionId::new()),
            Some(&cookie),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

/// The `data-lease-*` attributes and text of the first lease meter on a page.
fn lease_meter(page: &Page) -> (i64, i64, String, String) {
    let html = page.html();
    let sel = Selector::parse(".lease-meter").unwrap();
    let el = html.select(&sel).next().expect("a lease meter");
    let attr = |name: &str| el.value().attr(name).unwrap().to_string();
    (
        attr("data-lease-left").parse().unwrap(),
        attr("data-lease-ttl").parse().unwrap(),
        attr("data-lease-state"),
        el.text().collect::<String>().trim().to_string(),
    )
}

#[tokio::test]
async fn claimed_card_draws_its_lease() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = app.task(&infra, "Hold me", None).await;
    let open = app.get("/ui/board", Some(&cookie)).await;
    assert!(
        select_texts(&open, ".lease-meter").is_empty(),
        "no lease while open"
    );

    app.claim(&infra, task.id).await;
    let page = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        lease_meter(&page),
        (900, 900, "ok".into(), "lease 15m left".into())
    );
    assert_eq!(select_texts(&page, ".card .presence"), ["live"]);
    let detail = app
        .get(&format!("/ui/tasks/{}", task.id), Some(&cookie))
        .await;
    assert_eq!(lease_meter(&detail).0, 900);

    // Under a fifth of the lease left reads as caution; past expiry, as expired.
    app.clock.advance(time::Duration::minutes(13));
    let page = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        lease_meter(&page),
        (120, 900, "low".into(), "lease 2m left".into())
    );
    assert_eq!(
        select_texts(&page, ".card .presence"),
        ["stale"],
        "no heartbeat for 13 minutes"
    );
    app.clock.advance(time::Duration::minutes(5));
    let page = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        lease_meter(&page),
        (0, 900, "expired".into(), "lease expired".into())
    );
}

#[tokio::test]
async fn board_done_column_shows_unseen_newest_first() {
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let old = app.task(&infra, "old", None).await;
    app.complete(&infra, old.id).await;
    app.clock.advance(time::Duration::days(8));
    let parent = app.task(&infra, "parent", None).await;
    let child = app.task(&infra, "child", Some(parent.id)).await;
    let first = app.task(&infra, "first", None).await;
    app.complete(&infra, first.id).await;
    app.clock.advance(time::Duration::minutes(1));
    let second = app.task(&infra, "second", None).await;
    app.complete(&infra, second.id).await;
    app.complete(&infra, child.id).await;
    let cancelled = app.task(&infra, "dropped", None).await;
    app.state
        .tasks
        .cancel(Caller::Human, cancelled.id)
        .await
        .unwrap();

    let cookie = app.login().await;
    let page = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        select_texts(&page, ".column[data-status=done] .card-title"),
        ["second", "first"]
    );
    assert!(
        select_texts(&page, ".column[data-status=done] header")
            .join(" ")
            .contains("2 unseen")
    );
    assert_eq!(
        select_texts(&page, ".column[data-status=done] .auto-acked a"),
        ["1 auto-acked, unseen \u{2192}"]
    );
    assert!(page.body.contains("href=\"/ui/history?unseen=auto\""));
    assert!(page.body.contains("href=\"/ui/history\""));
    // Each per-card Ack button is named for its card.
    assert!(page.body.contains("aria-label=\"Ack second\""));
    assert!(page.body.contains("aria-label=\"Ack first\""));
    assert!(!page.text().contains("dropped"));
    assert!(!page.text().contains("7 days"));
}

#[tokio::test]
async fn auto_acked_line_hidden_at_zero() {
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let t = app.task(&infra, "recent", None).await;
    app.complete(&infra, t.id).await;
    let cookie = app.login().await;
    let page = app.get("/ui/board", Some(&cookie)).await;
    assert!(select_texts(&page, ".column[data-status=done] .auto-acked").is_empty());
}

#[tokio::test]
async fn auto_acked_link_carries_the_project_filter() {
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let pid = infra.agent.project_id.unwrap();
    let old = app.task(&infra, "old", None).await;
    app.complete(&infra, old.id).await;
    app.clock.advance(time::Duration::days(8));
    let cookie = app.login().await;
    let page = app
        .get(&format!("/ui/board?project={pid}"), Some(&cookie))
        .await;
    assert!(
        page.body.contains(&format!(
            "href=\"/ui/history?unseen=auto&#38;project={pid}\""
        )),
        "{}",
        page.body
    );
    // History keeps the filter too.
    assert!(
        page.body
            .contains(&format!("href=\"/ui/history?project={pid}\"")),
        "{}",
        page.body
    );
    // The fragment the board re-fetches renders the same head and Ack forms.
    let frag = app
        .get(&format!("/ui/board/columns?project={pid}"), Some(&cookie))
        .await;
    assert!(
        frag.body.contains(&format!("project={pid}\"")),
        "{}",
        frag.body
    );
}

async fn board_with_done(app: &App, titles: &[&str]) -> (Vec<Task>, String, String) {
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let mut tasks = Vec::new();
    for t in titles {
        let task = app.task(&infra, t, None).await;
        app.complete(&infra, task.id).await;
        tasks.push(task);
    }
    let cookie = app.login().await;
    let csrf = csrf_for(app, &cookie).await;
    (tasks, cookie, csrf)
}

#[tokio::test]
async fn ack_removes_card_and_redirects_back() {
    let app = app().await;
    let (tasks, cookie, csrf) = board_with_done(&app, &["one"]).await;
    let t = &tasks[0];
    let pid = t.project_id;
    let page = app
        .post_form(
            &format!("/ui/tasks/{}/ack", t.id),
            Some(&cookie),
            &format!("csrf={csrf}&back=%2Fui%2Fboard%3Fproject%3D{pid}"),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(
        page.location.as_deref(),
        Some(&*format!("/ui/board?project={pid}"))
    );
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert!(select_texts(&board, ".column[data-status=done] .card-title").is_empty());
    assert_eq!(
        app.state
            .store
            .get_task(t.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Done
    );
}

#[tokio::test]
async fn ack_forms_sit_outside_the_card() {
    let app = app().await;
    let (_, cookie, _) = board_with_done(&app, &["one"]).await;
    let board = app.get("/ui/board", Some(&cookie)).await;
    let col = ".column[data-status=done]";
    assert_eq!(
        select_texts(&board, &format!("{col} form.ack button")),
        ["Ack"]
    );
    assert!(select_texts(&board, &format!("{col} .card form")).is_empty());
    assert_eq!(
        select_texts(&board, &format!("{col} form.ack-all button")),
        ["Ack all"]
    );
}

#[tokio::test]
async fn ack_all_says_how_many_it_acks_when_unseen_exceeds_the_cards() {
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    for i in 0..=50 {
        let t = app.task(&infra, &format!("t{i}"), None).await;
        app.complete(&infra, t.id).await;
    }
    let cookie = app.login().await;
    let board = app.get("/ui/board", Some(&cookie)).await;
    let col = ".column[data-status=done]";
    assert_eq!(
        select_texts(&board, &format!("{col} form.ack-all button")),
        ["Ack 50 shown"]
    );
    assert_eq!(
        select_texts(&board, &format!("{col} .count")),
        ["51 unseen"]
    );
}

#[tokio::test]
async fn ack_all_acks_only_the_cards_shown() {
    let app = app().await;
    let (_, cookie, csrf) = board_with_done(&app, &["a", "b"]).await;
    let board = app.get("/ui/board", Some(&cookie)).await;
    let sel = Selector::parse(".column[data-status=done] form.ack-all input[name=id]").unwrap();
    let ids: Vec<String> = board
        .html()
        .select(&sel)
        .map(|e| e.value().attr("value").unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 2);
    // "c" finishes between the render and the click.
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let c = app.task(&infra, "c", None).await;
    app.complete(&infra, c.id).await;
    let mut fields = vec![("csrf", csrf.as_str()), ("back", "/ui/board")];
    fields.extend(ids.iter().map(|i| ("id", i.as_str())));
    let page = app
        .post_form(
            "/ui/done/ack",
            Some(&cookie),
            &serde_urlencoded::to_string(&fields).unwrap(),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.location.as_deref(), Some("/ui/board"));
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        select_texts(&board, ".column[data-status=done] .card-title"),
        ["c"]
    );
}

#[tokio::test]
async fn ack_back_never_leaves_the_ui() {
    let app = app().await;
    let (tasks, cookie, csrf) = board_with_done(&app, &["one"]).await;
    let path = format!("/ui/tasks/{}/ack", tasks[0].id);
    for back in [
        "//evil.example",
        "/\\evil.example",
        "https://evil.example",
        "/ui/tokens",
        "/ui/boardx",
        "/ui/board?x=\n",
        "/ui/board?q=caf\u{e9}",
        "",
    ] {
        let page = app
            .post_form(&path, Some(&cookie), &form(&csrf, &[("back", back)]))
            .await;
        assert_eq!(page.status, StatusCode::SEE_OTHER, "{back}");
        assert_eq!(page.location.as_deref(), Some("/ui/board"), "{back}");
    }
    for back in [
        "/ui/history?unseen=auto",
        "/ui/history",
        "/ui/board?project=x",
    ] {
        let page = app
            .post_form(&path, Some(&cookie), &form(&csrf, &[("back", back)]))
            .await;
        assert_eq!(page.location.as_deref(), Some(back));
    }
}

#[tokio::test]
async fn ack_rejects_bad_ids_and_missing_csrf() {
    let app = app().await;
    let (tasks, cookie, csrf) = board_with_done(&app, &["one"]).await;
    let id = tasks[0].id.to_string();
    let acked = |app: &App| {
        let store = app.state.store.clone();
        let tid = tasks[0].id;
        async move {
            store
                .get_task(tid)
                .await
                .unwrap()
                .unwrap()
                .acked_at
                .is_some()
        }
    };
    let bad = app
        .post_form(
            "/ui/done/ack",
            Some(&cookie),
            &form(&csrf, &[("id", &id), ("id", "garbage")]),
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!acked(&app).await);

    let single = format!("/ui/tasks/{id}/ack");
    for (path, body) in [
        ("/ui/done/ack", format!("id={id}")),
        ("/ui/done/ack", format!("csrf=wrong&id={id}")),
        (single.as_str(), String::new()),
        (single.as_str(), "csrf=wrong".to_string()),
    ] {
        let page = app.post_form(path, Some(&cookie), &body).await;
        assert_eq!(page.status, StatusCode::FORBIDDEN, "{path} {body}");
    }
    assert!(!acked(&app).await);

    let unknown = format!("/ui/tasks/{}/ack", TaskId::new());
    let page = app
        .post_form(&unknown, Some(&cookie), &form(&csrf, &[]))
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    let page = app
        .post_form("/ui/tasks/garbage/ack", Some(&cookie), &form(&csrf, &[]))
        .await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);
    let empty = app
        .post_form("/ui/done/ack", Some(&cookie), &form(&csrf, &[]))
        .await;
    assert_eq!(empty.status, StatusCode::SEE_OTHER);
    assert!(!acked(&app).await);
}

#[tokio::test]
async fn ack_updates_the_board_live() {
    let app = app().await;
    let (tasks, cookie, csrf) = board_with_done(&app, &["one"]).await;
    let mut events = app.state.bus.subscribe();
    let page = app
        .post_form(
            &format!("/ui/tasks/{}/ack", tasks[0].id),
            Some(&cookie),
            &form(&csrf, &[]),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("a live event")
        .unwrap();
    let frames = axle::ui::views::live_frames(&app.state, &event).await;
    assert!(frames.iter().any(|(name, _)| name == "board"), "{frames:?}");
}

#[tokio::test]
async fn board_filters_by_project() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let web_app = app
        .agent("github.com/acme/web-app", "mac", "claude-code")
        .await;
    app.task(&infra, "infra work", None).await;
    app.task(&web_app, "web_app work", None).await;

    let all = app.get("/ui/board", Some(&cookie)).await;
    let titles = select_texts(&all, ".card-title");
    assert_eq!(titles.len(), 2, "{titles:?}");

    for filter in [
        INFRA.to_string(),
        infra.agent.project_id.unwrap().to_string(),
    ] {
        let page = app
            .get(&format!("/ui/board?project={filter}"), Some(&cookie))
            .await;
        assert_eq!(page.status, StatusCode::OK);
        assert_eq!(
            select_texts(&page, ".card-title"),
            ["infra work"],
            "{filter}"
        );
    }
    let unknown = app
        .get("/ui/board?project=github.com/nobody/nothing", Some(&cookie))
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn display_name_not_key() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let parent = app.task(&infra, "Rotate DB creds", None).await;
    let child = app
        .task(&infra, "update Pulumi secret", Some(parent.id))
        .await;
    app.claim(&infra, child.id).await;

    for path in [
        "/ui/board".to_string(),
        format!("/ui/board?project={INFRA}"),
        "/ui/".to_string(),
        format!("/ui/tasks/{}/children", parent.id),
    ] {
        let page = app.get(&path, Some(&cookie)).await;
        assert_eq!(page.status, StatusCode::OK, "{path}");
        assert!(
            page.text().contains("acme/platform-infra"),
            "{path}: {}",
            page.body
        );
        // The key may only appear in `title` attributes (shown on hover).
        let html = page.html();
        let mut keyed_titles = 0;
        for node in html.root_element().descendants() {
            if let Some(text) = node.value().as_text() {
                assert!(!text.contains(INFRA), "{path}: text {:?}", &**text);
            }
            if let Some(el) = node.value().as_element() {
                for (name, value) in el.attrs() {
                    if name == "title" {
                        keyed_titles += usize::from(value.contains(INFRA));
                    } else {
                        assert!(!value.contains(INFRA), "{path}: {name}={value:?}");
                    }
                }
            }
        }
        assert!(keyed_titles > 0, "{path}: the key is on hover");
    }
}

#[tokio::test]
async fn peer_content_is_escaped() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let parent = app
        .task(&infra, "<script>alert(1)</script><b>x</b>", None)
        .await;
    app.task(&infra, "<img src=x onerror=alert(2)>", Some(parent.id))
        .await;
    app.claim(&infra, parent.id).await;
    for path in [
        "/ui/board".to_string(),
        "/ui/".to_string(),
        format!("/ui/tasks/{}/children", parent.id),
    ] {
        let page = app.get(&path, Some(&cookie)).await;
        assert!(!page.body.contains("<script>alert"), "{path}");
        assert!(!page.body.contains("<b>x"), "{path}");
        assert!(!page.body.contains("<img src=x"), "{path}");
    }
    // Rendered as text, exactly.
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        select_texts(&board, ".card-title"),
        ["<script>alert(1)</script><b>x</b>"]
    );
    let children = app
        .get(&format!("/ui/tasks/{}/children", parent.id), Some(&cookie))
        .await;
    assert!(select_texts(&children, ".child-title")[0].ends_with("<img src=x onerror=alert(2)>"));
}

#[tokio::test]
async fn overview_counters_and_agent_cards() {
    let app = app().await;
    let cookie = app.login().await;
    let _stale = app.agent(INFRA, "old-box", "codex").await;
    app.clock.advance(time::Duration::seconds(120));
    let mac = app.agent(INFRA, "mac", "claude-code").await;
    let _mac2 = app
        .agent("github.com/acme/web-app", "mac", "opencode")
        .await;
    let busy = app.task(&mac, "Busy with this", None).await;
    app.claim(&mac, busy.id).await;
    app.task(&mac, "Waiting 1", None).await;
    app.task(&mac, "Waiting 2", None).await;
    for importance in [Importance::Normal, Importance::Urgent] {
        app.state
            .messages
            .send(
                &Actor::Agent(mac.agent.id, mac.session.id),
                NewMessage {
                    to: Address::Human,
                    subject: Some("hi".into()),
                    body: "ping".into(),
                    importance,
                    reply_to: None,
                },
            )
            .await
            .unwrap();
    }

    let page = app.get("/ui/", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    let counter = |name: &str| select_texts(&page, &format!("[data-counter={name}] .value"));
    assert_eq!(counter("hosts"), ["1"]);
    assert_eq!(counter("sessions"), ["2"]);
    assert_eq!(counter("open"), ["2"]);
    assert_eq!(counter("unread"), ["2"]);
    assert_eq!(counter("urgent"), ["1"]);

    let cards = select_texts(&page, ".agent");
    assert_eq!(cards.len(), 3, "{cards:?}");
    let mac_card = cards
        .iter()
        .find(|c| c.contains("claude-code") && c.contains("mac"))
        .unwrap();
    assert!(mac_card.contains("Busy with this"), "{mac_card}");
    assert!(mac_card.contains("acme/platform-infra"), "{mac_card}");

    // The panel fragment is what live updates fetch.
    let panel = app.get("/ui/overview/panel", Some(&cookie)).await;
    assert_eq!(panel.status, StatusCode::OK);
    assert!(!panel.body.contains("<html"));
    assert_eq!(
        select_texts(&panel, "[data-counter=sessions] .value"),
        ["2"]
    );
}

/// Reads the stream until an event named `name` with data arrives within `wait`; returns
/// its data.
async fn wait_for_event(
    stream: &mut (impl futures_util::Stream<Item = String> + Unpin),
    name: &str,
    wait: Duration,
) -> Option<String> {
    let mut buf = String::new();
    tokio::time::timeout(wait, async {
        while let Some(chunk) = stream.next().await {
            buf.push_str(&chunk);
            while let Some(end) = buf.find("\n\n") {
                let frame: String = buf.drain(..end + 2).collect();
                let mut event = None;
                let mut data = Vec::new();
                for line in frame.lines() {
                    if let Some(n) = line.strip_prefix("event: ") {
                        event = Some(n.to_string());
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data.push(d.to_string());
                    } else if line == "data:" {
                        data.push(String::new());
                    }
                }
                // Like a browser, ignore events without data.
                if event.as_deref() == Some(name) && !data.is_empty() {
                    return Some(data.join("\n"));
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

#[tokio::test]
async fn live_stream_rerenders_changed_cards() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let parent = app.task(&infra, "Parent <one>", None).await;

    let res = app
        .router
        .clone()
        .oneshot(
            Request::get("/ui/live")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
    let mut stream = res
        .into_body()
        .into_data_stream()
        .map(|c| String::from_utf8(c.unwrap().to_vec()).unwrap());

    // A child change re-renders the parent's card (its progress) in place.
    app.task(&infra, "child", Some(parent.id)).await;
    let card = wait_for_event(
        &mut stream,
        &format!("task-{}", parent.id),
        Duration::from_secs(2),
    )
    .await
    .expect("parent card event");
    assert!(card.contains("0 of 1"), "{card}");
    assert!(!card.contains("<one>"), "{card}");
    let fragment = Html::parse_fragment(&card);
    let title = Selector::parse(".card-title").unwrap();
    assert_eq!(
        fragment
            .select(&title)
            .next()
            .unwrap()
            .text()
            .collect::<String>(),
        "Parent <one>"
    );
    assert!(
        card.contains(&format!("sse-swap=\"task-{}\"", parent.id)),
        "{card}"
    );
    assert!(
        wait_for_event(&mut stream, "overview", Duration::from_secs(2))
            .await
            .is_some()
    );

    // A top-level status change asks the board to re-fetch its columns.
    app.claim(&infra, parent.id).await;
    assert!(
        wait_for_event(&mut stream, "board", Duration::from_secs(2))
            .await
            .is_some()
    );
    let card = wait_for_event(
        &mut stream,
        &format!("task-{}", parent.id),
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert!(card.contains("data-status=\"claimed\""), "{card}");

    // Messages only refresh the overview.
    app.state
        .messages
        .send(
            &Actor::Human,
            NewMessage {
                to: Address::Agent(infra.agent.id),
                subject: None,
                body: "hi".into(),
                importance: Importance::Normal,
                reply_to: None,
            },
        )
        .await
        .unwrap();
    assert!(
        wait_for_event(&mut stream, "overview", Duration::from_secs(2))
            .await
            .is_some()
    );

    // The stream ends when the server shuts down.
    app.state.shutdown.trigger();
    let ended = tokio::time::timeout(Duration::from_secs(2), async {
        while stream.next().await.is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "stream ended on shutdown");
}

#[tokio::test]
async fn static_assets_are_embedded() {
    let app = app().await;
    for (path, ty) in [
        ("/ui/static/htmx.min.js", "text/javascript"),
        ("/ui/static/htmx-ext-sse.js", "text/javascript"),
        ("/ui/static/app.js", "text/javascript"),
        ("/ui/static/app.css", "text/css"),
    ] {
        let res = app
            .router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert!(
            res.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with(ty),
            "{path}"
        );
        // Revalidation by content hash.
        let etag = res.headers()[header::ETAG].clone();
        let again = app
            .router
            .clone()
            .oneshot(
                Request::get(path)
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED, "{path}");
    }
    let missing = app.get("/ui/static/nope.js", None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

/// Opens `/ui/live` with `cookie`; returns the body as text chunks.
async fn open_live(app: &App, cookie: &str) -> impl futures_util::Stream<Item = String> + Unpin {
    let res = app
        .router
        .clone()
        .oneshot(
            Request::get("/ui/live")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    res.into_body()
        .into_data_stream()
        .map(|c| String::from_utf8(c.unwrap().to_vec()).unwrap())
}

/// Everything the stream sends until it ends (fails if it does not end within `wait`).
async fn drain_until_end(
    stream: &mut (impl futures_util::Stream<Item = String> + Unpin),
    wait: Duration,
) -> String {
    tokio::time::timeout(wait, async {
        let mut all = String::new();
        while let Some(chunk) = stream.next().await {
            all.push_str(&chunk);
        }
        all
    })
    .await
    .expect("the live stream ended")
}

#[tokio::test]
async fn live_stream_ends_on_logout() {
    let app = app().await;
    let cookie = app.login().await;
    let csrf = csrf_of(&app.get("/ui/", Some(&cookie)).await);
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let mut stream = open_live(&app, &cookie).await;

    let page = app
        .post_form("/ui/logout", Some(&cookie), &format!("csrf={csrf}"))
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    app.task(&infra, "secret plans", None).await;
    let sent = drain_until_end(&mut stream, Duration::from_secs(2)).await;
    assert!(!sent.contains("event: task-"), "{sent}");
    assert!(!sent.contains("secret plans"), "{sent}");
}

#[tokio::test]
async fn live_stream_ends_when_the_session_expires() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let mut stream = open_live(&app, &cookie).await;
    app.clock
        .advance(time::Duration::hours(12) + time::Duration::seconds(1));
    app.task(&infra, "after expiry", None).await;
    let sent = drain_until_end(&mut stream, Duration::from_secs(2)).await;
    assert!(!sent.contains("after expiry"), "{sent}");
}

#[test]
fn stylesheet_hides_hidden_elements() {
    // Search hides items with the `hidden` attribute; `display: grid` on cards and agent
    // cards would otherwise override the browser's own `[hidden]` rule.
    let css = include_str!("../static/app.css");
    assert!(
        css.contains("[hidden] { display: none !important; }"),
        "app.css must force [hidden] to display: none"
    );
}

/// WCAG contrast ratio of two `#rrggbb` colours.
fn contrast(a: &str, b: &str) -> f64 {
    let lum = |hex: &str| {
        let c: Vec<f64> = (0..3)
            .map(|i| {
                let v =
                    f64::from(u8::from_str_radix(&hex[1 + 2 * i..3 + 2 * i], 16).unwrap()) / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            })
            .collect();
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    };
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// The value of `--name` in the first (light theme) `:root` block.
fn light_token(css: &str, name: &str) -> String {
    let root = &css[css.find(":root {").unwrap()..];
    let root = &root[..root.find('}').unwrap()];
    let at = root.find(&format!("--{name}:")).unwrap();
    root[at..]
        .split(':')
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn stylesheet_polish() {
    let css = include_str!("../static/app.css");
    // Light-theme status colours are readable small text on every surface.
    for token in ["ok", "warn", "danger", "muted", "accent"] {
        let fg = light_token(css, token);
        for bg in ["bg", "surface", "surface-2"] {
            let ratio = contrast(&fg, &light_token(css, bg));
            assert!(ratio >= 4.5, "--{token} on --{bg}: {ratio:.2}");
        }
    }
    // Five board columns fit a 1280 px window.
    assert!(css.contains("grid-auto-columns: minmax(220px, 1fr);"));
    // The compose form inside the reader is not pushed right.
    assert!(css.contains(".reader .compose { margin-left: 0; }"));
    // No coloured side-tab borders.
    for side in [
        "border-left:",
        "border-right:",
        "border-left-color",
        "border-inline-start",
    ] {
        assert!(!css.contains(side), "{side}");
    }
}

#[test]
fn live_updates_keep_subtasks_open() {
    let js = include_str!("../static/app.js");
    assert!(js.contains("openCards") && js.contains("restoreOpen"));
    assert!(js.contains("htmx:afterSettle") && js.contains("htmx:sseMessage"));
}

// ---------------------------------------------------------------- task 10: task page, inbox, actions

/// The session's CSRF token, read from the overview.
async fn csrf_for(app: &App, cookie: &str) -> String {
    csrf_of(&app.get("/ui/", Some(cookie)).await)
}

/// A form body with `csrf` first, values url-encoded.
fn form(csrf: &str, fields: &[(&str, &str)]) -> String {
    let mut all = vec![("csrf", csrf)];
    all.extend_from_slice(fields);
    serde_urlencoded::to_string(&all).unwrap()
}

#[tokio::test]
async fn human_creates_task_visible_on_board() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let project = infra.agent.project_id.unwrap();
    let csrf = csrf_for(&app, &cookie).await;
    let mut events = app.state.bus.subscribe();

    // The board offers the create form.
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        select_texts(&board, "form[action='/ui/tasks'] input[name=title]").len(),
        1
    );

    let page = app
        .post_form(
            "/ui/tasks",
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("project", &project.to_string()),
                    ("title", "Rotate the creds"),
                    ("body", "Please rotate them."),
                    ("parent", ""),
                ],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    let location = page.location.unwrap();
    let id: TaskId = location
        .strip_prefix("/ui/tasks/")
        .unwrap()
        .parse()
        .unwrap();
    let task = app.state.store.get_task(id).await.unwrap().unwrap();
    assert_eq!(task.created_by, Actor::Human);
    assert_eq!(task.title, "Rotate the creds");
    // Published like an agent's create, so agents wake and boards update.
    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(event, axle::events::Event::TaskChanged { id: e, .. } if e == id),
        "{event:?}"
    );

    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        select_texts(&board, "[data-status=open] .card-title"),
        ["Rotate the creds"]
    );
    let detail = app.get(&location, Some(&cookie)).await;
    assert_eq!(detail.status, StatusCode::OK);
    assert!(detail.text().contains("Please rotate them."));

    // A subtask from the task page.
    let page = app
        .post_form(
            "/ui/tasks",
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("project", &project.to_string()),
                    ("title", "Child step"),
                    ("body", ""),
                    ("parent", &id.to_string()),
                ],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    let detail = app.get(&location, Some(&cookie)).await;
    assert!(
        select_texts(&detail, ".child-title")
            .iter()
            .any(|t| t.ends_with("Child step"))
    );

    // A blank title is refused, and nothing is created.
    let page = app
        .post_form(
            "/ui/tasks",
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("project", &project.to_string()),
                    ("title", "  "),
                    ("body", ""),
                ],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::UNPROCESSABLE_ENTITY);
    // Without CSRF nothing happens.
    let page = app
        .post_form("/ui/tasks", Some(&cookie), "project=x&title=y&body=z")
        .await;
    assert_eq!(page.status, StatusCode::FORBIDDEN);
    assert_eq!(
        app.state
            .store
            .list_tasks(&axle::store::TaskFilter::default())
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn force_release_reopens_and_records_event() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = app.task(&infra, "Held task", None).await;
    app.claim(&infra, task.id).await;
    let csrf = csrf_for(&app, &cookie).await;

    let page_path = format!("/ui/tasks/{}", task.id);
    let detail = app.get(&page_path, Some(&cookie)).await;
    assert!(
        detail.text().contains("claude-code@mac"),
        "lease owner shown"
    );
    assert_eq!(
        select_texts(
            &detail,
            &format!("form[action='{page_path}/force-release'] button")
        )
        .len(),
        1
    );

    let page = app
        .post_form(
            &format!("{page_path}/force-release"),
            Some(&cookie),
            &form(&csrf, &[]),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert_eq!(page.location.as_deref(), Some(page_path.as_str()));
    let after = app.state.store.get_task(task.id).await.unwrap().unwrap();
    assert_eq!(after.status, axle::domain::TaskStatus::Open);
    assert_eq!(after.owner_agent_id, None);
    let events = app.state.store.task_events(task.id).await.unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.kind, axle::domain::TaskEventKind::ForceReleased);
    assert_eq!(last.actor, Actor::Human);
    let detail = app.get(&page_path, Some(&cookie)).await;
    assert!(
        select_texts(&detail, ".event-kind")
            .iter()
            .any(|k| k == "force released")
    );

    // Releasing an open task is a conflict, shown as an error page.
    let page = app
        .post_form(
            &format!("{page_path}/force-release"),
            Some(&cookie),
            &form(&csrf, &[]),
        )
        .await;
    assert_eq!(page.status, StatusCode::CONFLICT);

    // Cancel, as the human.
    let page = app
        .post_form(
            &format!("{page_path}/cancel"),
            Some(&cookie),
            &form(&csrf, &[]),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    let after = app.state.store.get_task(task.id).await.unwrap().unwrap();
    assert_eq!(after.status, axle::domain::TaskStatus::Cancelled);
}

#[tokio::test]
async fn task_page_shows_lease_children_evidence_and_paged_history() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let parent = app.task(&infra, "Parent", None).await;
    let child = app.task(&infra, "The child", Some(parent.id)).await;
    app.claim(&infra, child.id).await;
    app.state
        .tasks
        .complete(
            &infra,
            child.id,
            Completion {
                summary: "Rotated it".into(),
                changed_files: vec!["src/creds.rs".into()],
                tests: vec!["cargo test creds".into()],
                links: vec!["https://example.com/pr/1".into()],
            },
        )
        .await
        .unwrap();
    let detail = app
        .get(&format!("/ui/tasks/{}", child.id), Some(&cookie))
        .await;
    let evidence = select_texts(&detail, ".evidence").join(" ");
    for want in [
        "Rotated it",
        "src/creds.rs",
        "cargo test creds",
        "https://example.com/pr/1",
    ] {
        assert!(evidence.contains(want), "{want}: {evidence}");
    }
    let parent_page = app
        .get(&format!("/ui/tasks/{}", parent.id), Some(&cookie))
        .await;
    assert!(
        select_texts(&parent_page, ".child-title")
            .iter()
            .any(|t| t.ends_with("The child"))
    );

    // Long histories are paged: renew many times, then follow the "older" link.
    let busy = app.task(&infra, "Busy", None).await;
    app.claim(&infra, busy.id).await;
    for _ in 0..60 {
        app.clock.advance(time::Duration::seconds(1));
        app.state.tasks.renew(&infra, busy.id).await.unwrap();
    }
    let path = format!("/ui/tasks/{}", busy.id);
    let first = app.get(&path, Some(&cookie)).await;
    let kinds = select_texts(&first, ".event-kind");
    assert_eq!(kinds.len(), 50, "one page of events");
    let older = Selector::parse("a.older").unwrap();
    let href = first
        .html()
        .select(&older)
        .next()
        .expect("older link")
        .value()
        .attr("href")
        .unwrap()
        .to_string();
    let second = app.get(&href, Some(&cookie)).await;
    let rest = select_texts(&second, ".event-kind");
    assert_eq!(rest.len(), 12, "60 renewals + claimed + created = 62");
    assert_eq!(rest.last().unwrap(), "created");
}

#[tokio::test]
async fn message_html_escaped() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = app.task(&infra, "Thread", None).await;
    let evil = "<script>alert(1)</script>";
    let from = Actor::Agent(infra.agent.id, infra.session.id);
    let to_human = app
        .state
        .messages
        .send(
            &from,
            NewMessage {
                to: Address::Human,
                subject: Some("<b>subj</b>".into()),
                body: evil.into(),
                importance: Importance::Normal,
                reply_to: None,
            },
        )
        .await
        .unwrap();
    app.state
        .messages
        .send(
            &from,
            NewMessage {
                to: Address::Task(task.id),
                subject: None,
                body: evil.into(),
                importance: Importance::Normal,
                reply_to: None,
            },
        )
        .await
        .unwrap();
    for path in [
        format!("/ui/inbox?m={}", to_human.id),
        "/ui/inbox?view=all".to_string(),
        "/ui/inbox?q=alert".to_string(),
        format!("/ui/tasks/{}", task.id),
    ] {
        let page = app.get(&path, Some(&cookie)).await;
        assert_eq!(page.status, StatusCode::OK, "{path}");
        assert!(!page.body.contains("<script>alert"), "{path}");
        assert!(!page.body.contains("<b>subj"), "{path}");
    }
    let reader = app
        .get(&format!("/ui/inbox?m={}", to_human.id), Some(&cookie))
        .await;
    assert_eq!(select_texts(&reader, ".reader .peer-text"), [evil]);
    let thread = app
        .get(&format!("/ui/tasks/{}", task.id), Some(&cookie))
        .await;
    let bodies = select_texts(&thread, ".thread .peer-text");
    assert_eq!(bodies, ["body", evil]);
    // Each thread message names its sender and project.
    assert!(
        select_texts(&thread, ".thread .msg-from")
            .iter()
            .all(|f| f.contains("claude-code@mac"))
    );
}

#[tokio::test]
async fn inbox_split_view_receipts_and_search() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let from = Actor::Agent(infra.agent.id, infra.session.id);
    let csrf = csrf_for(&app, &cookie).await;
    let question = app
        .state
        .messages
        .send(
            &from,
            NewMessage {
                to: Address::Human,
                subject: Some("Need a decision".into()),
                body: "Blue or green?".into(),
                importance: Importance::Urgent,
                reply_to: None,
            },
        )
        .await
        .unwrap();

    // The human inbox lists it, unread.
    let inbox = app.get("/ui/inbox", Some(&cookie)).await;
    assert_eq!(
        select_texts(&inbox, "#inbox-list .msg-subject"),
        ["Need a decision"]
    );
    assert_eq!(select_texts(&inbox, "#inbox-list .unread").len(), 1);
    // Opening it shows the reader and marks it read.
    let open = app
        .get(&format!("/ui/inbox?m={}", question.id), Some(&cookie))
        .await;
    assert_eq!(
        select_texts(&open, ".reader .peer-text"),
        ["Blue or green?"]
    );
    assert_eq!(select_texts(&open, "#inbox-list .unread").len(), 0);

    // Reply to the agent as the human: it reaches the agent's inbox, replying to the question.
    let mut events = app.state.bus.subscribe();
    let page = app
        .post_form(
            "/ui/messages",
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("to_kind", "agent"),
                    ("to", &infra.agent.id.to_string()),
                    ("subject", "Re: Need a decision"),
                    ("body", "Green."),
                    ("importance", "normal"),
                    ("reply_to", &question.id.to_string()),
                ],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(event, axle::events::Event::MessageCreated { .. }),
        "{event:?}"
    );
    let got = app
        .state
        .messages
        .inbox(&infra, axle::service::messages::InboxFilter::default())
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].message.body, "Green.");
    assert_eq!(got[0].message.from, Actor::Human);
    assert_eq!(got[0].message.reply_to, Some(question.id));

    // Receipts in the all-project feed: the reply was delivered (the agent fetched it), the
    // question was replied to.
    let all = app.get("/ui/inbox?view=all", Some(&cookie)).await;
    let receipts = select_texts(&all, "#inbox-list .receipt");
    assert_eq!(receipts, ["delivered", "replied"], "newest first");
    app.state
        .messages
        .mark_read(Caller::Agent(&infra), &[got[0].message.id])
        .await
        .unwrap();
    let all = app.get("/ui/inbox?view=all", Some(&cookie)).await;
    assert_eq!(select_texts(&all, "#inbox-list .receipt")[0], "read");

    // A project-wide message nobody fetched yet is only "sent".
    let page = app
        .post_form(
            "/ui/messages",
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("to_kind", "project"),
                    ("to", &infra.agent.project_id.unwrap().to_string()),
                    ("subject", ""),
                    ("body", "Heads up, everyone"),
                    ("importance", "high"),
                ],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    let all = app.get("/ui/inbox?view=all", Some(&cookie)).await;
    assert_eq!(select_texts(&all, "#inbox-list .receipt")[0], "sent");

    // Search.
    let found = app.get("/ui/inbox?q=green", Some(&cookie)).await;
    assert_eq!(found.status, StatusCode::OK);
    let previews = select_texts(&found, "#inbox-list .msg-preview");
    assert_eq!(previews.len(), 2, "{previews:?}");

    // Archiving (acking) takes it out of the human inbox.
    let page = app
        .post_form(
            &format!("/ui/inbox/{}/archive", question.id),
            Some(&cookie),
            &form(&csrf, &[]),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    let inbox = app.get("/ui/inbox", Some(&cookie)).await;
    assert!(select_texts(&inbox, "#inbox-list .msg-subject").is_empty());
}

#[tokio::test]
async fn inbox_feed_is_paged() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let from = Actor::Agent(infra.agent.id, infra.session.id);
    for i in 0..55 {
        app.clock.advance(time::Duration::seconds(1));
        app.state
            .messages
            .send(
                &from,
                NewMessage {
                    to: Address::Project(infra.agent.project_id.unwrap()),
                    subject: Some(format!("note {i}")),
                    body: "x".into(),
                    importance: Importance::Normal,
                    reply_to: None,
                },
            )
            .await
            .unwrap();
    }
    let first = app.get("/ui/inbox?view=all", Some(&cookie)).await;
    let subjects = select_texts(&first, "#inbox-list .msg-subject");
    assert_eq!(subjects.len(), 50);
    assert_eq!(subjects[0], "note 54");
    let older = Selector::parse("a.older").unwrap();
    let href = first
        .html()
        .select(&older)
        .next()
        .expect("older link")
        .value()
        .attr("href")
        .unwrap()
        .replace("&amp;", "&");
    let second = app.get(&href, Some(&cookie)).await;
    let subjects = select_texts(&second, "#inbox-list .msg-subject");
    assert_eq!(subjects, ["note 4", "note 3", "note 2", "note 1", "note 0"]);
    assert!(second.html().select(&older).next().is_none());
}

#[tokio::test]
async fn post_into_thread_as_human() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = app.task(&infra, "Thread", None).await;
    let csrf = csrf_for(&app, &cookie).await;
    let path = format!("/ui/tasks/{}", task.id);
    let page = app
        .post_form(
            &format!("{path}/post"),
            Some(&cookie),
            &form(
                &csrf,
                &[("body", "From the human"), ("importance", "normal")],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert_eq!(page.location.as_deref(), Some(path.as_str()));
    let thread = app.state.messages.thread(task.id).await.unwrap();
    assert_eq!(thread.last().unwrap().from, Actor::Human);
    assert_eq!(thread.last().unwrap().body, "From the human");
    // The creating agent gets it in its inbox.
    let got = app
        .state
        .messages
        .inbox(&infra, axle::service::messages::InboxFilter::default())
        .await
        .unwrap();
    assert!(got.iter().any(|i| i.message.body == "From the human"));
    let detail = app.get(&path, Some(&cookie)).await;
    assert!(
        select_texts(&detail, ".thread .msg-from")
            .iter()
            .any(|f| f.contains("you"))
    );
    // An empty post is refused.
    let page = app
        .post_form(
            &format!("{path}/post"),
            Some(&cookie),
            &form(&csrf, &[("body", " ")]),
        )
        .await;
    assert_eq!(page.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn rename_project_sanitises_display_name() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let project = infra.agent.project_id.unwrap();
    let csrf = csrf_for(&app, &cookie).await;
    let path = format!("/ui/projects/{project}/rename");

    let projects = app.get("/ui/projects", Some(&cookie)).await;
    assert!(projects.text().contains(INFRA), "key shown");

    let page = app
        .post_form(
            &path,
            Some(&cookie),
            &form(&csrf, &[("display_name", "  Infra as Code  ")]),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert_eq!(page.location.as_deref(), Some("/ui/projects"));
    let stored = app.state.store.get_project(project).await.unwrap().unwrap();
    assert_eq!(stored.display_name, "Infra as Code");

    let too_long = "x".repeat(81);
    for bad in [
        "",
        "   ",
        "line\nbreak",
        "tab\there",
        "[axl from x]",
        "a]b",
        too_long.as_str(),
    ] {
        let page = app
            .post_form(&path, Some(&cookie), &form(&csrf, &[("display_name", bad)]))
            .await;
        assert_eq!(page.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}");
    }
    let ok80 = "é".repeat(80);
    let page = app
        .post_form(
            &path,
            Some(&cookie),
            &form(&csrf, &[("display_name", &ok80)]),
        )
        .await;
    assert_eq!(
        page.status,
        StatusCode::SEE_OTHER,
        "80 characters (not bytes) is fine"
    );
    let stored = app.state.store.get_project(project).await.unwrap().unwrap();
    assert_eq!(stored.display_name, ok80);
}

#[tokio::test]
async fn revoke_token_blocks_api() {
    let app = app().await;
    let cookie = app.login().await;
    let (id, plaintext) = app.state.auth.create("mac-claude").await.unwrap();
    let heartbeat = || {
        Request::post("/v1/heartbeat")
            .header(header::AUTHORIZATION, format!("Bearer {plaintext}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(
                r#"{{"session_id":"{}"}}"#,
                SessionId::new()
            )))
            .unwrap()
    };
    // Accepted (an unknown session, but not unauthorised).
    let before = app.send(heartbeat()).await;
    assert_ne!(before.status, StatusCode::UNAUTHORIZED, "{}", before.body);

    let tokens = app.get("/ui/tokens", Some(&cookie)).await;
    assert!(tokens.text().contains("mac-claude"));
    assert!(!tokens.body.contains(&plaintext), "never shows the secret");
    let csrf = csrf_of(&tokens);
    let page = app
        .post_form(
            &format!("/ui/tokens/{id}/revoke"),
            Some(&cookie),
            &form(&csrf, &[]),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert_eq!(page.location.as_deref(), Some("/ui/tokens"));

    let after = app.send(heartbeat()).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    let tokens = app.get("/ui/tokens", Some(&cookie)).await;
    assert!(
        select_texts(&tokens, ".token-state")
            .iter()
            .any(|s| s.starts_with("revoked"))
    );
}

#[tokio::test]
async fn live_stream_pings_task_pages_and_inbox() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let parent = app.task(&infra, "Parent", None).await;
    let mut stream = open_live(&app, &cookie).await;

    // A child's change re-fetches its own page and its parent's.
    let child = app.task(&infra, "Child", Some(parent.id)).await;
    let wait = Duration::from_secs(2);
    assert!(
        wait_for_event(&mut stream, &format!("detail-{}", child.id), wait)
            .await
            .is_some()
    );
    assert!(
        wait_for_event(&mut stream, &format!("detail-{}", parent.id), wait)
            .await
            .is_some()
    );

    // A thread post re-fetches the task page; any message re-fetches the inbox.
    app.state
        .messages
        .send(
            &Actor::Human,
            NewMessage {
                to: Address::Task(parent.id),
                subject: None,
                body: "hi".into(),
                importance: Importance::Normal,
                reply_to: None,
            },
        )
        .await
        .unwrap();
    // Frames come in order: `inbox`, then the thread's `detail-<id>`.
    assert!(wait_for_event(&mut stream, "inbox", wait).await.is_some());
    assert!(
        wait_for_event(&mut stream, &format!("detail-{}", parent.id), wait)
            .await
            .is_some()
    );
}

#[tokio::test]
async fn task_thread_is_paged() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = app.task(&infra, "Chatty", None).await;
    for i in 0..60 {
        app.clock.advance(time::Duration::seconds(1));
        app.state
            .messages
            .send(
                &Actor::Agent(infra.agent.id, infra.session.id),
                NewMessage {
                    to: Address::Task(task.id),
                    subject: None,
                    body: format!("post {i}"),
                    importance: Importance::Normal,
                    reply_to: None,
                },
            )
            .await
            .unwrap();
    }
    let path = format!("/ui/tasks/{}", task.id);
    let first = app.get(&path, Some(&cookie)).await;
    let bodies = select_texts(&first, ".thread .peer-text");
    assert_eq!(bodies.len(), 50, "the newest 50 posts");
    assert_eq!(bodies.first().unwrap(), "post 10", "shown oldest first");
    assert_eq!(bodies.last().unwrap(), "post 59");
    let earlier = Selector::parse("a.earlier").unwrap();
    let href = first
        .html()
        .select(&earlier)
        .next()
        .expect("earlier link")
        .value()
        .attr("href")
        .unwrap()
        .to_string();
    let second = app.get(&href, Some(&cookie)).await;
    let bodies = select_texts(&second, ".thread .peer-text");
    assert_eq!(bodies.len(), 11, "posts 0-9 and the task body");
    assert_eq!(bodies.first().unwrap(), "body");
    assert_eq!(bodies.last().unwrap(), "post 9");
    assert!(second.html().select(&earlier).next().is_none());
}

// ---------------------------------------------------------------- plan gate and needs-you

const WEB_APP: &str = "github.com/acme/web-app";

/// A human-created gated task in `by`'s project, claimed by `by` and with a plan submitted.
async fn submitted_plan(app: &App, by: &Resolved, title: &str) -> Task {
    let task = app
        .state
        .tasks
        .create(
            Caller::Human,
            NewTask {
                project_id: by.agent.project_id.unwrap(),
                title: title.into(),
                body: "split the work".into(),
                requires_plan: true,
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    app.claim(by, task.id).await;
    app.state
        .tasks
        .propose_plan(
            by,
            task.id,
            "1. platform-infra: add the bucket\n2. web-app: read it".into(),
        )
        .await
        .unwrap();
    task
}

#[tokio::test]
async fn new_task_form_has_lead_also_and_plan_first() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let web_app = app.agent(WEB_APP, "mac", "claude-code").await;
    let (lead, other) = (
        infra.agent.project_id.unwrap(),
        web_app.agent.project_id.unwrap(),
    );
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert!(board.text().contains("Lead project"));
    assert!(board.text().contains("Also involves"));
    assert_eq!(select_texts(&board, "input[name=also]").len(), 2);
    assert_eq!(
        select_texts(&board, "input[name=plan_first][checked]").len(),
        1,
        "plan first is ticked by default"
    );
    assert!(board.text().contains("Plan first"));

    let csrf = csrf_of(&board);
    let page = app
        .post_form(
            "/ui/tasks",
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("project", &lead.to_string()),
                    ("also", &other.to_string()),
                    ("also", &lead.to_string()),
                    ("title", "Cross-project"),
                    ("body", ""),
                    ("plan_first", "on"),
                ],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    let id: TaskId = page
        .location
        .unwrap()
        .strip_prefix("/ui/tasks/")
        .unwrap()
        .parse()
        .unwrap();
    let task = app.state.store.get_task(id).await.unwrap().unwrap();
    assert!(task.requires_plan);
    assert_eq!(task.project_id, lead);
    assert_eq!(
        app.state.store.task_projects(id).await.unwrap(),
        vec![other]
    );

    // Unticked: no gate.
    let page = app
        .post_form(
            "/ui/tasks",
            Some(&cookie),
            &form(&csrf, &[("project", &lead.to_string()), ("title", "Plain")]),
        )
        .await;
    let id: TaskId = page
        .location
        .unwrap()
        .strip_prefix("/ui/tasks/")
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        !app.state
            .store
            .get_task(id)
            .await
            .unwrap()
            .unwrap()
            .requires_plan
    );
}

#[tokio::test]
async fn plan_is_approved_from_the_task_page() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = submitted_plan(&app, &infra, "Gated work").await;
    let path = format!("/ui/tasks/{}", task.id);

    let page = app.get(&path, Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text().contains("Plan awaiting your approval"));
    // The plan's markdown is rendered: a numbered list.
    assert_eq!(
        select_texts(&page, "#plan .plan-body ol > li"),
        ["platform-infra: add the bucket", "web-app: read it"]
    );
    assert_eq!(
        select_texts(&page, "#plan button"),
        ["Approve plan", "Request changes"]
    );
    assert_eq!(select_texts(&page, ".msg-kind"), ["plan"]);
    assert!(select_texts(&page, ".task-meta .status")[0].contains("plan awaiting you"));

    // CSRF is required; a note is required to request changes.
    let csrf = csrf_of(&page);
    let refused = app
        .post_form(&format!("{path}/plan/approve"), Some(&cookie), "")
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let plan = input_value(&page, "#plan input[name=plan]");
    let refused = app
        .post_form(
            &format!("{path}/plan/changes"),
            Some(&cookie),
            &form(&csrf, &[("note", " "), ("plan", &plan)]),
        )
        .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);

    let page = app
        .post_form(
            &format!("{path}/plan/changes"),
            Some(&cookie),
            &form(&csrf, &[("note", "Do web-app first"), ("plan", &plan)]),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    let t = app.state.store.get_task(task.id).await.unwrap().unwrap();
    assert_eq!(t.plan_state, axle::domain::PlanState::ChangesRequested);
    let page = app.get(&path, Some(&cookie)).await;
    assert!(
        select_texts(&page, "#plan button").is_empty(),
        "nothing to decide"
    );
    assert!(page.text().contains("Do web-app first"));

    // Resubmitted, then approved.
    app.state
        .tasks
        .propose_plan(&infra, task.id, "web-app first, then platform-infra".into())
        .await
        .unwrap();
    let plan = input_value(
        &app.get(&path, Some(&cookie)).await,
        "#plan input[name=plan]",
    );
    let page = app
        .post_form(
            &format!("{path}/plan/approve"),
            Some(&cookie),
            &form(&csrf, &[("plan", &plan)]),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert_eq!(page.location.as_deref(), Some(path.as_str()));
    let t = app.state.store.get_task(task.id).await.unwrap().unwrap();
    assert_eq!(t.status, axle::domain::TaskStatus::Claimed);
    // Approving again is a conflict.
    let page = app
        .post_form(
            &format!("{path}/plan/approve"),
            Some(&cookie),
            &form(&csrf, &[("plan", &plan)]),
        )
        .await;
    assert_eq!(page.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn human_adds_projects_from_the_task_page() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let web_app = app.agent(WEB_APP, "mac", "claude-code").await;
    let other = web_app.agent.project_id.unwrap();
    let task = app.task(&infra, "Parent", None).await;
    let path = format!("/ui/tasks/{}", task.id);
    let page = app.get(&path, Some(&cookie)).await;
    assert_eq!(select_texts(&page, "#add-project option").len(), 1);
    let csrf = csrf_of(&page);

    let without = app
        .post_form(
            &format!("{path}/projects"),
            Some(&cookie),
            &format!("project={other}"),
        )
        .await;
    assert_eq!(without.status, StatusCode::FORBIDDEN);
    let page = app
        .post_form(
            &format!("{path}/projects"),
            Some(&cookie),
            &form(&csrf, &[("project", &other.to_string())]),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert_eq!(
        app.state.store.task_projects(task.id).await.unwrap(),
        vec![other]
    );
    let page = app.get(&path, Some(&cookie)).await;
    assert!(
        page.text().contains("Added project acme/web-app"),
        "{}",
        page.text()
    );
    assert!(select_texts(&page, "#add-project option").is_empty());
    // A subtask can now go to the added project; a child cannot take projects.
    let child = app
        .post_form(
            "/ui/tasks",
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("project", &other.to_string()),
                    ("title", "web-app part"),
                    ("parent", &task.id.to_string()),
                ],
            ),
        )
        .await;
    assert_eq!(child.status, StatusCode::SEE_OTHER, "{}", child.body);
    let child_path = child.location.unwrap();
    let page = app
        .post_form(
            &format!("{child_path}/projects"),
            Some(&cookie),
            &form(
                &csrf,
                &[("project", &infra.agent.project_id.unwrap().to_string())],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn needs_you_cards_match_the_count() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;

    // Nothing waiting: the strip is hidden and the count is zero.
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(select_texts(&board, "#needs-you[hidden]").len(), 1);
    // It re-fetches itself on every live event, like the overview.
    assert_eq!(
        select_texts(
            &board,
            "#needs-you[hx-trigger*='sse:overview'][hx-get^='/ui/needs-you']"
        )
        .len(),
        1
    );
    assert!(
        select_texts(&board, "#pulse")
            .join(" ")
            .contains("0 need you")
    );

    // A plan, a review of the human's task, an urgent and a normal message.
    let plan = submitted_plan(&app, &infra, "Gated work").await;
    let review = app
        .state
        .tasks
        .create(
            Caller::Human,
            NewTask {
                project_id: infra.agent.project_id.unwrap(),
                title: "Check my thing".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    app.claim(&infra, review.id).await;
    app.state
        .tasks
        .to_review(&infra, review.id, None)
        .await
        .unwrap();
    for (importance, subject) in [
        (Importance::Urgent, "Prod is down"),
        (Importance::Normal, "fyi"),
    ] {
        app.state
            .messages
            .send(
                &Actor::Agent(infra.agent.id, infra.session.id),
                NewMessage {
                    to: Address::Human,
                    subject: Some(subject.into()),
                    body: "details".into(),
                    importance,
                    reply_to: None,
                },
            )
            .await
            .unwrap();
    }
    app.clock.advance(time::Duration::minutes(12));

    for path in ["/ui/board", "/ui/"] {
        let page = app.get(path, Some(&cookie)).await;
        assert_eq!(select_texts(&page, "#needs-you[hidden]").len(), 0, "{path}");
        let whats = select_texts(&page, "#needs-you .need-what");
        assert_eq!(whats.len(), 3, "{path}: {whats:?} {}", page.text());
        assert!(whats[0].starts_with("Approve plan"));
        assert!(whats[1].starts_with("Review"));
        assert!(whats[2].starts_with("Reply"));
        assert_eq!(
            select_texts(&page, "#needs-you .card-title"),
            ["Gated work", "Check my thing", "Prod is down"]
        );
        assert!(select_texts(&page, ".need-wait")[0].contains("12m"));
        assert!(
            select_texts(&page, "#pulse")
                .join(" ")
                .contains("3 need you")
        );
        let link = Selector::parse("#pulse a[href='/ui/board#needs-you']").unwrap();
        assert_eq!(page.html().select(&link).count(), 1);
    }
    // The board's Claimed column shows the planning task with its plan state in words.
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        select_texts(&board, ".column[data-status=claimed] .card .status"),
        ["plan awaiting you"]
    );
    // The fragment (live re-fetch) is the same strip; approving inline returns to the board.
    let fragment = app.get("/ui/needs-you?back=/ui/board", Some(&cookie)).await;
    assert_eq!(select_texts(&fragment, ".need-card").len(), 3);
    let csrf = csrf_of(&board);
    let plan_id = input_value(&board, ".need-card input[name=plan]");
    let page = app
        .post_form(
            &format!("/ui/tasks/{}/plan/approve", plan.id),
            Some(&cookie),
            &form(&csrf, &[("back", "/ui/board"), ("plan", &plan_id)]),
        )
        .await;
    assert_eq!(page.location.as_deref(), Some("/ui/board#needs-you"));
    let fragment = app.get("/ui/needs-you", Some(&cookie)).await;
    assert_eq!(select_texts(&fragment, ".need-card").len(), 2);
    let pulse = app.get("/ui/pulse", Some(&cookie)).await;
    assert_eq!(select_texts(&pulse, ".pulse-item")[2], "2 need you");
}

fn input_value(page: &Page, css: &str) -> String {
    let sel = Selector::parse(css).unwrap();
    page.html()
        .select(&sel)
        .next()
        .unwrap_or_else(|| panic!("no {css}"))
        .value()
        .attr("value")
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn a_stale_plan_decision_shows_the_new_plan() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = submitted_plan(&app, &infra, "Gated work").await;
    let path = format!("/ui/tasks/{}", task.id);
    let page = app.get(&path, Some(&cookie)).await;
    let csrf = csrf_of(&page);
    let seen = input_value(&page, "#plan input[name=plan]");
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(input_value(&board, ".need-card input[name=plan]"), seen);

    // The agent replaces the plan while the human is reading the old one.
    app.state
        .tasks
        .propose_plan(&infra, task.id, "A different plan".into())
        .await
        .unwrap();
    for (action, extra) in [("approve", None), ("changes", Some("smaller"))] {
        let mut fields = vec![("plan", seen.as_str()), ("back", "/ui/board")];
        if let Some(note) = extra {
            fields.push(("note", note));
        }
        let page = app
            .post_form(
                &format!("{path}/plan/{action}"),
                Some(&cookie),
                &form(&csrf, &fields),
            )
            .await;
        assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
        assert_eq!(
            page.location.as_deref(),
            Some(format!("{path}?plan_changed=1#plan").as_str())
        );
    }
    let t = app.state.store.get_task(task.id).await.unwrap().unwrap();
    assert_eq!(t.plan_state, axle::domain::PlanState::Submitted);
    let page = app
        .get(&format!("{path}?plan_changed=1"), Some(&cookie))
        .await;
    assert!(
        select_texts(&page, "#plan .error")[0].contains("The plan changed"),
        "{}",
        page.text()
    );
    assert_eq!(
        select_texts(&page, "#plan .plan-body"),
        ["A different plan"]
    );
    assert_ne!(input_value(&page, "#plan input[name=plan]"), seen);
}

#[tokio::test]
async fn needs_you_shows_every_review_and_peeks_without_delivering() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let other = app.agent(INFRA, "linux", "codex").await;
    // A task an agent created, now in review: it waits on someone too.
    let t = app.task(&infra, "Agent-made work", None).await;
    app.claim(&other, t.id).await;
    app.state.tasks.to_review(&other, t.id, None).await.unwrap();
    let urgent = app
        .state
        .messages
        .send(
            &Actor::Agent(infra.agent.id, infra.session.id),
            NewMessage {
                to: Address::Human,
                subject: Some("Prod is down".into()),
                body: "details".into(),
                importance: Importance::Urgent,
                reply_to: None,
            },
        )
        .await
        .unwrap();
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        select_texts(&board, "#needs-you .card-title"),
        ["Agent-made work", "Prod is down"]
    );
    app.get("/ui/pulse", Some(&cookie)).await;
    app.get("/ui/needs-you", Some(&cookie)).await;
    let receipt = app
        .state
        .store
        .message_receipts(&[urgent.id])
        .await
        .unwrap();
    assert_eq!(
        receipt[0].delivered, 0,
        "showing the strip delivers nothing"
    );
}

/// An urgent message from `from` to the human.
async fn urgent_to_human(app: &App, from: &Resolved, subject: &str) -> axle::domain::Message {
    app.state
        .messages
        .send(
            &Actor::Agent(from.agent.id, from.session.id),
            NewMessage {
                to: Address::Human,
                subject: Some(subject.into()),
                body: "please look".into(),
                importance: Importance::Urgent,
                reply_to: None,
            },
        )
        .await
        .unwrap()
}

async fn need_titles(app: &App, cookie: &str) -> (Vec<String>, String) {
    let board = app.get("/ui/board", Some(cookie)).await;
    (
        select_texts(&board, "#needs-you .card-title"),
        select_texts(&board, "#pulse .pulse-item")[2].clone(),
    )
}

#[tokio::test]
async fn replying_or_dismissing_clears_a_reply_card() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let other = app.agent(INFRA, "linux", "codex").await;
    let first = urgent_to_human(&app, &infra, "Prod is down").await;
    let second = urgent_to_human(&app, &infra, "Heads up").await;
    assert_eq!(
        need_titles(&app, &cookie).await,
        (
            vec!["Prod is down".to_string(), "Heads up".to_string()],
            "2 need you".to_string()
        )
    );
    let board = app.get("/ui/board", Some(&cookie)).await;
    let csrf = csrf_of(&board);
    assert_eq!(
        select_texts(&board, ".need-card[data-need=reply] button"),
        ["Dismiss", "Dismiss"]
    );

    // A reply to someone other than the sender is refused and clears nothing.
    let page = app
        .post_form(
            "/ui/messages",
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("to_kind", "agent"),
                    ("to", &other.agent.id.to_string()),
                    ("reply_to", &first.id.to_string()),
                    ("body", "wrong person"),
                ],
            ),
        )
        .await;
    assert_eq!(
        page.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        page.body
    );
    assert_eq!(need_titles(&app, &cookie).await.0.len(), 2);

    // Replying to the sender archives the human's copy in the same write.
    let mut events = app.state.bus.subscribe();
    let page = app
        .post_form(
            "/ui/messages",
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("to_kind", "agent"),
                    ("to", &infra.agent.id.to_string()),
                    ("reply_to", &first.id.to_string()),
                    ("body", "on it"),
                ],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .is_ok(),
        "the strip refreshes live"
    );
    assert_eq!(
        need_titles(&app, &cookie).await,
        (vec!["Heads up".to_string()], "1 need you".to_string())
    );
    let receipt = app.state.store.message_receipts(&[first.id]).await.unwrap();
    assert!(receipt[0].replied);
    let agent_mail = app
        .state
        .messages
        .inbox(&infra, axle::service::messages::InboxFilter::default())
        .await
        .unwrap();
    assert!(agent_mail.iter().any(|i| i.message.body == "on it"));

    // Dismiss archives for the human only; it needs CSRF and goes back to the board.
    let path = format!("/ui/inbox/{}/dismiss", second.id);
    let refused = app.post_form(&path, Some(&cookie), "back=/ui/board").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let mut events = app.state.bus.subscribe();
    let page = app
        .post_form(&path, Some(&cookie), &form(&csrf, &[("back", "/ui/board")]))
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert_eq!(page.location.as_deref(), Some("/ui/board#needs-you"));
    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("a live event")
        .unwrap();
    let frames = axle::ui::views::live_frames(&app.state, &event).await;
    assert!(
        frames.iter().any(|(name, _)| name == "overview"),
        "{frames:?}"
    );
    assert_eq!(
        need_titles(&app, &cookie).await,
        (vec![], "0 need you".to_string())
    );
    let receipt = app
        .state
        .store
        .message_receipts(&[second.id])
        .await
        .unwrap();
    assert!(!receipt[0].replied);
}

#[tokio::test]
async fn live_refreshes_keep_drafts_and_leases_fresh() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = submitted_plan(&app, &infra, "Gated work").await;
    let board = app.get("/ui/board", Some(&cookie)).await;
    // Leases renewed by heartbeat publish nothing, so lease and presence views re-render on
    // a timer as well as on events.
    assert_eq!(
        select_texts(&board, "#columns[hx-trigger*='every 60s']").len(),
        1
    );
    assert_eq!(
        select_texts(
            &board,
            "#needs-you[data-keep-drafts][hx-trigger*='axl:refresh']"
        )
        .len(),
        1
    );
    let page = app
        .get(&format!("/ui/tasks/{}", task.id), Some(&cookie))
        .await;
    assert_eq!(
        select_texts(
            &page,
            "#task-live[data-keep-drafts][hx-trigger*='every 60s'][hx-trigger*='axl:refresh']"
        )
        .len(),
        1,
        "the plan form inside the live section keeps its draft"
    );
    let js = include_str!("../static/app.js");
    assert!(js.contains("[data-keep-drafts]") || js.contains("keepDrafts"));
    assert!(js.contains("hasDraft") && js.contains("axl:refresh"));
}

#[tokio::test]
async fn live_refresh_keeps_pagination_cursors() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = app.task(&infra, "Long", None).await;
    app.claim(&infra, task.id).await;
    for i in 0..60 {
        app.clock.advance(time::Duration::seconds(1));
        app.state.tasks.renew(&infra, task.id).await.unwrap();
        app.state
            .messages
            .send(
                &Actor::Agent(infra.agent.id, infra.session.id),
                NewMessage {
                    to: Address::Task(task.id),
                    subject: None,
                    body: format!("post {i}"),
                    importance: Importance::Normal,
                    reply_to: None,
                },
            )
            .await
            .unwrap();
    }
    let live = Selector::parse("#task-live").unwrap();
    let refresh = |page: &Page| {
        page.html()
            .select(&live)
            .next()
            .expect("live section")
            .value()
            .attr("hx-get")
            .unwrap()
            .to_string()
    };
    let link = |page: &Page, css: &str| {
        page.html()
            .select(&Selector::parse(css).unwrap())
            .next()
            .expect(css)
            .value()
            .attr("href")
            .unwrap()
            .to_string()
    };
    let path = format!("/ui/tasks/{}", task.id);
    let first = app.get(&path, Some(&cookie)).await;
    assert_eq!(refresh(&first), path, "the newest page refreshes as itself");

    // An earlier thread page and an older history page each refresh as themselves.
    let thread_href = link(&first, "a.earlier");
    let thread_page = app.get(&thread_href, Some(&cookie)).await;
    assert_eq!(refresh(&thread_page), thread_href);
    let history_href = link(&first, "a.older");
    let history_page = app.get(&history_href, Some(&cookie)).await;
    assert_eq!(refresh(&history_page), history_href);

    // Both cursors at once survive, and the refresh re-renders that same older page.
    let thread_cursor = thread_href.split_once('?').unwrap().1;
    let history_cursor = history_href.split_once('?').unwrap().1;
    let both = format!("{path}?{thread_cursor}&{history_cursor}");
    let both_page = app.get(&both, Some(&cookie)).await;
    let again = app.get(&refresh(&both_page), Some(&cookie)).await;
    assert_eq!(refresh(&again), both);
    let bodies = select_texts(&again, ".thread .peer-text");
    assert_eq!(
        bodies.last().unwrap(),
        "post 9",
        "still the earlier thread page"
    );
    assert_eq!(
        select_texts(&again, ".event-kind").last().unwrap(),
        "created",
        "still the older history page"
    );
}

#[tokio::test]
async fn live_refresh_updates_task_actions() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = app.task(&infra, "Actionable", None).await;
    let path = format!("/ui/tasks/{}", task.id);
    let page = app.get(&path, Some(&cookie)).await;
    assert!(
        select_texts(&page, "#task-actions").len() == 1,
        "the actions sidebar is addressable"
    );
    assert_eq!(
        select_texts(&page, "#task-live[hx-select-oob*='#task-actions']").len(),
        1,
        "every live refresh also swaps the sidebar"
    );
    assert!(
        select_texts(&page, "#task-live[data-drafts-also='#task-actions']").len() == 1,
        "sidebar drafts defer the refresh"
    );
    let js = include_str!("../static/app.js");
    assert!(js.contains("data-drafts-also") || js.contains("draftsAlso"));
    let actions = |p: &Page| select_texts(p, "#task-actions").join(" ");
    assert!(!actions(&page).contains("Force release"), "open task");

    // An agent claims it: the refresh response now carries Force release.
    app.claim(&infra, task.id).await;
    let live = Selector::parse("#task-live").unwrap();
    let hx_get = page
        .html()
        .select(&live)
        .next()
        .unwrap()
        .value()
        .attr("hx-get")
        .unwrap()
        .to_string();
    let refreshed = app.get(&hx_get, Some(&cookie)).await;
    assert!(
        actions(&refreshed).contains("Force release"),
        "{}",
        actions(&refreshed)
    );

    // Completion removes the stale actions again.
    app.state
        .tasks
        .complete(
            &infra,
            task.id,
            Completion {
                summary: "done".into(),
                ..Completion::default()
            },
        )
        .await
        .unwrap();
    let done = app.get(&hx_get, Some(&cookie)).await;
    assert!(
        !actions(&done).contains("Force release"),
        "{}",
        actions(&done)
    );
    assert!(
        !actions(&done).contains("Cancel task"),
        "{}",
        actions(&done)
    );
}

#[tokio::test]
async fn cmd_enter_sends_message_forms() {
    let app = app().await;
    let cookie = app.login().await;

    // app.js carries the one delegated handler, and the CSP is unchanged.
    let js = app.get("/ui/static/app.js", None).await;
    assert!(js.body.contains("requestSubmit") && js.body.contains("isComposing"));
    assert!(js.body.contains("metaKey") && js.body.contains("ctrlKey"));
    let res = app
        .router
        .clone()
        .oneshot(
            Request::get("/ui/board")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let csp = res.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("script-src 'self'") && !csp.contains("unsafe-inline"));

    // Every page with a textarea names the shortcut in words next to its send button.
    let agent = app.agent("proj", "host", "claude-code").await;
    let task = app.task(&agent, "Hinted", None).await;
    for path in [
        "/ui/board".to_string(),
        "/ui/inbox".to_string(),
        format!("/ui/tasks/{}", task.id),
    ] {
        let page = app.get(&path, Some(&cookie)).await;
        let textareas = page
            .html()
            .select(&Selector::parse("textarea").unwrap())
            .count();
        let hints = select_texts(&page, "[data-send-hint]");
        assert!(textareas > 0, "{path}: no textarea");
        assert!(
            !hints.is_empty() && hints.iter().all(|h| h.contains("Enter to send")),
            "{path}: {hints:?}"
        );
    }
}

fn href_of(page: &Page, css: &str) -> String {
    let sel = Selector::parse(css).unwrap();
    page.html()
        .select(&sel)
        .next()
        .unwrap_or_else(|| panic!("no element matches {css}"))
        .value()
        .attr("href")
        .unwrap()
        .to_string()
}

fn hidden_values(page: &Page, css: &str) -> Vec<String> {
    let sel = Selector::parse(css).unwrap();
    page.html()
        .select(&sel)
        .map(|e| e.value().attr("value").unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------- quick replies

const HOSTILE: &str = "<script>alert(1)</script>";

/// An urgent question from `from` to the human offering `options`.
async fn question(
    app: &App,
    from: &Resolved,
    subject: &str,
    options: &[&str],
) -> axle::domain::Message {
    let options: Vec<String> = options.iter().map(|o| o.to_string()).collect();
    app.state
        .messages
        .send_with_options(
            &Actor::Agent(from.agent.id, from.session.id),
            NewMessage {
                to: Address::Human,
                subject: Some(subject.into()),
                body: "which way?".into(),
                importance: Importance::Urgent,
                reply_to: None,
            },
            &options,
        )
        .await
        .unwrap()
}

/// Bodies of the mail waiting for `agent`.
async fn agent_bodies(app: &App, agent: &Resolved) -> Vec<String> {
    app.state
        .messages
        .inbox(agent, axle::service::messages::InboxFilter::default())
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.message.body)
        .collect()
}

#[tokio::test]
async fn history_lists_every_done_task_paged() {
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let mut last = None;
    for i in 0..=50 {
        let t = app.task(&infra, &format!("t{i}"), None).await;
        app.complete(&infra, t.id).await;
        app.clock.advance(time::Duration::minutes(1));
        last = Some(t);
    }
    // A subtask finishing last: History includes subtasks.
    let parent = last.unwrap();
    let child = app.task(&infra, "child", Some(parent.id)).await;
    app.complete(&infra, child.id).await;
    let cookie = app.login().await;

    let p1 = app.get("/ui/history", Some(&cookie)).await;
    assert_eq!(p1.status, StatusCode::OK);
    let titles = select_texts(&p1, ".history-row .title");
    assert_eq!(titles.len(), 50);
    assert_eq!(titles[0], "child");
    assert_eq!(titles[1], "t50");
    let next = href_of(&p1, "a[rel=next]");
    let p2 = app.get(&next, Some(&cookie)).await;
    assert_eq!(p2.status, StatusCode::OK);
    assert_eq!(select_texts(&p2, ".history-row .title"), ["t1", "t0"]);
    assert!(select_texts(&p2, "a[rel=next]").is_empty());

    let bad = app.get("/ui/history?after=nope", Some(&cookie)).await;
    assert_eq!(bad.status, StatusCode::NOT_FOUND);
    let unknown = app
        .get(
            &format!("/ui/history?after={}", TaskId::new()),
            Some(&cookie),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    // A cursor at a task that exists but is not done is as unknown as a missing one.
    let open = app.task(&infra, "still open", None).await;
    let not_done = app
        .get(&format!("/ui/history?after={}", open.id), Some(&cookie))
        .await;
    assert_eq!(not_done.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn history_shows_ack_state() {
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let old = app.task(&infra, "old", None).await;
    app.complete(&infra, old.id).await;
    app.clock.advance(time::Duration::days(8));
    let acked = app.task(&infra, "acked", None).await;
    app.complete(&infra, acked.id).await;
    app.state.tasks.ack_done(&[acked.id]).await.unwrap();
    app.clock.advance(time::Duration::minutes(1));
    let recent = app.task(&infra, "recent", None).await;
    app.complete(&infra, recent.id).await;
    let cookie = app.login().await;
    let page = app.get("/ui/history", Some(&cookie)).await;
    assert_eq!(
        select_texts(&page, ".history-row .title"),
        ["recent", "acked", "old"]
    );
    assert_eq!(
        select_texts(&page, ".history-row .ack-state"),
        ["unseen", "acked", "auto-acked"]
    );
    assert!(!page.text().contains("7 days"));
}

#[tokio::test]
async fn history_filters_by_project_and_title() {
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let web = app.agent("github.com/acme/web", "mac", "codex").await;
    for (who, title) in [
        (&infra, "infra work"),
        (&web, "fix the flaky test"),
        (&web, "100% sure_thing"),
    ] {
        let t = app.task(who, title, None).await;
        app.complete(who, t.id).await;
        app.clock.advance(time::Duration::minutes(1));
    }
    let cookie = app.login().await;
    let titles = |p: &Page| select_texts(p, ".history-row .title");
    let by_key = app
        .get(&format!("/ui/history?project={INFRA}"), Some(&cookie))
        .await;
    assert_eq!(titles(&by_key), ["infra work"]);
    let by_id = app
        .get(
            &format!("/ui/history?project={}", infra.agent.project_id.unwrap()),
            Some(&cookie),
        )
        .await;
    assert_eq!(titles(&by_id), ["infra work"]);
    let by_title = app.get("/ui/history?q=FLAKY", Some(&cookie)).await;
    assert_eq!(titles(&by_title), ["fix the flaky test"]);
    let literal = app.get("/ui/history?q=%25+sure_", Some(&cookie)).await;
    assert_eq!(titles(&literal), ["100% sure_thing"]);
    let blank = app.get("/ui/history?q=+", Some(&cookie)).await;
    assert_eq!(titles(&blank).len(), 3);
    let missing = app.get("/ui/history?project=nope", Some(&cookie)).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn history_next_link_keeps_filters() {
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    for i in 0..51 {
        let t = app.task(&infra, &format!("a&b {i}"), None).await;
        app.complete(&infra, t.id).await;
        app.clock.advance(time::Duration::minutes(1));
    }
    let cookie = app.login().await;
    let pid = infra.agent.project_id.unwrap();
    let page = app
        .get(&format!("/ui/history?project={pid}&q=a%26b"), Some(&cookie))
        .await;
    let next = href_of(&page, "a[rel=next]");
    assert!(next.starts_with("/ui/history?"), "{next}");
    assert!(next.contains(&format!("project={pid}")), "{next}");
    assert!(next.contains("q=a%26b"), "{next}");
    assert!(next.contains("after="), "{next}");
    let p2 = app.get(&next, Some(&cookie)).await;
    assert_eq!(select_texts(&p2, ".history-row .title"), ["a&b 0"]);
}

#[tokio::test]
async fn ack_these_clears_the_auto_acked_count() {
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    for title in ["old one", "old two"] {
        let t = app.task(&infra, title, None).await;
        app.complete(&infra, t.id).await;
    }
    app.clock.advance(time::Duration::days(8));
    let recent = app.task(&infra, "recent", None).await;
    app.complete(&infra, recent.id).await;
    let cookie = app.login().await;

    let all = app.get("/ui/history", Some(&cookie)).await;
    assert!(select_texts(&all, "form.ack-these").is_empty());

    let page = app.get("/ui/history?unseen=auto", Some(&cookie)).await;
    assert_eq!(select_texts(&page, ".history-row .title").len(), 2);
    assert_eq!(select_texts(&page, "form.ack-these button"), ["Ack these"]);
    let ids = hidden_values(&page, "form.ack-these input[name=id]");
    assert_eq!(ids.len(), 2);
    let back = hidden_values(&page, "form.ack-these input[name=back]");
    assert_eq!(back, ["/ui/history?unseen=auto"]);
    let csrf = csrf_of(&page);
    let mut fields = vec![("csrf", csrf.as_str()), ("back", back[0].as_str())];
    fields.extend(ids.iter().map(|i| ("id", i.as_str())));
    let post = app
        .post_form(
            "/ui/done/ack",
            Some(&cookie),
            &serde_urlencoded::to_string(&fields).unwrap(),
        )
        .await;
    assert_eq!(post.status, StatusCode::SEE_OTHER);
    assert_eq!(post.location.as_deref(), Some("/ui/history?unseen=auto"));

    let board = app.get("/ui/board", Some(&cookie)).await;
    assert!(select_texts(&board, ".auto-acked").is_empty());
    assert_eq!(
        select_texts(&board, ".column[data-status=done] .card-title"),
        ["recent"]
    );
    let after = app.get("/ui/history?unseen=auto", Some(&cookie)).await;
    assert!(select_texts(&after, ".history-row").is_empty());
    assert!(select_texts(&after, "form.ack-these").is_empty());
}

#[tokio::test]
async fn auto_unseen_honours_the_project_filter() {
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let web = app.agent("github.com/acme/web", "mac", "codex").await;
    for (who, title) in [(&infra, "infra old"), (&web, "web old")] {
        let t = app.task(who, title, None).await;
        app.complete(who, t.id).await;
    }
    app.clock.advance(time::Duration::days(8));
    let cookie = app.login().await;
    let pid = infra.agent.project_id.unwrap();
    let page = app
        .get(
            &format!("/ui/history?unseen=auto&project={pid}"),
            Some(&cookie),
        )
        .await;
    assert_eq!(select_texts(&page, ".history-row .title"), ["infra old"]);
    let back = hidden_values(&page, "form.ack-these input[name=back]");
    assert_eq!(back, [format!("/ui/history?unseen=auto&project={pid}")]);
}

#[tokio::test]
async fn history_is_in_the_nav_and_requires_login() {
    let app = app().await;
    let signed_out = app.get("/ui/history", None).await;
    assert!(
        signed_out
            .location
            .as_deref()
            .unwrap()
            .starts_with("/ui/login")
    );
    let cookie = app.login().await;
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert!(select_texts(&board, "nav.tabs a").contains(&"History".to_string()));
    let history = app.get("/ui/history", Some(&cookie)).await;
    assert_eq!(
        select_texts(&history, "nav.tabs a[aria-current=page]"),
        ["History"]
    );
}

#[tokio::test]
async fn bulk_ack_forms_link_each_id_to_its_card_for_the_filter() {
    let app = app().await;
    let (_, cookie, _) = board_with_done(&app, &["a", "b"]).await;
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        select_texts(
            &board,
            ".column[data-status=done] form.ack-all[data-bulk-ack]"
        )
        .len(),
        1
    );
    let ids = hidden_values(&board, "form.ack-all input[name=id]");
    let linked = hidden_values(&board, "form.ack-all input[name=id][data-ack-for]");
    assert_eq!(ids.len(), 2);
    assert_eq!(ids, linked);
    for id in &ids {
        assert_eq!(
            hidden_values(
                &board,
                &format!("form.ack-all input[data-ack-for=\"{id}\"]")
            )
            .len(),
            1
        );
        assert_eq!(select_texts(&board, &format!("article#task-{id}")).len(), 1);
    }
}

#[tokio::test]
async fn history_bulk_ack_links_each_id_to_its_row() {
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let t = app.task(&infra, "old", None).await;
    app.complete(&infra, t.id).await;
    app.clock.advance(time::Duration::days(8));
    let recent = app.task(&infra, "recent", None).await;
    app.complete(&infra, recent.id).await;
    let cookie = app.login().await;
    let page = app.get("/ui/history?unseen=auto", Some(&cookie)).await;
    let ids = hidden_values(&page, "form.ack-these[data-bulk-ack] input[name=id]");
    assert!(!ids.is_empty());
    assert_eq!(
        ids,
        hidden_values(&page, "form.ack-these input[name=id][data-ack-for]")
    );
    for id in &ids {
        assert_eq!(select_texts(&page, &format!("li#task-{id}")).len(), 1);
    }
}

#[tokio::test]
async fn per_card_ack_forms_link_to_their_card_for_the_filter() {
    let app = app().await;
    let (_, cookie, _) = board_with_done(&app, &["a", "b"]).await;
    let board = app.get("/ui/board", Some(&cookie)).await;
    let ids = hidden_values(&board, "form.ack-all input[name=id]");
    assert_eq!(ids.len(), 2);
    assert_eq!(select_texts(&board, "form.ack[data-ack-for]").len(), 2);
    for id in &ids {
        assert_eq!(
            select_texts(&board, &format!("form.ack[data-ack-for=\"{id}\"]")).len(),
            1
        );
        assert_eq!(select_texts(&board, &format!("article#task-{id}")).len(), 1);
    }
}

#[test]
fn app_js_hides_per_card_ack_with_its_card_and_reapplies_on_swap() {
    let js = include_str!("../static/app.js");
    assert!(js.contains("form.ack[data-ack-for]"));
    assert!(js.contains("form.hidden = "));
    assert!(js.contains("htmx:afterSwap"));
    assert!(js.contains("htmx:oobAfterSwap"));
}

#[test]
fn app_js_filters_bulk_ack_ids() {
    let js = include_str!("../static/app.js");
    assert!(js.contains("form[data-bulk-ack]"));
    assert!(js.contains("input[data-ack-for]"));
    assert!(js.contains("addEventListener(\"submit\""));
    assert!(js.contains("updateBulkAck("));
}

// ---------------------------------------------------------------- rendered markdown

/// Agent markdown with hostile HTML, a script link and a remote image mixed in.
const HOSTILE_MD: &str = "# Steps\n\n1. **add** the `bucket`\n2. read it\n\n\
                          | a | b |\n|---|---|\n| 1 | 2 |\n\n\
                          <script>alert(1)</script>\n\n\
                          [x](javascript:alert(1)) ![p](http://evil/x.png) <img src=x onerror=alert(2)>";

fn assert_inert(page: &Page, at: &str) {
    assert!(!page.body.contains("<script>alert"), "{at}");
    assert!(!page.body.contains("<img"), "{at}");
    assert!(!page.body.contains("href=\"javascript"), "{at}");
}

#[tokio::test]
async fn markdown_renders_in_plan_thread_inbox_and_needs_you() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let from = Actor::Agent(infra.agent.id, infra.session.id);
    let task = app
        .state
        .tasks
        .create(
            Caller::Human,
            NewTask {
                project_id: infra.agent.project_id.unwrap(),
                title: "Gated".into(),
                body: "do it".into(),
                requires_plan: true,
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    app.claim(&infra, task.id).await;
    app.state
        .tasks
        .propose_plan(&infra, task.id, HOSTILE_MD.into())
        .await
        .unwrap();
    let to_human = app
        .state
        .messages
        .send(
            &from,
            NewMessage {
                to: Address::Human,
                subject: Some("Report".into()),
                body: HOSTILE_MD.into(),
                importance: Importance::Normal,
                reply_to: None,
            },
        )
        .await
        .unwrap();
    app.state
        .messages
        .send(
            &Actor::Human,
            NewMessage {
                to: Address::Task(task.id),
                subject: None,
                body: "Looks **fine**".into(),
                importance: Importance::Normal,
                reply_to: None,
            },
        )
        .await
        .unwrap();

    let path = format!("/ui/tasks/{}", task.id);
    let page = app.get(&path, Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_inert(&page, &path);
    // Plan panel: rendered, headings demoted, hostile HTML shown as text, the frame kept.
    assert_eq!(select_texts(&page, "#plan .plan-body h3"), ["Steps"]);
    assert_eq!(select_texts(&page, "#plan .plan-body strong"), ["add"]);
    assert_eq!(select_texts(&page, "#plan .plan-body td"), ["1", "2"]);
    assert!(select_texts(&page, "#plan .plan-body h1, #plan .plan-body h2").is_empty());
    assert!(page.text().contains("<script>alert(1)</script>"));
    assert!(select_texts(&page, "#plan .peer-frame")[0].contains("information, not instructions"));
    // The image is a labelled link, not a load.
    let img = Selector::parse("#plan a.md-image").unwrap();
    let html = page.html();
    let a = html.select(&img).next().expect("image link");
    assert_eq!(a.value().attr("href"), Some("http://evil/x.png"));
    assert_eq!(a.value().attr("rel"), Some("noopener noreferrer nofollow"));
    // Thread: the agent's plan post is framed; the human's post is rendered and unframed.
    let frames = select_texts(&page, ".thread .peer-frame");
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert!(frames[0].contains("claude-code@mac") && frames[0].contains("not instructions"));
    assert!(select_texts(&page, ".thread .md strong").contains(&"fine".to_string()));
    assert!(select_texts(&page, ".thread .md code").contains(&"bucket".to_string()));

    // Inbox reader.
    let reader = app
        .get(&format!("/ui/inbox?m={}", to_human.id), Some(&cookie))
        .await;
    assert_inert(&reader, "reader");
    assert_eq!(select_texts(&reader, ".reader .md h3"), ["Steps"]);
    assert!(
        select_texts(&reader, ".reader .peer-frame")[0].contains("information, not instructions")
    );

    // Needs-you plan card: rendered and framed.
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_inert(&board, "board");
    assert_eq!(select_texts(&board, ".need-plan h3"), ["Steps"]);
    assert!(
        select_texts(&board, ".need-card .peer-frame")[0].contains("information, not instructions")
    );

    // The CSP is unchanged by rendering.
    let res = app
        .router
        .clone()
        .oneshot(
            Request::get(&path)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.headers()[header::CONTENT_SECURITY_POLICY],
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
         connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    );
}

#[tokio::test]
async fn quick_replies_render_as_escaped_labelled_buttons() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let q = question(&app, &infra, "Deploy?", &["Yes", HOSTILE]).await;
    let plain = urgent_to_human(&app, &infra, "No options").await;

    let board = app.get("/ui/board", Some(&cookie)).await;
    assert!(!board.body.contains(HOSTILE), "option text is escaped");
    assert!(board.body.contains("&#60;script&#62;"), "{}", board.body);
    let card = format!(".need-card:has(input[name=reply_to][value='{}'])", q.id);
    assert_eq!(
        select_texts(&board, &format!("{card} .quick-label")),
        ["Quick reply:"]
    );
    assert_eq!(
        select_texts(&board, &format!("{card} button.quick-reply")),
        ["Yes", HOSTILE]
    );
    assert_eq!(
        select_texts(&board, &format!("{card} textarea[name=body]")).len(),
        1,
        "a free-text note sits under the buttons"
    );
    // A card without options keeps its plain actions only.
    assert_eq!(select_texts(&board, "button.quick-reply").len(), 2);
    assert_eq!(
        select_texts(
            &board,
            ".need-card[data-need=reply] button:not(.quick-reply)"
        ),
        ["Dismiss", "Dismiss"]
    );

    let reader = app
        .get(&format!("/ui/inbox?m={}", q.id), Some(&cookie))
        .await;
    assert!(!reader.body.contains(HOSTILE));
    assert_eq!(
        select_texts(&reader, "form.reply .quick-label"),
        ["Quick reply:"]
    );
    assert_eq!(
        select_texts(&reader, "form.reply button.quick-reply"),
        ["Yes", HOSTILE]
    );
    // Buttons send without the typed text being required; the text box stays.
    assert_eq!(
        select_texts(&reader, "form.reply button.quick-reply[formnovalidate]").len(),
        2
    );
    assert_eq!(
        select_texts(&reader, "form.reply textarea#reply-body").len(),
        1
    );
    let none = app
        .get(&format!("/ui/inbox?m={}", plain.id), Some(&cookie))
        .await;
    assert!(select_texts(&none, ".quick-replies").is_empty());
}

#[tokio::test]
async fn clicking_a_quick_reply_replies_and_archives() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let first = question(&app, &infra, "Deploy?", &["Yes", "No"]).await;
    let second = question(&app, &infra, "Rollback?", &["Roll back", "Hold"]).await;
    let csrf = csrf_for(&app, &cookie).await;
    let to = infra.agent.id.to_string();
    let reply = |id: &str, option: &str, body: &str, back: &str| {
        form(
            &csrf,
            &[
                ("to_kind", "agent"),
                ("to", &to),
                ("reply_to", id),
                ("subject", "Re: x"),
                ("option", option),
                ("body", body),
                ("back", back),
            ],
        )
    };

    // CSRF is required.
    let first_id = first.id.to_string();
    let refused = app
        .post_form(
            "/ui/messages",
            Some(&cookie),
            &serde_urlencoded::to_string([
                ("to_kind", "agent"),
                ("to", to.as_str()),
                ("reply_to", first_id.as_str()),
                ("option", "Yes"),
            ])
            .unwrap(),
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(agent_bodies(&app, &infra).await.is_empty());

    // The option alone is the reply, threaded and archiving the human's copy; the card's
    // form goes back to its page.
    let page = app
        .post_form(
            "/ui/messages",
            Some(&cookie),
            &reply(&first_id, "Yes", "", "/ui/board"),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert_eq!(page.location.as_deref(), Some("/ui/board#needs-you"));
    let mail = app
        .state
        .messages
        .inbox(&infra, axle::service::messages::InboxFilter::default())
        .await
        .unwrap();
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].message.body, "Yes");
    assert_eq!(mail[0].message.reply_to, Some(first.id));
    assert!(mail[0].message.options.is_empty());
    assert!(app.state.store.message_receipts(&[first.id]).await.unwrap()[0].replied);
    assert_eq!(need_titles(&app, &cookie).await.0, ["Rollback?"]);

    // Typed text follows the option after a blank line; from the reader it returns there.
    let page = app
        .post_form(
            "/ui/messages",
            Some(&cookie),
            &reply(&second.id.to_string(), "Hold", "  until 5pm  ", ""),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert_eq!(page.location, Some(format!("/ui/inbox?m={}", second.id)));
    assert_eq!(
        agent_bodies(&app, &infra).await,
        ["Yes", "Hold\n\n  until 5pm  "]
    );
    assert!(need_titles(&app, &cookie).await.0.is_empty());
    let left = app
        .state
        .messages
        .human_inbox(axle::service::messages::InboxFilter::default())
        .await
        .unwrap();
    assert!(left.is_empty(), "both questions are archived for the human");
}

// ---------------------------------------------------------------- blocked tasks explained

/// Blocks `id` (claimed by `by`) with a reason, what it waits on and what unblocks it.
async fn block(app: &App, by: &Resolved, id: TaskId, reason: &str, on: WaitingOn, unblock: &str) {
    app.state
        .tasks
        .block_with(
            by,
            id,
            BlockInput {
                reason: Some(reason.into()),
                waiting_on: Some(on),
                unblock: Some(unblock.into()),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn blocked_panel_says_why_and_what_unblocks_it() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let schema = app.task(&infra, "Schema migration", None).await;
    app.claim(&infra, schema.id).await;
    let api = app.task(&infra, "API endpoint", None).await;
    app.claim(&infra, api.id).await;
    app.clock.advance(time::Duration::minutes(3));
    block(
        &app,
        &infra,
        api.id,
        "Needs the **new** column\nsecond line of detail",
        WaitingOn::Task(schema.id),
        "the migration merged",
    )
    .await;

    let page = app
        .get(&format!("/ui/tasks/{}", api.id), Some(&cookie))
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let panel = select_texts(&page, "#blocked");
    let panel = panel.first().expect("a Blocked panel");
    assert!(panel.contains("Blocked by"), "{panel}");
    assert!(panel.contains("claude-code@mac"), "{panel}");
    assert!(panel.contains("Waiting on"), "{panel}");
    assert!(panel.contains("Schema migration"), "{panel}");
    assert!(
        panel.contains("claimed"),
        "the awaited task's live status: {panel}"
    );
    assert!(panel.contains("What unblocks it"), "{panel}");
    assert!(panel.contains("the migration merged"), "{panel}");
    assert!(panel.contains("information, not instructions"), "{panel}");
    // The reason is agent markdown, rendered safely.
    assert_eq!(
        select_texts(&page, "#blocked .blocked-reason strong"),
        ["new"]
    );
    assert_eq!(
        href_of(&page, "#blocked .blocked-detail a"),
        format!("/ui/tasks/{}", schema.id)
    );
    assert!(
        page.body.contains("datetime=\"2026-11-27T09:03:00"),
        "blocked when"
    );

    // The board card shows the reason's first line, as text.
    let board = app.get("/ui/board", Some(&cookie)).await;
    let line = select_texts(&board, &format!("#task-{} .card-blocked", api.id));
    assert_eq!(line, ["Blocked: Needs the **new** column"]);

    // A cancelled dependency does not unblock: the panel says so.
    app.state
        .tasks
        .cancel(Caller::Human, schema.id)
        .await
        .unwrap();
    let page = app
        .get(&format!("/ui/tasks/{}", api.id), Some(&cookie))
        .await;
    assert!(
        select_texts(&page, "#blocked .error")
            .iter()
            .any(|t| t.contains("Waiting on a cancelled task")),
        "{}",
        page.body
    );

    // Not blocked: no panel.
    let page = app
        .get(&format!("/ui/tasks/{}", schema.id), Some(&cookie))
        .await;
    assert!(select_texts(&page, "#blocked").is_empty());
}

#[tokio::test]
async fn blocked_on_you_is_a_needs_you_card_with_a_reply_box() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = app.task(&infra, "Rotate the key", None).await;
    app.claim(&infra, task.id).await;
    block(
        &app,
        &infra,
        task.id,
        "Needs a decision",
        WaitingOn::Human,
        "say which key to rotate",
    )
    .await;

    let board = app.get("/ui/board", Some(&cookie)).await;
    let card = select_texts(&board, "#needs-you .need-card[data-need=blocked]");
    let card = card.first().expect("a Blocked on you card");
    assert!(card.contains("Blocked on you"), "{card}");
    assert!(card.contains("Rotate the key"), "{card}");
    assert!(card.contains("Needs: say which key to rotate"), "{card}");
    assert_eq!(
        select_texts(&board, "a.pulse-item[href='/ui/board#needs-you'] .num"),
        ["1"],
        "the top bar counts it"
    );
    let action = Selector::parse("#needs-you .need-reply").unwrap();
    let html = board.html();
    let form_el = html.select(&action).next().expect("a reply box");
    assert_eq!(
        form_el.value().attr("action"),
        Some(format!("/ui/tasks/{}/post", task.id).as_str())
    );
    assert_eq!(
        select_texts(&board, &format!("#task-{} .card-blocked-label", task.id)),
        ["Blocked on you:"]
    );
    let page = app
        .get(&format!("/ui/tasks/{}", task.id), Some(&cookie))
        .await;
    assert_eq!(select_texts(&page, "#blocked h2"), ["Blocked on you"]);

    // The reply needs CSRF.
    let path = format!("/ui/tasks/{}/post", task.id);
    let refused = app
        .post_form(
            &path,
            Some(&cookie),
            "body=rotate+the+old+one&back=%2Fui%2Fboard",
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    let csrf = csrf_for(&app, &cookie).await;
    let sent = app
        .post_form(
            &path,
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("body", "Rotate the old one"),
                    ("importance", "high"),
                    ("back", "/ui/board"),
                ],
            ),
        )
        .await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER, "{}", sent.body);
    assert_eq!(sent.location.as_deref(), Some("/ui/board#needs-you"));
    let thread = app.state.messages.thread(task.id).await.unwrap();
    let last = thread.last().unwrap();
    assert_eq!(last.from, Actor::Human);
    assert_eq!(last.body, "Rotate the old one");
    assert_eq!(last.to, Address::Task(task.id));
    let got = app
        .state
        .messages
        .inbox(&infra, axle::service::messages::InboxFilter::default())
        .await
        .unwrap();
    assert!(got.iter().any(|i| i.message.body == "Rotate the old one"));
}

#[tokio::test]
async fn hostile_block_text_is_escaped() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = app.task(&infra, "Hostile", None).await;
    app.claim(&infra, task.id).await;
    let evil = "<script>alert(1)</script><img src=x onerror=alert(2)>";
    block(
        &app,
        &infra,
        task.id,
        evil,
        WaitingOn::External(evil.into()),
        evil,
    )
    .await;
    for path in ["/ui/board".to_string(), format!("/ui/tasks/{}", task.id)] {
        let page = app.get(&path, Some(&cookie)).await;
        assert!(!page.body.contains("<script>alert"), "{path}");
        assert!(!page.body.contains("<img src=x"), "{path}");
    }
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(select_texts(&board, ".card-blocked-reason"), [evil]);
    let page = app
        .get(&format!("/ui/tasks/{}", task.id), Some(&cookie))
        .await;
    let panel = select_texts(&page, "#blocked");
    assert!(
        panel[0].contains("<script>alert(1)</script>"),
        "shown as text"
    );
}

// ---------------------------------------------------------------- board declutter

/// The `name` attribute of every element matching `css`.
fn attrs(page: &Page, css: &str, name: &str) -> Vec<String> {
    let sel = Selector::parse(css).unwrap();
    page.html()
        .select(&sel)
        .filter_map(|e| e.value().attr(name).map(str::to_string))
        .collect()
}

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

#[tokio::test]
async fn board_columns_show_twelve_with_the_true_count_and_show_more() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    for i in 1..=15 {
        app.task(&infra, &format!("Open task {i:02}"), None).await;
    }
    let res = app
        .router
        .clone()
        .oneshot(
            Request::get("/ui/board")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap(),
        CSP,
        "the CSP is unchanged"
    );

    let page = app.get("/ui/board", Some(&cookie)).await;
    let open = ".column[data-status=open]";
    assert_eq!(select_texts(&page, &format!("{open} .card")).len(), 12);
    assert_eq!(
        select_texts(&page, &format!("{open} > header .count")),
        ["15"],
        "the header counts every open task"
    );
    assert_eq!(
        attrs(&page, open, "data-capped"),
        [""],
        "a capped column says so"
    );
    assert_eq!(
        select_texts(&page, &format!("{open} .show-more")),
        ["Show 3 more"]
    );
    assert_eq!(
        attrs(&page, &format!("{open} .show-more"), "href"),
        ["/ui/board?expand=open"],
        "without JS the link renders the board with the column uncapped"
    );
    assert_eq!(
        attrs(&page, &format!("{open} .show-more"), "hx-get"),
        ["/ui/board/columns?expand=open"]
    );
    assert_eq!(
        attrs(&page, &format!("{open} .show-more"), "hx-push-url"),
        ["/ui/board?expand=open"]
    );
    assert!(
        select_texts(&page, ".column[data-status=claimed] .show-more").is_empty(),
        "an empty column has no link"
    );

    // The plain link: the whole board with Open uncapped and marked expanded.
    let all = app.get("/ui/board?expand=open", Some(&cookie)).await;
    assert_eq!(all.status, StatusCode::OK);
    assert_eq!(select_texts(&all, &format!("{open} .card")).len(), 15);
    assert!(select_texts(&all, &format!("{open} .show-more")).is_empty());
    assert_eq!(attrs(&all, open, "data-expanded"), [""]);
    assert!(attrs(&all, open, "data-capped").is_empty());
    assert_eq!(
        select_texts(&all, &format!("{open} .show-fewer")),
        ["Show top 12"]
    );
    assert_eq!(
        attrs(&all, &format!("{open} .show-fewer"), "href"),
        ["/ui/board"]
    );

    // The fragment the htmx link fetches: the same uncapped column, no page chrome.
    let frag = app
        .get("/ui/board/columns?expand=open", Some(&cookie))
        .await;
    assert_eq!(frag.status, StatusCode::OK);
    assert!(!frag.body.contains("<html"));
    assert_eq!(select_texts(&frag, &format!("{open} .card")).len(), 15);

    // Expansion keeps the project filter.
    let project = infra.agent.project_id.unwrap();
    let filtered = app
        .get(&format!("/ui/board?project={project}"), Some(&cookie))
        .await;
    assert_eq!(
        attrs(&filtered, &format!("{open} .show-more"), "href"),
        [format!("/ui/board?project={project}&expand=open")]
    );
}

#[tokio::test]
async fn capped_columns_put_what_needs_attention_first() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let mut plain = Vec::new();
    for i in 1..=12 {
        plain.push(app.task(&infra, &format!("Plain {i:02}"), None).await);
    }
    // Claimed first, so its lease runs low while the others are fresh.
    let expiring = app.task(&infra, "Lease running out", None).await;
    app.claim(&infra, expiring.id).await;
    app.clock.advance(time::Duration::minutes(13));
    for t in &plain {
        app.claim(&infra, t.id).await;
    }
    // A human-created task whose thread holds urgent mail for the human.
    let urgent = app
        .state
        .tasks
        .create(
            Caller::Human,
            NewTask {
                project_id: infra.agent.project_id.unwrap(),
                title: "Urgent thread".into(),
                body: "body".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    app.claim(&infra, urgent.id).await;
    app.state
        .messages
        .send(
            &Actor::Agent(infra.agent.id, infra.session.id),
            NewMessage {
                to: Address::Task(urgent.id),
                subject: Some("Prod is down".into()),
                body: "Need a call now".into(),
                importance: Importance::Urgent,
                reply_to: None,
            },
        )
        .await
        .unwrap();
    submitted_plan(&app, &infra, "Plan waits on you").await;
    // A parent whose subtask is blocked.
    let parent = app
        .task(&infra, "Parent with a blocked subtask", None)
        .await;
    let child = app.task(&infra, "stuck child", Some(parent.id)).await;
    app.claim(&infra, parent.id).await;
    app.claim(&infra, child.id).await;
    app.state
        .tasks
        .block(&infra, child.id, "waiting")
        .await
        .unwrap();

    let page = app.get("/ui/board", Some(&cookie)).await;
    let claimed = select_texts(&page, ".column[data-status=claimed] .card-title");
    assert_eq!(claimed.len(), 12, "{claimed:?}");
    assert_eq!(
        claimed[..5],
        [
            "Urgent thread",
            "Plan waits on you",
            "Parent with a blocked subtask",
            "Lease running out",
            "Plain 01",
        ],
        "{claimed:?}"
    );
    assert_eq!(
        select_texts(&page, ".column[data-status=claimed] > header .count"),
        ["16"]
    );
    assert_eq!(
        select_texts(&page, ".column[data-status=claimed] .show-more"),
        ["Show 4 more"]
    );
    // Uncapped, the column keeps the same order and shows the rest after.
    let all = app.get("/ui/board?expand=claimed", Some(&cookie)).await;
    let claimed = select_texts(&all, ".column[data-status=claimed] .card-title");
    assert_eq!(claimed.len(), 16);
    assert_eq!(claimed[3], "Lease running out");
    assert_eq!(claimed[15], "Plain 12");
}

#[tokio::test]
async fn compact_cards_drop_the_detail_and_keep_the_signals() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let codex = app.agent(INFRA, "build-01", "codex").await;
    let parent = app.task(&infra, "Rotate DB creds", None).await;
    let a = app.task(&infra, "update secret", Some(parent.id)).await;
    let b = app.task(&infra, "roll users", Some(parent.id)).await;
    let _c = app.task(&infra, "update ES", Some(parent.id)).await;
    app.complete(&codex, a.id).await;
    app.claim(&infra, b.id).await;
    app.claim(&infra, parent.id).await;
    let lone = app.task(&infra, "Stuck alone", None).await;
    app.claim(&infra, lone.id).await;
    app.state
        .tasks
        .block(&infra, lone.id, "waiting on the vendor\nmore detail")
        .await
        .unwrap();

    // Detailed (the default) offers Compact.
    let page = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(select_texts(&page, ".density a"), ["Detailed", "Compact"]);
    assert_eq!(
        select_texts(&page, ".density a[aria-current=true]"),
        ["Detailed"]
    );
    assert!(!select_texts(&page, ".card .stations").is_empty());

    // Choosing Compact sets a cookie and returns to the board (filter and expansion kept).
    let set = app
        .get("/ui/board/density?set=compact&expand=open", Some(&cookie))
        .await;
    assert_eq!(set.status, StatusCode::SEE_OTHER);
    assert_eq!(set.location.as_deref(), Some("/ui/board?expand=open"));
    let density = set.set_cookie.expect("a density cookie");
    assert!(density.starts_with("axl_density=compact;"), "{density}");
    assert!(density.contains("Path=/ui") && density.contains("SameSite=Strict"));
    let both = format!("{cookie}; {}", cookie_pair(&density));

    let page = app.get("/ui/board", Some(&both)).await;
    assert_eq!(
        select_texts(&page, ".density a[aria-current=true]"),
        ["Compact"]
    );
    let card = format!("#task-{}", parent.id);
    assert_eq!(attrs(&page, &card, "data-density"), ["compact"]);
    assert!(select_texts(&page, &format!("{card} .stations")).is_empty());
    assert!(select_texts(&page, &format!("{card} .owners")).is_empty());
    assert!(select_texts(&page, &format!("{card} details.children")).is_empty());
    assert!(select_texts(&page, &format!("{card} .card-owner")).is_empty());
    assert!(select_texts(&page, &format!("{card} .card-parent")).is_empty());
    assert!(select_texts(&page, &format!("{card} .lease-meter")).is_empty());
    // One row: state with its word, the title with its tooltip, project key (all projects),
    // holder, lease, the subtask count.
    assert_eq!(
        select_texts(&page, &format!("{card} .state .status")),
        ["claimed"],
        "the status word stays beside the glyph"
    );
    assert_eq!(
        attrs(&page, &format!("{card} .card-title"), "title"),
        ["Rotate DB creds"]
    );
    assert_eq!(
        select_texts(&page, &format!("{card} .card-title a")),
        ["Rotate DB creds"]
    );
    assert_eq!(
        select_texts(&page, &format!("{card} .project")),
        ["acme/platform-infra"]
    );
    assert_eq!(
        select_texts(&page, &format!("{card} .owner")),
        ["claude-code@mac"]
    );
    assert_eq!(
        attrs(&page, &format!("{card} .row-lease"), "data-lease-state").len(),
        1
    );
    assert_eq!(select_texts(&page, &format!("{card} .ratio")), ["1/3"]);
    assert_eq!(
        attrs(&page, &format!("{card} .ratio"), "title"),
        ["1 of 3 subtasks done"]
    );
    // No body preview, no blocked line: the reason is only in the tooltip.
    assert!(select_texts(&page, &format!("{card} .card-blocked")).is_empty());
    let stuck = format!("#task-{}", lone.id);
    assert!(select_texts(&page, &format!("{stuck} .card-blocked")).is_empty());
    assert_eq!(
        attrs(&page, &format!("{stuck} .state"), "title"),
        ["Blocked: waiting on the vendor"]
    );
    assert!(!page.body.contains("more detail"));
    // An unowned open task says so.
    let open = app.task(&infra, "Nobody yet", None).await;
    let page = app.get("/ui/board", Some(&both)).await;
    assert_eq!(
        select_texts(&page, &format!("#task-{} .owner", open.id)),
        ["open"]
    );
    // A compact card re-fetches itself on its live event rather than taking the shared
    // (detailed) render.
    assert!(attrs(&page, &card, "sse-swap").is_empty());
    assert_eq!(
        attrs(&page, &card, "hx-get"),
        [format!("/ui/tasks/{}/card", parent.id)]
    );
    assert_eq!(
        attrs(&page, &card, "hx-trigger"),
        [format!("sse:task-{}", parent.id)]
    );
    let one = app
        .get(&format!("/ui/tasks/{}/card", parent.id), Some(&both))
        .await;
    assert_eq!(one.status, StatusCode::OK);
    assert_eq!(attrs(&one, ".card", "data-density"), ["compact"]);
    assert_eq!(select_texts(&one, ".ratio"), ["1/3"]);
    let detailed = app
        .get(&format!("/ui/tasks/{}/card", parent.id), Some(&cookie))
        .await;
    assert!(!select_texts(&detailed, ".stations").is_empty());

    // Back to Detailed.
    let set = app.get("/ui/board/density?set=detailed", Some(&both)).await;
    assert_eq!(set.location.as_deref(), Some("/ui/board"));
    assert!(set.set_cookie.unwrap().starts_with("axl_density=detailed;"));
}

#[tokio::test]
async fn compact_rows_show_only_attention_chips() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let set = app
        .get("/ui/board/density?set=compact", Some(&cookie))
        .await;
    let both = format!("{cookie}; {}", cookie_pair(&set.set_cookie.unwrap()));

    // A lease under a fifth left.
    let expiring = app.task(&infra, "Lease running out", None).await;
    app.claim(&infra, expiring.id).await;
    app.clock.advance(time::Duration::minutes(13));
    // Urgent mail in a human-created task's thread.
    let urgent = app
        .state
        .tasks
        .create(
            Caller::Human,
            NewTask {
                project_id: infra.agent.project_id.unwrap(),
                title: "Urgent thread".into(),
                body: "body".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    app.claim(&infra, urgent.id).await;
    app.state
        .messages
        .send(
            &Actor::Agent(infra.agent.id, infra.session.id),
            NewMessage {
                to: Address::Task(urgent.id),
                subject: Some("Prod is down".into()),
                body: "Need a call now".into(),
                importance: Importance::Urgent,
                reply_to: None,
            },
        )
        .await
        .unwrap();
    let plan = submitted_plan(&app, &infra, "Plan waits on you").await;
    let on_you = app.task(&infra, "Needs a decision", None).await;
    app.claim(&infra, on_you.id).await;
    block(
        &app,
        &infra,
        on_you.id,
        "Pick a region for the new bucket before I can go on with the rest",
        WaitingOn::Human,
        "answer in the thread",
    )
    .await;
    // A parent with one subtask blocked, one in review and one done.
    let parent = app.task(&infra, "Parent with subtasks", None).await;
    let stuck = app.task(&infra, "stuck child", Some(parent.id)).await;
    let checked = app.task(&infra, "checked child", Some(parent.id)).await;
    let finished = app.task(&infra, "finished child", Some(parent.id)).await;
    app.claim(&infra, parent.id).await;
    app.claim(&infra, stuck.id).await;
    app.state
        .tasks
        .block(&infra, stuck.id, "waiting")
        .await
        .unwrap();
    app.claim(&infra, checked.id).await;
    app.state
        .tasks
        .to_review(&infra, checked.id, None)
        .await
        .unwrap();
    app.complete(&infra, finished.id).await;
    let done = app.task(&infra, "Shipped it", None).await;
    app.complete(&infra, done.id).await;

    let page = app.get("/ui/board", Some(&both)).await;
    let chips = |id: TaskId| select_texts(&page, &format!("#task-{id} .chip"));
    assert_eq!(chips(urgent.id), ["urgent"]);
    assert_eq!(chips(plan.id), ["plan awaiting you"]);
    assert_eq!(
        select_texts(&page, &format!("#task-{} .state .status", plan.id)),
        ["planning"],
        "the short status word"
    );
    assert_eq!(chips(on_you.id), ["blocked on you"]);
    let tip = attrs(&page, &format!("#task-{} .chip", on_you.id), "title");
    assert_eq!(
        tip,
        ["Blocked on you: Pick a region for the new bucket before…"]
    );
    assert!(!page.body.contains("go on with the rest"));
    assert_eq!(chips(parent.id), ["1 blocked", "1 review"]);
    assert_eq!(
        select_texts(&page, &format!("#task-{} .ratio", parent.id)),
        ["1/3"]
    );
    assert!(chips(expiring.id).is_empty());
    assert_eq!(
        attrs(
            &page,
            &format!("#task-{} .row-lease", expiring.id),
            "data-lease-state"
        ),
        ["low"]
    );
    assert_eq!(
        select_texts(&page, &format!("#task-{} .row-lease", expiring.id)),
        ["lease 2m left"]
    );
    assert_eq!(
        attrs(
            &page,
            &format!("#task-{} .row-lease", urgent.id),
            "data-lease-state"
        ),
        ["ok"]
    );
    // A Done row keeps its Ack, as a small button beside it.
    assert_eq!(
        select_texts(
            &page,
            &format!("form.ack[data-ack-for=\"{}\"] button.compact-ack", done.id)
        ),
        ["Ack"]
    );

    // Filtered to one project, the project tag goes and the live re-fetch keeps that.
    let project = infra.agent.project_id.unwrap();
    let filtered = app
        .get(&format!("/ui/board?project={project}"), Some(&both))
        .await;
    let row = format!("#task-{}", urgent.id);
    assert!(select_texts(&filtered, &format!("{row} .project")).is_empty());
    assert_eq!(
        attrs(&filtered, &row, "hx-get"),
        [format!("/ui/tasks/{}/card?filtered=1", urgent.id)]
    );
    assert!(!select_texts(&page, &format!("{row} .project")).is_empty());
    let one = app
        .get(
            &format!("/ui/tasks/{}/card?filtered=1", urgent.id),
            Some(&both),
        )
        .await;
    assert!(select_texts(&one, ".project").is_empty());
    assert_eq!(select_texts(&one, ".chip"), ["urgent"]);

    // Detailed is unchanged: no chips, the full rows.
    let detailed = app.get("/ui/board", Some(&cookie)).await;
    assert!(select_texts(&detailed, ".chip").is_empty());
    assert!(select_texts(&detailed, ".compact-ack").is_empty());
    assert!(!select_texts(&detailed, &format!("#task-{} .card-owner", urgent.id)).is_empty());
    assert!(!select_texts(&detailed, &format!("#task-{} .lease-meter", urgent.id)).is_empty());
    assert!(!select_texts(&detailed, &format!("#task-{} .stations", parent.id)).is_empty());
    assert_eq!(
        select_texts(
            &detailed,
            &format!("#task-{} .card-blocked-reason", on_you.id)
        ),
        ["Pick a region for the new bucket before I can go on with the rest"]
    );
}

#[tokio::test]
async fn filtering_can_fetch_every_card() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    for i in 1..=13 {
        app.task(&infra, &format!("Open task {i:02}"), None).await;
    }
    let page = app.get("/ui/board", Some(&cookie)).await;
    // The columns region re-fetches on a filter (the script asks with expand=all).
    let trigger = attrs(&page, "#columns", "hx-trigger");
    assert!(trigger[0].contains("axl:refetch"), "{trigger:?}");
    let all = app.get("/ui/board/columns?expand=all", Some(&cookie)).await;
    assert_eq!(
        select_texts(&all, ".column[data-status=open] .card").len(),
        13
    );
    assert!(
        attrs(&all, ".column[data-status=open]", "data-expanded").is_empty(),
        "a filter's fetch does not count as expanding the column"
    );
    assert!(select_texts(&all, ".show-more").is_empty());
    let js = app.get("/ui/static/app.js", None).await;
    assert!(js.body.contains("axl:refetch") && js.body.contains("data-capped"));
}

#[tokio::test]
async fn urgent_thread_mail_and_its_archiving_refresh_the_board() {
    use axle::events::Event;
    let app = app().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let task = app
        .state
        .tasks
        .create(
            Caller::Human,
            NewTask {
                project_id: infra.agent.project_id.unwrap(),
                title: "Human task".into(),
                body: "body".into(),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    let send = |importance| {
        let app = &app;
        let infra = &infra;
        async move {
            app.state
                .messages
                .send(
                    &Actor::Agent(infra.agent.id, infra.session.id),
                    NewMessage {
                        to: Address::Task(task.id),
                        subject: None,
                        body: "x".into(),
                        importance,
                        reply_to: None,
                    },
                )
                .await
                .unwrap()
        }
    };
    let board = |frames: &[(String, String)]| frames.iter().any(|(n, _)| n == "board");
    let created = |m: &axle::domain::Message| Event::MessageCreated {
        id: m.id,
        to: m.to,
        project_id: m.project_id,
    };

    let normal = send(Importance::Normal).await;
    let frames = axle::ui::views::live_frames(&app.state, &created(&normal)).await;
    assert!(
        !board(&frames),
        "ordinary thread mail does not re-rank: {frames:?}"
    );

    let urgent = send(Importance::Urgent).await;
    let frames = axle::ui::views::live_frames(&app.state, &created(&urgent)).await;
    assert!(
        board(&frames),
        "urgent thread mail re-ranks the board: {frames:?}"
    );

    let archived = Event::HumanArchived {
        archived: vec![urgent.id],
    };
    let frames = axle::ui::views::live_frames(&app.state, &archived).await;
    assert!(board(&frames), "archiving it re-ranks too: {frames:?}");
    let archived = Event::HumanArchived {
        archived: vec![normal.id],
    };
    let frames = axle::ui::views::live_frames(&app.state, &archived).await;
    assert!(!board(&frames), "{frames:?}");
}

#[tokio::test]
async fn density_links_follow_the_current_expansion() {
    let app = app().await;
    let cookie = app.login().await;
    let page = app.get("/ui/board?expand=open", Some(&cookie)).await;
    // Without JS the links carry the page's expansion.
    let hrefs = attrs(&page, ".density a", "href");
    assert!(hrefs.iter().all(|h| h.contains("expand=open")), "{hrefs:?}");
    // With JS, a click re-reads the expansion from the columns on screen.
    assert_eq!(
        attrs(&page, ".density a[data-density-link]", "href").len(),
        2
    );
    let js = app.get("/ui/static/app.js", None).await;
    assert!(js.body.contains("data-density-link"));
}

#[tokio::test]
async fn a_normal_question_needs_you_until_answered() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let q = app
        .state
        .messages
        .send_with_options(
            &Actor::Agent(infra.agent.id, infra.session.id),
            NewMessage {
                to: Address::Human,
                subject: Some("Rebase first?".into()),
                body: "which way?".into(),
                importance: Importance::Normal,
                reply_to: None,
            },
            &["Rebase".into(), "Merge".into()],
        )
        .await
        .unwrap();
    let board = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(
        select_texts(&board, "#needs-you .card-title"),
        ["Rebase first?"]
    );
    assert_eq!(
        select_texts(&board, ".need-card button.quick-reply"),
        ["Rebase", "Merge"]
    );

    // A one-click answer takes it out of Needs you.
    let csrf = csrf_of(&board);
    let page = app
        .post_form(
            "/ui/messages",
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("to_kind", "agent"),
                    ("to", &infra.agent.id.to_string()),
                    ("reply_to", &q.id.to_string()),
                    ("option", "Rebase"),
                    ("body", ""),
                ],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    let fragment = app.get("/ui/needs-you", Some(&cookie)).await;
    assert!(select_texts(&fragment, ".need-card").is_empty());
}

// ---------------------------------------------------------------- related links

#[tokio::test]
async fn task_page_lists_related_tasks_and_the_human_links_them() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let original = app.task(&infra, "Original work", None).await;
    let again = app.task(&infra, "Same work again", None).await;
    let later = app.task(&infra, "Later work", None).await;
    let csrf = csrf_for(&app, &cookie).await;
    let path = format!("/ui/tasks/{}", original.id);

    let page = app.get(&path, Some(&cookie)).await;
    assert_eq!(select_texts(&page, "[aria-label=Related] .count"), ["0"]);

    // Without the CSRF token nothing is linked.
    let page = app
        .post_form(
            &format!("{path}/related"),
            Some(&cookie),
            &format!("task={}&kind=relates", later.id),
        )
        .await;
    assert_eq!(page.status, StatusCode::FORBIDDEN);
    assert!(
        app.state
            .tasks
            .related(original.id)
            .await
            .unwrap()
            .is_empty()
    );

    // A pasted link works as well as an id.
    let page = app
        .post_form(
            &format!("/ui/tasks/{}/related", later.id),
            Some(&cookie),
            &form(
                &csrf,
                &[
                    ("task", &format!("http://axle.local{path}")),
                    ("kind", "follow-up"),
                ],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    app.clock.advance(time::Duration::seconds(1));
    let page = app
        .post_form(
            &format!("/ui/tasks/{}/related", again.id),
            Some(&cookie),
            &form(
                &csrf,
                &[("task", &original.id.to_string()), ("kind", "duplicate")],
            ),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    let again_now = app.state.store.get_task(again.id).await.unwrap().unwrap();
    assert_eq!(again_now.status, TaskStatus::Cancelled);

    let page = app.get(&path, Some(&cookie)).await;
    assert_eq!(
        select_texts(&page, ".related .related-kind"),
        ["follow-up", "duplicated by"]
    );
    let titles = select_texts(&page, ".related .child-title");
    assert!(titles[0].ends_with("Later work"), "{titles:?}");
    assert!(titles[1].ends_with("Same work again"), "{titles:?}");
    assert_eq!(
        select_texts(&page, ".related .child-state"),
        ["open", "cancelled"]
    );
    let other = app
        .get(&format!("/ui/tasks/{}", later.id), Some(&cookie))
        .await;
    assert_eq!(
        select_texts(&other, ".related .related-kind"),
        ["follow-up of"]
    );

    // Bad input is refused.
    for (task, kind) in [
        (original.id.to_string(), "relates".to_string()),
        ("not-a-task".into(), "relates".into()),
        (later.id.to_string(), "blocks".into()),
    ] {
        let page = app
            .post_form(
                &format!("{path}/related"),
                Some(&cookie),
                &form(&csrf, &[("task", &task), ("kind", &kind)]),
            )
            .await;
        assert_eq!(
            page.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{task} {kind}"
        );
    }

    // Unlink from the target's side removes it from both.
    let page = app
        .post_form(
            &format!("{path}/related/{}/remove", later.id),
            Some(&cookie),
            &form(&csrf, &[]),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    let page = app.get(&path, Some(&cookie)).await;
    assert_eq!(
        select_texts(&page, ".related .related-kind"),
        ["duplicated by"]
    );
    assert!(app.state.tasks.related(later.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn human_moves_a_task_under_a_parent() {
    let app = app().await;
    let cookie = app.login().await;
    let infra = app.agent(INFRA, "mac", "claude-code").await;
    let parent = app.task(&infra, "Parent", None).await;
    let loose = app.task(&infra, "Loose", None).await;
    let csrf = csrf_for(&app, &cookie).await;
    let path = format!("/ui/tasks/{}", loose.id);
    let page = app.get(&path, Some(&cookie)).await;
    assert_eq!(
        select_texts(&page, "form[action$='/parent'] button"),
        ["Make it a subtask"]
    );

    let page = app
        .post_form(
            &format!("{path}/parent"),
            Some(&cookie),
            &format!("parent={}", parent.id),
        )
        .await;
    assert_eq!(page.status, StatusCode::FORBIDDEN, "CSRF is required");
    let page = app
        .post_form(
            &format!("{path}/parent"),
            Some(&cookie),
            &form(&csrf, &[("parent", &format!("/ui/tasks/{}", parent.id))]),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    let page = app.get(&path, Some(&cookie)).await;
    assert!(select_texts(&page, ".crumb")[0].contains("Parent"));
    assert!(select_texts(&page, "form[action$='/parent']").is_empty());
    assert!(
        select_texts(&page, ".event-kind")
            .iter()
            .any(|k| k == "moved under")
    );
    let page = app
        .get(&format!("/ui/tasks/{}", parent.id), Some(&cookie))
        .await;
    assert!(select_texts(&page, "[aria-label=Subtasks] .child-title")[0].ends_with("Loose"));

    // A second move is refused as a conflict.
    let other = app.task(&infra, "Other", None).await;
    let page = app
        .post_form(
            &format!("{path}/parent"),
            Some(&cookie),
            &form(&csrf, &[("parent", &other.id.to_string())]),
        )
        .await;
    assert_eq!(page.status, StatusCode::CONFLICT, "{}", page.body);
}

// ---------------------------------------------------------------- delivery stage columns

mod stage_board {
    use super::*;
    use axle::domain::DeliveryStage::{self, InPr, Merged, Released};

    const WEB: &str = "github.com/acme/web";
    const SHOP: &str = "github.com/acme/shop";

    async fn set_stages(app: &App, agent: &Resolved, stages: &[DeliveryStage]) {
        let project = app
            .state
            .store
            .get_project(agent.agent.project_id.unwrap())
            .await
            .unwrap()
            .unwrap();
        app.state
            .delivery
            .set_stages(&project, stages.to_vec())
            .await
            .unwrap();
    }

    /// A task with a linked PR, completed into `delivering`.
    async fn delivering(app: &App, by: &Resolved, title: &str, pr: &str) -> Task {
        let task = app.task(by, title, None).await;
        app.claim(by, task.id).await;
        app.state
            .delivery
            .link_pr(Caller::Agent(by), task.id, pr)
            .await
            .unwrap();
        let done = app
            .state
            .tasks
            .complete(
                by,
                task.id,
                Completion {
                    summary: "done".into(),
                    ..Completion::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(done.status, TaskStatus::Delivering);
        done
    }

    fn column_names(page: &Page) -> Vec<String> {
        select_texts(page, ".column > header h2")
    }

    async fn two_projects() -> (App, String, Resolved, Resolved) {
        let app = app().await;
        let cookie = app.login().await;
        let web = app.agent(WEB, "mac", "claude-code").await;
        let shop = app.agent(SHOP, "mac", "claude-code").await;
        set_stages(&app, &web, &[InPr, Merged]).await;
        (app, cookie, web, shop)
    }

    #[tokio::test]
    async fn stage_columns_per_project() {
        let (app, cookie, web, shop) = two_projects().await;
        set_stages(&app, &shop, &[Released]).await;
        let web_id = web.agent.project_id.unwrap();

        let filtered = app
            .get(&format!("/ui/board?project={web_id}"), Some(&cookie))
            .await;
        assert_eq!(
            column_names(&filtered),
            [
                "Open", "Claimed", "Blocked", "Review", "In PR", "Merged", "Done"
            ]
        );
        let all = app.get("/ui/board", Some(&cookie)).await;
        assert_eq!(
            column_names(&all),
            [
                "Open", "Claimed", "Blocked", "Review", "In PR", "Merged", "Released", "Done"
            ]
        );
    }

    #[tokio::test]
    async fn task_sits_in_own_column() {
        let (app, cookie, web, shop) = two_projects().await;
        // The task reaches `in_pr` while the shop has it enabled, then the shop narrows to
        // Released only.
        set_stages(&app, &shop, &[InPr, Released]).await;
        delivering(
            &app,
            &shop,
            "shop work",
            "https://github.com/acme/shop/pull/3",
        )
        .await;
        set_stages(&app, &shop, &[Released]).await;
        delivering(&app, &web, "web work", "https://github.com/acme/web/pull/4").await;

        let page = app.get("/ui/board", Some(&cookie)).await;
        let html = page.html();
        let sections = Selector::parse("section.column").unwrap();
        let titles = Selector::parse(".card-title").unwrap();
        let in_column = |stage: &str| -> Vec<String> {
            html.select(&sections)
                .filter(|c| c.value().attr("data-stage") == Some(stage))
                .flat_map(|c| {
                    c.select(&titles)
                        .map(|t| t.text().collect::<String>().trim().to_string())
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        assert_eq!(in_column("in_pr"), ["web work"]);
        assert_eq!(in_column("released"), ["shop work"]);
        assert_eq!(
            select_texts(&page, ".waiting-for"),
            ["waiting for Released"]
        );
        for status in ["open", "claimed", "blocked", "review", "done"] {
            let cards =
                Selector::parse(&format!("section.column[data-status={status}] .card")).unwrap();
            assert_eq!(html.select(&cards).count(), 0, "{status}");
        }
    }

    #[tokio::test]
    async fn empty_stage_column_sentence() {
        let (app, cookie, _web, _shop) = two_projects().await;
        let page = app.get("/ui/board", Some(&cookie)).await;
        let empties = select_texts(&page, "section.column[data-stage] .empty");
        assert_eq!(
            empties,
            ["Nothing waiting for merge", "Nothing waiting for release"]
        );
    }

    #[tokio::test]
    async fn card_pr_chip_and_short_id() {
        let (app, cookie, web, _shop) = two_projects().await;
        delivering(
            &app,
            &web,
            "web work",
            "https://github.com/acme/web/pull/12",
        )
        .await;
        let page = app.get("/ui/board", Some(&cookie)).await;
        let html = page.html();
        let link = Selector::parse("a[href=\"https://github.com/acme/web/pull/12\"]").unwrap();
        let a = html.select(&link).next().expect("PR chip");
        assert_eq!(a.text().collect::<String>(), "PR #12");
        assert_eq!(a.value().attr("rel"), Some("noopener noreferrer"));
        let ids = select_texts(&page, ".short-id");
        assert!(ids.iter().any(|t| t.starts_with("axl-")), "{ids:?}");
        assert_eq!(select_texts(&page, ".state .stage-word"), ["In PR"]);
    }

    #[tokio::test]
    async fn stage_column_head_reads_label_dot_count() {
        let (app, cookie, web, _shop) = two_projects().await;
        delivering(
            &app,
            &web,
            "web work",
            "https://github.com/acme/web/pull/12",
        )
        .await;
        let page = app.get("/ui/board", Some(&cookie)).await;
        let html = page.html();
        let head = Selector::parse("section.column[data-stage=in_pr] > header").unwrap();
        let text: Vec<String> = html
            .select(&head)
            .next()
            .unwrap()
            .text()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect();
        assert_eq!(text.join(" "), "In PR · 1");
    }

    #[tokio::test]
    async fn stranded_delivering_task_sits_in_review() {
        let app = app().await;
        let cookie = app.login().await;
        let web = app.agent(WEB, "mac", "claude-code").await;
        let task = app.task(&web, "stranded", None).await;
        // Stages are off, so no path produces this; force the row.
        sqlx::query("UPDATE tasks SET status = 'delivering' WHERE id = ?")
            .bind(task.id.to_string())
            .execute(&app.pool)
            .await
            .unwrap();
        let page = app.get("/ui/board", Some(&cookie)).await;
        let html = page.html();
        let sel =
            Selector::parse("section.column[data-status=review] .card .state .status").unwrap();
        let words: Vec<String> = html
            .select(&sel)
            .map(|e| e.text().collect::<String>())
            .collect();
        assert_eq!(words, ["delivering"]);
    }

    #[tokio::test]
    async fn no_stages_board_unchanged() {
        let app = app().await;
        let cookie = app.login().await;
        let web = app.agent(WEB, "mac", "claude-code").await;
        app.task(&web, "plain", None).await;
        let page = app.get("/ui/board", Some(&cookie)).await;
        assert_eq!(
            column_names(&page),
            ["Open", "Claimed", "Blocked", "Review", "Done"]
        );
    }
}

// ---------------------------------------------------------------- task delivery panel

mod delivery_panel {
    use super::*;
    use axle::domain::DeliveryStage::{self, InPr, Merged, Released};
    use axle::store::StageMove;

    const WEB: &str = "github.com/acme/web";
    const PR: &str = "https://github.com/acme/web/pull/12";

    async fn setup(stages: &[DeliveryStage]) -> (App, String, String, Resolved) {
        let app = app().await;
        let cookie = app.login().await;
        let web = app.agent(WEB, "mac", "claude-code").await;
        let project = app
            .state
            .store
            .get_project(web.agent.project_id.unwrap())
            .await
            .unwrap()
            .unwrap();
        app.state
            .delivery
            .set_stages(&project, stages.to_vec())
            .await
            .unwrap();
        let csrf = csrf_for(&app, &cookie).await;
        (app, cookie, csrf, web)
    }

    async fn delivering(app: &App, by: &Resolved) -> Task {
        let task = app.task(by, "Ship it", None).await;
        app.claim(by, task.id).await;
        app.state
            .delivery
            .link_pr(Caller::Agent(by), task.id, PR)
            .await
            .unwrap();
        app.state
            .tasks
            .complete(
                by,
                task.id,
                Completion {
                    summary: "done".into(),
                    ..Completion::default()
                },
            )
            .await
            .unwrap()
    }

    async fn move_to(app: &App, task: &Task, to: DeliveryStage, implied: Vec<DeliveryStage>) {
        app.state
            .store
            .advance_stage(&StageMove {
                task: task.id,
                to,
                implied,
                signal_id: None,
                detail: None,
                actor: Actor::Human,
                token: None,
                now: app.state.clock.now(),
            })
            .await
            .unwrap()
            .unwrap();
    }

    fn buttons(page: &Page) -> Vec<String> {
        select_texts(page, ".delivery-panel button")
    }

    const PR_LINK: &str = ".delivery-panel a[href=\"https://github.com/acme/web/pull/12\"]";

    #[tokio::test]
    async fn delivery_panel_timeline() {
        let (app, cookie, _csrf, web) = setup(&[InPr, Merged, Released]).await;
        let task = delivering(&app, &web).await;
        move_to(&app, &task, Released, vec![Merged]).await;
        let page = app
            .get(&format!("/ui/tasks/{}", task.id), Some(&cookie))
            .await;
        assert_eq!(
            select_texts(&page, ".delivery-panel .short-id"),
            [axle::domain::short_id(task.id)]
        );
        assert_eq!(
            select_texts(&page, ".delivery-panel .stage-label"),
            ["In PR", "Merged", "Released"]
        );
        assert_eq!(
            select_texts(&page, ".delivery-panel .stage-state"),
            ["reached", "implied", "reached"]
        );
        let notes = select_texts(&page, ".delivery-panel .stage-note");
        assert!(
            notes.iter().any(|n| n == "implied by Released"),
            "{notes:?}"
        );
        assert_eq!(select_texts(&page, PR_LINK), ["PR #12"]);

        // A stage not yet reached reads waiting.
        let (app, cookie, _csrf, web) = setup(&[InPr, Merged]).await;
        let task = delivering(&app, &web).await;
        let page = app
            .get(&format!("/ui/tasks/{}", task.id), Some(&cookie))
            .await;
        assert_eq!(
            select_texts(&page, ".delivery-panel .stage-state"),
            ["reached", "waiting"]
        );
    }

    #[tokio::test]
    async fn link_pr_form() {
        let (app, cookie, csrf, web) = setup(&[InPr, Merged]).await;
        let task = app.task(&web, "Ship it", None).await;
        app.claim(&web, task.id).await;
        let path = format!("/ui/tasks/{}", task.id);

        let page = app.get(&path, Some(&cookie)).await;
        assert!(buttons(&page).contains(&"Link PR".to_string()));

        // Without CSRF nothing happens.
        let denied = app
            .post_form(
                &format!("{path}/pr"),
                Some(&cookie),
                &serde_urlencoded::to_string([("url", PR)]).unwrap(),
            )
            .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);

        // A bad URL shows the exact message in the panel.
        let bad = app
            .post_form(
                &format!("{path}/pr"),
                Some(&cookie),
                &form(&csrf, &[("url", "https://github.com/other/repo/pull/3")]),
            )
            .await;
        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", bad.body);
        assert_eq!(
            select_texts(&bad, ".delivery-panel .error"),
            ["pr must be a pull request in github.com/acme/web"]
        );

        let ok = app
            .post_form(
                &format!("{path}/pr"),
                Some(&cookie),
                &form(&csrf, &[("url", PR)]),
            )
            .await;
        assert_eq!(ok.status, StatusCode::SEE_OTHER, "{}", ok.body);
        assert_eq!(ok.location.as_deref(), Some(path.as_str()));
        let page = app.get(&path, Some(&cookie)).await;
        assert_eq!(select_texts(&page, PR_LINK), ["PR #12"]);
        assert!(buttons(&page).contains(&"Unlink PR".to_string()));

        // Unlink removes it again.
        let un = app
            .post_form(
                &format!("{path}/pr/unlink"),
                Some(&cookie),
                &form(&csrf, &[]),
            )
            .await;
        assert_eq!(un.status, StatusCode::SEE_OTHER, "{}", un.body);
        assert!(app.state.store.task_pr(task.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn advance_and_mark_done_buttons() {
        let (app, cookie, csrf, web) = setup(&[InPr, Merged, Released]).await;
        let task = delivering(&app, &web).await;
        let path = format!("/ui/tasks/{}", task.id);
        let page = app.get(&path, Some(&cookie)).await;
        let b = buttons(&page);
        assert!(b.contains(&"Advance stage".to_string()), "{b:?}");
        assert!(b.contains(&"Mark done".to_string()), "{b:?}");

        let r = app
            .post_form(&format!("{path}/advance"), Some(&cookie), &form(&csrf, &[]))
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        let t = app.state.store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(t.delivery_stage, Some(Merged));
        assert_eq!(t.status, TaskStatus::Delivering);

        let r = app
            .post_form(
                &format!("{path}/mark-done"),
                Some(&cookie),
                &form(&csrf, &[]),
            )
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        let t = app.state.store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Done);

        // Done: neither button, and a repeat is refused in the panel.
        let page = app.get(&path, Some(&cookie)).await;
        let b = buttons(&page);
        assert!(!b.contains(&"Advance stage".to_string()), "{b:?}");
        assert!(!b.contains(&"Mark done".to_string()), "{b:?}");
        let r = app
            .post_form(
                &format!("{path}/mark-done"),
                Some(&cookie),
                &form(&csrf, &[]),
            )
            .await;
        assert_eq!(r.status, StatusCode::CONFLICT);
        assert_eq!(
            select_texts(&r, ".delivery-panel .error"),
            ["this task is not delivering"]
        );
    }

    #[tokio::test]
    async fn unlink_hidden_after_merge() {
        let (app, cookie, _csrf, web) = setup(&[InPr, Merged]).await;
        let task = delivering(&app, &web).await;
        let path = format!("/ui/tasks/{}", task.id);
        // Delivering: the store refuses an unlink, so the button is not offered.
        let page = app.get(&path, Some(&cookie)).await;
        assert!(!buttons(&page).contains(&"Unlink PR".to_string()));

        move_to(&app, &task, Merged, vec![]).await;
        let page = app.get(&path, Some(&cookie)).await;
        assert!(!buttons(&page).contains(&"Unlink PR".to_string()));
    }
}
