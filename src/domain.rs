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

#[cfg(test)]
mod tests {
    use super::*;

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
