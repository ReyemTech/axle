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
    Actor, Address, AgentId, Eligibility, Importance, Message, MessageId, ProjectId, SessionId,
    Task, TaskEvent, TaskEventKind, TaskId, TaskStatus, TokenId,
};
use crate::events::{Event, EventBus};
use crate::service::identity::{IdentityError, NAME_MAX, Resolved, check_label};
use crate::service::messages::{MessageError, SUBJECT_MAX, check_body, check_subject};
use crate::store::{
    ClaimRequest, Claimant, EditRequest, LeaseUpdate, Store, StoreError, TaskFilter,
    TransitionRequest, Transitioned, claim_event,
};

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
            // A task's projects never change after creation, so checking outside the insert
            // is safe.
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
            version: 0,
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
        };
        self.store
            .insert_task(&task, &new.extra_projects, by.token(), &event, &first)
            .await?;
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
    pub async fn renew(&self, caller: &Resolved, id: TaskId) -> Result<Task, TaskError> {
        // A task's TTL never changes after creation, so reading it outside the write is safe.
        let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
        let until = self.clock.now() + task.lease_ttl.unwrap_or(self.lease_ttl);
        self.owner_transition(
            caller,
            id,
            &[TaskStatus::Claimed],
            TaskStatus::Claimed,
            None,
            LeaseUpdate::Set(until),
            false,
            TaskEventKind::Renewed,
            None,
            None,
        )
        .await
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
            &[TaskStatus::Claimed, TaskStatus::Blocked, TaskStatus::Review],
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

    /// `claimed → blocked`. The owner keeps the task and its lease stops expiring.
    pub async fn block(
        &self,
        caller: &Resolved,
        id: TaskId,
        reason: &str,
    ) -> Result<Task, TaskError> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(TaskError::ReasonRequired);
        }
        self.owner_transition(
            caller,
            id,
            &[TaskStatus::Claimed],
            TaskStatus::Blocked,
            Some(TaskStatus::Blocked),
            LeaseUpdate::Clear,
            false,
            TaskEventKind::Blocked,
            Some(reason.to_string()),
            None,
        )
        .await
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
            &[TaskStatus::Claimed, TaskStatus::Blocked, TaskStatus::Review],
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
        } = done;
        let children_complete = *children_complete;
        self.changed(task, event.kind).await;
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
                eprintln!("coord: failed to read parent task {parent_id}: {e}");
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
        };
        if let Err(e) = self.store.insert_message(&message, token).await {
            eprintln!("coord: failed to notify parent task {}: {e}", parent.id);
            return;
        }
        if let Some(bus) = &self.events {
            bus.emit(Event::MessageCreated {
                id: message.id,
                to: message.to,
                project_id: message.project_id,
            })
            .await;
        }
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
    /// `InvalidTransition` errors; `to` is what is written (`None` keeps the status).
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
        let session = caller.session.id;
        for _ in 0..MAX_GUARD_RETRIES {
            let req = TransitionRequest {
                task: id,
                from: from.to_vec(),
                to,
                owner: Some(session),
                lease,
                clear_owner,
                now: self.clock.now(),
                token: Some(caller.session.token_id),
                event: self.event(id, actor_of(caller), kind, note.clone(), payload.clone()),
            };
            if let Some(done) = self.store.transition_task(&req).await? {
                if kind != TaskEventKind::Renewed {
                    self.after_transition(&done, &req.event, req.token).await;
                }
                return Ok(done.task);
            }
            let task = self.store.get_task(id).await?.ok_or(TaskError::NotFound)?;
            if !from.contains(&task.status) {
                return Err(TaskError::InvalidTransition {
                    from: task.status,
                    to: target,
                });
            }
            if task.owner_session_id != Some(session) {
                return Err(if self.store.session_held_task(id, session).await? {
                    TaskError::LeaseLost
                } else {
                    TaskError::NotOwner
                });
            }
            // The task changed between the guarded write and the re-read; try again.
        }
        Err(TaskError::LeaseLost)
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

fn actor_of(r: &Resolved) -> Actor {
    Actor::Agent(r.agent.id, r.session.id)
}
