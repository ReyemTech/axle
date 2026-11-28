//! Page and fragment handlers and the view models they render (askama, auto-escaped: peer
//! strings are never marked safe). Projects are labelled by display name; the key is only
//! ever a `title` (hover) attribute.

use std::collections::{HashMap, HashSet};

use askama::Template;
use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use super::SessionCtx;
use super::needs::{NeedCard, needs_you};
use crate::domain::{
    Actor, Address, Agent, AgentId, Importance, PlanState, Project, ProjectId, Task, TaskId,
    TaskStatus,
};
use crate::events::Event;
use crate::http::AppState;
use crate::service::tasks::Progress;
use crate::store::{DoneFilter, Store, StoreError, StoreResult, TaskFilter, TaskOrder};

/// The Done column shows at most this many cards (`Ack all` acks exactly these).
pub const DONE_COLUMN_LIMIT: u32 = 50;
/// The overview shows agents seen within this window.
pub const AGENT_WINDOW: Duration = Duration::hours(24);
/// The deepest parent chain the live stream walks up to find a card.
const MAX_DEPTH: usize = 16;

const COLUMNS: [(TaskStatus, &str); 5] = [
    (TaskStatus::Open, "Open"),
    (TaskStatus::Claimed, "Claimed"),
    (TaskStatus::Blocked, "Blocked"),
    (TaskStatus::Review, "Review"),
    (TaskStatus::Done, "Done"),
];

// ---------------------------------------------------------------- errors

/// A failed UI request: 404 for unknown things, 500 (logged) for store errors.
#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error("not found: {0}")]
    NotFound(&'static str),
    /// A form value the action cannot use (422).
    #[error("{0}")]
    Invalid(String),
    /// The action does not fit the current state, like releasing an open task (409).
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Internal(String),
}

impl From<StoreError> for UiError {
    fn from(e: StoreError) -> Self {
        UiError::Internal(e.to_string())
    }
}

impl From<askama::Error> for UiError {
    fn from(e: askama::Error) -> Self {
        UiError::Internal(format!("template: {e}"))
    }
}

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        match self {
            UiError::NotFound(what) => error_page(StatusCode::NOT_FOUND, what),
            UiError::Invalid(why) => error_page(StatusCode::UNPROCESSABLE_ENTITY, &why),
            UiError::Conflict(why) => error_page(StatusCode::CONFLICT, &why),
            UiError::Internal(e) => {
                eprintln!("axl: ui error: {e}");
                error_page(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong.")
            }
        }
    }
}

pub(super) type UiResult<T> = Result<T, UiError>;

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage<'a> {
    message: &'a str,
}

/// A minimal page for errors (`message` is escaped like everything else).
pub fn error_page(status: StatusCode, message: &str) -> Response {
    match (ErrorPage { message }).render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(_) => status.into_response(),
    }
}

pub(super) fn html(t: &impl Template) -> UiResult<Response> {
    Ok(Html(t.render()?).into_response())
}

// ---------------------------------------------------------------- login

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage<'a> {
    csrf: Option<String>,
    logout: bool,
    /// The top bar's counts (`None` when signed out).
    pulse: Option<Pulse>,
    nav: &'static str,
    error: Option<&'a str>,
    /// Where to return after signing in (only ever `/ui/connect`).
    next: Option<&'static str>,
}

pub fn login_page(status: StatusCode, error: Option<&str>, next: Option<&'static str>) -> Response {
    let page = LoginPage {
        next,
        csrf: None,
        logout: false,
        pulse: None,
        nav: "",
        error,
    };
    match page.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(e) => UiError::from(e).into_response(),
    }
}

// ---------------------------------------------------------------- labels

/// A project as the UI shows it: display name, with the key on hover.
pub struct ProjectLabel {
    pub name: String,
    pub key: String,
    /// The project's line colour, `1..=LINES` (`0`: unknown project).
    pub line: u8,
}

/// How many distinct project line colours the stylesheet defines.
pub const LINES: usize = 6;

