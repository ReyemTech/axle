//! Ids and structs shared by every layer.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

macro_rules! id_type {
    ($($(#[$meta:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// A new time-ordered (UUID v7) id.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }
    )+};
}

id_type!(ProjectId, AgentId, SessionId, TaskId, MessageId, TokenId);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: ProjectId,
    /// Normalised key, e.g. `github.com/reyemtech/iac`. Never changes.
    pub key: String,
    /// Human label shown in the UI (editable).
    pub display_name: String,
    pub created_at: OffsetDateTime,
}

/// Stable identity `(project, host, harness)`; `project_id` is `None` for agents with no
/// home project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: AgentId,
    pub project_id: Option<ProjectId>,
    pub host: String,
    pub harness: String,
    pub created_at: OffsetDateTime,
}

/// One live harness process of an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    pub agent_id: AgentId,
    pub token_id: TokenId,
    pub started_at: OffsetDateTime,
    pub last_heartbeat_at: OffsetDateTime,
    pub last_tool_call_at: OffsetDateTime,
}

/// Generates `as_str` / `FromStr` / `Display` for a fieldless enum stored as text.
macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(format!(concat!("unknown ", stringify!($name), " {:?}"), other)),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Open,
    Claimed,
    Blocked,
    Review,
    Done,
    Cancelled,
}

text_enum!(TaskStatus {
    Open => "open",
    Claimed => "claimed",
    Blocked => "blocked",
    Review => "review",
    Done => "done",
    Cancelled => "cancelled",
});

/// Optional claim filter: when set, only agents with this harness / on this host may claim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Eligibility {
    pub harness: Option<String>,
    pub host: Option<String>,
}

/// Who did something: an agent session, or the human (UI / admin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    Agent(AgentId, SessionId),
    Human,
}

impl Actor {
    /// `(agent, session)` columns; both `None` for the human.
    pub fn ids(&self) -> (Option<AgentId>, Option<SessionId>) {
        match *self {
            Actor::Agent(a, s) => (Some(a), Some(s)),
            Actor::Human => (None, None),
        }
    }

    /// Inverse of `ids`: anything but a full pair is the human.
    pub fn from_ids(agent: Option<AgentId>, session: Option<SessionId>) -> Actor {
        match (agent, session) {
            (Some(a), Some(s)) => Actor::Agent(a, s),
            _ => Actor::Human,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    /// Home project.
    pub project_id: ProjectId,
    pub parent_id: Option<TaskId>,
    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    pub owner_session_id: Option<SessionId>,
    pub owner_agent_id: Option<AgentId>,
    /// `None` while not claimed, and while `blocked` / `review` (those never expire).
    pub lease_expires_at: Option<OffsetDateTime>,
    /// Incremented by every successful claim.
    pub lease_epoch: i64,
    /// Per-task lease length (whole seconds); `None` uses the server-wide TTL.
    pub lease_ttl: Option<time::Duration>,
    pub eligibility: Eligibility,
    pub created_by: Actor,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    /// Incremented by every state change.
    pub version: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEventKind {
    Created,
    Claimed,
    Renewed,
    Released,
    Blocked,
    Review,
    Done,
    Cancelled,
    LeaseExpiredTakeover,
    ForceReleased,
    ChildClaimed,
    ChildBlocked,
    ChildDone,
    ChildCancelled,
    ChildrenComplete,
}

text_enum!(TaskEventKind {
    Created => "created",
    Claimed => "claimed",
    Renewed => "renewed",
    Released => "released",
    Blocked => "blocked",
    Review => "review",
    Done => "done",
    Cancelled => "cancelled",
    LeaseExpiredTakeover => "lease_expired_takeover",
    ForceReleased => "force_released",
    ChildClaimed => "child_claimed",
    ChildBlocked => "child_blocked",
    ChildDone => "child_done",
    ChildCancelled => "child_cancelled",
    ChildrenComplete => "children_complete",
});

/// One append-only entry in a task's history.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskEvent {
    pub id: Uuid,
    pub task_id: TaskId,
    pub at: OffsetDateTime,
    pub actor: Actor,
    pub kind: TaskEventKind,
    pub note: Option<String>,
    /// Structured detail, e.g. the previous owner on a takeover or a completion report.
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Importance {
    #[default]
    Normal,
    High,
    Urgent,
}

text_enum!(Importance {
    Normal => "normal",
    High => "high",
    Urgent => "urgent",
});

/// Where a message goes (spec §9). The project is part of the address, not an access
/// barrier: any agent may address any project or agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Address {
    /// Every agent whose home project this is.
    Project(ProjectId),
    /// Durable: any session of the agent may read it; acked once for the agent.
    Agent(AgentId),
    /// One live session only; acked per session.
    Session(SessionId),
    /// The task's thread: delivered to its current owner and its creator.
    Task(TaskId),
    /// Mario's inbox.
    Human,
}

impl Address {
    /// `(to_kind, to_id)` columns.
    pub fn columns(&self) -> (&'static str, Option<Uuid>) {
        match *self {
            Address::Project(p) => ("project", Some(p.as_uuid())),
            Address::Agent(a) => ("agent", Some(a.as_uuid())),
            Address::Session(s) => ("session", Some(s.as_uuid())),
            Address::Task(t) => ("task", Some(t.as_uuid())),
            Address::Human => ("human", None),
        }
    }

    /// Inverse of `columns`.
    pub fn from_columns(kind: &str, id: Option<Uuid>) -> Result<Address, String> {
        Ok(match (kind, id) {
            ("project", Some(id)) => Address::Project(id.into()),
            ("agent", Some(id)) => Address::Agent(id.into()),
            ("session", Some(id)) => Address::Session(id.into()),
            ("task", Some(id)) => Address::Task(id.into()),
            ("human", None) => Address::Human,
            (kind, id) => return Err(format!("bad message address {kind:?} {id:?}")),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    /// The project the message is about: the addressed project, the addressed agent's /
    /// session's home project or the task's project; for `Human`, the sender's home project.
    pub project_id: Option<ProjectId>,
    pub from: Actor,
    pub to: Address,
    pub subject: Option<String>,
    pub body: String,
    pub importance: Importance,
    /// Set for task-thread messages.
    pub task_id: Option<TaskId>,
    pub reply_to: Option<MessageId>,
    pub created_at: OffsetDateTime,
}

/// A message as one recipient sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxItem {
    pub message: Message,
    /// When this recipient first fetched it.
    pub delivered_at: OffsetDateTime,
    pub read_at: Option<OffsetDateTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_enums_round_trip_as_text() {
        for s in [
            TaskStatus::Open,
            TaskStatus::Claimed,
            TaskStatus::Blocked,
            TaskStatus::Review,
            TaskStatus::Done,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(s.as_str().parse::<TaskStatus>().unwrap(), s);
        }
        assert_eq!(
            TaskEventKind::LeaseExpiredTakeover.to_string(),
            "lease_expired_takeover"
        );
        assert!("nope".parse::<TaskStatus>().is_err());
    }

    #[test]
    fn ids_round_trip_through_display_and_from_str() {
        let id = TaskId::new();
        assert_eq!(id.to_string().parse::<TaskId>().unwrap(), id);
        assert!("not-a-uuid".parse::<AgentId>().is_err());
    }

    #[test]
    fn ids_are_uuid_v7() {
        assert_eq!(SessionId::new().as_uuid().get_version_num(), 7);
    }
}
