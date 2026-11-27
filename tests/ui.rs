//! Web UI (spec §12, §7): login, CSRF, overview, board and the live stream, against
//! in-memory SQLite with oneshot requests and HTML assertions.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use coord::clock::TestClock;
use coord::config::ServerConfig;
use coord::domain::{Actor, Address, Importance, SessionId, Task, TaskId};
use coord::http::{AppState, router};
use coord::service::identity::{IdentityInput, IdentityService, Resolved};
use coord::service::messages::NewMessage;
use coord::service::tasks::{Caller, ClaimOutcome, Completion, NewTask};
use coord::store::sqlite::SqliteStore;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use scraper::{Html, Selector};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use time::macros::datetime;
use tower::ServiceExt;

const SECRET: &str = "test-secret";
const IAC: &str = "github.com/reyemtech/iac";

struct App {
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
    let store = Arc::new(SqliteStore::from_pool(pool).await.unwrap());
    let clock = Arc::new(TestClock::new(datetime!(2026-11-27 9:00 UTC)));
    let mut config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "COORD_ADMIN_SECRET" => Some(SECRET.into()),
        _ => None,
    })
    .unwrap();
    configure(&mut config);
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

/// `coord_ui=abc` from a `Set-Cookie` value.
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
    let page = app.get("/ui/", Some("coord_ui=forged")).await;
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
    assert!(set_cookie.starts_with("coord_ui="), "{set_cookie}");
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
    let app = app_with(|c| c.trusted_header = Some("x-forwarded-user".into())).await;
    assert_eq!(
        app.get("/ui/", None).await.location.as_deref(),
        Some("/ui/login")
    );
    let page = app
        .send(
            Request::get("/ui/")
                .header("x-forwarded-user", "mario")
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
                .header("x-forwarded-user", "mario")
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
                .header("x-forwarded-user", "mario")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf={csrf}")))
                .unwrap(),
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn board_columns_and_progress() {
    let app = app().await;
    let cookie = app.login().await;
    let iac = app.agent(IAC, "mac", "claude-code").await;
    let codex = app.agent(IAC, "cac-1", "codex").await;
    let parent = app.task(&iac, "Rotate DB creds", None).await;
    let a = app
        .task(&iac, "update Pulumi secret", Some(parent.id))
        .await;
    let b = app.task(&iac, "roll CNPG users", Some(parent.id)).await;
    let _c = app
        .task(&iac, "update ExternalSecret", Some(parent.id))
        .await;
    app.complete(&codex, a.id).await;
    app.claim(&iac, b.id).await;
    app.claim(&iac, parent.id).await;

    let page = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        select_texts(&page, ".column > header h2"),
        ["Open", "Claimed", "Blocked", "Review", "Done"]
    );
    let text = page.text();
    assert!(text.contains("1/3"), "{text}");
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
    assert!(rows[0].contains("update Pulumi secret") && rows[0].contains("codex@cac-1"));
    assert!(rows[1].contains("roll CNPG users") && rows[1].contains("claude-code@mac"));
    assert!(rows[2].contains("(open)"), "{rows:?}");

    let missing = app
        .get(
            &format!("/ui/tasks/{}/children", SessionId::new()),
            Some(&cookie),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn board_done_column_shows_last_seven_days() {
    let app = app().await;
    let iac = app.agent(IAC, "mac", "claude-code").await;
    let old = app.task(&iac, "old work", None).await;
    app.complete(&iac, old.id).await;
    app.clock.advance(time::Duration::days(8));
    let recent = app.task(&iac, "recent work", None).await;
    app.complete(&iac, recent.id).await;
    let cancelled = app.task(&iac, "dropped", None).await;
    app.state
        .tasks
        .cancel(Caller::Human, cancelled.id)
        .await
        .unwrap();

    let cookie = app.login().await;
    let page = app.get("/ui/board", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    let done = select_texts(&page, ".column[data-status=done] .card-title");
    assert_eq!(done, ["recent work"]);
    assert!(!page.text().contains("dropped"));
}

#[tokio::test]
async fn board_filters_by_project() {
    let app = app().await;
    let cookie = app.login().await;
    let iac = app.agent(IAC, "mac", "claude-code").await;
    let nimbus = app
        .agent("github.com/reyemtech/nimbus", "mac", "claude-code")
        .await;
    app.task(&iac, "iac work", None).await;
    app.task(&nimbus, "nimbus work", None).await;

    let all = app.get("/ui/board", Some(&cookie)).await;
    let titles = select_texts(&all, ".card-title");
    assert_eq!(titles.len(), 2, "{titles:?}");

    for filter in [IAC.to_string(), iac.agent.project_id.unwrap().to_string()] {
        let page = app
            .get(&format!("/ui/board?project={filter}"), Some(&cookie))
            .await;
        assert_eq!(page.status, StatusCode::OK);
        assert_eq!(select_texts(&page, ".card-title"), ["iac work"], "{filter}");
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
    let iac = app.agent(IAC, "mac", "claude-code").await;
    let parent = app.task(&iac, "Rotate DB creds", None).await;
    let child = app
        .task(&iac, "update Pulumi secret", Some(parent.id))
        .await;
    app.claim(&iac, child.id).await;

    for path in [
        "/ui/board".to_string(),
        format!("/ui/board?project={IAC}"),
        "/ui/".to_string(),
        format!("/ui/tasks/{}/children", parent.id),
    ] {
        let page = app.get(&path, Some(&cookie)).await;
        assert_eq!(page.status, StatusCode::OK, "{path}");
        assert!(
            page.text().contains("reyemtech/iac"),
            "{path}: {}",
            page.body
        );
        // The key may only appear in `title` attributes (shown on hover).
        let html = page.html();
        let mut keyed_titles = 0;
        for node in html.root_element().descendants() {
            if let Some(text) = node.value().as_text() {
                assert!(!text.contains(IAC), "{path}: text {:?}", &**text);
            }
            if let Some(el) = node.value().as_element() {
                for (name, value) in el.attrs() {
                    if name == "title" {
                        keyed_titles += usize::from(value.contains(IAC));
                    } else {
                        assert!(!value.contains(IAC), "{path}: {name}={value:?}");
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
    let iac = app.agent(IAC, "mac", "claude-code").await;
    let parent = app
        .task(&iac, "<script>alert(1)</script><b>x</b>", None)
        .await;
    app.task(&iac, "<img src=x onerror=alert(2)>", Some(parent.id))
        .await;
    app.claim(&iac, parent.id).await;
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
    let _stale = app.agent(IAC, "old-box", "codex").await;
    app.clock.advance(time::Duration::seconds(120));
    let mac = app.agent(IAC, "mac", "claude-code").await;
    let _mac2 = app
        .agent("github.com/reyemtech/nimbus", "mac", "opencode")
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
    assert!(mac_card.contains("reyemtech/iac"), "{mac_card}");

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
    let iac = app.agent(IAC, "mac", "claude-code").await;
    let parent = app.task(&iac, "Parent <one>", None).await;

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
    app.task(&iac, "child", Some(parent.id)).await;
    let card = wait_for_event(
        &mut stream,
        &format!("task-{}", parent.id),
        Duration::from_secs(2),
    )
    .await
    .expect("parent card event");
    assert!(card.contains("0/1"), "{card}");
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
    app.claim(&iac, parent.id).await;
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
                to: Address::Agent(iac.agent.id),
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
    let iac = app.agent(IAC, "mac", "claude-code").await;
    let mut stream = open_live(&app, &cookie).await;

    let page = app
        .post_form("/ui/logout", Some(&cookie), &format!("csrf={csrf}"))
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    app.task(&iac, "secret plans", None).await;
    let sent = drain_until_end(&mut stream, Duration::from_secs(2)).await;
    assert!(!sent.contains("event: task-"), "{sent}");
    assert!(!sent.contains("secret plans"), "{sent}");
}

#[tokio::test]
async fn live_stream_ends_when_the_session_expires() {
    let app = app().await;
    let cookie = app.login().await;
    let iac = app.agent(IAC, "mac", "claude-code").await;
    let mut stream = open_live(&app, &cookie).await;
    app.clock
        .advance(time::Duration::hours(12) + time::Duration::seconds(1));
    app.task(&iac, "after expiry", None).await;
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