/// Each project's line colour: projects in key order take the colours in turn, so a
/// project keeps its colour across renders until projects are added before it.
pub(super) fn line_colours<'p>(
    projects: impl IntoIterator<Item = &'p Project>,
) -> HashMap<ProjectId, u8> {
    let mut keyed: Vec<&Project> = projects.into_iter().collect();
    keyed.sort_by(|a, b| a.key.cmp(&b.key));
    keyed
        .into_iter()
        .enumerate()
        .map(|(i, p)| (p.id, (i % LINES) as u8 + 1))
        .collect()
}

/// Caches project and agent lookups for one render.
pub(super) struct Labels<'a> {
    pub(super) store: &'a dyn Store,
    pub(super) projects: HashMap<ProjectId, Project>,
    lines: HashMap<ProjectId, u8>,
    agents: HashMap<AgentId, Option<Agent>>,
    /// Task titles, fetched once per render.
    task_titles: HashMap<TaskId, Option<String>>,
}

impl<'a> Labels<'a> {
    pub(super) async fn load(store: &'a dyn Store) -> StoreResult<Self> {
        let projects: HashMap<ProjectId, Project> = store
            .list_projects()
            .await?
            .into_iter()
            .map(|p| (p.id, p))
            .collect();
        Ok(Self {
            store,
            lines: line_colours(projects.values()),
            projects,
            agents: HashMap::new(),
            task_titles: HashMap::new(),
        })
    }

    /// A task's title (`None` for an unknown task), read once per render.
    pub(super) async fn task_title(&mut self, id: TaskId) -> StoreResult<Option<String>> {
        if !self.task_titles.contains_key(&id) {
            let title = self.store.get_task(id).await?.map(|t| t.title);
            self.task_titles.insert(id, title);
        }
        Ok(self.task_titles[&id].clone())
    }

    pub(super) fn project(&self, id: ProjectId) -> ProjectLabel {
        match self.projects.get(&id) {
            Some(p) => ProjectLabel {
                name: p.display_name.clone(),
                key: p.key.clone(),
                line: self.lines.get(&id).copied().unwrap_or(0),
            },
            None => ProjectLabel {
                name: "unknown project".into(),
                key: id.to_string(),
                line: 0,
            },
        }
    }

    async fn agent(&mut self, id: AgentId) -> StoreResult<Option<&Agent>> {
        if !self.agents.contains_key(&id) {
            let agent = self.store.get_agent(id).await?;
            self.agents.insert(id, agent);
        }
        Ok(self.agents[&id].as_ref())
    }

    /// `harness@host`, like `claude-code@mac`.
    pub(super) async fn agent_label(&mut self, id: AgentId) -> StoreResult<String> {
        Ok(match self.agent(id).await? {
            Some(a) => format!("{}@{}", a.harness, a.host),
            None => "unknown agent".into(),
        })
    }

    /// An agent's label, or `you` for the human.
    pub(super) async fn actor_label(&mut self, actor: Actor) -> StoreResult<String> {
        match actor {
            Actor::Agent(a, _) => self.agent_label(a).await,
            Actor::Human => Ok("you".into()),
        }
    }

    /// Who a message is addressed to, for people.
    pub(super) async fn address_label(&mut self, to: Address) -> StoreResult<String> {
        Ok(match to {
            Address::Project(p) => format!("everyone in {}", self.project(p).name),
            Address::Agent(a) => self.agent_label(a).await?,
            Address::Session(s) => match self.store.get_session(s).await? {
                Some(session) => format!(
                    "{} (one session)",
                    self.agent_label(session.agent_id).await?
                ),
                None => "unknown session".into(),
            },
            Address::Task(t) => match self.task_title(t).await? {
                Some(title) => format!("thread: {title}"),
                None => "unknown task".into(),
            },
            Address::Human => "you".into(),
        })
    }
}

// ---------------------------------------------------------------- cards

