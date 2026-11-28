//! Delivery: the project's stage settings, PR links, and the human's stage moves.
//!
//! Business rules live in the store's guarded writes and in `crate::delivery`; this layer
//! validates input, checks who may do what, explains a refusal, and publishes the changes.

use std::sync::Arc;

use uuid::Uuid;

use time::Duration;

use crate::clock::Clock;
use crate::config::ServerConfig;
use crate::delivery::links::{PrUrl, parse_pr_url};
use crate::delivery::stages::{column, implied_between, next_enabled_after};
use crate::domain::{
    Actor, DeliverySignal, DeliveryStage, PrLinkedBy, Project, ProjectDelivery, SignalKind,
    SignalOutcome, SignalSource, Task, TaskEventKind, TaskId, TaskPr, TaskStageRow, TaskStatus,
};
use crate::events::EventBus;
use crate::service::announce::Notifier;
use crate::service::tasks::{Caller, TaskError};
use crate::store::{
    LinkOutcome, LinkPrRequest, NewTaskPr, StageMove, Store, StoreError, UnblockRule, Unblocked,
    delivered_note,
};

/// What went wrong with a delivery operation; `Display` is the message shown to the caller.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    /// The URL is not a pull request in the project's repo; the message says which repo.
    #[error("{0}")]
    BadPrUrl(String),
    #[error("{} is already linked to task {task}", pr_words(*.number))]
    TakenBy { number: Option<i64>, task: TaskId },
    #[error(
        "this task was already merged via {}; open a new task for follow-up work",
        merged_words(*.number)
    )]
    AlreadyMerged { number: Option<i64> },
    #[error("only the task's owner or creator can report its PR")]
    NotAllowed,
    #[error("this task is delivering; use Mark done or Force release")]
    Delivering,
    #[error("this task is already at its last delivery stage")]
    NoNextStage,
    #[error("this task is not delivering")]
    NotDelivering,
    #[error(transparent)]
    Task(#[from] TaskError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn pr_words(number: Option<i64>) -> String {
    number.map_or_else(|| "this PR".to_string(), |n| format!("PR #{n}"))
}

fn merged_words(number: Option<i64>) -> String {
    number.map_or_else(|| "its PR".to_string(), |n| format!("#{n}"))
}

/// Everything the task page and board need to show a task's delivery.
#[derive(Debug, Clone)]
pub struct DeliveryView {
    /// The furthest stage reached (enabled or not).
    pub stage: Option<DeliveryStage>,
    pub stages_enabled: Vec<DeliveryStage>,
    /// The board column and whether the task is still waiting for it.
    pub column: Option<(DeliveryStage, bool)>,
    pub pr: Option<TaskPr>,
    pub timeline: Vec<TaskStageRow>,
}

pub struct DeliveryService {
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    events: Option<EventBus>,
    /// The lease an unblocked task's owner gets back (as `TaskService`).
    lease_ttl: Duration,
    /// A session this long without a heartbeat no longer gets its unblocked task back.
    presence_stale: Duration,
}

