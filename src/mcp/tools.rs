//! The tool set (spec §11): parameters, schemas and implementations. Names and parameters
//! are part of the agent-facing contract; none of them identifies the caller.

use std::collections::HashSet;
use std::str::FromStr;

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AxleMcp, Labels, ToolError, project_view, render, timestamp};
use crate::domain::{
    Address, Agent, DeliveryStage, Eligibility, Importance, LinkKind, MessageId, Project,
    ProjectId, Session, SessionId, Task, TaskId, TaskStatus, WaitingOn, short_id,
};
use crate::service::identity::Resolved;
use crate::service::messages::{InboxFilter, NewMessage, check_body, check_subject};
use crate::service::tasks::{
    BlockInput, Caller, ClaimOutcome, Completion, MoveContext, NewTask, UNBLOCK_MAX,
};
use crate::store::{DoneFilter, TaskFilter};

/// Page size for inbox and search results when none is asked for, and the most allowed.
const PAGE: u32 = 20;
const PAGE_MAX: u32 = 100;
/// Sessions shown per agent in `agent_list` (most recent first).
const SESSIONS_PER_AGENT: usize = 10;

/// Every tool description starts with this, so agents never invent ids for themselves.
macro_rules! described {
    ($text:literal) => {
        concat!(
            "Identity is automatic; never pass ids or tokens for yourself. ",
            $text
        )
    };
}

/// Resolves the caller or returns its error as the tool result.
macro_rules! me {
    ($self:ident, $parts:ident) => {
        match $self.identify(&$parts).await {
            Ok(me) => me,
            Err(e) => return render(Err(e)),
        }
    };
}