pub struct CardView {
    pub id: String,
    pub title: String,
    pub status: &'static str,
    /// The signal aspect and the word beside it (a planning task's plan state shows here).
    pub aspect: &'static str,
    pub label: &'static str,
    pub project: ProjectLabel,
    /// The agent holding the task (or that finished it).
    pub owner: Option<String>,
    /// Whether the holder's session is live, for a task still held (`None` otherwise).
    pub owner_live: Option<bool>,
    /// The parent `(id, title)` of a subtask shown as its own card.
    pub parent: Option<(String, String)>,
    /// The running lease of a claimed task.
    pub lease: Option<LeaseView>,
    pub progress: Option<ProgressView>,
    /// A blocked task's reason: its first line, cut short (agent-written text).
    pub blocked_line: Option<String>,
    /// A blocked task waits on the human.
    pub blocked_on_you: bool,
}

/// Characters of a blocked task's reason shown on its card.
const BLOCKED_LINE: usize = 90;

/// The first non-blank line of `reason`, cut to `BLOCKED_LINE` characters.
pub(super) fn first_line(reason: &str) -> Option<String> {
    let line = reason.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(match line.char_indices().nth(BLOCKED_LINE) {
        Some((cut, _)) => format!("{}…", line[..cut].trim_end()),
        None => line.to_string(),
    })
}

/// A running lease: seconds left out of its full length, and the same in words.
/// The page counts `left` down between renders.
pub struct LeaseView {
    pub left: i64,
    pub ttl: i64,
    pub text: String,
}

/// "expired", "42s left", "12m left", "1h 5m left" (shown after the word "lease").
pub(super) fn lease_left_text(left: i64) -> String {
    match left {
        s if s <= 0 => "expired".into(),
        s if s < 60 => format!("{s}s left"),
        s if s < 3600 => format!("{}m left", s / 60),
        s => format!("{}h {}m left", s / 3600, s % 3600 / 60),
    }
}

/// A task's state as a signal aspect and its word: the status, except that a planning task
/// whose plan waits on the human reads "plan awaiting you".
pub fn state_mark(task: &Task) -> (&'static str, &'static str) {
    match (task.status, task.plan_state) {
        (TaskStatus::Planning, PlanState::Submitted) => ("plan-submitted", "plan awaiting you"),
        (TaskStatus::Planning, PlanState::ChangesRequested) => ("planning", "plan in revision"),
        (status, _) => (status.as_str(), status.as_str()),
    }
}

/// The lease of a `claimed` or `planning` task (`None` for any other status, or without an
/// expiry, like a plan awaiting approval).
pub(super) fn lease_view(
    now: OffsetDateTime,
    default_ttl: Duration,
    task: &Task,
) -> Option<LeaseView> {
    if !matches!(task.status, TaskStatus::Claimed | TaskStatus::Planning) {
        return None;
    }
    let until = task.lease_expires_at?;
    let ttl = task.lease_ttl.unwrap_or(default_ttl).whole_seconds().max(1);
    let left = (until - now).whole_seconds().clamp(0, ttl);
    Some(LeaseView {
        left,
        ttl,
        text: lease_left_text(left),
    })
}

/// One child on a parent's line.
pub struct Station {
    pub status: &'static str,
    pub title: String,
}

pub struct ProgressView {
    pub done: u32,
    pub total: u32,
    /// Each child, in order: the parent's line of stations.
    pub stations: Vec<Station>,
    /// Children that are `blocked`, shown as a badge so they are seen without expanding.
    pub blocked: u32,
    pub owners: Vec<OwnerGroup>,
}

/// The agents holding a parent's unfinished children in one project.
pub struct OwnerGroup {
    pub project: ProjectLabel,
    pub agents: Vec<String>,
}

/// What every card on one render shares: the time, and the server's lease and presence
/// settings.
#[derive(Clone, Copy)]
struct CardCtx {
    now: OffsetDateTime,
    lease_ttl: Duration,
    presence_stale: Duration,
}

impl CardCtx {
    fn of(state: &AppState) -> Self {
        Self {
            now: state.clock.now(),
            lease_ttl: state.config.lease_ttl,
            presence_stale: state.config.presence_stale,
        }
    }
}

