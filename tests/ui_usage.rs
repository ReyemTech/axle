//! The Usage page (`/ui/usage`) and its top-bar tab, against in-memory SQLite, seeded through
//! `state.usage.ingest`.

use std::sync::Arc;

use axle::clock::TestClock;
use axle::config::ServerConfig;
use axle::domain::{SessionId, Task};
use axle::http::{AppState, router};
use axle::service::identity::{IdentityInput, IdentityService, Resolved};
use axle::service::tasks::{Caller, ClaimOutcome, NewTask};
use axle::store::sqlite::SqliteStore;
use axle::ui::usage::{compact, usd};
use axle::usage::report::UsageReport;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use scraper::{ElementRef, Html, Selector};
use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use time::macros::datetime;
use tower::ServiceExt;

const SECRET: &str = "test-secret-0123456789";
const SENTENCE: &str = "Estimated at API prices; subscription use is not billed per token.";

struct App {
    state: AppState,
    router: Router,
    cookie: String,
}

struct Page {
    status: StatusCode,
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

    /// The text of every element matching `css`, whitespace collapsed.
    fn texts(&self, css: &str) -> Vec<String> {
        let sel = Selector::parse(css).unwrap();
        self.html().select(&sel).map(|e| squash(&e)).collect()
    }

    /// The breakdown table's body rows, each as its cells' texts.
    fn table(&self) -> Vec<Vec<String>> {
        let rows = Selector::parse("table.usage-table tbody tr").unwrap();
        let cells = Selector::parse("th, td").unwrap();
        self.html()
            .select(&rows)
            .map(|r| r.select(&cells).map(|c| squash(&c)).collect())
            .collect()
    }
}

fn squash(e: &ElementRef<'_>) -> String {
    e.text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
    let store = Arc::new(SqliteStore::from_pool(pool).await.unwrap());
    let clock = Arc::new(TestClock::new(datetime!(2026-11-27 9:00 UTC)));
    let config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some(SECRET.into()),
        _ => None,
    })
    .unwrap();
    let state = AppState::new(store, clock, config);
    let mut app = App {
        router: router(state.clone()),
        state,
        cookie: String::new(),
    };
    let req = Request::post("/ui/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("secret={SECRET}")))
        .unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let set_cookie = res.headers()[header::SET_COOKIE].to_str().unwrap();
    app.cookie = set_cookie.split(';').next().unwrap().trim().to_string();
    app
}

