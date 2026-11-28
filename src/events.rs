//! In-process event bus (spec §10). The MVP runs one replica, so events go out over a
//! `tokio::sync::broadcast` channel; each is first written to the `outbox` table so a
//! Postgres LISTEN/NOTIFY relay can be added later for several replicas.
//!
//! Services publish after their transaction commits. Events are wake-up hints: a subscriber
//! that lags or misses one still finds everything durable through its inbox and the tasks.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::clock::Clock;
use crate::domain::{
    Address, EnrollmentId, EnrollmentStatus, MessageId, ProjectId, RequestState, SessionId,
    TaskEventKind, TaskId, TaskStatus, WakeOutcome, WakeTrigger,
};
use crate::store::{Store, StoreResult};

/// Events buffered per subscriber before it starts lagging.
const CAPACITY: usize = 1024;

/// Serialised as the variant's fields only (the variant is the SSE event name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Event {
    MessageCreated {
        id: MessageId,
        to: Address,
        project_id: Option<ProjectId>,
    },
    TaskChanged {
        id: TaskId,
        project_id: ProjectId,
        status: TaskStatus,
        kind: TaskEventKind,
    },
    /// Messages left the human's inbox (archived, dismissed or answered).
    HumanArchived { archived: Vec<MessageId> },
    /// An enrollment (one-time code or device login) changed state.
    EnrollmentChanged {
        id: EnrollmentId,
        status: EnrollmentStatus,
    },
    /// A wake prompt for one session. Reaches only that session's `/v1/events` stream; the
    /// bridge types `text` into the harness.
    WakeRequested {
        session: SessionId,
        task: Option<TaskId>,
        trigger: WakeTrigger,
        text: String,
    },
    /// How a wake attempt turned out. A UI hint only; no agent sees it.
    WakeResult {
        session: SessionId,
        task: Option<TaskId>,
        outcome: WakeOutcome,
    },
    /// A work request was filed. A UI hint for the needs-you panel; no agent sees it.
    RequestCreated {
        request: TaskId,
        project_id: ProjectId,
        state: RequestState,
    },
    /// The human approved or declined a work request. A UI hint; no agent sees it.
    RequestDecided {
        request: TaskId,
        project_id: ProjectId,
        approved: bool,
    },
}

impl Event {
    /// The variant name: the SSE `event:` field and the outbox `kind`.
    pub fn name(&self) -> &'static str {
        match self {
            Event::MessageCreated { .. } => "MessageCreated",
            Event::TaskChanged { .. } => "TaskChanged",
            Event::HumanArchived { .. } => "HumanArchived",
            Event::EnrollmentChanged { .. } => "EnrollmentChanged",
            Event::WakeRequested { .. } => "WakeRequested",
            Event::WakeResult { .. } => "WakeResult",
            Event::RequestCreated { .. } => "RequestCreated",
            Event::RequestDecided { .. } => "RequestDecided",
        }
    }

    pub fn project_id(&self) -> Option<ProjectId> {
        match *self {
            Event::MessageCreated { project_id, .. } => project_id,
            Event::TaskChanged { project_id, .. } => Some(project_id),
            Event::RequestCreated { project_id, .. } | Event::RequestDecided { project_id, .. } => {
                Some(project_id)
            }
            Event::HumanArchived { .. }
            | Event::EnrollmentChanged { .. }
            | Event::WakeRequested { .. }
            | Event::WakeResult { .. } => None,
        }
    }
}

#[derive(Clone)]
pub struct EventBus {
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(store: Arc<dyn Store>, clock: Arc<dyn Clock>) -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self { store, clock, tx }
    }

    /// Writes the outbox row, then broadcasts to current subscribers (having none is fine).
    pub async fn publish(&self, event: Event) -> StoreResult<()> {
        let payload = serde_json::to_value(&event).expect("events serialise");
        self.store
            .insert_outbox(event.name(), event.project_id(), &payload, self.clock.now())
            .await?;
        let _ = self.tx.send(event);
        Ok(())
    }

    /// `publish` for callers whose write has already committed: a failure is logged, not
    /// returned, so the caller does not report (and retry) a change that did happen.
    pub async fn emit(&self, event: Event) {
        if let Err(e) = self.publish(event).await {
            eprintln!("axl: failed to publish event: {e}");
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AgentId;

    #[test]
    fn events_serialise_as_their_fields_and_round_trip() {
        let agent = AgentId::new();
        let e = Event::MessageCreated {
            id: MessageId::new(),
            to: Address::Agent(agent),
            project_id: None,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            v["to"],
            serde_json::json!({"kind": "agent", "id": agent.to_string()})
        );
        assert_eq!(serde_json::from_value::<Event>(v).unwrap(), e);

        let t = Event::TaskChanged {
            id: TaskId::new(),
            project_id: ProjectId::new(),
            status: TaskStatus::Review,
            kind: TaskEventKind::ChildrenComplete,
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["status"], "review");
        assert_eq!(v["kind"], "children_complete");
        assert_eq!(serde_json::from_value::<Event>(v).unwrap(), t);
        assert_eq!(
            serde_json::to_value(Address::Human).unwrap(),
            serde_json::json!({"kind": "human"})
        );
    }

    #[test]
    fn wake_and_request_events_round_trip_as_themselves() {
        let events = [
            Event::WakeRequested {
                session: SessionId::new(),
                task: Some(TaskId::new()),
                trigger: WakeTrigger::HumanPost,
                text: "[axl from human] hello".into(),
            },
            Event::WakeRequested {
                session: SessionId::new(),
                task: None,
                trigger: WakeTrigger::Queue,
                text: String::new(),
            },
            Event::WakeResult {
                session: SessionId::new(),
                task: None,
                outcome: WakeOutcome::Delivered,
            },
            Event::RequestCreated {
                request: TaskId::new(),
                project_id: ProjectId::new(),
                state: RequestState::AwaitingHuman,
            },
            Event::RequestDecided {
                request: TaskId::new(),
                project_id: ProjectId::new(),
                approved: true,
            },
        ];
        for e in events {
            let v = serde_json::to_value(&e).unwrap();
            assert_eq!(serde_json::from_value::<Event>(v).unwrap(), e);
        }
        let v = serde_json::to_value(Event::RequestCreated {
            request: TaskId::new(),
            project_id: ProjectId::new(),
            state: RequestState::Queued,
        })
        .unwrap();
        assert_eq!(v["state"], "queued");
    }

    #[test]
    fn wake_and_request_events_have_names_and_projects() {
        let project = ProjectId::new();
        let wake = Event::WakeRequested {
            session: SessionId::new(),
            task: None,
            trigger: WakeTrigger::PeerPost,
            text: "x".into(),
        };
        assert_eq!(wake.name(), "WakeRequested");
        assert_eq!(wake.project_id(), None);
        let result = Event::WakeResult {
            session: SessionId::new(),
            task: None,
            outcome: WakeOutcome::Failed,
        };
        assert_eq!(result.name(), "WakeResult");
        assert_eq!(result.project_id(), None);
        let created = Event::RequestCreated {
            request: TaskId::new(),
            project_id: project,
            state: RequestState::Routed,
        };
        assert_eq!(created.name(), "RequestCreated");
        assert_eq!(created.project_id(), Some(project));
        let decided = Event::RequestDecided {
            request: TaskId::new(),
            project_id: project,
            approved: false,
        };
        assert_eq!(decided.name(), "RequestDecided");
        assert_eq!(decided.project_id(), Some(project));
    }
}
