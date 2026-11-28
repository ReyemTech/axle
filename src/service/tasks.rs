//! Tasks: creation, atomic claims, renewable leases and the status lifecycle (spec §8).
//!
//! Every guarded write is a single store call; this layer decides *what* to write and
//! explains a guard miss by re-reading the task.

use std::sync::Arc;

use time::Duration;
use uuid::Uuid;

use crate::clock::Clock;
use crate::config::ServerConfig;
use crate::domain::{
    Actor, Address, AgentId, Eligibility, Importance, Message, MessageId, MessageKind, PlanState,
    ProjectId, SessionId, Task, TaskEvent, TaskEventKind, TaskId, TaskStatus, TokenId, WaitingOn,
};
use crate::events::{Event, EventBus};
use crate::service::identity::{IdentityError, NAME_MAX, Resolved, check_label};
use crate::service::messages::{MessageError, SUBJECT_MAX, check_body, check_subject};
use crate::store::{
    AddProjectsRequest, BlockWrite, ClaimRequest, Claimant, EditRequest, LeaseUpdate, Store,
    StoreError, TaskFilter, TransitionRequest, Transitioned, UnblockRule, claim_event,
};

/// Most characters in a block's "what unblocks it" and in an external `waiting_on`.
pub const UNBLOCK_MAX: usize = 500;
/// Most links followed when checking a wait (ancestors, or tasks waiting on tasks).
const MAX_CHAIN: usize = 1000;

/// What `TaskService::block_with` writes. On a task already blocked, each `None` keeps the
/// current value.
#[derive(Debug, Clone, Default)]
pub struct BlockInput {
    /// Why the task is blocked (required to block; optional on an update).
    pub reason: Option<String>,
    pub waiting_on: Option<WaitingOn>,
    /// What would unblock it (at most `UNBLOCK_MAX` characters).
    pub unblock: Option<String>,
}

/// Who is calling a task operation that agents and the human may both perform.
#[derive(Debug, Clone, Copy)]
pub enum Caller<'a> {
    Agent(&'a Resolved),
    Human,
}

impl Caller<'_> {
    fn actor(&self) -> Actor {
        match self {
            Caller::Agent(r) => actor_of(r),
            Caller::Human => Actor::Human,
        }
    }

    fn token(&self) -> Option<TokenId> {
        match self {
            Caller::Agent(r) => Some(r.session.token_id),
            Caller::Human => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NewTask {
    pub project_id: ProjectId,
    pub parent_id: Option<TaskId>,
    pub title: String,
    pub body: String,
    /// Additional participating projects (parent tasks spanning repos).
    pub extra_projects: Vec<ProjectId>,
    pub eligibility: Eligibility,
    /// Lease length for this task (whole seconds, at least 1 s); `None` uses the server TTL.
    pub lease_ttl: Option<Duration>,
    /// The plan gate: claiming moves it to `planning`, and the human approves a plan before
    /// the work starts.
    pub requires_plan: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub summary: String,
    pub changed_files: Vec<String>,
    pub tests: Vec<String>,
    pub links: Vec<String>,
}

// `Claimed` carries the whole task by design (the interface the tools render); outcomes are
// short-lived values, so boxing would only add noise.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    Claimed(Task),
    AlreadyOwned {
        by: AgentId,
        /// `None` for `blocked` / `review` tasks, whose leases never expire.
        until: Option<time::OffsetDateTime>,
    },
    NotEligible,
    NotFound,
    /// The task is `done` or `cancelled`.
    Closed(TaskStatus),
}

/// How far a parent's direct children have got.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Progress {
    /// Children that are `done` or `cancelled`.
    pub done: u32,
    pub total: u32,
    /// Children that are `blocked`.
    pub blocked: u32,
    /// Agents currently holding an unfinished child (`claimed`, `blocked` or `review`),
    /// grouped by the child's project, in first-seen order.
    pub owners_by_project: Vec<(ProjectId, Vec<AgentId>)>,
}

impl Progress {
    pub fn of(children: &[Task]) -> Self {
        let mut progress = Progress {
            total: children.len() as u32,
            ..Progress::default()
        };
        for child in children {
            if matches!(child.status, TaskStatus::Done | TaskStatus::Cancelled) {
                progress.done += 1;
                continue;
            }
            if child.status == TaskStatus::Blocked {
                progress.blocked += 1;
            }
            let Some(owner) = child.owner_agent_id else {
                continue;
            };
            let slot = match progress
                .owners_by_project
                .iter()
                .position(|(p, _)| *p == child.project_id)
            {
                Some(i) => &mut progress.owners_by_project[i].1,
                None => {
                    progress.owners_by_project.push((child.project_id, vec![]));
                    &mut progress
                        .owners_by_project
                        .last_mut()
                        .expect("just pushed")
                        .1
                }
            };
            if !slot.contains(&owner) {
                slot.push(owner);
            }
        }
        progress
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskDetail {
    pub task: Task,
    pub children: Vec<Task>,
    /// Counts direct children only.
    pub progress: Progress,
    pub events: Vec<TaskEvent>,
}

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("not the owner of this task")]
    NotOwner,
    #[error("lease lost: another session now owns this task")]
    LeaseLost,
    #[error("cannot move a {from} task to {to}")]
    InvalidTransition { from: TaskStatus, to: TaskStatus },
    #[error("task not found")]
    NotFound,
    #[error("the task is {0}")]
    Closed(TaskStatus),
    #[error("a reason is required to block a task")]
    ReasonRequired,
    #[error(
        "the task is already blocked: pass a new `reason`, `waiting_on` and/or `unblock` to \
         update it"
    )]
    NothingToUpdate,
    #[error("a task cannot wait on itself")]
    WaitOnSelf,
    #[error("the task to wait on does not exist")]
    WaitTargetNotFound,
    #[error("cannot wait on a {0} task: it will never unblock this one")]
    WaitOnClosed(TaskStatus),
    #[error(
        "a subtask cannot wait on its own parent or ancestor: the parent finishes after its \
         subtasks, so neither would ever move"
    )]
    WaitOnAncestor,
    #[error(
        "waiting on task {0} would make a cycle: it already waits (directly or through other \
         tasks) on this one"
    )]
    WaitCycle(TaskId),
    #[error("a task lease TTL must be at least one second")]
    InvalidLeaseTtl,
    /// An eligibility harness or host outside the label charset or length.
    #[error("invalid {field}: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    #[error("child tasks belong to one project")]
    ChildExtraProjects,
    #[error("a child task's project must be its parent's project or one the parent spans")]
    ChildProjectMismatch,
    #[error("{}", plan_hint(*.0))]
    PlanNotApproved(PlanState),
    #[error("a plan can only be proposed while the task is planning (it is {0})")]
    NotPlanning(TaskStatus),
    #[error("there is no submitted plan to decide on (the plan is {})", plan_words(*.0))]
    NoPlanToReview(PlanState),
    #[error("a plan needs some text")]
    EmptyPlan,
    /// The decision named a plan that has since been replaced.
    #[error("the plan changed — review the new version")]
    PlanChanged,
    #[error("a note is required to request changes")]
    NoteRequired,
    #[error("unknown project")]
    UnknownProject,
    /// The task body is over the message cap, or a promoted message does not exist.
    #[error(transparent)]
    Message(#[from] MessageError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub struct TaskService {
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    lease_ttl: Duration,
    idle_renew_cap: Duration,
    /// A session is stale (open to same-agent takeover) after this long without a heartbeat.
    presence_stale: Duration,
    events: Option<EventBus>,
}

