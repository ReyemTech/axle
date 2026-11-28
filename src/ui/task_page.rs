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
use super::needs::latest_plan;
use super::usage::{compact, usd};
use super::views::{
    Labels, LeaseView, ProjectLabel, ProjectOption, Pulse, UiError, UiResult, ago, html,
    lease_view, project_options, pulse_counts, state_mark,
};
use crate::domain::{
    Actor, Message, MessageId, PlanState, Task, TaskEvent, TaskEventKind, TaskId, TaskStatus,
    WaitingOn,
};
use crate::http::AppState;
use crate::service::tasks::plan_words;
use crate::store::{StoreResult, TaskFilter, TaskScope, UsageFilter, UsageGroup};

/// Events per page of a task's history.
pub const EVENTS_PER_PAGE: u32 = 50;
/// Messages per page of a task's thread.
pub const THREAD_PER_PAGE: u32 = 50;

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
    /// `message`, `plan` or `system`.
    pub kind: &'static str,
    /// Written by an agent (not the human): shown with the peer frame.
    pub agent_written: bool,
    /// Quick replies the sending agent offered the human (peer text, escaped on render).
    pub options: Vec<String>,
}

impl MessageView {
    /// The body as safe HTML from its markdown (see [`super::markdown`]).
    pub fn body_html(&self) -> String {
        super::markdown::render(&self.body, Some(&format!("msg-{}", self.id)))
    }
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
            .task_title(t)
            .await?
            .map(|title| (t.to_string(), title)),
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
        kind: m.kind.as_str(),
        agent_written: matches!(m.from, Actor::Agent(..)),
        options: m.options.clone(),
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

/// A link to another task, as this task's page lists it.
pub struct RelatedLink {
    pub id: String,
    /// The link in words, before the other task's title (e.g. "duplicate of").
    pub label: &'static str,
    pub kind: &'static str,
    pub status: &'static str,
    pub project: ProjectLabel,
    pub title: String,
}

pub struct ChildLink {
    pub id: String,
    pub status: &'static str,
    pub project: ProjectLabel,
    pub title: String,
    pub owner: Option<String>,
}

/// The latest plan of a task, for the plan panel.
pub struct PlanView {
    /// The plan message's id, sent back with a decision.
    pub id: String,
    pub from: String,
    pub at: Stamp,
    pub body: String,
}

impl PlanView {
    /// The plan as safe HTML from its markdown (see [`super::markdown`]).
    pub fn body_html(&self) -> String {
        super::markdown::render(&self.body, Some(&format!("plan-{}", self.id)))
    }
}

/// The Blocked panel: why, who and since when, what it waits on and what would unblock it.
pub struct BlockedView {
    /// Agent-written (`None`: blocked before reasons were kept, and none was found).
    pub reason: Option<String>,
    pub by: Option<String>,
    pub since: Option<Stamp>,
    /// `none`, `human`, `task` or `external`.
    pub waiting_kind: &'static str,
    /// For `task`: the awaited task's id, title and live status.
    pub waiting_task: Option<WaitingTask>,
    /// For `external`: agent-written.
    pub waiting_text: Option<String>,
    /// Agent-written.
    pub unblock: Option<String>,
}

pub struct WaitingTask {
    pub id: String,
    pub title: String,
    pub status: &'static str,
    /// `cancelled`: it will never unblock this task.
    pub cancelled: bool,
}

impl BlockedView {
    /// The reason as safe HTML from its markdown (see [`super::markdown`]).
    pub fn reason_html(&self) -> Option<String> {
        self.reason
            .as_deref()
            .map(|r| super::markdown::render(r, Some("blocked")))
    }
}

async fn blocked_view(
    store: &dyn crate::store::Store,
    now: OffsetDateTime,
    task: &Task,
    owner: Option<&str>,
) -> StoreResult<Option<BlockedView>> {
    if task.status != TaskStatus::Blocked {
        return Ok(None);
    }
    let block = task.block.as_ref();
    let waiting = block.and_then(|b| b.waiting_on.as_ref());
    let waiting_task = match waiting.and_then(WaitingOn::task) {
        Some(id) => store.get_task(id).await?.map(|w| WaitingTask {
            id: w.id.to_string(),
            title: w.title,
            status: w.status.as_str(),
            cancelled: w.status == TaskStatus::Cancelled,
        }),
        None => None,
    };
    Ok(Some(BlockedView {
        reason: block.map(|b| b.reason.clone()),
        by: owner.map(str::to_string),
        since: block.and_then(|b| b.since).map(|t| Stamp::new(now, t)),
        waiting_kind: waiting.map_or("none", WaitingOn::kind),
        waiting_task,
        waiting_text: waiting.and_then(WaitingOn::text).map(str::to_string),
        unblock: block.and_then(|b| b.unblock.clone()),
    }))
}

pub struct TaskView {
    pub id: String,
    pub title: String,
    pub status: &'static str,
    pub aspect: &'static str,
    pub label: &'static str,
    /// Whether the task has the plan gate, and where its plan is, in words.
    pub requires_plan: bool,
    pub plan_words: &'static str,
    /// The plan awaits the human's decision.
    pub can_decide: bool,
    /// Participating projects besides the lead.
    pub also: Vec<ProjectLabel>,
    /// A top-level task that is not closed: projects can be added.
    pub can_add_project: bool,
    /// A top-level task with no extra projects: it can be moved under a parent.
    pub can_move: bool,
    pub project: ProjectLabel,
    pub created_by: String,
    pub created_at: Stamp,
    pub parent: Option<(String, String)>,
    pub owner: Option<String>,
    /// The lease, in words.
    pub lease: String,
    /// The running lease of a claimed task, drawn as a bar.
    pub lease_bar: Option<LeaseView>,
    pub eligibility: Option<String>,
    pub can_release: bool,
    pub can_cancel: bool,
    /// What the task's sessions used, when anything was reported.
    pub usage: Option<UsageLine>,
}

/// The task page's one-line usage summary and where it links.
pub struct UsageLine {
    pub text: String,
    pub href: String,
}

/// The usage line for `id` (its subtasks included when it has any); `None` without usage.
async fn usage_line(state: &AppState, id: TaskId) -> StoreResult<Option<UsageLine>> {
    let subtasks = state
        .store
        .count_tasks(&TaskFilter {
            parent: Some(id),
            ..TaskFilter::default()
        })
        .await?
        > 0;
    let filter = UsageFilter {
        task: Some(TaskScope::Task { id, subtasks }),
        ..UsageFilter::default()
    };
    let rows = state.usage.summary(&filter, UsageGroup::Total).await?;
    let Some(row) = rows.into_iter().find(|r| r.requests > 0) else {
        return Ok(None);
    };
    let tokens = row.input + row.output + row.cache_write + row.cache_read;
    let mut text = format!(
        "Usage {} tokens \u{b7} {} API-equivalent{} \u{b7} {} session{}",
        compact(tokens),
        usd(row.cost_micros),
        if row.estimated > 0 { " est." } else { "" },
        row.sessions,
        if row.sessions == 1 { "" } else { "s" },
    );
    if subtasks {
        text.push_str(" \u{b7} incl. subtasks");
    }
    Ok(Some(UsageLine {
        text,
        href: format!("/ui/usage?task={id}&group=agent&range=all"),
    }))
}

#[derive(Template)]
#[template(path = "task.html")]
struct TaskPage {
    csrf: Option<String>,
    logout: bool,
    /// The top bar's counts (`None` when signed out).
    pulse: Option<Pulse>,
    nav: &'static str,
    task: TaskView,
    /// What the live region re-fetches: this page, with its pagination cursors.
    refresh: String,
    thread: Vec<MessageView>,
    events: Vec<EventView>,
    /// Link to the next (older) page of events.
    older: Option<String>,
    /// Whether this is a later page of the history.
    paged: bool,
    /// Link to the previous (earlier) page of the thread.
    earlier: Option<String>,
    /// Whether this is an earlier page of the thread.
    thread_paged: bool,
    children: Vec<ChildLink>,
    related: Vec<RelatedLink>,
    evidence: Option<Evidence>,
    plan: Option<PlanView>,
    /// A decision was refused because the plan changed.
    plan_changed: bool,
    blocked: Option<BlockedView>,
    /// Projects that can still be added.
    addable: Vec<ProjectOption>,
    /// Projects a subtask may be created in.
    child_projects: Vec<ProjectOption>,
}

#[derive(Deserialize)]
pub struct TaskQuery {
    /// History cursor: show events older than this event id.
    #[serde(default)]
    pub before: Option<String>,
    /// Thread cursor: show messages sent before this message id.
    #[serde(default)]
    pub thread_before: Option<String>,
    /// Set when a decision named a plan that had been replaced.
    #[serde(default)]
    pub plan_changed: Option<String>,
}

fn lease_text(now: OffsetDateTime, task: &Task, owner: Option<&str>) -> String {
    let owner = owner.unwrap_or("nobody");
    match task.status {
        TaskStatus::Open => "Not claimed.".into(),
        TaskStatus::Planning => match (task.plan_state, task.lease_expires_at) {
            (PlanState::Submitted, _) => format!(
                "Held by {owner} for planning; the lease does not expire while the plan awaits \
                 your approval."
            ),
            (_, Some(until)) if until > now => format!(
                "Held by {owner} for planning until {} (renewed by heartbeat).",
                Stamp::new(now, until).text
            ),
            (_, Some(until)) => format!(
                "Held by {owner} for planning, lease expired at {}; any eligible agent may take \
                 it over.",
                Stamp::new(now, until).text
            ),
            (_, None) => format!("Held by {owner} for planning."),
        },
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
    let extra = store.task_projects(id).await?;
    let (aspect, label) = state_mark(&task);
    let open = !matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled);
    let blocked = blocked_view(store, now, &task, owner.as_deref()).await?;
    let view = TaskView {
        id: task.id.to_string(),
        title: task.title.clone(),
        status: task.status.as_str(),
        aspect,
        label,
        requires_plan: task.requires_plan,
        plan_words: plan_words(task.plan_state),
        can_decide: task.status == TaskStatus::Planning && task.plan_state == PlanState::Submitted,
        also: extra.iter().map(|p| labels.project(*p)).collect(),
        can_add_project: task.parent_id.is_none() && open,
        can_move: task.parent_id.is_none() && extra.is_empty(),
        project: labels.project(task.project_id),
        created_by: labels.actor_label(task.created_by).await?,
        created_at: Stamp::new(now, task.created_at),
        parent,
        lease: lease_text(now, &task, owner.as_deref()),
        lease_bar: lease_view(now, state.config.lease_ttl, &task),
        owner,
        eligibility,
        can_release: matches!(
            task.status,
            TaskStatus::Planning | TaskStatus::Claimed | TaskStatus::Blocked | TaskStatus::Review
        ),
        can_cancel: !matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled),
        usage: usage_line(&state, id).await?,
    };