async fn card(labels: &mut Labels<'_>, ctx: CardCtx, task: &Task) -> StoreResult<CardView> {
    let children = labels
        .store
        .list_tasks(&TaskFilter {
            parent: Some(task.id),
            ..TaskFilter::default()
        })
        .await?;
    let progress = if children.is_empty() {
        None
    } else {
        let p = Progress::of(&children);
        let mut owners = Vec::with_capacity(p.owners_by_project.len());
        for (project, agents) in &p.owners_by_project {
            let mut names = Vec::with_capacity(agents.len());
            for a in agents {
                names.push(labels.agent_label(*a).await?);
            }
            owners.push(OwnerGroup {
                project: labels.project(*project),
                agents: names,
            });
        }
        Some(ProgressView {
            done: p.done,
            total: p.total,
            stations: children
                .iter()
                .map(|c| Station {
                    status: c.status.as_str(),
                    title: c.title.clone(),
                })
                .collect(),
            blocked: p.blocked,
            owners,
        })
    };
    let owner = match task.owner_agent_id {
        Some(a) => Some(labels.agent_label(a).await?),
        None => None,
    };
    let held = matches!(
        task.status,
        TaskStatus::Planning | TaskStatus::Claimed | TaskStatus::Blocked | TaskStatus::Review
    );
    let owner_live = match task.owner_session_id {
        Some(s) if held => Some(
            labels
                .store
                .get_session(s)
                .await?
                .is_some_and(|s| s.last_heartbeat_at >= ctx.now - ctx.presence_stale),
        ),
        _ => None,
    };
    let parent = match task.parent_id {
        Some(p) => labels
            .task_title(p)
            .await?
            .map(|title| (p.to_string(), title)),
        None => None,
    };
    let (aspect, label) = state_mark(task);
    Ok(CardView {
        id: task.id.to_string(),
        title: task.title.clone(),
        status: task.status.as_str(),
        aspect,
        label,
        project: labels.project(task.project_id),
        owner,
        owner_live,
        parent,
        lease: lease_view(ctx.now, ctx.lease_ttl, task),
        progress,
        blocked_line: task.block.as_ref().and_then(|b| first_line(&b.reason)),
        blocked_on_you: task
            .block
            .as_ref()
            .is_some_and(|b| b.waiting_on == Some(crate::domain::WaitingOn::Human)),
    })
}

#[derive(Template)]
#[template(path = "_card.html")]
struct CardFragment {
    card: CardView,
}

// ---------------------------------------------------------------- board

pub struct ColumnView {
    pub status: &'static str,
    pub label: &'static str,
    pub cards: Vec<CardView>,
    /// `Some` only for the Done column.
    pub done: Option<DoneHead>,
}

/// The Done column's head: what you have not acked, and the History links.
pub struct DoneHead {
    /// Top-level unseen done tasks (may exceed the cards shown).
    pub unseen: u64,
    /// Auto-acked unseen done tasks, subtasks included (the same set History `?unseen=auto`
    /// lists).
    pub auto_unseen: u64,
    /// The bulk button: "Ack all", or "Ack N shown" when more are unseen than the cards.
    pub ack_all_label: String,
    /// `/ui/history`, plus `?project=<id>` when the board is filtered.
    pub history_href: String,
    /// `/ui/history?unseen=auto`, plus `&project=<id>` when the board is filtered.
    pub auto_href: String,
}

#[derive(Clone)]
pub struct ProjectOption {
    pub id: String,
    pub name: String,
    pub key: String,
    pub line: u8,
    pub selected: bool,
}

#[derive(Template)]
#[template(path = "board.html")]
struct BoardPage {
    csrf: Option<String>,
    logout: bool,
    /// The top bar's counts (`None` when signed out).
    pulse: Option<Pulse>,
    nav: &'static str,
    projects: Vec<ProjectOption>,
    /// The filtered project's id (for the columns re-fetch URL).
    filter: Option<String>,
    columns: Vec<ColumnView>,
    needs: Vec<NeedCard>,
    back: &'static str,
    /// Where the Done column's Ack forms return to: the board, with its project filter.
    ack_back: String,
}

#[derive(Template)]
#[template(path = "_columns.html")]
struct ColumnsFragment {
    csrf: Option<String>,
    columns: Vec<ColumnView>,
    ack_back: String,
}

#[derive(Deserialize)]
pub struct BoardQuery {
    /// A project key or id.
    #[serde(default)]
    pub project: Option<String>,
}