/// A guarded write that agents retry only when a re-read says the guard should now pass.
const MAX_GUARD_RETRIES: usize = 3;

/// What an agent should do next when the plan gate refuses something.
fn plan_hint(state: PlanState) -> &'static str {
    match state {
        PlanState::Submitted => {
            "the plan is waiting for the human's approval: wait for their answer (it arrives as \
             a message) before doing the work"
        }
        PlanState::ChangesRequested => {
            "the human asked for changes to the plan: revise it and submit it again with \
             task_propose_plan"
        }
        _ => {
            "the plan is not approved yet: write a spec or plan, submit it with \
             task_propose_plan and wait for the human's approval"
        }
    }
}

/// A plan state in words, for people.
pub fn plan_words(state: PlanState) -> &'static str {
    match state {
        PlanState::None => "not started",
        PlanState::Drafting => "being drafted",
        PlanState::Submitted => "awaiting approval",
        PlanState::ChangesRequested => "being revised",
        PlanState::Approved => "approved",
    }
}

/// Every plan state but `Submitted`: the states in which a planning lease runs.
const LEASED_PLAN_STATES: [PlanState; 4] = [
    PlanState::None,
    PlanState::Drafting,
    PlanState::ChangesRequested,
    PlanState::Approved,
];

impl TaskService {
    pub fn new(
        store: Arc<dyn Store>,
        clock: Arc<dyn Clock>,
        lease_ttl: Duration,
        idle_renew_cap: Duration,
        presence_stale: Duration,
    ) -> Self {
        Self {
            store,
            clock,
            lease_ttl,
            idle_renew_cap,
            presence_stale,
            events: None,
        }
    }

    /// Publishes `TaskChanged` on `bus` after every status change (creation, claims and the
    /// lifecycle transitions; lease renewals are not changes).
    pub fn with_events(mut self, bus: EventBus) -> Self {
        self.events = Some(bus);
        self
    }

    pub fn from_config(store: Arc<dyn Store>, clock: Arc<dyn Clock>, cfg: &ServerConfig) -> Self {
        Self::new(
            store,
            clock,
            cfg.lease_ttl,
            cfg.idle_renew_cap,
            cfg.presence_stale,
        )
    }

    /// Creates an open task; its body becomes the first message of its thread (same
    /// transaction).
    pub async fn create(&self, by: Caller<'_>, new: NewTask) -> Result<Task, TaskError> {
        self.insert(by, new, None).await
    }