// ---------------------------------------------------------------- parameters

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProjectListParams {
    /// Page size (default 20, at most 100).
    pub limit: Option<u32>,
    /// Page cursor: the `next` value of the previous page.
    pub after: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentListParams {
    /// Only agents whose home project this is (key, e.g. `github.com/acme/platform-infra`, or
    /// display name).
    pub project: Option<String>,
    /// Page size (default 20, at most 100).
    pub limit: Option<u32>,
    /// Page cursor: the `next` value of the previous page.
    pub after: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImportanceParam {
    Normal,
    High,
    Urgent,
}

impl From<ImportanceParam> for Importance {
    fn from(i: ImportanceParam) -> Self {
        match i {
            ImportanceParam::Normal => Importance::Normal,
            ImportanceParam::High => Importance::High,
            ImportanceParam::Urgent => Importance::Urgent,
        }
    }
}

/// Exactly one recipient.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ToParams {
    /// Everyone whose home project this is (key or display name).
    pub project: Option<String>,
    /// One agent, as `<project>/<host>/<harness>` (or `<host>/<harness>` for an agent with no
    /// home project); any of its sessions may read it.
    pub agent: Option<String>,
    /// One live session (its id, as shown by `agent_list`).
    pub session: Option<String>,
    /// A task's thread (task id).
    pub task: Option<String>,
    /// The human's inbox.
    pub human: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMessageParams {
    pub to: ToParams,
    pub subject: Option<String>,
    /// At most 64 KB.
    pub body: String,
    /// Defaults to `normal`.
    pub importance: Option<ImportanceParam>,
    /// Only for `to: {human: true}`: 2 to 4 short answers (one line, at most 80 characters
    /// each, no duplicates) the human can reply with in one click. Their reply is an ordinary
    /// message whose body starts with the chosen answer.
    pub options: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchInboxParams {
    /// Only messages about this project (key or display name).
    pub project: Option<String>,
    /// Only messages in this task's thread (task id).
    pub task: Option<String>,
    /// Only messages of exactly this importance.
    pub importance: Option<ImportanceParam>,
    /// Page size (default 20, at most 100).
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AckParams {
    /// Message ids from `fetch_inbox`.
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Words that must all appear in the subject or body.
    pub query: String,
    /// Key or display name.
    pub project: Option<String>,
    /// Default 20, at most 100.
    pub limit: Option<u32>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct EligibilityParams {
    /// Only agents with this harness may claim (e.g. `claude-code`).
    pub harness: Option<String>,
    /// Only agents on this host may claim.
    pub host: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskCreateParams {
    pub title: String,
    /// The first message of the task's thread (at most 64 KB).
    pub body: String,
    /// Home project (key or display name). Defaults to your home project.
    pub project: Option<String>,
    /// Parent task id, to create a child task.
    pub parent: Option<String>,
    /// Extra projects a parent task spans (keys or display names).
    pub participating_projects: Option<Vec<String>>,
    pub eligibility: Option<EligibilityParams>,
    /// Promote this message (id) to the task; an empty title / body default to its own.
    pub from_message: Option<String>,
    /// Require a plan the human approves before work starts (claiming moves the task to
    /// `planning`). Usually left off by agents.
    pub requires_plan: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StatusParam {
    Open,
    Planning,
    Claimed,
    Blocked,
    Review,
    Delivering,
    Done,
    Cancelled,
}

impl From<StatusParam> for TaskStatus {
    fn from(s: StatusParam) -> Self {
        match s {
            StatusParam::Open => TaskStatus::Open,
            StatusParam::Planning => TaskStatus::Planning,
            StatusParam::Claimed => TaskStatus::Claimed,
            StatusParam::Blocked => TaskStatus::Blocked,
            StatusParam::Review => TaskStatus::Review,
            StatusParam::Delivering => TaskStatus::Delivering,
            StatusParam::Done => TaskStatus::Done,
            StatusParam::Cancelled => TaskStatus::Cancelled,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskListParams {
    /// Key or display name; also matches parent tasks that span it.
    pub project: Option<String>,
    pub status: Option<StatusParam>,
    /// Owning agent, as `<project>/<host>/<harness>`.
    pub owner: Option<String>,
    /// Parent task id: list its children.
    pub parent: Option<String>,
    /// Page size (default 20, at most 100).
    pub limit: Option<u32>,
    /// Page cursor: the `next` value of the previous page.
    pub after: Option<String>,
    /// Also return done tasks the human acked or that auto-acked (default false).
    pub include_acked: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskIdParams {
    /// Task id.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskClaimParams {
    /// Claim this task.
    pub id: Option<String>,
    /// Instead, claim the oldest open task in your home project that you are eligible for.
    pub next: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskPostParams {
    /// Task id.
    pub id: String,
    /// At most 64 KB.
    pub body: String,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UpdateStatusParam {
    Blocked,
    Review,
}

/// What a blocked task waits on: exactly one of the three.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitingOnParam {
    /// `true`: waits on the human (shown to them as "Blocked on you").
    pub human: Option<bool>,
    /// Waits on this task (id); when it is done, yours is unblocked automatically.
    pub task: Option<String>,
    /// Waits on something outside Axle, in words (at most 500 characters).
    pub external: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskUpdateParams {
    /// Task id.
    pub id: String,
    /// `blocked` (needs `reason`) or `review` (work done, awaiting the coordinator / human).
    /// `blocked` on a task you already hold blocked updates its reason, `waiting_on` and/or
    /// `unblock`.
    pub status: Option<UpdateStatusParam>,
    /// Why it is blocked, or a note for review.
    pub reason: Option<String>,
    /// With `status: blocked`: what it waits on, `{"human": true}`, `{"task": "<id>"}` or
    /// `{"external": "<text>"}`.
    pub waiting_on: Option<WaitingOnParam>,
    /// With `status: blocked`: what would unblock it (at most 500 characters).
    pub unblock: Option<String>,
    /// New title (creator or owner only).
    pub title: Option<String>,
    /// New body (creator or owner only).
    pub body: Option<String>,
    /// Projects to add to a top-level task (keys or display names; creator or owner only).
    /// Projects are never removed; adding one already listed does nothing.
    pub add_projects: Option<Vec<String>>,
    /// Links from this task to other tasks (creator or owner only; at most 20 per task). Both
    /// tasks show the link. `duplicate` cancels this task as a duplicate of the target.
    pub related: Option<Vec<RelatedParam>>,
    /// Task ids whose link with this task to remove, whichever task added it (creator or
    /// owner only).
    pub unrelate: Option<Vec<String>>,
    /// Move this top-level task under this parent task (id; creator or owner only). The
    /// parent must be open for subtasks and in the task's project or one it spans.
    pub parent: Option<String>,
    /// URL of the pull request you opened for this task (creator or owner). Report it as
    /// soon as it exists; reporting another PR replaces the link until the task is merged.
    pub pr: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RelatedParam {
    /// The other task's id.
    pub id: String,
    pub kind: LinkKindParam,
}

/// What this task is to the other one.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LinkKindParam {
    /// The tasks are related.
    Relates,
    /// This task follows up on the other.
    FollowUp,
    /// This task duplicates the other: it is cancelled (refused once it is done).
    Duplicate,
}

impl From<LinkKindParam> for LinkKind {
    fn from(k: LinkKindParam) -> Self {
        match k {
            LinkKindParam::Relates => LinkKind::Relates,
            LinkKindParam::FollowUp => LinkKind::FollowUp,
            LinkKindParam::Duplicate => LinkKind::Duplicate,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskProposePlanParams {
    /// Task id (one you hold in `planning`).
    pub id: String,
    /// The spec or plan, as text (at most 64 KB). For a parent spanning projects, list the
    /// subtasks per project.
    pub plan: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskReleaseParams {
    /// Task id.
    pub id: String,
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskCompleteParams {
    /// Task id.
    pub id: String,
    pub summary: String,
    pub changed_files: Option<Vec<String>>,
    pub tests: Option<Vec<String>>,
    pub links: Option<Vec<String>>,
}

// ---------------------------------------------------------------- tools

#[tool_router(vis = "pub(super)")]
impl AxleMcp {
    #[tool(description = described!(
        "Who you are: project, agent, session, the tasks you hold and your unread count. \
         Call it after a restart or compaction."
    ))]
    async fn whoami(&self, Extension(parts): Extension<Parts>) -> CallToolResult {
        let me = me!(self, parts);
        render(self.whoami_impl(&me).await)
    }

    #[tool(description = described!(
        "Projects with display names, live agent counts and open task counts, by key \
         (byte order); paged (`limit`, `after` = the previous page's `next`)."
    ))]
    async fn project_list(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ProjectListParams>,
    ) -> CallToolResult {
        let _me = me!(self, parts);
        render(self.project_list_impl(p).await)
    }

    #[tool(description = described!(
        "Agents and their sessions, with presence and the tasks each session holds; \
         optionally only agents whose home project is `project`. Ordered by agent label; \
         paged (`limit`, `after` = the previous page's `next`)."
    ))]
    async fn agent_list(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AgentListParams>,
    ) -> CallToolResult {
        let _me = me!(self, parts);
        render(self.agent_list_impl(p).await)
    }

    #[tool(description = described!(
        "Send a message to exactly one of: a project (everyone in it), an agent \
         (`<project>/<host>/<harness>`), a live session, a task thread, or the human. \
         Messages you send are never shown back to you, including in your own other sessions. \
         Asking the human a question with a few clear answers? Pass them as `options` and \
         the human can reply in one click."
    ))]
    async fn send_message(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SendMessageParams>,
    ) -> CallToolResult {
        let me = me!(self, parts);
        render(self.send_message_impl(&me, p).await)
    }

    #[tool(description = described!(
        "Your unacknowledged messages, oldest first; they come back until you ack them. \
         Agent mail is acked once for all your sessions. Messages you sent yourself are not \
         shown, not even to your own other sessions."
    ))]
    async fn fetch_inbox(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<FetchInboxParams>,
    ) -> CallToolResult {
        let me = me!(self, parts);
        render(self.fetch_inbox_impl(&me, p).await)
    }

    #[tool(description = described!("Acknowledge messages from your inbox by id."))]
    async fn ack_messages(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AckParams>,
    ) -> CallToolResult {
        let me = me!(self, parts);
        render(self.ack_impl(&me, p).await)
    }

    #[tool(description = described!(
        "Full-text search over message subjects and bodies (all history), newest first."
    ))]
    async fn search_messages(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SearchParams>,
    ) -> CallToolResult {
        let _me = me!(self, parts);
        render(self.search_impl(p).await)
    }

    #[tool(description = described!(
        "Create an open task; its body is the first message of its thread. Optional parent \
         (child task), participating projects (parent spanning repos), eligibility \
         (harness / host) and from_message (promote a message)."
    ))]
    async fn task_create(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskCreateParams>,
    ) -> CallToolResult {
        let me = me!(self, parts);
        render(self.task_create_impl(&me, p).await)
    }

    #[tool(description = described!(
        "List tasks, oldest first, filtered by project, status, owner and parent; paged \
         (`limit`, `after` = the previous page's `next`). Done tasks the human has acked, or \
         that finished more than the auto-ack window ago, are left out unless `include_acked` \
         is true."
    ))]
    async fn task_list(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskListParams>,
    ) -> CallToolResult {
        let _me = me!(self, parts);
        render(self.task_list_impl(p).await)
    }

    #[tool(description = described!(
        "A task with its children, progress, related tasks, history and thread."
    ))]
    async fn task_get(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskIdParams>,
    ) -> CallToolResult {
        let _me = me!(self, parts);
        render(self.task_get_impl(p).await)
    }

    #[tool(description = described!(
        "Claim a task by id, or `next: true` for the oldest open task in your home project \
         you are eligible for. The lease lasts 15 minutes by default and is renewed while you \
         keep making tool calls."
    ))]
    async fn task_claim(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskClaimParams>,
    ) -> CallToolResult {
        let me = me!(self, parts);
        render(self.task_claim_impl(&me, p).await)
    }

    #[tool(description = described!("Post a reply in a task's thread."))]
    async fn task_post(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskPostParams>,
    ) -> CallToolResult {
        let me = me!(self, parts);
        render(self.task_post_impl(&me, p).await)
    }

    #[tool(description = described!(
        "Set a task you own to `blocked` (with a reason, and optionally `waiting_on` and \
         `unblock`: what it waits on and what would unblock it) or `review`, edit its title or \
         body (creator or owner), add participating projects to a top-level task \
         (`add_projects`; creator or owner), and/or link it to other tasks (`related`: \
         `relates`, `follow-up` or `duplicate`, which cancels this task; `unrelate` removes \
         links; creator or owner), and/or move a top-level task under a parent (`parent`; \
         creator or owner). `blocked` on a task you hold blocked updates its reason, \
         `waiting_on` or `unblock`. A task waiting on another task is unblocked when that task \
         is done. Report the pull request you opened for it with `pr` (its URL; creator \
         or owner)."
    ))]
    async fn task_update(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskUpdateParams>,
    ) -> CallToolResult {
        let me = me!(self, parts);
        render(self.task_update_impl(&me, p).await)
    }

    #[tool(description = described!(
        "Submit the spec or plan of a task you hold in `planning` for the human's approval; \
         calling it again replaces the plan under review. Then wait: the approval (or a \
         request for changes) arrives as a message, and the task becomes `claimed` once \
         approved. While planning you may post, renew or release, but not create subtasks, \
         block, review or complete."
    ))]
    async fn task_propose_plan(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskProposePlanParams>,
    ) -> CallToolResult {
        let me = me!(self, parts);
        render(self.task_propose_plan_impl(&me, p).await)
    }

    #[tool(description = described!("Extend your lease on a claimed or planning task."))]
    async fn task_renew(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskIdParams>,
    ) -> CallToolResult {
        let me = me!(self, parts);
        render(self.task_renew_impl(&me, p).await)
    }

    #[tool(description = described!("Give up a task you own; it goes back to open."))]
    async fn task_release(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskReleaseParams>,
    ) -> CallToolResult {
        let me = me!(self, parts);
        render(self.task_release_impl(&me, p).await)
    }

    #[tool(description = described!(
        "Mark a task you own done, with a summary and evidence (changed files, tests, links)."
    ))]
    async fn task_complete(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskCompleteParams>,
    ) -> CallToolResult {
        let me = me!(self, parts);
        render(self.task_complete_impl(&me, p).await)
    }
}

