//! Projects, agents and sessions: first-contact resolution of a caller's identity.

use crate::clock::Clock;
use crate::domain::{Agent, Project, Session, SessionId, TokenId};
use crate::store::{Store, StoreError};

/// What a caller presents (from the bridge or `X-Axl-*` headers plus its token).
#[derive(Debug, Clone)]
pub struct IdentityInput {
    pub project_key: Option<String>,
    pub host: String,
    pub harness: String,
    pub session: SessionId,
    pub token: TokenId,
}

/// The caller's identity, fully materialised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub project: Option<Project>,
    pub agent: Agent,
    pub session: Session,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("missing {0}")]
    Missing(&'static str),
    /// A host, harness or project key outside the allowed charset or length. These values
    /// become labels inside peer-data frames, so they must not be able to forge one.
    #[error("invalid {field}: {reason}")]
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
    #[error("session {session} already belongs to another agent")]
    SessionAgentMismatch { session: SessionId },
    /// A session is bound to the token that created it.
    #[error("session {session} was registered with a different token")]
    SessionTokenMismatch { session: SessionId },
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub struct IdentityService;

impl IdentityService {
    /// Creates the project, agent and session on first contact; afterwards returns the
    /// existing rows. A blank or absent project key means "no home project".
    pub async fn resolve(
        store: &dyn Store,
        clock: &dyn Clock,
        input: IdentityInput,
    ) -> Result<Resolved, IdentityError> {
        let host = input.host.trim();
        let harness = input.harness.trim();
        if host.is_empty() {
            return Err(IdentityError::Missing("host"));
        }
        if harness.is_empty() {
            return Err(IdentityError::Missing("harness"));
        }
        check_label("host", host, NAME_MAX, false)?;
        check_label("harness", harness, NAME_MAX, false)?;
        let now = clock.now();

        let key = input
            .project_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty());
        if let Some(key) = key {
            check_label("project key", key, PROJECT_KEY_MAX, true)?;
        }
        let project = match key {
            Some(key) => Some(
                store
                    .upsert_project(key, &default_display_name(key), now)
                    .await?,
            ),
            None => None,
        };

        let agent = store
            .upsert_agent(project.as_ref().map(|p| p.id), host, harness, now)
            .await?;
        let session = store
            .create_session(input.session, agent.id, input.token, now)
            .await?;
        if session.token_id != input.token {
            return Err(IdentityError::SessionTokenMismatch {
                session: session.id,
            });
        }
        if session.agent_id != agent.id {
            return Err(IdentityError::SessionAgentMismatch {
                session: session.id,
            });
        }
        Ok(Resolved {
            project,
            agent,
            session,
        })
    }

    /// The identity of an already-registered session; `None` if the session is unknown.
    pub async fn load(store: &dyn Store, id: SessionId) -> Result<Option<Resolved>, IdentityError> {
        let Some(session) = store.get_session(id).await? else {
            return Ok(None);
        };
        let agent = store
            .get_agent(session.agent_id)
            .await?
            .ok_or(StoreError::NotFound("agent"))?;
        let project = match agent.project_id {
            Some(p) => Some(
                store
                    .get_project(p)
                    .await?
                    .ok_or(StoreError::NotFound("project"))?,
            ),
            None => None,
        };
        Ok(Some(Resolved {
            project,
            agent,
            session,
        }))
    }
}

/// Longest accepted host or harness, in bytes.
pub const NAME_MAX: usize = 64;
/// Longest accepted project key, in bytes.
pub const PROJECT_KEY_MAX: usize = 200;

/// Accepts only `[A-Za-z0-9._:@-]` (plus `/` when `slash`), at most `max` bytes. Hosts and
/// harnesses may not contain `/`, which separates the parts of an agent label.
pub fn check_label(
    field: &'static str,
    value: &str,
    max: usize,
    slash: bool,
) -> Result<(), IdentityError> {
    if value.len() > max {
        return Err(IdentityError::Invalid {
            field,
            reason: "too long",
        });
    }
    let ok = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b".:_@-".contains(&b) || (slash && b == b'/'));
    if !ok {
        return Err(IdentityError::Invalid {
            field,
            reason: if slash {
                "only letters, digits and . _ : @ - / are allowed"
            } else {
                "only letters, digits and . _ : @ - are allowed"
            },
        });
    }
    Ok(())
}

/// The last two path segments of a project key: `github.com/acme/platform-infra` → `acme/platform-infra`.
pub fn default_display_name(key: &str) -> String {
    let segments: Vec<&str> = key.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [] => key.to_string(),
        [only] => (*only).to_string(),
        [.., owner, repo] => format!("{owner}/{repo}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{IdentityError, NAME_MAX, check_label, default_display_name};

    #[test]
    fn labels_are_restricted_to_a_safe_charset() {
        assert!(check_label("host", "mac-mini.local", NAME_MAX, false).is_ok());
        assert!(check_label("project key", "github.com/a/b", 200, true).is_ok());
        for bad in ["a b", "x]y", "a/b", "(info)", "é", "a\nb"] {
            assert!(
                matches!(
                    check_label("host", bad, NAME_MAX, false),
                    Err(IdentityError::Invalid { field: "host", .. })
                ),
                "{bad:?}"
            );
        }
        assert!(check_label("host", &"a".repeat(NAME_MAX + 1), NAME_MAX, false).is_err());
    }

    #[test]
    fn display_name_is_last_two_segments() {
        assert_eq!(
            default_display_name("github.com/acme/platform-infra"),
            "acme/platform-infra"
        );
        assert_eq!(
            default_display_name("gitlab.com/group/sub/repo"),
            "sub/repo"
        );
        assert_eq!(
            default_display_name("acme/platform-infra/"),
            "acme/platform-infra"
        );
        assert_eq!(default_display_name("solo"), "solo");
    }
}