    /// Promotes a message to a task. An empty title defaults to the message's subject (else
    /// the first line of its body) and an empty body to the message's body; the task's first
    /// thread message replies to the promoted one.
    pub async fn create_from_message(
        &self,
        by: Caller<'_>,
        source: MessageId,
        mut new: NewTask,
    ) -> Result<Task, TaskError> {
        let message = self
            .store
            .get_message(source)
            .await?
            .ok_or(MessageError::NotFound("message"))?;
        if new.title.trim().is_empty() {
            let title = message
                .subject
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| message.body.lines().next().unwrap_or_default().to_string());
            // A derived title is cut to fit rather than refused.
            new.title = title.chars().take(SUBJECT_MAX).collect();
        }
        if new.body.trim().is_empty() {
            new.body = message.body;
        }
        self.insert(by, new, Some(source)).await
    }

    async fn insert(
        &self,
        by: Caller<'_>,
        new: NewTask,
        reply_to: Option<MessageId>,
    ) -> Result<Task, TaskError> {
        check_body(&new.body)?;
        check_subject("title", &new.title)?;
        check_eligibility(&new.eligibility)?;
        let lease_ttl = match new.lease_ttl {
            Some(d) if d < Duration::SECOND => return Err(TaskError::InvalidLeaseTtl),
            Some(d) => Some(Duration::seconds(d.whole_seconds())),
            None => None,
        };
        if let Some(parent_id) = new.parent_id {
            if !new.extra_projects.is_empty() {
                return Err(TaskError::ChildExtraProjects);
            }
            let parent = self
                .store
                .get_task(parent_id)
                .await?
                .ok_or(TaskError::NotFound)?;
            if parent.status == TaskStatus::Planning {
                return Err(TaskError::PlanNotApproved(parent.plan_state));
            }
            // A task's projects are only ever added to, so checking outside the insert is
            // safe (the store re-checks the parent's status under its lock).
            if parent.project_id != new.project_id
                && !self
                    .store
                    .task_projects(parent_id)
                    .await?
                    .contains(&new.project_id)
            {
                return Err(TaskError::ChildProjectMismatch);
            }
        }
        let now = self.clock.now();
        let task = Task {
            id: TaskId::new(),
            project_id: new.project_id,
            parent_id: new.parent_id,
            title: new.title,
            body: new.body,
            status: TaskStatus::Open,
            owner_session_id: None,
            owner_agent_id: None,
            lease_expires_at: None,
            lease_epoch: 0,
            lease_ttl,
            eligibility: new.eligibility,
            created_by: by.actor(),
            created_at: now,
            updated_at: now,
            done_at: None,
            acked_at: None,
            version: 0,
            requires_plan: new.requires_plan,
            plan_state: PlanState::None,
            block: None,
        };
        let event = self.event(task.id, by.actor(), TaskEventKind::Created, None, None);
        let first = Message {
            id: MessageId::new(),
            project_id: Some(task.project_id),
            from: task.created_by,
            to: Address::Task(task.id),
            subject: Some(task.title.clone()),
            body: task.body.clone(),
            importance: Importance::Normal,
            task_id: Some(task.id),
            reply_to,
            created_at: now,
            kind: MessageKind::Message,
            options: Vec::new(),
        };
        if !self
            .store
            .insert_task(&task, &new.extra_projects, by.token(), &event, &first)
            .await?
        {
            let parent = task.parent_id.expect("only a child insert is refused");
            let state = self
                .store
                .get_task(parent)
                .await?
                .map_or(PlanState::Drafting, |p| p.plan_state);
            return Err(TaskError::PlanNotApproved(state));
        }
        self.changed(&task, TaskEventKind::Created).await;
        Ok(task)
    }

    pub async fn claim(&self, caller: &Resolved, id: TaskId) -> Result<ClaimOutcome, TaskError> {
        let req = self.claim_request(caller);
        let attempt = self.store.claim_task(id, &req).await?;
        let Some(previous) = attempt.previous else {
            return Ok(ClaimOutcome::NotFound);
        };
        Ok(match attempt.claimed {
            Some(task) => {
                self.changed(&task, claim_event(&previous, &req).kind).await;
                ClaimOutcome::Claimed(task)
            }
            None => match (previous.status, previous.owner_agent_id) {
                (s @ (TaskStatus::Done | TaskStatus::Cancelled), _) => ClaimOutcome::Closed(s),
                _ if !attempt.eligible => ClaimOutcome::NotEligible,
                (_, Some(by)) => ClaimOutcome::AlreadyOwned {
                    by,
                    until: previous.lease_expires_at,
                },
                // An eligible open task always matches the guard; only a corrupt row lands here.
                (status, None) => {
                    return Err(StoreError::Corrupt(format!(
                        "task {id} is {status} without an owner"
                    ))
                    .into());
                }
            },
        })
    }

    /// Claims the oldest open task in `project` the caller is eligible for.
    pub async fn claim_next(
        &self,
        caller: &Resolved,
        project: ProjectId,
    ) -> Result<Option<Task>, TaskError> {
        let task = self
            .store
            .claim_next_task(project, &self.claim_request(caller))
            .await?;
        if let Some(task) = &task {
            self.changed(task, TaskEventKind::Claimed).await;
        }
        Ok(task)
    }

    /// Explicitly extends the caller's lease on a `claimed` task to `now + ttl` (the task's
    /// own TTL, else the server's).
    /// A `planning` task renews too, except while its plan waits on the human: that lease
    /// does not run, so renewing it is a successful no-op.
    pub async fn renew(&self, caller: &Resolved, id: TaskId) -> Result<Task, TaskError> {
        // A task's TTL never changes after creation, so reading it outside the write is safe.
        let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
        if task.status == TaskStatus::Planning
            && task.plan_state == PlanState::Submitted
            && task.owner_session_id == Some(caller.session.id)
        {
            return Ok(task);
        }
        let until = self.clock.now() + task.lease_ttl.unwrap_or(self.lease_ttl);
        self.guarded(
            caller,
            id,
            Guarded {
                from: &[TaskStatus::Claimed, TaskStatus::Planning],
                target: TaskStatus::Claimed,
                to: None,
                lease: LeaseUpdate::Set(until),
                plan_from: Some(&LEASED_PLAN_STATES),
                ..Guarded::new(TaskEventKind::Renewed)
            },
        )
        .await
    }

    /// The owner submits (or resubmits) the plan of a `planning` task: it is stored as a `plan`
    /// message in the thread addressed to the human, the plan state becomes `submitted` and the
    /// lease stops running until the human decides. One transaction.
    pub async fn propose_plan(
        &self,
        caller: &Resolved,
        id: TaskId,
        plan: String,
    ) -> Result<(Task, Message), TaskError> {
        if plan.trim().is_empty() {
            return Err(TaskError::EmptyPlan);
        }
        check_body(&plan)?;
        let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
        let message = Message {
            id: MessageId::new(),
            project_id: Some(task.project_id),
            from: actor_of(caller),
            to: Address::Human,
            subject: Some(prefixed("Plan: ", &task.title)),
            body: plan,
            importance: Importance::High,
            task_id: Some(id),
            reply_to: None,
            created_at: self.clock.now(),
            kind: MessageKind::Plan,
            options: Vec::new(),
        };
        let done = self
            .guarded(
                caller,
                id,
                Guarded {
                    from: &[TaskStatus::Planning],
                    target: TaskStatus::Planning,
                    to: None,
                    lease: LeaseUpdate::Clear,
                    plan_to: Some(PlanState::Submitted),
                    message: Some(message.clone()),
                    archive_plans: true,
                    ..Guarded::new(TaskEventKind::PlanSubmitted)
                },
            )
            .await;
        match done {
            Ok(task) => {
                self.message_created(&message).await;
                Ok((task, message))
            }
            Err(TaskError::InvalidTransition { from, .. }) => Err(TaskError::NotPlanning(from)),
            Err(e) => Err(e),
        }
    }

    /// The human approves the submitted plan `plan` (the one they read): `planning → claimed`
    /// with a fresh lease, and a system message tells the owner's session to go ahead. Refused
    /// with `PlanChanged` when a newer plan has been submitted since.
    pub async fn approve_plan(&self, id: TaskId, plan: MessageId) -> Result<Task, TaskError> {
        let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
        let until = self.clock.now() + task.lease_ttl.unwrap_or(self.lease_ttl);
        let body = format!(
            "The human approved your plan for \"{}\". The task is claimed by you now: go ahead \
             with the work as planned.",
            task.title
        );
        self.decide_plan(
            &task,
            TaskStatus::Claimed,
            PlanState::Approved,
            LeaseUpdate::Set(until),
            TaskEventKind::PlanApproved,
            None,
            plan,
            ("plan approved", body, MessageKind::System),
        )
        .await
    }

    /// The human asks for changes (a note is required): the plan state becomes
    /// `changes_requested`, the task stays `planning` with a running lease again, and the note
    /// goes to the owner's session.
    pub async fn request_plan_changes(
        &self,
        id: TaskId,
        plan: MessageId,
        note: &str,
    ) -> Result<Task, TaskError> {
        let note = note.trim();
        if note.is_empty() {
            return Err(TaskError::NoteRequired);
        }
        check_body(note)?;
        let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
        let until = self.clock.now() + task.lease_ttl.unwrap_or(self.lease_ttl);
        self.decide_plan(
            &task,
            TaskStatus::Planning,
            PlanState::ChangesRequested,
            LeaseUpdate::Set(until),
            TaskEventKind::PlanChangesRequested,
            Some(note.to_string()),
            plan,
            (
                "plan changes requested",
                note.to_string(),
                MessageKind::Message,
            ),
        )
        .await
    }

    /// Adds participating projects to a top-level task (append-only). Allowed for the human,
    /// the creating agent and the session holding the task. Adding projects already listed is
    /// a no-op; otherwise a system note in the thread and a `projects_added` event record it.
    pub async fn add_projects(
        &self,
        by: Caller<'_>,
        id: TaskId,
        projects: &[ProjectId],
    ) -> Result<Task, TaskError> {
        for p in projects {
            if self.store.get_project(*p).await?.is_none() {
                return Err(TaskError::UnknownProject);
            }
        }
        let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
        if task.parent_id.is_some() {
            return Err(TaskError::ChildExtraProjects);
        }
        let now = self.clock.now();
        let req = AddProjectsRequest {
            task: id,
            projects,
            editor: match by {
                Caller::Agent(r) => Some((r.agent.id, r.session.id)),
                Caller::Human => None,
            },
            token: by.token(),
            event: self.event(id, by.actor(), TaskEventKind::ProjectsAdded, None, None),
            note: Message {
                id: MessageId::new(),
                project_id: Some(task.project_id),
                from: by.actor(),
                to: Address::Task(id),
                subject: Some("projects added".into()),
                body: String::new(),
                importance: Importance::Normal,
                task_id: Some(id),
                reply_to: None,
                created_at: now,
                kind: MessageKind::System,
                options: Vec::new(),
            },
        };
        match self.store.add_task_projects(&req).await? {
            Some(done) => {
                if !done.added.is_empty() {
                    self.changed(&done.task, TaskEventKind::ProjectsAdded).await;
                    self.message_created(&req.note).await;
                }
                Ok(done.task)
            }
            None => {
                let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
                if matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled) {
                    return Err(TaskError::Closed(task.status));
                }
                Err(TaskError::NotOwner)
            }
        }
    }

    /// Gives the task back: `open`, no owner, no lease.
    pub async fn release(
        &self,
        caller: &Resolved,
        id: TaskId,
        note: Option<String>,
    ) -> Result<Task, TaskError> {
        self.owner_transition(
            caller,
            id,
            &[
                TaskStatus::Planning,
                TaskStatus::Claimed,
                TaskStatus::Blocked,
                TaskStatus::Review,
            ],
            TaskStatus::Open,
            Some(TaskStatus::Open),
            LeaseUpdate::Clear,
            true,
            TaskEventKind::Released,
            note,
            None,
        )
        .await
    }

    /// `claimed → blocked` with only a reason (see `block_with`).
    pub async fn block(
        &self,
        caller: &Resolved,
        id: TaskId,
        reason: &str,
    ) -> Result<Task, TaskError> {
        self.block_with(
            caller,
            id,
            BlockInput {
                reason: Some(reason.to_string()),
                ..BlockInput::default()
            },
        )
        .await
    }

    /// `claimed → blocked`: the owner keeps the task and its lease stops expiring. A reason is
    /// required; what it waits on and what would unblock it are optional. On a task the
    /// caller already holds `blocked`, it updates the given fields instead (the others are
    /// kept) and records a `block_updated` event with the previous values.
    ///
    /// Waiting on a task is refused when that task is done, cancelled, unknown, this task or
    /// one of its ancestors, or (through the tasks it waits on) waits on this task. When the
    /// awaited task becomes `done`, this one is unblocked automatically (`complete`).
    pub async fn block_with(
        &self,
        caller: &Resolved,
        id: TaskId,
        input: BlockInput,
    ) -> Result<Task, TaskError> {
        let reason = input
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_string);
        if let Some(r) = &reason {
            check_body(r)?;
        }
        let unblock = input
            .unblock
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string);
        if unblock
            .as_ref()
            .is_some_and(|u| u.chars().count() > UNBLOCK_MAX)
        {
            return Err(TaskError::InvalidField {
                field: "unblock",
                reason: "at most 500 characters",
            });
        }
        let waiting_on = match input.waiting_on {
            Some(WaitingOn::External(text)) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    return Err(TaskError::InvalidField {
                        field: "waiting_on.external",
                        reason: "describe what the task waits on",
                    });
                }
                if text.chars().count() > UNBLOCK_MAX {
                    return Err(TaskError::InvalidField {
                        field: "waiting_on.external",
                        reason: "at most 500 characters",
                    });
                }
                Some(WaitingOn::External(text))
            }
            other => other,
        };
        if let Some(target) = waiting_on.as_ref().and_then(WaitingOn::task) {
            self.check_wait(id, target).await?;
        }
        let current = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
        let updating = current.status == TaskStatus::Blocked;
        if !updating && reason.is_none() {
            return Err(TaskError::ReasonRequired);
        }
        if updating && reason.is_none() && waiting_on.is_none() && unblock.is_none() {
            return Err(TaskError::NothingToUpdate);
        }
        let mut detail = serde_json::Map::new();
        if let Some(w) = &waiting_on {
            detail.insert("waiting_on".into(), w.to_json());
        }
        if let Some(u) = &unblock {
            detail.insert("unblock".into(), u.clone().into());
        }
        // An update's previous values are read by the store under the write's lock.
        let (from, to, kind): (&[TaskStatus], _, _) = if updating {
            (&[TaskStatus::Blocked], None, TaskEventKind::BlockUpdated)
        } else {
            (
                &[TaskStatus::Claimed],
                Some(TaskStatus::Blocked),
                TaskEventKind::Blocked,
            )
        };
        let result = self
            .guarded(
                caller,
                id,
                Guarded {
                    from,
                    target: TaskStatus::Blocked,
                    to,
                    lease: if updating {
                        LeaseUpdate::Keep
                    } else {
                        LeaseUpdate::Clear
                    },
                    note: reason.clone(),
                    payload: (!detail.is_empty()).then(|| detail.into()),
                    block: Some(BlockWrite {
                        reason,
                        waiting_on: waiting_on.clone(),
                        unblock,
                        record_previous: updating,
                    }),
                    ..Guarded::new(kind)
                },
            )
            .await;
        match result {
            // The store's guard also misses when the awaited task closed or a wait cycle
            // appeared after the check above: say which.
            Err(e) => {
                if let Some(target) = waiting_on.as_ref().and_then(WaitingOn::task) {
                    self.check_wait(id, target).await?;
                }
                Err(e)
            }
            ok => ok,
        }
    }

    /// Why `task` may not wait on `target`, if it may not (the store enforces the same rule).
    async fn check_wait(&self, task: TaskId, target: TaskId) -> Result<(), TaskError> {
        if task == target {
            return Err(TaskError::WaitOnSelf);
        }
        let waited = self
            .store
            .get_task(target)
            .await?
            .ok_or(TaskError::WaitTargetNotFound)?;
        if matches!(waited.status, TaskStatus::Done | TaskStatus::Cancelled) {
            return Err(TaskError::WaitOnClosed(waited.status));
        }
        // Ancestors of `task`: a child waiting on its parent would never be unblocked.
        let mut cursor = self.store.get_task(task).await?.and_then(|t| t.parent_id);
        for _ in 0..MAX_CHAIN {
            let Some(p) = cursor else { break };
            if p == target {
                return Err(TaskError::WaitOnAncestor);
            }
            cursor = self.store.get_task(p).await?.and_then(|t| t.parent_id);
        }
        // The tasks `target` waits on, transitively.
        let mut next = Some(waited);
        for _ in 0..MAX_CHAIN {
            let Some(t) = next else { break };
            let Some(on) = t
                .block
                .as_ref()
                .filter(|_| t.status == TaskStatus::Blocked)
                .and_then(|b| b.waiting_on.as_ref())
                .and_then(WaitingOn::task)
            else {
                break;
            };
            if on == task {
                return Err(TaskError::WaitCycle(target));
            }
            next = self.store.get_task(on).await?;
        }
        Ok(())
    }

    /// `claimed | blocked → review`: work done, awaiting the coordinator or the human.
    pub async fn to_review(
        &self,
        caller: &Resolved,
        id: TaskId,
        note: Option<String>,
    ) -> Result<Task, TaskError> {
        self.owner_transition(
            caller,
            id,
            &[TaskStatus::Claimed, TaskStatus::Blocked],
            TaskStatus::Review,
            Some(TaskStatus::Review),
            LeaseUpdate::Clear,
            false,
            TaskEventKind::Review,
            note,
            None,
        )
        .await
    }

    /// `claimed | review → done`. The owner is kept as the record of who did it.
    pub async fn complete(
        &self,
        caller: &Resolved,
        id: TaskId,
        done: Completion,
    ) -> Result<Task, TaskError> {
        let payload = serde_json::json!({
            "summary": done.summary,
            "changed_files": done.changed_files,
            "tests": done.tests,
            "links": done.links,
        });
        self.owner_transition(
            caller,
            id,
            &[TaskStatus::Claimed, TaskStatus::Review],
            TaskStatus::Done,
            Some(TaskStatus::Done),
            LeaseUpdate::Clear,
            false,
            TaskEventKind::Done,
            Some(done.summary),
            Some(payload),
        )
        .await
    }

    /// Changes the title and/or body (`None` keeps each). Allowed for the creating agent and
    /// for the session holding the task (`claimed`, `blocked` or `review`); never on a `done`
    /// or `cancelled` task. The check is part of the write. The replaced values are kept in
    /// the `edited` event, so history stays append-only.
    pub async fn edit(
        &self,
        caller: &Resolved,
        id: TaskId,
        title: Option<String>,
        body: Option<String>,
    ) -> Result<Task, TaskError> {
        if let Some(body) = &body {
            check_body(body)?;
        }
        if let Some(title) = &title {
            check_subject("title", title)?;
        }
        let req = EditRequest {
            task: id,
            title: title.as_deref(),
            body: body.as_deref(),
            agent: caller.agent.id,
            session: caller.session.id,
            token: Some(caller.session.token_id),
            event: self.event(id, actor_of(caller), TaskEventKind::Edited, None, None),
        };
        if let Some(task) = self.store.edit_task(&req).await? {
            self.changed(&task, TaskEventKind::Edited).await;
            return Ok(task);
        }
        let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
        if matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled) {
            return Err(TaskError::Closed(task.status));
        }
        Err(
            if self.store.session_held_task(id, caller.session.id).await? {
                TaskError::LeaseLost
            } else {
                TaskError::NotOwner
            },
        )
    }

    /// Cancels a not-yet-closed task. Allowed for the human and for the creating agent.
    pub async fn cancel(&self, by: Caller<'_>, id: TaskId) -> Result<Task, TaskError> {
        let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
        if let Caller::Agent(r) = by
            && !matches!(task.created_by, Actor::Agent(agent, _) if agent == r.agent.id)
        {
            return Err(TaskError::NotOwner);
        }
        self.unowned_transition(
            id,
            by.actor(),
            by.token(),
            &[
                TaskStatus::Open,
                TaskStatus::Planning,
                TaskStatus::Claimed,
                TaskStatus::Blocked,
                TaskStatus::Review,
            ],
            TaskStatus::Cancelled,
            false,
            TaskEventKind::Cancelled,
        )
        .await
    }

    /// Human only: takes the task away from its owner and reopens it.
    pub async fn force_release(&self, id: TaskId) -> Result<Task, TaskError> {
        self.unowned_transition(
            id,
            Actor::Human,
            None,
            &[
                TaskStatus::Planning,
                TaskStatus::Claimed,
                TaskStatus::Blocked,
                TaskStatus::Review,
            ],
            TaskStatus::Open,
            true,
            TaskEventKind::ForceReleased,
        )
        .await
    }

    /// Heartbeat renewal: extends the session's live `claimed` leases to `now + ttl` (per
    /// task TTL, else the server's), but
    /// only while the session made a tool call within `idle_renew_cap`. Returns how many
    /// leases were extended.
    pub async fn renew_by_heartbeat(&self, session: SessionId) -> Result<u64, TaskError> {
        let Some(s) = self.store.get_session(session).await? else {
            return Ok(0);
        };
        let now = self.clock.now();
        if now - s.last_tool_call_at > self.idle_renew_cap {
            return Ok(0);
        }
        Ok(self
            .store
            .extend_session_leases(session, now, self.lease_ttl)
            .await?)
    }

    pub async fn get(&self, id: TaskId) -> Result<TaskDetail, TaskError> {
        let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
        let children = self
            .store
            .list_tasks(&TaskFilter {
                parent: Some(id),
                ..TaskFilter::default()
            })
            .await?;
        let events = self.store.task_events(id).await?;
        Ok(TaskDetail {
            task,
            progress: Progress::of(&children),
            children,
            events,
        })
    }

    pub async fn list(&self, filter: TaskFilter) -> Result<Vec<Task>, TaskError> {
        Ok(self.store.list_tasks(&filter).await?)
    }

    /// The human's ack. Publishes `Event::TaskChanged { kind: TaskEventKind::Acked, .. }` once
    /// for each listed id that is in the newly-acked set. Writes no task_events row.
    pub async fn ack_done(&self, ids: &[TaskId]) -> Result<Vec<TaskId>, TaskError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let acked = self.store.ack_done_tasks(ids, self.clock.now()).await?;
        let mut seen = std::collections::HashSet::new();
        // The ack has committed: a failed read only costs that id's event, so log and go on.
        for id in ids {
            if acked.contains(id) && seen.insert(*id) {
                match self.store.get_task(*id).await {
                    Ok(Some(task)) => self.changed(&task, TaskEventKind::Acked).await,
                    Ok(None) => {}
                    Err(e) => eprintln!("axl: failed to read acked task {id}: {e}"),
                }
            }
        }
        Ok(acked)
    }

    // ---------------------------------------------------------------- internals

    async fn changed(&self, task: &Task, kind: TaskEventKind) {
        if let Some(bus) = &self.events {
            bus.emit(Event::TaskChanged {
                id: task.id,
                project_id: task.project_id,
                status: task.status,
                kind,
            })
            .await;
        }
    }

    /// After a committed transition: publishes the change and, for a child, tells the
    /// coordinator (spec §8). A child that is blocked, done or cancelled posts a message into
    /// the parent's thread (delivered to the parent's owner and creator), as does the closing
    /// of the last open child (the store reports it wrote `children_complete`); the parent's
    /// move to `review` that the store makes then is published too. The transition has already committed, so failures here are logged.
    async fn after_transition(
        &self,
        done: &Transitioned,
        event: &TaskEvent,
        token: Option<TokenId>,
    ) {
        let Transitioned {
            task,
            children_complete,
            unblocked,
        } = done;
        let children_complete = *children_complete;
        self.changed(task, event.kind).await;
        // Wake-up (later) wakes the owner of an unblocked task from these events.
        for u in unblocked {
            self.changed(&u.task, TaskEventKind::Unblocked).await;
            self.message_created(&u.note).await;
        }
        let Some(parent_id) = task.parent_id else {
            return;
        };
        if !matches!(
            event.kind,
            TaskEventKind::Blocked | TaskEventKind::Done | TaskEventKind::Cancelled
        ) {
            return;
        }
        let parent = match self.store.get_task(parent_id).await {
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(e) => {
                eprintln!("axl: failed to read parent task {parent_id}: {e}");
                return;
            }
        };
        let (status, detail) = match event.kind {
            TaskEventKind::Blocked => ("blocked", event.note.as_deref()),
            TaskEventKind::Done => ("done", event.note.as_deref()),
            _ => ("cancelled", None),
        };
        let body = match detail.map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) => format!(
                "Child task \"{}\" ({}) is {status}: {d}",
                task.title, task.id
            ),
            None => format!("Child task \"{}\" ({}) is {status}.", task.title, task.id),
        };
        let importance = if event.kind == TaskEventKind::Blocked {
            Importance::High
        } else {
            Importance::Normal
        };
        self.notify_parent(
            &parent,
            event.actor,
            token,
            &format!("child task {status}"),
            body,
            importance,
        )
        .await;
        if !children_complete {
            return;
        }
        if parent.status == TaskStatus::Review {
            self.changed(&parent, TaskEventKind::ChildrenComplete).await;
        }
        let body = format!(
            "All child tasks are done or cancelled; the parent task is {}.",
            parent.status
        );
        self.notify_parent(
            &parent,
            event.actor,
            token,
            "child tasks complete",
            body,
            Importance::High,
        )
        .await;
    }

    /// Posts an automatic message into the parent task's thread.
    async fn notify_parent(
        &self,
        parent: &Task,
        from: Actor,
        token: Option<TokenId>,
        subject: &str,
        body: String,
        importance: Importance,
    ) {
        let message = Message {
            id: MessageId::new(),
            project_id: Some(parent.project_id),
            from,
            to: Address::Task(parent.id),
            subject: Some(subject.to_string()),
            body,
            importance,
            task_id: Some(parent.id),
            reply_to: None,
            created_at: self.clock.now(),
            kind: MessageKind::System,
            options: Vec::new(),
        };
        if let Err(e) = self.store.insert_message(&message, token).await {
            eprintln!("axl: failed to notify parent task {}: {e}", parent.id);
            return;
        }
        self.message_created(&message).await;
    }

    fn claim_request(&self, caller: &Resolved) -> ClaimRequest {
        let now = self.clock.now();
        ClaimRequest {
            claimant: Claimant {
                agent: caller.agent.id,
                session: caller.session.id,
                token: caller.session.token_id,
                project: caller.agent.project_id,
                harness: caller.agent.harness.clone(),
                host: caller.agent.host.clone(),
            },
            now,
            live_since: now - self.presence_stale,
            default_ttl: self.lease_ttl,
            event_id: Uuid::now_v7(),
        }
    }

    fn event(
        &self,
        task_id: TaskId,
        actor: Actor,
        kind: TaskEventKind,
        note: Option<String>,
        payload: Option<serde_json::Value>,
    ) -> TaskEvent {
        TaskEvent {
            id: Uuid::now_v7(),
            task_id,
            at: self.clock.now(),
            actor,
            kind,
            note,
            payload,
        }
    }

    /// A transition only the owning session may make. `target` names the destination in
    /// `InvalidTransition` errors; `to` is what is written (`None` keeps the status). Going
    /// back to `open` resets the plan state.
    #[allow(clippy::too_many_arguments)]
    async fn owner_transition(
        &self,
        caller: &Resolved,
        id: TaskId,
        from: &[TaskStatus],
        target: TaskStatus,
        to: Option<TaskStatus>,
        lease: LeaseUpdate,
        clear_owner: bool,
        kind: TaskEventKind,
        note: Option<String>,
        payload: Option<serde_json::Value>,
    ) -> Result<Task, TaskError> {
        self.guarded(
            caller,
            id,
            Guarded {
                from,
                target,
                to,
                lease,
                clear_owner,
                note,
                payload,
                plan_to: (to == Some(TaskStatus::Open)).then_some(PlanState::None),
                ..Guarded::new(kind)
            },
        )
        .await
    }

    /// The owner-guarded transition behind `owner_transition`, with the plan guard and an
    /// optional message written in the same transaction. A task in `planning` that the
    /// transition does not start from is refused with `PlanNotApproved`.
    async fn guarded(
        &self,
        caller: &Resolved,
        id: TaskId,
        g: Guarded<'_>,
    ) -> Result<Task, TaskError> {
        let session = caller.session.id;
        for _ in 0..MAX_GUARD_RETRIES {
            let req = TransitionRequest {
                task: id,
                from: g.from.to_vec(),
                to: g.to,
                owner: Some(session),
                lease: g.lease,
                clear_owner: g.clear_owner,
                now: self.clock.now(),
                token: Some(caller.session.token_id),
                event: self.event(
                    id,
                    actor_of(caller),
                    g.kind,
                    g.note.clone(),
                    g.payload.clone(),
                ),
                plan_from: g.plan_from.map(<[PlanState]>::to_vec),
                plan_to: g.plan_to,
                message: g.message.clone(),
                latest_plan: None,
                archive_plans: g.archive_plans,
                block: g.block.clone(),
                unblock: (g.to == Some(TaskStatus::Done)).then(|| self.unblock_rule()),
            };
            if let Some(done) = self.store.transition_task(&req).await? {
                if g.kind != TaskEventKind::Renewed {
                    self.after_transition(&done, &req.event, req.token).await;
                }
                return Ok(done.task);
            }
            let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
            if !g.from.contains(&task.status) {
                if task.status == TaskStatus::Planning {
                    return Err(TaskError::PlanNotApproved(task.plan_state));
                }
                return Err(TaskError::InvalidTransition {
                    from: task.status,
                    to: g.target,
                });
            }
            if task.owner_session_id != Some(session) {
                return Err(if self.store.session_held_task(id, session).await? {
                    TaskError::LeaseLost
                } else {
                    TaskError::NotOwner
                });
            }
            if let Some(states) = g.plan_from
                && !states.contains(&task.plan_state)
            {
                return Err(TaskError::PlanNotApproved(task.plan_state));
            }
            // The task changed between the guarded write and the re-read; try again.
        }
        Err(TaskError::LeaseLost)
    }

    /// The human's decision on a submitted plan: guarded on `planning` + `submitted` and on the
    /// owner session read beforehand, so the message reaches the session that wrote the plan.
    #[allow(clippy::too_many_arguments)]
    async fn decide_plan(
        &self,
        task: &Task,
        to: TaskStatus,
        plan_to: PlanState,
        lease: LeaseUpdate,
        kind: TaskEventKind,
        note: Option<String>,
        plan: MessageId,
        (subject, body, message_kind): (&str, String, MessageKind),
    ) -> Result<Task, TaskError> {
        let id = task.id;
        let mut current = task.clone();
        for _ in 0..MAX_GUARD_RETRIES {
            if current.status != TaskStatus::Planning {
                return Err(TaskError::InvalidTransition {
                    from: current.status,
                    to,
                });
            }
            let Some(owner) = current.owner_session_id else {
                return Err(TaskError::NoPlanToReview(current.plan_state));
            };
            if current.plan_state != PlanState::Submitted {
                return Err(TaskError::NoPlanToReview(current.plan_state));
            }
            let now = self.clock.now();
            let message = Message {
                id: MessageId::new(),
                project_id: Some(current.project_id),
                from: Actor::Human,
                to: Address::Session(owner),
                subject: Some(subject.to_string()),
                body: body.clone(),
                importance: Importance::High,
                task_id: Some(id),
                reply_to: None,
                created_at: now,
                kind: message_kind,
                options: Vec::new(),
            };
            let req = TransitionRequest {
                task: id,
                from: vec![TaskStatus::Planning],
                to: Some(to),
                owner: Some(owner),
                lease,
                clear_owner: false,
                now,
                token: None,
                event: self.event(id, Actor::Human, kind, note.clone(), None),
                plan_from: Some(vec![PlanState::Submitted]),
                plan_to: Some(plan_to),
                message: Some(message.clone()),
                latest_plan: Some(plan),
                archive_plans: true,
                block: None,
                unblock: None,
            };
            if let Some(done) = self.store.transition_task(&req).await? {
                self.after_transition(&done, &req.event, None).await;
                self.message_created(&message).await;
                return Ok(done.task);
            }
            current = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
            if current.status == TaskStatus::Planning
                && current.plan_state == PlanState::Submitted
                && self.latest_plan(id).await? != Some(plan)
            {
                return Err(TaskError::PlanChanged);
            }
        }
        Err(TaskError::NoPlanToReview(current.plan_state))
    }

    /// The id of a task's newest `plan` message.
    async fn latest_plan(&self, id: TaskId) -> Result<Option<MessageId>, TaskError> {
        Ok(self
            .store
            .task_thread(id)
            .await?
            .into_iter()
            .rev()
            .find(|m| m.kind == MessageKind::Plan)
            .map(|m| m.id))
    }

    /// How a finished task unblocks its dependents: like the task's owner reclaiming it while
    /// its session is live, else like a release.
    fn unblock_rule(&self) -> UnblockRule {
        UnblockRule {
            live_since: self.clock.now() - self.presence_stale,
            default_ttl: self.lease_ttl,
        }
    }

    async fn message_created(&self, message: &Message) {
        if let Some(bus) = &self.events {
            bus.emit(Event::MessageCreated {
                id: message.id,
                to: message.to,
                project_id: message.project_id,
            })
            .await;
        }
    }

    /// A transition with no ownership guard (authorisation is the caller's job).
    #[allow(clippy::too_many_arguments)]
    async fn unowned_transition(
        &self,
        id: TaskId,
        actor: Actor,
        token: Option<TokenId>,
        from: &[TaskStatus],
        to: TaskStatus,
        clear_owner: bool,
        kind: TaskEventKind,
    ) -> Result<Task, TaskError> {
        let req = TransitionRequest {
            task: id,
            from: from.to_vec(),
            to: Some(to),
            owner: None,
            lease: LeaseUpdate::Clear,
            clear_owner,
            now: self.clock.now(),
            token,
            event: self.event(id, actor, kind, None, None),
            plan_from: None,
            plan_to: (to == TaskStatus::Open).then_some(PlanState::None),
            message: None,
            latest_plan: None,
            archive_plans: false,
            block: None,
            unblock: None,
        };
        match self.store.transition_task(&req).await? {
            Some(done) => {
                self.after_transition(&done, &req.event, req.token).await;
                Ok(done.task)
            }
            None => {
                let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
                Err(TaskError::InvalidTransition {
                    from: task.status,
                    to,
                })
            }
        }
    }
}