impl DeliveryService {
    /// A service with the default lease TTL and presence window (see `ServerConfig`).
    pub fn new(store: Arc<dyn Store>, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            clock,
            events: None,
            lease_ttl: Duration::minutes(15),
            presence_stale: Duration::seconds(90),
        }
    }

    /// A service that unblocks waiting tasks with the server's lease TTL and presence window,
    /// exactly as `TaskService::from_config` does.
    pub fn from_config(store: Arc<dyn Store>, clock: Arc<dyn Clock>, cfg: &ServerConfig) -> Self {
        Self {
            lease_ttl: cfg.lease_ttl,
            presence_stale: cfg.presence_stale,
            ..Self::new(store, clock)
        }
    }

    /// How a task that delivery finished unblocks its dependents (as `TaskService`'s rule).
    fn unblock_rule(&self) -> UnblockRule {
        UnblockRule {
            live_since: self.clock.now() - self.presence_stale,
            default_ttl: self.lease_ttl,
        }
    }

    /// Publishes the tasks a committed write unblocked, as `TaskService` does after a `done`.
    async fn publish_unblocked(&self, unblocked: &[Unblocked]) {
        let notifier = self.notifier();
        for u in unblocked {
            notifier.changed(&u.task, TaskEventKind::Unblocked).await;
            notifier.message_created(&u.note).await;
        }
    }

    /// Publishes `TaskChanged` on `bus` after every change this service makes.
    pub fn with_events(mut self, bus: EventBus) -> Self {
        self.events = Some(bus);
        self
    }

    fn notifier(&self) -> Notifier {
        Notifier::new(self.store.clone(), self.clock.clone(), self.events.clone())
    }

    /// The project's settings: its stored row, or the defaults (no stages).
    pub async fn settings(&self, project: &Project) -> Result<ProjectDelivery, DeliveryError> {
        Ok(match self.store.get_project_delivery(project.id).await? {
            Some(d) => d,
            None => ProjectDelivery::defaults(project, self.clock.now()),
        })
    }

    /// Enables exactly `stages` (sorted, deduplicated; empty turns delivery off). Returns the
    /// tasks that moved to `done` because of it, parents included.
    pub async fn set_stages(
        &self,
        project: &Project,
        mut stages: Vec<DeliveryStage>,
    ) -> Result<Vec<Task>, DeliveryError> {
        if self.store.get_project(project.id).await?.is_none() {
            return Err(StoreError::NotFound("project").into());
        }
        stages.sort();
        stages.dedup();
        let mut d = self.settings(project).await?;
        d.stages = stages;
        d.updated_at = self.clock.now();
        let applied = self
            .store
            .set_project_delivery(&d, None, Some(self.unblock_rule()))
            .await?;
        self.publish_unblocked(&applied.unblocked).await;
        // Each task the stages finished is announced as `advance` and `mark_done` announce
        // theirs: its coordinator hears, and so does every ancestor the parent rule moved
        // (whose moves `announce` publishes; those are the rest of `moved`).
        let notifier = self.notifier();
        let note = delivered_note(&d.stages);
        for (task, children_complete) in &applied.finished {
            notifier
                .announce(
                    task,
                    TaskEventKind::Delivered,
                    Some(&note),
                    Actor::Human,
                    None,
                    *children_complete,
                )
                .await;
        }
        Ok(applied.moved)
    }

    /// Links `raw_url` as the task's PR. Agents: the task's owner or creator; the human: any
    /// task. Works for a project without stages (linking is offline).
    pub async fn link_pr(
        &self,
        by: Caller<'_>,
        task: TaskId,
        raw_url: &str,
    ) -> Result<Task, DeliveryError> {
        let current = self
            .store
            .get_task(task)
            .await?
            .ok_or(TaskError::NotFound)?;
        if matches!(current.status, TaskStatus::Done | TaskStatus::Cancelled) {
            return Err(TaskError::Closed(current.status).into());
        }
        let (linked_by, actor, token) = match by {
            Caller::Agent(r) => {
                let agent = r.agent.id;
                let creator = matches!(current.created_by, Actor::Agent(a, _) if a == agent);
                if current.owner_agent_id != Some(agent) && !creator {
                    return Err(DeliveryError::NotAllowed);
                }
                (
                    PrLinkedBy::Agent,
                    Actor::Agent(agent, r.session.id),
                    Some(r.session.token_id),
                )
            }
            Caller::Human => (PrLinkedBy::Human, Actor::Human, None),
        };
        let project = self
            .store
            .get_project(current.project_id)
            .await?
            .ok_or(TaskError::UnknownProject)?;
        let pr = match parse_pr_url(raw_url, &project.key).map_err(DeliveryError::BadPrUrl)? {
            PrUrl::Github { repo, number, url } => NewTaskPr {
                repo: repo.to_string(),
                number: Some(i64::from(number)),
                url,
                linked_by,
            },
            PrUrl::Link { url } => NewTaskPr {
                repo: project.key.clone(),
                number: None,
                url,
                linked_by,
            },
        };
        let number = pr.number;
        let outcome = self
            .store
            .link_pr(&LinkPrRequest {
                task,
                pr,
                actor,
                token,
                now: self.clock.now(),
                replace: true,
            })
            .await?;
        let notifier = self.notifier();
        match outcome {
            LinkOutcome::Linked(t) => {
                notifier.changed(&t, TaskEventKind::PrLinked).await;
                Ok(t)
            }
            LinkOutcome::Relinked { task: t, .. } => {
                notifier.changed(&t, TaskEventKind::PrRelinked).await;
                Ok(t)
            }
            LinkOutcome::Unchanged(t) => Ok(t),
            LinkOutcome::TakenBy(other) => Err(DeliveryError::TakenBy {
                number,
                task: other,
            }),
            LinkOutcome::AlreadyMerged { number } => Err(DeliveryError::AlreadyMerged { number }),
            LinkOutcome::Closed(s) => Err(TaskError::Closed(s).into()),
            LinkOutcome::NotFound => Err(TaskError::NotFound.into()),
            // `replace` is true, so the store never refuses for an existing link.
            LinkOutcome::HasLink => Err(StoreError::Corrupt("unexpected HasLink".into()).into()),
        }
    }

    /// Human: removes the task's PR link. A task without one is returned unchanged.
    pub async fn unlink_pr(&self, task: TaskId) -> Result<Task, DeliveryError> {
        let removed = self
            .store
            .unlink_pr(task, Actor::Human, None, self.clock.now())
            .await?;
        let current = self
            .store
            .get_task(task)
            .await?
            .ok_or(TaskError::NotFound)?;
        if removed.is_some() {
            self.notifier()
                .changed(&current, TaskEventKind::PrUnlinked)
                .await;
            return Ok(current);
        }
        match current.status {
            TaskStatus::Delivering => Err(DeliveryError::Delivering),
            TaskStatus::Done | TaskStatus::Cancelled => {
                Err(TaskError::Closed(current.status).into())
            }
            _ => match self.store.task_pr(task).await? {
                // The link is still there: the stage guard (merged) refused.
                Some(pr) => Err(DeliveryError::AlreadyMerged { number: pr.number }),
                None => Ok(current),
            },
        }
    }

    /// Human: **Advance stage**, one enabled stage forward from where the task has got to.
    /// Skipped stages are recorded as implied, and the move is logged as a human signal.
    pub async fn advance(&self, task: TaskId) -> Result<Task, DeliveryError> {
        let current = self.delivering(task).await?;
        let project = self
            .store
            .get_project(current.project_id)
            .await?
            .ok_or(TaskError::UnknownProject)?;
        let enabled = self.settings(&project).await?.stages;
        // A delivering parent with its own PR shows the minimum of its own and its
        // children's stages; move on from its own progress (its `task_stages`). The shown
        // stage is never ahead of that, and is all a parent without a PR has.
        let from = self
            .store
            .task_stages(task)
            .await?
            .iter()
            .map(|s| s.stage)
            .max()
            .max(current.delivery_stage);
        let to = next_enabled_after(&enabled, from).ok_or(DeliveryError::NoNextStage)?;
        let now = self.clock.now();
        let signal_id = Uuid::now_v7();
        let moved = self
            .store
            .advance_stage(&StageMove {
                task,
                to,
                implied: implied_between(from, to),
                signal_id: Some(signal_id),
                detail: None,
                actor: Actor::Human,
                token: None,
                now,
                unblock: Some(self.unblock_rule()),
            })
            .await?
            .ok_or(DeliveryError::NotDelivering)?;
        self.store
            .insert_signal(&DeliverySignal {
                id: signal_id,
                project_id: current.project_id,
                kind: signal_kind(to),
                source: SignalSource::Human,
                version: None,
                sha: None,
                environment: None,
                url: None,
                delivery_id: None,
                outcome: SignalOutcome::Applied,
                matched: 1,
                received_at: now,
            })
            .await?;
        self.publish_unblocked(&moved.unblocked).await;
        let kind = if moved.delivered {
            TaskEventKind::Delivered
        } else {
            TaskEventKind::StageReached
        };
        self.notifier()
            .announce(
                &moved.task,
                kind,
                None,
                Actor::Human,
                None,
                moved.children_complete,
            )
            .await;
        Ok(moved.task)
    }

    /// Human: **Mark done**, skipping the remaining stages.
    pub async fn mark_done(&self, task: TaskId) -> Result<Task, DeliveryError> {
        self.delivering(task).await?;
        let done = self
            .store
            .mark_delivered(
                task,
                Actor::Human,
                None,
                self.clock.now(),
                Some(self.unblock_rule()),
            )
            .await?
            .ok_or(DeliveryError::NotDelivering)?;
        self.publish_unblocked(&done.unblocked).await;
        self.notifier()
            .announce(
                &done.task,
                TaskEventKind::Delivered,
                Some("marked done by the human"),
                Actor::Human,
                None,
                done.children_complete,
            )
            .await;
        Ok(done.task)
    }

    /// The task's delivery state; `None` when its project has no enabled stages, even with a
    /// PR linked (the MCP `delivery` object is null there).
    pub async fn view(&self, task: &Task) -> Result<Option<DeliveryView>, DeliveryError> {
        let project = self
            .store
            .get_project(task.project_id)
            .await?
            .ok_or(TaskError::UnknownProject)?;
        let stages_enabled = self.settings(&project).await?.stages;
        if stages_enabled.is_empty() {
            return Ok(None);
        }
        let pr = self.store.task_pr(task.id).await?;
        Ok(Some(DeliveryView {
            stage: task.delivery_stage,
            column: column(&stages_enabled, task.delivery_stage),
            stages_enabled,
            pr,
            timeline: self.store.task_stages(task.id).await?,
        }))
    }

    /// The task, which must be `delivering`.
    async fn delivering(&self, task: TaskId) -> Result<Task, DeliveryError> {
        let current = self
            .store
            .get_task(task)
            .await?
            .ok_or(TaskError::NotFound)?;
        if current.status == TaskStatus::Delivering {
            Ok(current)
        } else {
            Err(DeliveryError::NotDelivering)
        }
    }
}

fn signal_kind(stage: DeliveryStage) -> SignalKind {
    match stage {
        DeliveryStage::InPr => SignalKind::Pr,
        DeliveryStage::Merged => SignalKind::Merged,
        DeliveryStage::Released => SignalKind::Released,
        DeliveryStage::Deployed => SignalKind::Deployed,
    }
}