/// Every project as a `<select>` option, by name (`selected`: the chosen one).
pub(super) fn project_options(
    labels: &Labels<'_>,
    selected: Option<ProjectId>,
) -> Vec<ProjectOption> {
    let mut projects: Vec<ProjectOption> = labels
        .projects
        .values()
        .map(|p| ProjectOption {
            id: p.id.to_string(),
            name: p.display_name.clone(),
            key: p.key.clone(),
            line: labels.lines.get(&p.id).copied().unwrap_or(0),
            selected: selected == Some(p.id),
        })
        .collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.key.cmp(&b.key)));
    projects
}

/// The project named by `?project=` (its id or its key); `None` when absent or blank.
pub(super) async fn board_filter(store: &dyn Store, q: &BoardQuery) -> UiResult<Option<Project>> {
    let Some(raw) = q
        .project
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let found = match raw.parse::<ProjectId>() {
        Ok(id) => store.get_project(id).await?,
        Err(_) => store.get_project_by_key(raw).await?,
    };
    found.map(Some).ok_or(UiError::NotFound("No such project."))
}

/// Tasks per column. Open, Claimed and Done hold top-level tasks (subtasks show on their
/// parent's card); Blocked and Review also hold subtasks, so everything waiting on someone is
/// in its column. Claimed also holds `planning` tasks (the block is occupied, the work not yet
/// started). Done holds the top-level tasks you have not acked (at most `DONE_COLUMN_LIMIT`),
/// newest first.
async fn columns(
    state: &AppState,
    labels: &mut Labels<'_>,
    project: Option<ProjectId>,
) -> StoreResult<Vec<ColumnView>> {
    let ctx = CardCtx::of(state);
    let since = ctx.now - state.config.done_auto_ack;
    let mut out = Vec::with_capacity(COLUMNS.len());
    for (status, label) in COLUMNS {
        let mut done = None;
        let mut tasks: Vec<Task> = if status == TaskStatus::Done {
            let unseen = TaskFilter {
                project,
                status: Some(TaskStatus::Done),
                top_level: true,
                done: DoneFilter::Unseen { since },
                order: TaskOrder::DoneNewest,
                ..TaskFilter::default()
            };
            let auto_unseen = state
                .store
                .count_tasks(&TaskFilter {
                    project,
                    status: Some(TaskStatus::Done),
                    done: DoneFilter::AutoAckedUnseen { since },
                    ..TaskFilter::default()
                })
                .await?;
            done = Some(DoneHead {
                unseen: state.store.count_tasks(&unseen).await?,
                auto_unseen,
                ack_all_label: String::new(),
                history_href: match project {
                    Some(p) => format!("/ui/history?project={p}"),
                    None => "/ui/history".to_string(),
                },
                auto_href: match project {
                    Some(p) => format!("/ui/history?unseen=auto&project={p}"),
                    None => "/ui/history?unseen=auto".to_string(),
                },
            });
            state
                .store
                .list_tasks(&TaskFilter {
                    limit: Some(DONE_COLUMN_LIMIT),
                    ..unseen
                })
                .await?
        } else {
            state
                .store
                .list_tasks(&TaskFilter {
                    project,
                    status: Some(status),
                    ..TaskFilter::default()
                })
                .await?
        };
        if status == TaskStatus::Claimed {
            tasks.extend(
                state
                    .store
                    .list_tasks(&TaskFilter {
                        project,
                        status: Some(TaskStatus::Planning),
                        ..TaskFilter::default()
                    })
                    .await?,
            );
            tasks.sort_by_key(|t| (t.created_at, t.id));
        }
        let tasks: Vec<Task> = tasks
            .into_iter()
            .filter(|t| {
                t.parent_id.is_none() || matches!(status, TaskStatus::Blocked | TaskStatus::Review)
            })
            .collect();
        let mut cards = Vec::with_capacity(tasks.len());
        for t in &tasks {
            cards.push(card(labels, ctx, t).await?);
        }
        if let Some(d) = done.as_mut() {
            d.ack_all_label = if d.unseen > cards.len() as u64 {
                format!("Ack {} shown", cards.len())
            } else {
                "Ack all".to_string()
            };
        }
        out.push(ColumnView {
            status: status.as_str(),
            label,
            cards,
            done,
        });
    }
    Ok(out)
}