/// Eligibility values are shown to models next to peer data, so they follow the same label
/// rules as hosts and harnesses.
fn check_eligibility(e: &Eligibility) -> Result<(), TaskError> {
    let fields = [
        ("eligibility harness", &e.harness),
        ("eligibility host", &e.host),
    ];
    for (field, value) in fields {
        if let Some(v) = value
            && let Err(IdentityError::Invalid { field, reason }) =
                check_label(field, v, NAME_MAX, false)
        {
            return Err(TaskError::InvalidField { field, reason });
        }
    }
    Ok(())
}

/// The options of an owner-guarded transition (see `TaskService::guarded`).
struct Guarded<'a> {
    from: &'a [TaskStatus],
    /// The destination named in `InvalidTransition` errors.
    target: TaskStatus,
    /// What is written (`None` keeps the status).
    to: Option<TaskStatus>,
    lease: LeaseUpdate,
    clear_owner: bool,
    kind: TaskEventKind,
    note: Option<String>,
    payload: Option<serde_json::Value>,
    plan_from: Option<&'a [PlanState]>,
    plan_to: Option<PlanState>,
    message: Option<Message>,
    archive_plans: bool,
    block: Option<BlockWrite>,
}

impl Guarded<'_> {
    fn new(kind: TaskEventKind) -> Self {
        Self {
            from: &[],
            target: TaskStatus::Claimed,
            to: None,
            lease: LeaseUpdate::Keep,
            clear_owner: false,
            kind,
            note: None,
            payload: None,
            plan_from: None,
            plan_to: None,
            message: None,
            archive_plans: false,
            block: None,
        }
    }
}

/// `prefix` + `text`, cut to the subject cap.
fn prefixed(prefix: &str, text: &str) -> String {
    format!("{prefix}{text}")
        .chars()
        .take(SUBJECT_MAX)
        .collect()
}

fn actor_of(r: &Resolved) -> Actor {
    Actor::Agent(r.agent.id, r.session.id)
}