// ---------------------------------------------------------------- implementations

type ToolResult = Result<Value, ToolError>;

impl AxleMcp {
    fn labels(&self) -> Labels<'_> {
        Labels::new(&*self.state.store)
    }

    async fn whoami_impl(&self, me: &Resolved) -> ToolResult {
        let mut labels = self.labels();
        let held = self
            .state
            .tasks
            .list(TaskFilter {
                owner: Some(me.agent.id),
                ..TaskFilter::default()
            })
            .await?;
        let mut leases = vec![];
        for t in held.iter().filter(|t| {
            t.owner_session_id == Some(me.session.id)
                && matches!(
                    t.status,
                    TaskStatus::Planning
                        | TaskStatus::Claimed
                        | TaskStatus::Blocked
                        | TaskStatus::Review
                )
        }) {
            leases.push(json!({
                "id": t.id,
                "title": labels.task_title(t).await?,
                "status": t.status,
                "project": labels.project(t.project_id).await?,
                "lease_expires_at": t.lease_expires_at.map(timestamp),
            }));
        }
        Ok(json!({
            "project": me.project.as_ref().map(project_view),
            "agent": labels.agent(me.agent.id).await?,
            "host": me.agent.host,
            "harness": me.agent.harness,
            "session": me.session.id,
            "leases": leases,
            "unread": self.state.messages.unread_count(me).await?,
        }))
    }

    async fn project_list_impl(&self, p: ProjectListParams) -> ToolResult {
        let now = self.state.clock.now();
        let limit = page(p.limit) as usize;
        // Sort bytewise here: the cursor compares bytewise, and the store's ORDER BY follows
        // the database collation, which may disagree (e.g. Postgres en_US).
        let mut projects: Vec<Project> = self.state.store.list_projects().await?;
        projects.sort_by(|a, b| a.key.cmp(&b.key));
        projects.retain(|proj| {
            p.after
                .as_deref()
                .is_none_or(|after| proj.key.as_str() > after)
        });
        let next = more(&mut projects, limit).map(|last| last.key.clone());
        let mut out = vec![];
        for proj in &projects {
            let live: HashSet<_> = self
                .state
                .store
                .list_sessions(Some(proj.id))
                .await?
                .into_iter()
                .filter(|(_, s)| now - s.last_heartbeat_at <= self.state.config.presence_stale)
                .map(|(a, _)| a.id)
                .collect();
            let open = self
                .state
                .tasks
                .list(TaskFilter {
                    project: Some(proj.id),
                    status: Some(TaskStatus::Open),
                    ..TaskFilter::default()
                })
                .await?
                .len();
            out.push(json!({
                "key": proj.key,
                "display_name": proj.display_name,
                "live_agents": live.len(),
                "open_tasks": open,
            }));
        }
        Ok(json!({ "projects": out, "next": next }))
    }

    async fn agent_list_impl(&self, p: AgentListParams) -> ToolResult {
        let project = match &p.project {
            Some(name) => Some(self.project(name).await?.id),
            None => None,
        };
        let now = self.state.clock.now();
        let limit = page(p.limit) as usize;
        let mut labels = self.labels();
        // Sessions come oldest first; group them by agent.
        let mut agents: Vec<(String, Agent, Vec<Session>)> = vec![];
        for (agent, session) in self.state.store.list_sessions(project).await? {
            match agents.iter_mut().find(|(_, a, _)| a.id == agent.id) {
                Some((_, _, sessions)) => sessions.push(session),
                None => {
                    let label = labels.agent(agent.id).await?;
                    agents.push((label, agent, vec![session]));
                }
            }
        }
        agents.sort_by(|a, b| a.0.cmp(&b.0));
        agents.retain(|(label, _, _)| {
            p.after
                .as_deref()
                .is_none_or(|after| label.as_str() > after)
        });
        let next = more(&mut agents, limit).map(|(label, _, _)| label.clone());
        let mut out = vec![];
        for (label, agent, sessions) in agents {
            let held = self
                .state
                .tasks
                .list(TaskFilter {
                    owner: Some(agent.id),
                    ..TaskFilter::default()
                })
                .await?;
            let session_count = sessions.len();
            let mut session_views = vec![];
            // The most recent sessions only: old ones accumulate and are rarely interesting.
            for s in sessions.iter().rev().take(SESSIONS_PER_AGENT) {
                let mut tasks = vec![];
                for t in held.iter().filter(|t| {
                    t.owner_session_id == Some(s.id)
                        && matches!(
                            t.status,
                            TaskStatus::Planning
                                | TaskStatus::Claimed
                                | TaskStatus::Blocked
                                | TaskStatus::Review
                        )
                }) {
                    tasks.push(json!({
                        "id": t.id,
                        "title": labels.task_title(t).await?,
                        "status": t.status,
                    }));
                }
                session_views.push(json!({
                    "session": s.id,
                    "live": now - s.last_heartbeat_at <= self.state.config.presence_stale,
                    "last_seen_at": timestamp(s.last_heartbeat_at),
                    "tasks": tasks,
                }));
            }
            out.push(json!({
                "agent": label,
                "project": match agent.project_id {
                    Some(p) => Some(labels.project(p).await?),
                    None => None,
                },
                "host": agent.host,
                "harness": agent.harness,
                "session_count": session_count,
                "sessions": session_views,
            }));
        }
        Ok(json!({ "agents": out, "next": next }))
    }

    async fn send_message_impl(&self, me: &Resolved, p: SendMessageParams) -> ToolResult {
        let to = self.address(p.to).await?;
        let message = self
            .state
            .messages
            .send_with_options(
                &actor(me),
                NewMessage {
                    to,
                    subject: p.subject.filter(|s| !s.trim().is_empty()),
                    body: p.body,
                    importance: p.importance.map(Into::into).unwrap_or_default(),
                    reply_to: None,
                },
                p.options.as_deref().unwrap_or_default(),
            )
            .await?;
        Ok(json!({
            "id": message.id,
            "to": self.labels().address(message.to).await?,
        }))
    }

    async fn fetch_inbox_impl(&self, me: &Resolved, p: FetchInboxParams) -> ToolResult {
        let filter = InboxFilter {
            project: match &p.project {
                Some(name) => Some(self.project(name).await?.id),
                None => None,
            },
            task: p.task.as_deref().map(|t| parse_id("task", t)).transpose()?,
            importance: p.importance.map(Into::into),
            min_importance: None,
            limit: Some(page(p.limit)),
        };
        let items = self.state.messages.inbox(me, filter).await?;
        let ids: Vec<MessageId> = items.iter().map(|i| i.message.id).collect();
        self.state
            .messages
            .mark_read(Caller::Agent(me), &ids)
            .await?;
        let mut labels = self.labels();
        let mut out = Vec::with_capacity(items.len());
        for item in &items {
            out.push(labels.message(&item.message).await?);
        }
        Ok(json!({ "messages": out }))
    }

    async fn ack_impl(&self, me: &Resolved, p: AckParams) -> ToolResult {
        let ids = p
            .ids
            .iter()
            .map(|id| parse_id("message", id))
            .collect::<Result<Vec<MessageId>, _>>()?;
        let acked = self.state.messages.ack(Caller::Agent(me), &ids).await?;
        Ok(json!({ "acked": acked }))
    }

    async fn search_impl(&self, p: SearchParams) -> ToolResult {
        let project = match &p.project {
            Some(name) => Some(self.project(name).await?.id),
            None => None,
        };
        let found = self
            .state
            .messages
            .search(&p.query, project, page(p.limit))
            .await?;
        let mut labels = self.labels();
        let mut out = Vec::with_capacity(found.len());
        for m in &found {
            out.push(labels.message(m).await?);
        }
        Ok(json!({ "messages": out }))
    }

    /// A task without its body, plus its short id and delivery state.
    async fn summary(&self, labels: &mut Labels<'_>, t: &Task) -> ToolResult {
        let mut v = labels.task_summary(t).await?;
        self.add_delivery(&mut v, t).await?;
        Ok(v)
    }

    /// A task with its body, plus its short id and delivery state.
    async fn full(&self, labels: &mut Labels<'_>, t: &Task) -> ToolResult {
        let mut v = labels.task(t).await?;
        self.add_delivery(&mut v, t).await?;
        Ok(v)
    }

    /// `short_id` and `delivery`. The PR's title is third-party text and is left out.
    async fn add_delivery(&self, v: &mut Value, t: &Task) -> Result<(), ToolError> {
        v["short_id"] = json!(short_id(t.id));
        v["delivery"] = match self.state.delivery.view(t).await? {
            None => Value::Null,
            Some(d) => json!({
                "stage": d.stage,
                "stages_enabled": d.stages_enabled,
                "pr": d.pr.map(|pr| json!({
                    "url": pr.url,
                    "number": pr.number,
                    "state": pr.state,
                    "ci": pr.ci_state,
                })),
            }),
        };
        Ok(())
    }

    async fn task_create_impl(&self, me: &Resolved, p: TaskCreateParams) -> ToolResult {
        let parent = p
            .parent
            .as_deref()
            .map(|id| parse_id::<TaskId>("parent task", id))
            .transpose()?;
        let project_id = match (&p.project, &me.project, parent) {
            (Some(name), _, _) => self.project(name).await?.id,
            (None, Some(home), _) => home.id,
            (None, None, Some(parent)) => {
                self.state
                    .tasks
                    .get(parent)
                    .await
                    .map_err(ToolError::from)?
                    .task
                    .project_id
            }
            (None, None, None) => {
                return Err(ToolError::invalid(
                    "you have no home project; pass `project` (key or display name)",
                ));
            }
        };
        let mut extra_projects = vec![];
        for name in p.participating_projects.unwrap_or_default() {
            let id = self.project(&name).await?.id;
            if id != project_id && !extra_projects.contains(&id) {
                extra_projects.push(id);
            }
        }
        let eligibility = p.eligibility.unwrap_or_default();
        let new = NewTask {
            project_id,
            parent_id: parent,
            title: p.title,
            body: p.body,
            extra_projects,
            eligibility: Eligibility {
                harness: non_blank(eligibility.harness),
                host: non_blank(eligibility.host),
            },
            lease_ttl: None,
            requires_plan: p.requires_plan.unwrap_or(false),
        };
        let task = match p.from_message {
            Some(m) => {
                let source = parse_id("message", &m)?;
                self.state
                    .tasks
                    .create_from_message(Caller::Agent(me), source, new)
                    .await?
            }
            None => {
                if new.title.trim().is_empty() {
                    return Err(ToolError::invalid("a task needs a title"));
                }
                self.state.tasks.create(Caller::Agent(me), new).await?
            }
        };
        self.full(&mut self.labels(), &task).await
    }

    async fn task_list_impl(&self, p: TaskListParams) -> ToolResult {
        let limit = page(p.limit);
        let filter = TaskFilter {
            project: match &p.project {
                Some(name) => Some(self.project(name).await?.id),
                None => None,
            },
            status: p.status.map(Into::into),
            owner: match &p.owner {
                Some(label) => Some(self.agent(label).await?.id),
                None => None,
            },
            parent: p
                .parent
                .as_deref()
                .map(|id| parse_id("parent task", id))
                .transpose()?,
            after: p
                .after
                .as_deref()
                .map(|id| parse_id("cursor task", id))
                .transpose()?,
            // One extra row says whether there is another page.
            limit: Some(limit + 1),
            done: if p.include_acked.unwrap_or(false) {
                DoneFilter::Any
            } else {
                DoneFilter::Unseen {
                    since: self.state.clock.now() - self.state.config.done_auto_ack,
                }
            },
            ..TaskFilter::default()
        };
        let mut tasks = self.state.tasks.list(filter).await?;
        let next = more(&mut tasks, limit as usize).map(|last| last.id);
        let mut labels = self.labels();
        let mut out = Vec::with_capacity(tasks.len());
        for t in &tasks {
            out.push(self.summary(&mut labels, t).await?);
        }
        Ok(json!({ "tasks": out, "next": next }))
    }

    async fn task_get_impl(&self, p: TaskIdParams) -> ToolResult {
        let id: TaskId = parse_id("task", &p.id)?;
        let detail = self.state.tasks.get(id).await?;
        let thread = self.state.messages.thread(id).await?;
        let mut labels = self.labels();
        let project = Some(detail.task.project_id);
        let mut children = vec![];
        for c in &detail.children {
            children.push(self.summary(&mut labels, c).await?);
        }
        let mut events = vec![];
        for e in &detail.events {
            let note = match &e.note {
                Some(n) => Some(labels.wrap(e.actor, project, n).await?),
                None => None,
            };
            events.push(json!({
                "kind": e.kind,
                "at": timestamp(e.at),
                "by": labels.actor(e.actor).await?,
                "note": note,
            }));
        }
        let mut messages = vec![];
        for m in &thread {
            messages.push(labels.message(m).await?);
        }
        let mut related = vec![];
        for r in self.state.tasks.related(id).await? {
            related.push(json!({
                "id": r.other.id,
                "kind": r.link.kind.as_str(),
                "direction": if r.outgoing { "outgoing" } else { "incoming" },
                "label": r.label(),
                "title": labels.task_title(&r.other).await?,
                "status": r.other.status,
            }));
        }
        Ok(json!({
            "task": self.full(&mut labels, &detail.task).await?,
            "progress": { "done": detail.progress.done, "total": detail.progress.total },
            "children": children,
            "related": related,
            "events": events,
            "thread": messages,
        }))
    }

    async fn task_claim_impl(&self, me: &Resolved, p: TaskClaimParams) -> ToolResult {
        let mut labels = self.labels();
        match (p.id.as_deref(), p.next.unwrap_or(false)) {
            (Some(id), false) => {
                let id: TaskId = parse_id("task", id)?;
                match self.state.tasks.claim(me, id).await? {
                    ClaimOutcome::Claimed(task) => {
                        with_claim_step(self.summary(&mut labels, &task).await?, &task)
                    }
                    ClaimOutcome::AlreadyOwned { by, until } => {
                        let by = labels.agent(by).await?;
                        Err(ToolError::invalid(match until {
                            Some(t) => {
                                format!("already owned by {by}, lease until {}", timestamp(t))
                            }
                            None => format!(
                                "already owned by {by} (blocked, in review or with a plan \
                                 awaiting approval: its lease does not expire)"
                            ),
                        }))
                    }
                    ClaimOutcome::NotEligible => Err(ToolError::invalid(
                        "you are not eligible for this task (harness, host or project)",
                    )),
                    ClaimOutcome::NotFound => Err(ToolError::invalid("task not found")),
                    ClaimOutcome::Closed(status) => {
                        Err(ToolError::invalid(format!("the task is {status}")))
                    }
                }
            }
            (None, true) => {
                let Some(home) = &me.project else {
                    return Err(ToolError::invalid(
                        "you have no home project, so there is no `next` task; claim one by id",
                    ));
                };
                match self.state.tasks.claim_next(me, home.id).await? {
                    Some(task) => with_claim_step(self.summary(&mut labels, &task).await?, &task),
                    None => Ok(json!({
                        "task": null,
                        "note": format!("no open task you are eligible for in {}", home.key),
                    })),
                }
            }
            _ => Err(ToolError::invalid(
                "pass exactly one of `id` or `next: true`",
            )),
        }
    }

    async fn task_post_impl(&self, me: &Resolved, p: TaskPostParams) -> ToolResult {
        let id: TaskId = parse_id("task", &p.id)?;
        let message = self
            .state
            .messages
            .send(
                &actor(me),
                NewMessage {
                    to: Address::Task(id),
                    subject: None,
                    body: p.body,
                    importance: Importance::Normal,
                    reply_to: None,
                },
            )
            .await?;
        Ok(json!({ "id": message.id, "task": id }))
    }

    /// Every input is validated before the first write (title, body, reason, project names),
    /// so rejected input leaves the task untouched. The status change (owner-guarded) runs
    /// before the edit. After a
    /// successful status change the caller holds the task, so the edit's guard passes.
    async fn task_update_impl(&self, me: &Resolved, p: TaskUpdateParams) -> ToolResult {
        let id: TaskId = parse_id("task", &p.id)?;
        let add_projects = p.add_projects.unwrap_or_default();
        let mut related = Vec::new();
        for r in p.related.unwrap_or_default() {
            related.push((parse_id::<TaskId>("task", &r.id)?, LinkKind::from(r.kind)));
        }
        let mut unrelate = Vec::new();
        for r in p.unrelate.unwrap_or_default() {
            unrelate.push(parse_id::<TaskId>("task", &r)?);
        }
        let parent = match p.parent.as_deref() {
            Some(raw) => Some(parse_id::<TaskId>("parent task", raw)?),
            None => None,
        };
        if p.status.is_none()
            && p.title.is_none()
            && p.body.is_none()
            && add_projects.is_empty()
            && related.is_empty()
            && unrelate.is_empty()
            && parent.is_none()
            && p.pr.is_none()
        {
            return Err(ToolError::invalid(
                "nothing to update: pass `status`, `title`, `body`, `add_projects`, `related`, \
                 `unrelate`, `parent` and/or `pr`",
            ));
        }
        if parent == Some(id) {
            return Err(ToolError::from(crate::service::tasks::TaskError::OwnParent));
        }
        if related.iter().any(|(t, _)| *t == id) {
            return Err(ToolError::from(crate::service::tasks::TaskError::SelfLink));
        }
        let mut projects = Vec::with_capacity(add_projects.len());
        for name in &add_projects {
            projects.push(self.project(name).await?.id);
        }
        if !projects.is_empty() {
            let current = self
                .state
                .tasks
                .get(id)
                .await
                .map_err(ToolError::from)?
                .task;
            if current.parent_id.is_some() {
                return Err(ToolError::from(
                    crate::service::tasks::TaskError::ChildExtraProjects,
                ));
            }
            if matches!(current.status, TaskStatus::Done | TaskStatus::Cancelled) {
                return Err(ToolError::from(crate::service::tasks::TaskError::Closed(
                    current.status,
                )));
            }
        }
        if p.reason.is_some() && p.status.is_none() {
            return Err(ToolError::invalid(
                "`reason` goes with `status`; pass `status: blocked` or `status: review`",
            ));
        }
        let blocking = matches!(p.status, Some(UpdateStatusParam::Blocked));
        if (p.waiting_on.is_some() || p.unblock.is_some()) && !blocking {
            return Err(ToolError::invalid(
                "`waiting_on` and `unblock` go with `status: blocked`",
            ));
        }
        let waiting_on = match p.waiting_on {
            Some(w) => Some(waiting_on(w)?),
            None => None,
        };
        if p.unblock
            .as_deref()
            .is_some_and(|u| u.trim().chars().count() > UNBLOCK_MAX)
        {
            return Err(ToolError::invalid(
                "`unblock` is at most 500 characters: say briefly what would unblock the task",
            ));
        }
        if p.title.as_deref().is_some_and(|t| t.trim().is_empty()) {
            return Err(ToolError::invalid("a task needs a title"));
        }
        if let Some(title) = &p.title {
            check_subject("title", title).map_err(ToolError::from)?;
        }
        if let Some(body) = &p.body {
            check_body(body).map_err(ToolError::from)?;
        }
        let reason = non_blank(p.reason);
        if let Some(reason) = &reason {
            check_body(reason).map_err(ToolError::from)?;
        }
        // Links and the move are checked as far as they can be without a lock, so a request
        // they would refuse changes nothing (the status and edit below run first).
        if !related.is_empty() || !unrelate.is_empty() {
            self.state
                .tasks
                .check_relate(Caller::Agent(me), id, &related, &unrelate)
                .await?;
        }
        if let Some(parent) = parent {
            let ctx = MoveContext {
                adds_projects: !projects.is_empty(),
                // Review clears the wait; a block with `waiting_on` replaces it; a block
                // update without one keeps it.
                moved_wait: match (p.status, &waiting_on) {
                    (Some(UpdateStatusParam::Review), _) => Some(None),
                    (Some(UpdateStatusParam::Blocked), Some(WaitingOn::Task(t))) => Some(Some(*t)),
                    (Some(UpdateStatusParam::Blocked), Some(_)) => Some(None),
                    _ => None,
                },
            };
            self.state
                .tasks
                .check_move(Caller::Agent(me), id, parent, &ctx)
                .await?;
        }
        let mut task = match p.status {
            // A first block needs a reason; the service says so (an update may omit it).
            Some(UpdateStatusParam::Blocked) => Some(
                self.state
                    .tasks
                    .block_with(
                        me,
                        id,
                        BlockInput {
                            reason,
                            waiting_on,
                            unblock: p.unblock,
                        },
                    )
                    .await?,
            ),
            Some(UpdateStatusParam::Review) => {
                Some(self.state.tasks.to_review(me, id, reason).await?)
            }
            None => None,
        };
        if p.title.is_some() || p.body.is_some() {
            task = Some(self.state.tasks.edit(me, id, p.title, p.body).await?);
        }
        if !projects.is_empty() {
            task = Some(
                self.state
                    .tasks
                    .add_projects(Caller::Agent(me), id, &projects)
                    .await?,
            );
        }
        if let Some(parent) = parent {
            task = Some(
                self.state
                    .tasks
                    .move_under(Caller::Agent(me), id, parent)
                    .await?,
            );
        }
        if !related.is_empty() || !unrelate.is_empty() {
            task = Some(
                self.state
                    .tasks
                    .relate(Caller::Agent(me), id, &related, &unrelate)
                    .await?,
            );
        }
        if let Some(url) = &p.pr {
            task = Some(
                self.state
                    .delivery
                    .link_pr(Caller::Agent(me), id, url)
                    .await?,
            );
        }
        let task = task.expect("at least one update ran");
        self.summary(&mut self.labels(), &task).await
    }

    async fn task_propose_plan_impl(&self, me: &Resolved, p: TaskProposePlanParams) -> ToolResult {
        let id: TaskId = parse_id("task", &p.id)?;
        let (task, plan) = self.state.tasks.propose_plan(me, id, p.plan).await?;
        let mut v = self.summary(&mut self.labels(), &task).await?;
        v["plan_message"] = json!(plan.id);
        v["next_step"] = json!(
            "Wait for the human's decision; it arrives as a message. Do not start the work until \
             the task is `claimed` (approved)."
        );
        Ok(v)
    }

    async fn task_renew_impl(&self, me: &Resolved, p: TaskIdParams) -> ToolResult {
        let task = self.state.tasks.renew(me, parse_id("task", &p.id)?).await?;
        self.summary(&mut self.labels(), &task).await
    }

    async fn task_release_impl(&self, me: &Resolved, p: TaskReleaseParams) -> ToolResult {
        let task = self
            .state
            .tasks
            .release(me, parse_id("task", &p.id)?, non_blank(p.note))
            .await?;
        self.summary(&mut self.labels(), &task).await
    }

    async fn task_complete_impl(&self, me: &Resolved, p: TaskCompleteParams) -> ToolResult {
        if p.summary.trim().is_empty() {
            return Err(ToolError::invalid("a summary is required"));
        }
        let task = self
            .state
            .tasks
            .complete(
                me,
                parse_id("task", &p.id)?,
                Completion {
                    summary: p.summary,
                    changed_files: p.changed_files.unwrap_or_default(),
                    tests: p.tests.unwrap_or_default(),
                    links: p.links.unwrap_or_default(),
                },
            )
            .await?;
        let mut v = self.summary(&mut self.labels(), &task).await?;
        if task.status == TaskStatus::Delivering {
            let project = self
                .state
                .store
                .get_project(task.project_id)
                .await?
                .ok_or_else(|| ToolError::internal("task without a project"))?;
            let last = self
                .state
                .delivery
                .settings(&project)
                .await?
                .last_stage()
                .map_or("its last stage", DeliveryStage::label);
            v["next_step"] = json!(format!(
                "Delivery is tracked by Axle; this task moves to done when it reaches {last}. \
                 Nothing more to do."
            ));
        }
        Ok(v)
    }

    // ------------------------------------------------------------ name resolution

    /// A project by key, else by display name (exact, then ignoring case).
    async fn project(&self, name: &str) -> Result<Project, ToolError> {
        let name = name.trim();
        if let Some(p) = self.state.store.get_project_by_key(name).await? {
            return Ok(p);
        }
        let all = self.state.store.list_projects().await?;
        let exact: Vec<&Project> = all.iter().filter(|p| p.display_name == name).collect();
        let matches = if exact.is_empty() {
            all.iter()
                .filter(|p| p.display_name.eq_ignore_ascii_case(name))
                .collect()
        } else {
            exact
        };
        match matches.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(ToolError::invalid(format!("unknown project {name:?}"))),
            many => Err(ToolError::invalid(format!(
                "project name {name:?} is ambiguous; use a key: {}",
                many.iter()
                    .map(|p| p.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// An agent from `<project>/<host>/<harness>` or `<host>/<harness>`.
    async fn agent(&self, label: &str) -> Result<Agent, ToolError> {
        let mut parts = label.trim().rsplitn(3, '/');
        let (Some(harness), Some(host)) = (parts.next(), parts.next()) else {
            return Err(ToolError::invalid(format!(
                "agent {label:?} must look like <project>/<host>/<harness>"
            )));
        };
        let project: Option<ProjectId> = match parts.next() {
            Some(name) => Some(self.project(name).await?.id),
            None => None,
        };
        self.state
            .store
            .list_sessions(project)
            .await?
            .into_iter()
            .map(|(a, _)| a)
            .find(|a| a.project_id == project && a.host == host && a.harness == harness)
            .ok_or_else(|| ToolError::invalid(format!("unknown agent {label:?}")))
    }

    async fn address(&self, to: ToParams) -> Result<Address, ToolError> {
        let human = to.human.unwrap_or(false);
        let set = [
            to.project.is_some(),
            to.agent.is_some(),
            to.session.is_some(),
            to.task.is_some(),
            human,
        ]
        .into_iter()
        .filter(|s| *s)
        .count();
        if set != 1 {
            return Err(ToolError::invalid(
                "`to` needs exactly one of project, agent, session, task or human: true",
            ));
        }
        Ok(if let Some(name) = to.project {
            Address::Project(self.project(&name).await?.id)
        } else if let Some(label) = to.agent {
            Address::Agent(self.agent(&label).await?.id)
        } else if let Some(s) = to.session {
            Address::Session(parse_id::<SessionId>("session", &s)?)
        } else if let Some(t) = to.task {
            Address::Task(parse_id("task", &t)?)
        } else {
            Address::Human
        })
    }
}

/// A claimed `planning` task tells the agent what the plan gate expects of it.
/// A claimed task's summary with the branch hint (its short id) and, in `planning`, the plan
/// step.
fn with_claim_step(mut v: Value, task: &crate::domain::Task) -> ToolResult {
    v["branch_hint"] = json!(short_id(task.id));
    with_plan_step(v, task)
}

fn with_plan_step(mut v: Value, task: &crate::domain::Task) -> ToolResult {
    if task.status == TaskStatus::Planning {
        v["next_step"] = json!(
            "This task needs a plan the human approves before any work. Write a spec or plan \
             and submit it with task_propose_plan (for a parent spanning projects, list the \
             subtasks per project), then wait for the approval message. Until then you cannot \
             create subtasks, block, review or complete it."
        );
    }
    Ok(v)
}

fn actor(me: &Resolved) -> crate::domain::Actor {
    crate::domain::Actor::Agent(me.agent.id, me.session.id)
}

fn parse_id<T: FromStr>(what: &str, s: &str) -> Result<T, ToolError> {
    s.trim()
        .parse()
        .map_err(|_| ToolError::invalid(format!("{what} id {s:?} is not a valid id")))
}

/// Truncates `items` to `limit` and, if that dropped any, returns the last kept item (the
/// next page's cursor).
fn more<T>(items: &mut Vec<T>, limit: usize) -> Option<&T> {
    if items.len() <= limit {
        return None;
    }
    items.truncate(limit);
    items.last()
}

fn page(limit: Option<u32>) -> u32 {
    limit.unwrap_or(PAGE).clamp(1, PAGE_MAX)
}

fn non_blank(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// `waiting_on` as the service takes it: exactly one of `human: true`, `task`, `external`.
fn waiting_on(w: WaitingOnParam) -> Result<WaitingOn, ToolError> {
    const SHAPE: &str = "`waiting_on` takes exactly one of {\"human\": true}, \
                         {\"task\": \"<id>\"} or {\"external\": \"<text>\"}";
    match (w.human, w.task, w.external) {
        (Some(true), None, None) => Ok(WaitingOn::Human),
        (None, Some(t), None) => Ok(WaitingOn::Task(parse_id("waiting_on task", &t)?)),
        (None, None, Some(text)) => match non_blank(Some(text)) {
            Some(text) if text.chars().count() > UNBLOCK_MAX => Err(ToolError::invalid(
                "`waiting_on.external` is at most 500 characters",
            )),
            Some(text) => Ok(WaitingOn::External(text)),
            None => Err(ToolError::invalid(
                "`waiting_on.external` needs some text: what does the task wait on?",
            )),
        },
        _ => Err(ToolError::invalid(SHAPE)),
    }
}