    // The newest page of the thread (one extra tells whether an earlier page exists),
    // shown oldest first.
    let thread_before = match q.thread_before.as_deref().filter(|b| !b.is_empty()) {
        Some(b) => Some(
            b.parse::<MessageId>()
                .map_err(|_| UiError::Invalid("Bad thread cursor.".into()))?,
        ),
        None => None,
    };
    let mut posts = store
        .task_thread_page(id, thread_before, THREAD_PER_PAGE + 1)
        .await?;
    let earlier = if posts.len() > THREAD_PER_PAGE as usize {
        posts.truncate(THREAD_PER_PAGE as usize);
        posts
            .last()
            .map(|m| format!("/ui/tasks/{id}?thread_before={}", m.id))
    } else {
        None
    };
    posts.reverse();
    let mut thread = Vec::with_capacity(posts.len());
    for m in &posts {
        thread.push(message_view(&mut labels, now, m).await?);
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
            project: labels.project(c.project_id),
            title: c.title,
            owner,
        });
    }

    let related = state
        .tasks
        .related(id)
        .await
        .map_err(|e| match e {
            crate::service::tasks::TaskError::Store(e) => UiError::from(e),
            e => UiError::Invalid(e.to_string()),
        })?
        .into_iter()
        .map(|r| RelatedLink {
            id: r.other.id.to_string(),
            label: r.label(),
            kind: r.link.kind.as_str(),
            status: r.other.status.as_str(),
            project: labels.project(r.other.project_id),
            title: r.other.title,
        })
        .collect();

    let plan = match latest_plan(&state, id).await? {
        Some(m) => Some(PlanView {
            id: m.id.to_string(),
            from: labels.actor_label(m.from).await?,
            at: Stamp::new(now, m.created_at),
            body: m.body,
        }),
        None => None,
    };
    let mut addable = project_options(&labels, None);
    addable.retain(|p| {
        p.id != task.project_id.to_string() && !extra.iter().any(|e| e.to_string() == p.id)
    });

    // A subtask lives in the task's project or one of its extra projects.
    let mut allowed = vec![task.project_id];
    allowed.extend(extra.iter().copied());
    let child_projects = allowed
        .into_iter()
        .map(|p| {
            let label = labels.project(p);
            ProjectOption {
                id: p.to_string(),
                name: label.name,
                key: label.key,
                line: label.line,
                selected: p == task.project_id,
            }
        })
        .collect();

    // The live refresh re-renders the page being viewed, not the newest one.
    let cursors: Vec<String> = [
        thread_before.map(|m| format!("thread_before={m}")),
        before.map(|e| format!("before={e}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let refresh = if cursors.is_empty() {
        format!("/ui/tasks/{id}")
    } else {
        format!("/ui/tasks/{id}?{}", cursors.join("&"))
    };

    html(&TaskPage {
        refresh,
        logout: session.logout,
        csrf: Some(session.csrf),
        pulse: Some(pulse_counts(&state).await?),
        nav: "board",
        task: view,
        thread,
        events,
        older,
        paged: before.is_some(),
        earlier,
        thread_paged: thread_before.is_some(),
        children,
        related,
        evidence,
        child_projects,
        plan,
        plan_changed: q.plan_changed.is_some(),
        blocked,
        addable,
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