impl App {
    async fn get(&self, path: &str) -> Page {
        let req = Request::get(path)
            .header(header::COOKIE, &self.cookie)
            .body(Body::empty())
            .unwrap();
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Page {
            status,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

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

    async fn task(&self, by: &Resolved, title: &str) -> Task {
        self.state
            .tasks
            .create(
                Caller::Agent(by),
                NewTask {
                    project_id: by.agent.project_id.unwrap(),
                    title: title.into(),
                    body: "body".into(),
                    ..NewTask::default()
                },
            )
            .await
            .unwrap()
    }

    async fn claim(&self, by: &Resolved, task: &Task) {
        let outcome = self.state.tasks.claim(by, task.id).await.unwrap();
        assert!(matches!(outcome, ClaimOutcome::Claimed(_)), "{outcome:?}");
    }

    /// Reports one record for `by`'s session.
    async fn report(
        &self,
        by: &Resolved,
        key: &str,
        model: &str,
        billing: &str,
        (input, output): (i64, i64),
        at: &str,
    ) {
        let report = UsageReport {
            session_id: by.session.id,
            parser: "claude-code/1".into(),
            harness_version: None,
            harness_session_id: None,
            records: vec![json!({
                "key": key, "at": at, "model": model,
                "input": input, "output": output,
                "cache_write_5m": 0, "cache_write_1h": 0, "cache_read": 0, "reasoning": 0,
                "billing": billing,
            })],
        };
        let out = self.state.usage.ingest(by, &report).await.unwrap();
        assert_eq!(out.accepted, 1, "{out:?}");
    }
}

#[tokio::test]
async fn tab_and_empty_page() {
    let app = app().await;
    let overview = app.get("/ui/").await;
    let sel = Selector::parse("nav.tabs a[href=\"/ui/usage\"]").unwrap();
    let html = overview.html();
    let tab = html.select(&sel).next().expect("Usage tab");
    assert_eq!(squash(&tab), "Usage");

    let page = app.get("/ui/usage").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert_eq!(page.texts("nav.tabs a[aria-current=page]"), ["Usage"]);
    assert!(page.text().contains(SENTENCE));
    assert!(page.text().contains("No usage matches."));
    assert!(page.text().contains("UTC"));
}

#[tokio::test]
async fn totals_and_default_project_breakdown() {
    let app = app().await;
    let infra = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    let web = app.agent("github.com/acme/web", "mac", "claude-code").await;
    let at = "2026-11-27T08:00:00Z";
    app.report(
        &infra,
        "a1",
        "claude-opus-5-5",
        "api",
        (1_000_000, 10_000),
        at,
    )
    .await;
    app.report(&infra, "a2", "claude-opus-5-5", "api", (200_000, 2_345), at)
        .await;
    app.report(&web, "b1", "claude-opus-5-5", "api", (845, 0), at)
        .await;

    let page = app.get("/ui/usage").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let html = page.html();
    let input = Selector::parse("dl.usage-totals [data-total=input] dd").unwrap();
    let dd = html.select(&input).next().expect("input total");
    assert_eq!(squash(&dd), "1.20M");
    assert_eq!(dd.value().attr("title"), Some("1,200,845"));
    let output = Selector::parse("dl.usage-totals [data-total=output] dd").unwrap();
    let dd = html.select(&output).next().expect("output total");
    assert_eq!(squash(&dd), "12.3k");
    assert_eq!(dd.value().attr("title"), Some("12,345"));
    // 1_200_845 × $4/M + 12_345 × $20/M = $4.803380 + $0.246900 = $5.050280
    let cost = Selector::parse("dl.usage-totals [data-total=cost] dd").unwrap();
    let dd = html.select(&cost).next().expect("cost total");
    assert_eq!(squash(&dd), "$5.05");
    assert_eq!(dd.value().attr("title"), Some("$5.050280"));

    assert!(
        page.texts("table.usage-table th")
            .contains(&"API-equivalent cost".to_string())
    );
    assert!(
        page.texts("table.usage-table th")
            .contains(&"Share of cost".to_string())
    );
    let swatches = page.texts("table.usage-table tbody span.project[data-line]");
    assert_eq!(swatches.len(), 2, "{swatches:?}");
    let rows = page.table();
    assert_eq!(rows.len(), 2);
    assert!(rows[0][0].contains("infra"), "{rows:?}");
    assert_eq!(rows[0][1], "2");
    assert!(rows[1][0].contains("web"), "{rows:?}");
    let bars =
        Selector::parse("table.usage-table .share-bar[data-share][aria-hidden=true]").unwrap();
    assert_eq!(html.select(&bars).count(), 2);
    assert!(rows[0].last().unwrap().ends_with('%'), "{rows:?}");
}

#[tokio::test]
async fn group_by_task_shows_no_task() {
    let app = app().await;
    let a = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    app.report(
        &a,
        "k1",
        "claude-opus-5-5",
        "api",
        (100, 10),
        "2026-11-27T08:00:00Z",
    )
    .await;
    let t = app.task(&a, "Rotate the certs").await;
    app.claim(&a, &t).await;
    app.report(
        &a,
        "k2",
        "claude-opus-5-5",
        "api",
        (500, 10),
        "2026-11-27T09:00:00Z",
    )
    .await;

    let page = app.get("/ui/usage?group=task").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let names: Vec<String> = page.table().into_iter().map(|r| r[0].clone()).collect();
    assert!(names.contains(&"no task".to_string()), "{names:?}");
    assert!(names.contains(&"Rotate the certs".to_string()), "{names:?}");
    let link = format!("table.usage-table a[href=\"/ui/tasks/{}\"]", t.id);
    assert_eq!(page.texts(&link), ["Rotate the certs"]);

    // The task filter narrows to it.
    let page = app
        .get(&format!("/ui/usage?task={}&group=task", t.id))
        .await;
    let names: Vec<String> = page.table().into_iter().map(|r| r[0].clone()).collect();
    assert_eq!(names, ["Rotate the certs"]);
    let page = app.get("/ui/usage?task=none&group=task").await;
    let names: Vec<String> = page.table().into_iter().map(|r| r[0].clone()).collect();
    assert_eq!(names, ["no task"]);
}

#[tokio::test]
async fn unknown_model_shows_no_price_last() {
    let app = app().await;
    let a = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    let at = "2026-11-27T08:00:00Z";
    // The unpriced model has far more tokens, but sorts after every priced row.
    app.report(&a, "k1", "mystery-1", "api", (9_000_000, 0), at)
        .await;
    app.report(&a, "k2", "claude-opus-5-5", "api", (100, 0), at)
        .await;
    app.report(&a, "k3", "claude-haiku-4-5", "api", (1_000, 0), at)
        .await;

    let page = app.get("/ui/usage?group=model").await;
    let rows = page.table();
    assert_eq!(rows.len(), 3, "{rows:?}");
    let last = rows.last().unwrap();
    assert_eq!(last[0], "mystery-1");
    assert_eq!(last[6], "no price");
    assert_ne!(rows[0][6], "no price");

    // A group mixing priced and unpriced records says how many had no price.
    let page = app.get("/ui/usage?group=project").await;
    let rows = page.table();
    assert!(rows[0][6].ends_with("+ 1 no price"), "{rows:?}");
}

/// The Totals counter `name`: its text and its `title`.
fn total(page: &Page, name: &str) -> (String, String) {
    let html = page.html();
    let sel = Selector::parse(&format!("dl.usage-totals [data-total={name}] dd")).unwrap();
    let dd = html.select(&sel).next().expect("total");
    (squash(&dd), dd.value().attr("title").unwrap().to_string())
}

#[tokio::test]
async fn totals_with_nothing_priced_say_no_price() {
    let app = app().await;
    let a = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    let at = "2026-11-27T08:00:00Z";
    app.report(&a, "k1", "mystery-1", "api", (9_000, 0), at)
        .await;
    app.report(&a, "k2", "mystery-2", "subscription", (1_000, 0), at)
        .await;

    let page = app.get("/ui/usage?group=model").await;
    assert_eq!(total(&page, "cost"), ("no price".into(), "no price".into()));
    assert_eq!(
        total(&page, "subscription"),
        ("no price".into(), "no price".into())
    );
    // Nothing has a cost, so the share is of tokens.
    assert!(
        page.texts("table.usage-table th")
            .contains(&"Share of tokens".to_string())
    );
}

#[tokio::test]
async fn totals_with_some_unpriced_are_a_lower_bound() {
    let app = app().await;
    let a = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    let at = "2026-11-27T08:00:00Z";
    // 1_000_000 × $4/M = $4.00 on a subscription; 500_000 × $4/M = $2.00 on the API.
    app.report(
        &a,
        "s1",
        "claude-opus-5-5",
        "subscription",
        (1_000_000, 0),
        at,
    )
    .await;
    app.report(&a, "p1", "claude-opus-5-5", "api", (500_000, 0), at)
        .await;
    app.report(&a, "m1", "mystery-1", "api", (9_000_000, 0), at)
        .await;

    let page = app.get("/ui/usage?group=model").await;
    assert_eq!(
        total(&page, "cost"),
        (
            "\u{2265} $6.00 (some models unpriced)".into(),
            "\u{2265} $6.000000".into()
        )
    );
    assert_eq!(
        total(&page, "subscription"),
        (
            "\u{2265} $4.00 (some models unpriced)".into(),
            "\u{2265} $4.000000".into()
        )
    );
    assert!(
        page.texts("table.usage-table th")
            .contains(&"Share of priced cost".to_string())
    );
    let rows = page.table();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0][0], "claude-opus-5-5");
    assert_eq!(rows[0][6], "$6.00 est.");
    assert_eq!(rows[0][7], "100%");
    assert_eq!(rows[1][0], "mystery-1");
    assert_eq!(rows[1][6], "no price");
    assert_eq!(rows[1][7], "no price");

