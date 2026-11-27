//! The task page (spec §12): the thread, lease state, paged history, children and evidence,
//! with the human's actions on the task. Everything peer-written is rendered as escaped text.

use askama::Template;
use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::SessionCtx;
use super::views::{
    Labels, ProjectLabel, ProjectOption, UiError, UiResult, ago, html, status_icon,
};
use crate::domain::{Message, Task, TaskEvent, TaskEventKind, TaskId, TaskStatus};
use crate::http::AppState;
use crate::store::{StoreResult, TaskFilter};

/// Events per page of a task's history.
pub const EVENTS_PER_PAGE: u32 = 50;

/// A point in time three ways: readable, machine (`datetime`) and relative.
pub struct Stamp {
    pub text: String,
    pub iso: String,
    pub ago: String,
}

impl Stamp {
    pub fn new(now: OffsetDateTime, t: OffsetDateTime) -> Self {
        let utc = t.to_offset(time::UtcOffset::UTC);
        Self {
            text: format!(
                "{:04}-{:02}-{:02} {:02}:{:02} UTC",
                utc.year(),
                u8::from(utc.month()),
                utc.day(),
                utc.hour(),
                utc.minute()
            ),
            iso: t.format(&Rfc3339).unwrap_or_default(),
            ago: ago(now, t),
        }
    }
}

/// One message as the UI shows it: sender and recipient labels, body as peer text.
pub struct MessageView {
    pub id: String,
    pub from: String,
    pub project: Option<ProjectLabel>,
    pub to: String,
    pub subject: Option<String>,
    pub body: String,
    pub preview: String,
    pub importance: &'static str,
    pub at: Stamp,
    /// The thread it belongs to: `(task id, title)`.
    pub task: Option<(String, String)>,
    /// `sent` / `delivered` / `read` / `replied`, where shown.
    pub receipt: Option<&'static str>,
    /// For the human inbox: not yet read.
    pub unread: bool,
    /// Link to open it in the inbox reader.
    pub href: String,
}