pub async fn board(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    Query(q): Query<BoardQuery>,
) -> UiResult<Response> {
    let filter = board_filter(&*state.store, &q).await?;
    let mut labels = Labels::load(&*state.store).await?;
    let filter_id = filter.as_ref().map(|p| p.id);
    let columns = columns(&state, &mut labels, filter_id).await?;
    let projects = project_options(&labels, filter_id);
    let needs = needs_you(&state, &mut labels).await?;
    html(&BoardPage {
        logout: session.logout,
        csrf: Some(session.csrf),
        pulse: Some(pulse_counts(&state).await?),
        nav: "board",
        projects,
        filter: filter.map(|p| p.id.to_string()),
        columns,
        needs,
        back: "/ui/board",
        ack_back: board_url(filter_id),
    })
}

/// The board's URL, with the project filter when there is one.
fn board_url(project: Option<ProjectId>) -> String {
    match project {
        Some(p) => format!("/ui/board?project={p}"),
        None => "/ui/board".to_string(),
    }
}

pub async fn board_columns(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    Query(q): Query<BoardQuery>,
) -> UiResult<Response> {
    let filter = board_filter(&*state.store, &q).await?.map(|p| p.id);
    let mut labels = Labels::load(&*state.store).await?;
    let columns = columns(&state, &mut labels, filter).await?;
    html(&ColumnsFragment {
        csrf: Some(session.csrf),
        columns,
        ack_back: board_url(filter),
    })
}

// ---------------------------------------------------------------- children

pub struct ChildView {
    pub status: &'static str,
    pub project: ProjectLabel,
    pub title: String,
    pub owner: Option<String>,
}

#[derive(Template)]
#[template(path = "_children.html")]
struct ChildrenFragment {
    children: Vec<ChildView>,
}

pub async fn children(State(state): State<AppState>, Path(id): Path<String>) -> UiResult<Response> {
    let id: TaskId = id.parse().map_err(|_| UiError::NotFound("No such task."))?;
    if state.store.get_task(id).await?.is_none() {
        return Err(UiError::NotFound("No such task."));
    }
    let tasks = state
        .store
        .list_tasks(&TaskFilter {
            parent: Some(id),
            ..TaskFilter::default()
        })
        .await?;
    let mut labels = Labels::load(&*state.store).await?;
    let mut children = Vec::with_capacity(tasks.len());
    for t in tasks {
        let owner = match t.owner_agent_id {
            Some(a) => Some(labels.agent_label(a).await?),
            None => None,
        };
        children.push(ChildView {
            status: t.status.as_str(),
            project: labels.project(t.project_id),
            title: t.title,
            owner,
        });
    }
    html(&ChildrenFragment { children })
}

// ---------------------------------------------------------------- overview

pub struct Counters {
    pub hosts: usize,
    pub sessions: usize,
    pub open: usize,
    pub unread: u32,
    pub urgent: u32,
}

pub struct AgentCard {
    pub id: String,
    pub project: Option<ProjectLabel>,
    pub host: String,
    pub harness: String,
    pub current_task: Option<String>,
    pub live: bool,
    pub last_seen: String,
    pub last_seen_at: String,
}

#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewPage {
    csrf: Option<String>,
    logout: bool,
    /// The top bar's counts (`None` when signed out).
    pulse: Option<Pulse>,
    nav: &'static str,
    counters: Counters,
    agents: Vec<AgentCard>,
    needs: Vec<NeedCard>,
    back: &'static str,
}

#[derive(Template)]
#[template(path = "_overview_panel.html")]
struct OverviewPanel {
    counters: Counters,
    agents: Vec<AgentCard>,
}