    // Without the unpriced model the totals are exact, as before.
    let page = app.get("/ui/usage?model=claude-opus-5-5").await;
    assert_eq!(total(&page, "cost"), ("$6.00".into(), "$6.000000".into()));
    assert_eq!(
        total(&page, "subscription"),
        ("$4.00".into(), "$4.000000".into())
    );
}

#[tokio::test]
async fn subscription_shows_est() {
    let app = app().await;
    let sub = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    let api = app.agent("github.com/acme/web", "mac", "claude-code").await;
    let at = "2026-11-27T08:00:00Z";
    app.report(
        &sub,
        "s1",
        "claude-opus-5-5",
        "subscription",
        (1_000_000, 0),
        at,
    )
    .await;
    app.report(&api, "p1", "claude-opus-5-5", "api", (500_000, 0), at)
        .await;

    let page = app.get("/ui/usage").await;
    let rows = page.table();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows[0][0].contains("infra"));
    assert_eq!(rows[0][6], "$4.00 est.");
    assert_eq!(rows[1][6], "$2.00");
    let html = page.html();
    let sel = Selector::parse("dl.usage-totals [data-total=subscription] dd").unwrap();
    assert_eq!(squash(&html.select(&sel).next().unwrap()), "$4.00");
}

#[tokio::test]
async fn filters_narrow_and_keep_state() {
    let app = app().await;
    let claude = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    let codex = app.agent("github.com/acme/infra", "linux", "codex").await;
    app.report(
        &claude,
        "c1",
        "claude-opus-5-5",
        "api",
        (100, 0),
        "2026-11-27T08:00:00Z",
    )
    .await;
    app.report(
        &codex,
        "x1",
        "gpt-5.2",
        "api",
        (100, 0),
        "2026-11-25T08:00:00Z",
    )
    .await;

    let page = app.get("/ui/usage?harness=codex&group=agent").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let names: Vec<String> = page.table().into_iter().map(|r| r[0].clone()).collect();
    assert_eq!(names, ["codex@linux"]);
    assert_eq!(
        page.texts("select[name=harness] option[selected]"),
        ["codex"]
    );
    assert_eq!(page.texts("select[name=group] option[selected]"), ["Agent"]);

    let page = app.get("/ui/usage?group=day&range=7d").await;
    let days = page.texts("ol.usage-days li");
    assert!(days.len() == 7 || days.len() == 8, "{days:?}");
    assert!(days.iter().any(|d| d.starts_with("2026-11-25")), "{days:?}");
    assert_eq!(
        page.texts("select[name=range] option[selected]"),
        ["7 days"]
    );

    // Today is per hour.
    let page = app.get("/ui/usage?range=today").await;
    let hours = page.texts("ol.usage-days li");
    assert_eq!(hours.len(), 10, "{hours:?}");

    // A custom range keeps its dates and counts whole UTC days.
    let page = app
        .get("/ui/usage?range=custom&from=2026-11-25&to=2026-11-25&group=agent")
        .await;
    let names: Vec<String> = page.table().into_iter().map(|r| r[0].clone()).collect();
    assert_eq!(names, ["codex@linux"]);
    let html = page.html();
    let from = Selector::parse("input[name=from]").unwrap();
    assert_eq!(
        html.select(&from).next().unwrap().value().attr("value"),
        Some("2026-11-25")
    );
}

