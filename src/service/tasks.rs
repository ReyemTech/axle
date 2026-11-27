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
    Actor, AgentId, Eligibility, ProjectId, SessionId, Task, TaskEvent, TaskEventKind, TaskId,
    TaskStatus, TokenId,
};
use crate::service::identity::Resolved;
use crate::store::{
    ClaimRequest, Claimant, LeaseUpdate, Store, StoreError, TaskFilter, TransitionRequest,
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

#[derive(Debug, Clone, PartialEq)]
pub struct TaskDetail {
    pub task: Task,
    pub children: Vec<Task>,
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
    #[error("a reason is required to block a task")]
    ReasonRequired,
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub struct TaskService {
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    lease_ttl: Duration,
    idle_renew_cap: Duration,
}

/// A guarded write that agents retry only when a re-read says the guard should now pass.
const MAX_GUARD_RETRIES: usize = 3;

impl TaskService {
    pub fn new(
        store: Arc<dyn Store>,
        clock: Arc<dyn Clock>,
        lease_ttl: Duration,
        idle_renew_cap: Duration,
    ) -> Self {
        Self {
            store,
            clock,
            lease_ttl,
            idle_renew_cap,
        }
    }

    pub fn from_config(store: Arc<dyn Store>, clock: Arc<dyn Clock>, cfg: &ServerConfig) -> Self {
        Self::new(store, clock, cfg.lease_ttl, cfg.idle_renew_cap)
    }

    pub async fn create(&self, by: Caller<'_>, new: NewTask) -> Result<Task, TaskError> {
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
            eligibility: new.eligibility,
            created_by: by.actor(),
            created_at: now,
            updated_at: now,
            version: 0,
        };
        let event = self.event(task.id, by.actor(), TaskEventKind::Created, None, None);
        self.store
            .insert_task(&task, &new.extra_projects, by.token(), &event)
            .await?;
        Ok(task)
    }

    pub async fn claim(&self, caller: &Resolved, id: TaskId) -> Result<ClaimOutcome, TaskError> {
        let attempt = self
            .store
            .claim_task(id, &self.claim_request(caller))
            .await?;
        let Some(previous) = attempt.previous else {
            return Ok(ClaimOutcome::NotFound);
        };
        Ok(match attempt.claimed {
            Some(task) => ClaimOutcome::Claimed(task),
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
        Ok(self
            .store
            .claim_next_task(project, &self.claim_request(caller))
            .await?)
    }

    /// Explicitly extends the caller's lease on a `claimed` task to `now + ttl`.
    pub async fn renew(&self, caller: &Resolved, id: TaskId) -> Result<Task, TaskError> {
        let until = self.clock.now() + self.lease_ttl;
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

    /// Heartbeat renewal: extends the session's live `claimed` leases to `now + ttl`, but
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
            .extend_session_leases(session, now, now + self.lease_ttl)
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
            children,
            events,
        })
    }

    pub async fn list(&self, filter: TaskFilter) -> Result<Vec<Task>, TaskError> {
        Ok(self.store.list_tasks(&filter).await?)
    }

    // ---------------------------------------------------------------- internals

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
            lease_until: now + self.lease_ttl,
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
            if let Some(task) = self.store.transition_task(&req).await? {
                return Ok(task);
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
            Some(task) => Ok(task),
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

fn actor_of(r: &Resolved) -> Actor {
    Actor::Agent(r.agent.id, r.session.id)
}
