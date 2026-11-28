//! The History page: every finished task, newest first, 50 to a page. Filters by project
//! and title; `?unseen=auto` lists the auto-acked, unseen set the board links to, with an
//! "Ack these" form for the rows on the page.

use askama::Template;
use axum::Extension;
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;
use time::format_description::well_known::Rfc3339;

use super::SessionCtx;
use super::views::{
    BoardQuery, Labels, ProjectLabel, ProjectOption, Pulse, UiError, UiResult, ago, board_filter,
    html, project_options, pulse_counts,
};
use crate::domain::{TaskId, TaskStatus, ack_state};
use crate::http::AppState;
use crate::store::{DoneFilter, TaskFilter, TaskOrder};

/// Tasks per History page.
pub const HISTORY_PAGE: u32 = 50;

#[derive(Deserialize, Default)]
pub struct HistoryQuery {
    /// A project id or key, as on the board.
    #[serde(default)]
    pub project: Option<String>,
    /// Title search; blank means none.
    #[serde(default)]
    pub q: Option<String>,
    /// `auto`: only the auto-acked, unseen tasks. Anything else is ignored.
    #[serde(default)]
    pub unseen: Option<String>,
    /// Cursor: the last task id of the previous page.
    #[serde(default)]
    pub after: Option<String>,
}

pub struct HistoryRow {
    pub id: String,
    pub title: String,
    pub project: ProjectLabel,
    /// The parent `(id, title)` when the task is a subtask.
    pub parent: Option<(String, String)>,
    /// RFC 3339, for `<time datetime>`.
    pub done_at: String,
    pub done_ago: String,
    /// `unseen`, `acked` or `auto-acked`.
    pub state: &'static str,
}

#[derive(Template)]
#[template(path = "history.html")]
struct HistoryPage {
    csrf: Option<String>,
    logout: bool,
    pulse: Option<Pulse>,
    nav: &'static str,
    projects: Vec<ProjectOption>,
    /// The filtered project's id.
    filter: Option<String>,
    q: String,
    auto: bool,
    rows: Vec<HistoryRow>,
    /// The next page's full href, filters kept.
    next: Option<String>,
    /// This page's URL, where Ack these returns to.
    back: String,
    /// This page's filters without the cursor and `unseen`, to switch the unseen view.
    all_href: String,
    auto_href: String,
}

/// `/ui/history` with the given parameters, in a fixed order, URL-encoded.
fn href(auto: bool, project: Option<&str>, q: &str, after: Option<&str>) -> String {
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    if auto {
        pairs.push(("unseen", "auto"));
    }
    if let Some(p) = project {
        pairs.push(("project", p));
    }
    if !q.is_empty() {
        pairs.push(("q", q));
    }
    if let Some(a) = after {
        pairs.push(("after", a));
    }
    let query = serde_urlencoded::to_string(&pairs).unwrap_or_default();
    if query.is_empty() {
        "/ui/history".to_string()
    } else {
        format!("/ui/history?{query}")
    }
}

/// `GET /ui/history`.
pub async fn history(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    Query(q): Query<HistoryQuery>,
) -> UiResult<Response> {
    let now = state.clock.now();
    let project = board_filter(
        &*state.store,
        &BoardQuery {
            project: q.project.clone(),
        },
    )
    .await?;
    let project_id = project.as_ref().map(|p| p.id);
    let search =
        q.q.as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
    let auto = q.unseen.as_deref() == Some("auto");

    // The store answers an unknown cursor with an empty page; a bad one is a 404 here.
    let after_raw = q.after.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let after: Option<TaskId> = match after_raw {
        Some(raw) => {
            let id: TaskId = raw
                .parse()
                .map_err(|_| UiError::NotFound("No such page."))?;
            // The cursor must be a done task in the filtered project, like every page's last row.
            match state.store.get_task(id).await? {
                Some(t)
                    if t.status == TaskStatus::Done
                        && project_id.is_none_or(|p| p == t.project_id) => {}
                _ => return Err(UiError::NotFound("No such page.")),
            }
            Some(id)
        }
        None => None,
    };

    let mut tasks = state
        .store
        .list_tasks(&TaskFilter {
            project: project_id,
            status: Some(TaskStatus::Done),
            title: (!search.is_empty()).then(|| search.clone()),
            order: TaskOrder::DoneNewest,
            after,
            limit: Some(HISTORY_PAGE + 1),
            done: if auto {
                DoneFilter::AutoAckedUnseen {
                    since: now - state.config.done_auto_ack,
                }
            } else {
                DoneFilter::Any
            },
            ..TaskFilter::default()
        })
        .await?;
    let more = tasks.len() > HISTORY_PAGE as usize;
    tasks.truncate(HISTORY_PAGE as usize);

    let project_param = project_id.map(|p| p.to_string());
    let next = match (more, tasks.last()) {
        (true, Some(last)) => Some(href(
            auto,
            project_param.as_deref(),
            &search,
            Some(&last.id.to_string()),
        )),
        _ => None,
    };

    let mut labels = Labels::load(&*state.store).await?;
    let mut rows = Vec::with_capacity(tasks.len());
    for t in &tasks {
        let parent = match t.parent_id {
            Some(p) => labels
                .task_title(p)
                .await?
                .map(|title| (p.to_string(), title)),
            None => None,
        };
        let done_at = t.done_at.unwrap_or(t.updated_at);
        rows.push(HistoryRow {
            id: t.id.to_string(),
            title: t.title.clone(),
            project: labels.project(t.project_id),
            parent,
            done_at: done_at.format(&Rfc3339).unwrap_or_default(),
            done_ago: ago(now, done_at),
            state: ack_state(t, now, state.config.done_auto_ack).map_or("unseen", |s| s.as_str()),
        });
    }

    html(&HistoryPage {
        logout: session.logout,
        csrf: Some(session.csrf),
        pulse: Some(pulse_counts(&state).await?),
        nav: "history",
        projects: project_options(&labels, project_id),
        filter: project_param.clone(),
        q: search.clone(),
        auto,
        rows,
        next,
        back: href(auto, project_param.as_deref(), &search, after_raw),
        all_href: href(false, project_param.as_deref(), &search, None),
        auto_href: href(true, project_param.as_deref(), &search, None),
    })
}