#[tokio::test]
async fn bad_filters_never_500() {
    let app = app().await;
    let a = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    app.report(
        &a,
        "k1",
        "claude-opus-5-5",
        "api",
        (100, 0),
        "2026-11-27T08:00:00Z",
    )
    .await;
    for path in [
        "/ui/usage?task=garbage",
        "/ui/usage?agent=garbage",
        "/ui/usage?project=nope",
        "/ui/usage?billing=free",
        "/ui/usage?group=weird&range=forever",
    ] {
        let page = app.get(path).await;
        assert_eq!(page.status, StatusCode::OK, "{path}: {}", page.body);
        if !path.contains("group=weird") {
            assert!(page.table().is_empty(), "{path}");
            assert!(page.text().contains("No usage matches."), "{path}");
        }
    }
    // `from` after `to` is swapped.
    let page = app
        .get("/ui/usage?range=custom&from=2026-11-28&to=2026-11-10")
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert_eq!(page.table().len(), 1);

    let page = app.get("/ui/usage?range=custom&from=bad").await;
    assert_eq!(page.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.text().contains("Dates are YYYY-MM-DD."));
}

#[test]
fn compact_and_usd() {
    assert_eq!(compact(845.0), "845");
    assert_eq!(compact(12_345.0), "12.3k");
    assert_eq!(compact(1_240_000.0), "1.24M");
    assert_eq!(compact(3_100_000_000.0), "3.10B");
    assert_eq!(compact(999_999.0), "1.00M");
    assert_eq!(usd(0.0), "$0.00");
    assert_eq!(usd(5_000.0), "<$0.01");
    assert_eq!(usd(3_400_000.0), "$3.40");
    assert_eq!(usd(1_234_560_000.0), "$1,234.56");
}

