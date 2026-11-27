//! Projects, agents and sessions: first-contact resolution of a caller's identity.

use crate::clock::Clock;
use crate::domain::{Agent, Project, Session, SessionId, TokenId};
use crate::store::{Store, StoreError};

/// What a caller presents (from the bridge or `X-Coord-*` headers plus its token).
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
        let now = clock.now();

        let key = input
            .project_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty());
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

/// The last two path segments of a project key: `github.com/reyemtech/iac` → `reyemtech/iac`.
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
    use super::default_display_name;

    #[test]
    fn display_name_is_last_two_segments() {
        assert_eq!(
            default_display_name("github.com/reyemtech/iac"),
            "reyemtech/iac"
        );
        assert_eq!(
            default_display_name("gitlab.com/group/sub/repo"),
            "sub/repo"
        );
        assert_eq!(default_display_name("reyemtech/iac/"), "reyemtech/iac");
        assert_eq!(default_display_name("solo"), "solo");
    }
}