/// The first 140 characters of a body, on one line.
fn preview(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(140) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

pub(super) async fn message_view(
    labels: &mut Labels<'_>,
    now: OffsetDateTime,
    m: &Message,
) -> StoreResult<MessageView> {
    let task = match m.task_id {
        Some(t) => labels
            .store
            .get_task(t)
            .await?
            .map(|task| (t.to_string(), task.title)),
        None => None,
    };
    Ok(MessageView {
        id: m.id.to_string(),
        from: labels.actor_label(m.from).await?,
        project: m.project_id.map(|p| labels.project(p)),
        to: labels.address_label(m.to).await?,
        subject: m.subject.clone().filter(|s| !s.trim().is_empty()),
        preview: preview(&m.body),
        body: m.body.clone(),
        importance: m.importance.as_str(),
        at: Stamp::new(now, m.created_at),
        task,
        receipt: None,
        unread: false,
        href: format!("/ui/inbox?view=all&m={}", m.id),
    })
}

pub struct EventView {
    pub kind: String,
    pub actor: String,
    pub at: Stamp,
    pub note: Option<String>,
    /// For child events, the child's title.
    pub child: Option<String>,
}

/// What a `done` event recorded as proof of the work.
#[derive(Default, Deserialize)]
pub struct Evidence {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub tests: Vec<String>,
    #[serde(default)]
    pub links: Vec<String>,
}

pub struct ChildLink {
    pub id: String,
    pub status: &'static str,
    pub icon: &'static str,
    pub project: ProjectLabel,
    pub title: String,
    pub owner: Option<String>,
}

pub struct TaskView {
    pub id: String,
    pub title: String,
    pub status: &'static str,
    pub project: ProjectLabel,
    pub created_by: String,
    pub created_at: Stamp,
    pub parent: Option<(String, String)>,
    pub owner: Option<String>,
    /// The lease, in words.
    pub lease: String,
    pub eligibility: Option<String>,
    pub can_release: bool,
    pub can_cancel: bool,
}

#[derive(Template)]
#[template(path = "task.html")]
struct TaskPage {
    csrf: Option<String>,
    nav: &'static str,
    task: TaskView,
    thread: Vec<MessageView>,
    events: Vec<EventView>,
    /// Link to the next (older) page of events.
    older: Option<String>,
    /// Whether this is a later page of the history.
    paged: bool,
    children: Vec<ChildLink>,
    evidence: Option<Evidence>,
    /// Projects a subtask may be created in.
    child_projects: Vec<ProjectOption>,
}

#[derive(Deserialize)]
pub struct TaskQuery {
    /// History cursor: show events older than this event id.
    #[serde(default)]
    pub before: Option<String>,
}

fn lease_text(now: OffsetDateTime, task: &Task, owner: Option<&str>) -> String {
    let owner = owner.unwrap_or("nobody");
    match task.status {
        TaskStatus::Open => "Not claimed.".into(),
        TaskStatus::Claimed => match task.lease_expires_at {
            Some(until) if until > now => format!(
                "Held by {owner} until {} (renewed by heartbeat).",
                Stamp::new(now, until).text
            ),
            Some(until) => format!(
                "Held by {owner}, lease expired at {}; any eligible agent may take it over.",
                Stamp::new(now, until).text
            ),
            None => format!("Held by {owner}."),
        },
        TaskStatus::Blocked | TaskStatus::Review => format!(
            "Held by {owner}; the lease does not expire while {}.",
            task.status
        ),
        TaskStatus::Done => format!("Finished by {owner}."),
        TaskStatus::Cancelled => "Cancelled.".into(),
    }
}

async fn event_view(
    labels: &mut Labels<'_>,
    now: OffsetDateTime,
    e: &TaskEvent,
) -> StoreResult<EventView> {
    let child = e
        .payload
        .as_ref()
        .and_then(|p| p.get("child_title"))
        .and_then(|t| t.as_str())
        .map(str::to_string);
    Ok(EventView {
        kind: e.kind.as_str().replace('_', " "),
        actor: labels.actor_label(e.actor).await?,
        at: Stamp::new(now, e.at),
        note: e.note.clone(),
        child,
    })
}

pub async fn task_page(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    Path(id): Path<String>,
    Query(q): Query<TaskQuery>,
) -> UiResult<Response> {
    let id: TaskId = id.parse().map_err(|_| UiError::NotFound("No such task."))?;
    let store = &*state.store;
    let task = store
        .get_task(id)
        .await?
        .ok_or(UiError::NotFound("No such task."))?;
    let before = match q.before.as_deref().filter(|b| !b.is_empty()) {
        Some(b) => Some(
            b.parse::<uuid::Uuid>()
                .map_err(|_| UiError::Invalid("Bad history cursor.".into()))?,
        ),
        None => None,
    };
    let now = state.clock.now();
    let mut labels = Labels::load(store).await?;

    let owner = match task.owner_agent_id {
        Some(a) => Some(labels.agent_label(a).await?),
        None => None,
    };
    let parent = match task.parent_id {
        Some(p) => store
            .get_task(p)
            .await?
            .map(|parent| (p.to_string(), parent.title)),
        None => None,
    };
    let eligibility = match (&task.eligibility.harness, &task.eligibility.host) {
        (None, None) => None,
        (harness, host) => Some(format!(
            "{} on {}",
            harness.as_deref().unwrap_or("any harness"),
            host.as_deref().unwrap_or("any host")
        )),
    };
    let view = TaskView {
        id: task.id.to_string(),
        title: task.title.clone(),
        status: task.status.as_str(),
        project: labels.project(task.project_id),
        created_by: labels.actor_label(task.created_by).await?,
        created_at: Stamp::new(now, task.created_at),
        parent,
        lease: lease_text(now, &task, owner.as_deref()),
        owner,
        eligibility,
        can_release: matches!(
            task.status,
            TaskStatus::Claimed | TaskStatus::Blocked | TaskStatus::Review
        ),
        can_cancel: !matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled),
    };

    let mut thread = Vec::new();
    for m in state
        .messages
        .thread(id)
        .await
        .map_err(|e| UiError::Internal(e.to_string()))?
    {
        thread.push(message_view(&mut labels, now, &m).await?);
    }

    // One extra event tells whether an older page exists.
    let mut page = store
        .task_events_page(id, None, before, EVENTS_PER_PAGE + 1)
        .await?;
    let more = page.len() > EVENTS_PER_PAGE as usize;
    page.truncate(EVENTS_PER_PAGE as usize);
    let older = if more {
        page.last()
            .map(|e| format!("/ui/tasks/{id}?before={}", e.id))
    } else {
        None
    };
    let mut events = Vec::with_capacity(page.len());
    for e in &page {
        events.push(event_view(&mut labels, now, e).await?);
    }

    let evidence = store
        .task_events_page(id, Some(TaskEventKind::Done), None, 1)
        .await?
        .into_iter()
        .next()
        .and_then(|e| e.payload)
        .map(|p| serde_json::from_value::<Evidence>(p).unwrap_or_default());

    let kids = store
        .list_tasks(&TaskFilter {
            parent: Some(id),
            ..TaskFilter::default()
        })
        .await?;
    let mut children = Vec::with_capacity(kids.len());
    for c in kids {
        let owner = match c.owner_agent_id {
            Some(a) => Some(labels.agent_label(a).await?),
            None => None,
        };
        children.push(ChildLink {
            id: c.id.to_string(),
            status: c.status.as_str(),
            icon: status_icon(c.status),
            project: labels.project(c.project_id),
            title: c.title,
            owner,
        });
    }

    // A subtask lives in the task's project or one of its extra projects.
    let mut allowed = vec![task.project_id];
    allowed.extend(store.task_projects(id).await?);
    let child_projects = allowed
        .into_iter()
        .map(|p| {
            let label = labels.project(p);
            ProjectOption {
                id: p.to_string(),
                name: label.name,
                key: label.key,
                selected: p == task.project_id,
            }
        })
        .collect();

    html(&TaskPage {
        csrf: Some(session.csrf),
        nav: "board",
        task: view,
        thread,
        events,
        older,
        paged: before.is_some(),
        children,
        evidence,
        child_projects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn preview_flattens_and_cuts() {
        assert_eq!(preview("a\n\n b"), "a b");
        let long = "x".repeat(200);
        let p = preview(&long);
        assert_eq!(p.chars().count(), 141);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn stamps_read_in_utc() {
        let now = datetime!(2026-11-27 9:05 UTC);
        let s = Stamp::new(now, datetime!(2026-11-27 9:00 UTC));
        assert_eq!(s.text, "2026-11-27 09:00 UTC");
        assert_eq!(s.ago, "5m ago");
        assert!(s.iso.starts_with("2026-11-27T09:00:00"));
    }
}