const USAGE_CSS: &str = "p.usage-line a";

#[tokio::test]
async fn task_line_shows_and_links() {
    let app = app().await;
    let a = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    let t = app.task(&a, "Rotate the certs").await;
    app.claim(&a, &t).await;
    // 12 + 845 + 0 + 3,120 + 48,211 = 52,188 tokens
    let report = UsageReport {
        session_id: a.session.id,
        parser: "claude-code/1".into(),
        harness_version: None,
        harness_session_id: None,
        records: vec![json!({
            "key": "k1", "at": "2026-11-27T09:00:00Z", "model": "claude-opus-5-5",
            "input": 12, "output": 845,
            "cache_write_5m": 48_211, "cache_write_1h": 0, "cache_read": 3120, "reasoning": 0,
            "billing": "subscription",
        })],
    };
    app.state.usage.ingest(&a, &report).await.unwrap();

    let page = app.get(&format!("/ui/tasks/{}", t.id)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let text = page.texts(USAGE_CSS);
    assert_eq!(text.len(), 1, "{text:?}");
    assert_eq!(
        text[0],
        "Usage 52.2k tokens \u{b7} $0.26 API-equivalent est. \u{b7} 1 session"
    );
    let sel = format!(
        "p.usage-line a[href=\"/ui/usage?task={}&group=agent&range=all\"]",
        t.id
    );
    assert_eq!(page.texts(&sel).len(), 1);
}

#[tokio::test]
async fn parent_line_includes_subtasks() {
    let app = app().await;
    let a = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    let parent = app.task(&a, "Parent").await;
    let child = app
        .state
        .tasks
        .create(
            Caller::Agent(&a),
            NewTask {
                project_id: a.agent.project_id.unwrap(),
                title: "Child".into(),
                body: "body".into(),
                parent_id: Some(parent.id),
                ..NewTask::default()
            },
        )
        .await
        .unwrap();
    app.claim(&a, &child).await;
    app.report(
        &a,
        "k1",
        "claude-opus-5-5",
        "api",
        (1_000, 10),
        "2026-11-27T09:00:00Z",
    )
    .await;

    let page = app.get(&format!("/ui/tasks/{}", parent.id)).await;
    let text = page.texts(USAGE_CSS);
    assert_eq!(text.len(), 1, "{text:?}");
    assert!(
        text[0].ends_with("1 session \u{b7} incl. subtasks"),
        "{text:?}"
    );
    assert!(!text[0].contains(" est."), "{text:?}");
    let page = app.get(&format!("/ui/tasks/{}", child.id)).await;
    assert!(!page.texts(USAGE_CSS)[0].contains("incl. subtasks"));
}

#[tokio::test]
async fn no_usage_no_line() {
    let app = app().await;
    let a = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    let t = app.task(&a, "Quiet").await;
    app.claim(&a, &t).await;
    let page = app.get(&format!("/ui/tasks/{}", t.id)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.texts("p.usage-line").is_empty());
}

#[tokio::test]
async fn unknown_model_line_says_no_price() {
    let app = app().await;
    let a = app
        .agent("github.com/acme/infra", "mac", "claude-code")
        .await;
    let t = app.task(&a, "Mystery").await;
    app.claim(&a, &t).await;
    app.report(
        &a,
        "k1",
        "mystery-1",
        "api",
        (12_000, 0),
        "2026-11-27T09:00:00Z",
    )
    .await;
    let page = app.get(&format!("/ui/tasks/{}", t.id)).await;
    assert_eq!(
        page.texts(USAGE_CSS),
        ["Usage 12.0k tokens \u{b7} no price \u{b7} 1 session"]
    );
}