/// "12s ago", "5m ago", "3h ago", "2d ago".
pub(super) fn ago(now: OffsetDateTime, t: OffsetDateTime) -> String {
    let secs = (now - t).whole_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

async fn overview_data(state: &AppState) -> UiResult<(Counters, Vec<AgentCard>)> {
    let now = state.clock.now();
    let live_since = now - state.config.presence_stale;
    let sessions = state.store.list_sessions(None).await?;
    let live: Vec<_> = sessions
        .iter()
        .filter(|(_, s)| s.last_heartbeat_at >= live_since)
        .collect();
    let hosts: HashSet<&str> = live.iter().map(|(a, _)| a.host.as_str()).collect();
    let open = state
        .store
        .list_tasks(&TaskFilter {
            status: Some(TaskStatus::Open),
            ..TaskFilter::default()
        })
        .await?
        .len();
    let unread = state
        .messages
        .human_unread_count(None)
        .await
        .map_err(|e| UiError::Internal(e.to_string()))?;
    let urgent = state
        .messages
        .human_unread_count(Some(Importance::Urgent))
        .await
        .map_err(|e| UiError::Internal(e.to_string()))?;
    let counters = Counters {
        hosts: hosts.len(),
        sessions: live.len(),
        open,
        unread,
        urgent,
    };

    // One card per agent: its most recently seen session, if seen within the window.
    let mut latest: HashMap<AgentId, (&Agent, OffsetDateTime)> = HashMap::new();
    for (agent, session) in &sessions {
        let seen = session.last_heartbeat_at;
        let entry = latest.entry(agent.id).or_insert((agent, seen));
        if seen > entry.1 {
            entry.1 = seen;
        }
    }
    let mut recent: Vec<_> = latest
        .into_values()
        .filter(|(_, seen)| *seen >= now - AGENT_WINDOW)
        .collect();
    recent.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.harness.cmp(&b.0.harness))
            .then_with(|| a.0.host.cmp(&b.0.host))
    });
    let labels = Labels::load(&*state.store).await?;
    let mut agents = Vec::with_capacity(recent.len());
    for (agent, seen) in recent {
        let current_task = state
            .store
            .list_tasks(&TaskFilter {
                owner: Some(agent.id),
                ..TaskFilter::default()
            })
            .await?
            .into_iter()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Planning
                        | TaskStatus::Claimed
                        | TaskStatus::Blocked
                        | TaskStatus::Review
                )
            })
            .max_by_key(|t| t.updated_at)
            .map(|t| t.title);
        agents.push(AgentCard {
            id: agent.id.to_string(),
            project: agent.project_id.map(|p| labels.project(p)),
            host: agent.host.clone(),
            harness: agent.harness.clone(),
            current_task,
            live: seen >= live_since,
            last_seen: ago(now, seen),
            last_seen_at: seen.format(&Rfc3339).unwrap_or_default(),
        });
    }
    Ok((counters, agents))
}

pub async fn overview(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
) -> UiResult<Response> {
    let (counters, agents) = overview_data(&state).await?;
    let mut labels = Labels::load(&*state.store).await?;
    let needs = needs_you(&state, &mut labels).await?;
    html(&OverviewPage {
        logout: session.logout,
        csrf: Some(session.csrf),
        pulse: Some(pulse_counts(&state).await?),
        nav: "overview",
        counters,
        agents,
        needs,
        back: "/ui/",
    })
}

pub async fn overview_panel(State(state): State<AppState>) -> UiResult<Response> {
    let (counters, agents) = overview_data(&state).await?;
    html(&OverviewPanel { counters, agents })
}

// ---------------------------------------------------------------- pulse

/// The top bar's live state: agents with a live session, blocked tasks (at any depth, the
/// same tasks the board's Blocked column shows) and how many "Needs you" cards there are.
pub struct Pulse {
    pub live: usize,
    pub blocked: usize,
    pub need: usize,
}

pub async fn pulse_counts(state: &AppState) -> StoreResult<Pulse> {
    let live_since = state.clock.now() - state.config.presence_stale;
    let live: HashSet<AgentId> = state
        .store
        .list_sessions(None)
        .await?
        .iter()
        .filter(|(_, s)| s.last_heartbeat_at >= live_since)
        .map(|(a, _)| a.id)
        .collect();
    let blocked = state
        .store
        .list_tasks(&TaskFilter {
            status: Some(TaskStatus::Blocked),
            ..TaskFilter::default()
        })
        .await?
        .len();
    let mut labels = Labels::load(&*state.store).await?;
    let need = needs_you(state, &mut labels).await?.len();
    Ok(Pulse {
        live: live.len(),
        blocked,
        need,
    })
}

#[derive(Template)]
#[template(path = "_pulse.html")]
struct PulseFragment {
    p: Pulse,
}

pub async fn pulse(State(state): State<AppState>) -> UiResult<Response> {
    html(&PulseFragment {
        p: pulse_counts(&state).await?,
    })
}

// ---------------------------------------------------------------- live frames

/// Data for frames that only trigger a re-fetch. Browsers drop SSE events whose data is
/// empty, so it must not be.
const PING: &str = "1";

/// `(event name, data)` pairs for the UI stream; see `super::live`.
pub type Frames = Vec<(String, String)>;

/// What every page should re-fetch after missed events. (A task page re-fetches on its
/// own `detail-<id>` frames only; after a lag it catches up on the next change.)
pub fn refresh_frames() -> Frames {
    vec![
        ("board".into(), PING.into()),
        ("overview".into(), PING.into()),
        ("inbox".into(), PING.into()),
    ]
}

/// The frames one bus event produces. A lookup failure degrades to a full refresh.
pub async fn live_frames(state: &AppState, event: &Event) -> Frames {
    match task_frames(state, event).await {
        Ok(frames) => frames,
        Err(e) => {
            eprintln!("axl: ui live update failed: {e}");
            refresh_frames()
        }
    }
}

async fn task_frames(state: &AppState, event: &Event) -> UiResult<Frames> {
    let (id, kind) = match *event {
        Event::TaskChanged { id, kind, .. } => (id, kind),
        Event::HumanArchived { .. } => {
            return Ok(vec![
                ("inbox".to_string(), PING.to_string()),
                ("overview".into(), PING.into()),
            ]);
        }
        // The tokens page lists pending enrollments next to the tokens they mint.
        Event::EnrollmentChanged { .. } => {
            return Ok(vec![("tokens".to_string(), PING.to_string())]);
        }
        Event::MessageCreated { to, .. } => {
            let mut frames = vec![("inbox".to_string(), PING.to_string())];
            if let Address::Task(t) = to {
                frames.push((format!("detail-{t}"), PING.into()));
            }
            frames.push(("overview".into(), PING.into()));
            return Ok(frames);
        }
    };
    // The card on the board is the task's top-level ancestor; the task page of the task and
    // of every ancestor (their children lists) re-fetch.
    let mut root = state
        .store
        .get_task(id)
        .await?
        .ok_or(UiError::NotFound("task"))?;
    let mut frames = vec![(format!("detail-{id}"), PING.to_string())];
    for _ in 0..MAX_DEPTH {
        let Some(parent) = root.parent_id else { break };
        root = state
            .store
            .get_task(parent)
            .await?
            .ok_or(UiError::NotFound("task"))?;
        frames.push((format!("detail-{parent}"), PING.into()));
    }
    // Subtasks have cards of their own while blocked or in review, so any status change may
    // move a card between columns.
    if kind != crate::domain::TaskEventKind::Edited {
        frames.push(("board".into(), PING.into()));
    }
    let mut labels = Labels::load(&*state.store).await?;
    let card = card(&mut labels, CardCtx::of(state), &root).await?;
    // SSE data must be one line per `data:` field; HTML does not care about the newlines.
    let html = CardFragment { card }.render()?.replace(['\r', '\n'], " ");
    frames.push((format!("task-{}", root.id), html));
    frames.push(("overview".into(), PING.into()));
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::ago;
    use time::Duration;
    use time::macros::datetime;

    #[test]
    fn ago_picks_the_largest_unit() {
        let now = datetime!(2026-11-27 9:00 UTC);
        assert_eq!(ago(now, now), "0s ago");
        assert_eq!(ago(now, now - Duration::seconds(59)), "59s ago");
        assert_eq!(ago(now, now - Duration::minutes(5)), "5m ago");
        assert_eq!(ago(now, now - Duration::hours(3)), "3h ago");
        assert_eq!(ago(now, now - Duration::days(2)), "2d ago");
        assert_eq!(ago(now, now + Duration::seconds(5)), "0s ago");
    }
}
