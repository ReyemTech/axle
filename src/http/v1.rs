//! The small JSON API used by the bridge and the wake-up adapters (spec §5, §10): register,
//! heartbeat, the per-session SSE event stream and an inbox summary for hooks.
//!
//! Every route sits behind `require_token`, and a session is only usable with the token
//! that registered it.

use std::collections::HashMap;
use std::convert::Infallible;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use super::{ApiError, AppState, AuthToken};
use crate::domain::{
    Actor, Address, AgentId, Importance, MessageId, ProjectId, SessionId, TaskId, TaskStatus,
};
use crate::events::Event;
use crate::service::identity::{IdentityError, IdentityInput, IdentityService, Resolved};
use crate::service::messages::InboxFilter;
use crate::store::{Store, TaskFilter};

/// Longest inbox-summary preview, in characters.
pub const PREVIEW_CHARS: usize = 120;
/// Inbox-summary page size when the caller does not ask for one, and the most it may ask.
const SUMMARY_LIMIT: u32 = 20;
const SUMMARY_MAX: u32 = 100;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/register", post(register))
        .route("/heartbeat", post(heartbeat))
        .route("/events", get(events))
        .route("/inbox/summary", get(inbox_summary))
}

// ---------------------------------------------------------------- register

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub session_id: SessionId,
    pub host: String,
    pub harness: String,
    #[serde(default)]
    pub project_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProjectView {
    pub key: String,
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub project: Option<ProjectView>,
}

async fn register(
    State(state): State<AppState>,
    Extension(AuthToken(token)): Extension<AuthToken>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiError> {
    let r = IdentityService::resolve(
        &*state.store,
        &*state.clock,
        IdentityInput {
            project_key: req.project_key,
            host: req.host,
            harness: req.harness,
            session: req.session_id,
            token,
        },
    )
    .await
    .map_err(identity_error)?;
    Ok(Json(RegisterResponse {
        session_id: r.session.id,
        agent_id: r.agent.id,
        project: r.project.map(|p| ProjectView {
            key: p.key,
            display_name: p.display_name,
        }),
    }))
}

fn identity_error(e: IdentityError) -> ApiError {
    match e {
        IdentityError::Missing(_) | IdentityError::Invalid { .. } => {
            ApiError::new(StatusCode::BAD_REQUEST, e.to_string())
        }
        IdentityError::SessionAgentMismatch { .. } => {
            ApiError::new(StatusCode::CONFLICT, e.to_string())
        }
        IdentityError::SessionTokenMismatch { .. } => {
            ApiError::new(StatusCode::FORBIDDEN, e.to_string())
        }
        IdentityError::Store(e) => ApiError::internal(e),
    }
}

/// The registered session `id`, which must belong to `token`.
async fn session_for(
    state: &AppState,
    token: crate::domain::TokenId,
    id: SessionId,
) -> Result<Resolved, ApiError> {
    let r = IdentityService::load(&*state.store, id)
        .await
        .map_err(identity_error)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown session"))?;
    if r.session.token_id != token {
        return Err(identity_error(IdentityError::SessionTokenMismatch {
            session: id,
        }));
    }
    Ok(r)
}

// ---------------------------------------------------------------- heartbeat

#[derive(Debug, Deserialize)]
pub struct HeartbeatRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatResponse {
    pub unread: u32,
    /// Tasks this session holds (`claimed`, `blocked` or `review`).
    pub leases: Vec<TaskId>,
}

async fn heartbeat(
    State(state): State<AppState>,
    Extension(AuthToken(token)): Extension<AuthToken>,
    Json(req): Json<HeartbeatRequest>,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    let r = session_for(&state, token, req.session_id).await?;
    state
        .store
        .touch_heartbeat(r.session.id, state.clock.now())
        .await
        .map_err(ApiError::internal)?;
    state
        .tasks
        .renew_by_heartbeat(r.session.id)
        .await
        .map_err(ApiError::internal)?;
    let unread = state
        .messages
        .unread_count(&r)
        .await
        .map_err(ApiError::internal)?;
    let leases = state
        .tasks
        .list(TaskFilter {
            owner: Some(r.agent.id),
            ..TaskFilter::default()
        })
        .await
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|t| {
            t.owner_session_id == Some(r.session.id)
                && matches!(
                    t.status,
                    TaskStatus::Claimed | TaskStatus::Blocked | TaskStatus::Review
                )
        })
        .map(|t| t.id)
        .collect();
    Ok(Json(HeartbeatResponse { unread, leases }))
}

// ---------------------------------------------------------------- events

#[derive(Debug, Deserialize)]
pub struct SessionQuery {
    pub session: SessionId,
}

/// SSE stream of the events relevant to one session. Event name = `Event` variant, data =
/// its JSON. A subscriber that falls behind gets a `Lagged` event (data: how many were
/// dropped) and should re-read its inbox. The stream ends when the server shuts down.
async fn events(
    State(state): State<AppState>,
    Extension(AuthToken(token)): Extension<AuthToken>,
    Query(q): Query<SessionQuery>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let r = session_for(&state, token, q.session).await?;
    // Subscribe before responding, so nothing published after this request is missed.
    let rx = state.bus.subscribe();
    let watcher = Watcher {
        store: state.store.clone(),
        session: r.session.id,
        agent: r.agent.id,
        project: r.agent.project_id,
    };
    let shutdown = state.shutdown.clone();
    let stream = futures_util::stream::unfold((rx, watcher), move |(mut rx, w)| {
        let shutdown = shutdown.clone();
        async move {
            loop {
                let received = tokio::select! {
                    received = rx.recv() => received,
                    // End the response so graceful shutdown is not held open by this client.
                    () = shutdown.wait() => return None,
                };
                match received {
                    Ok(event) => {
                        if w.wants(&event).await {
                            let data = serde_json::to_string(&event).expect("events serialise");
                            let sse = SseEvent::default().event(event.name()).data(data);
                            return Some((Ok(sse), (rx, w)));
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        let sse = SseEvent::default().event("Lagged").data(n.to_string());
                        return Some((Ok(sse), (rx, w)));
                    }
                    Err(RecvError::Closed) => return None,
                }
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Decides which events one session sees.
struct Watcher {
    store: std::sync::Arc<dyn Store>,
    session: SessionId,
    agent: AgentId,
    project: Option<ProjectId>,
}

impl Watcher {
    /// Messages addressed to this session, its agent, its home project, or a task thread the
    /// agent owns or created — never human-addressed mail, and never mail the agent sent
    /// itself. Task changes for tasks in its home project (or spanning it) or that the agent
    /// owns or created. A lookup failure drops the event (they are hints; the inbox is the
    /// record).
    async fn wants(&self, event: &Event) -> bool {
        let result = match *event {
            Event::MessageCreated { id, to, .. } => self.wants_message(id, to).await,
            Event::TaskChanged { id, project_id, .. } => self.wants_task(id, project_id).await,
        };
        result.unwrap_or_else(|e| {
            eprintln!("axl: dropping event for session {}: {e}", self.session);
            false
        })
    }

    async fn wants_message(&self, id: MessageId, to: Address) -> crate::store::StoreResult<bool> {
        let addressed = match to {
            Address::Human => false,
            Address::Session(s) => return Ok(s == self.session),
            Address::Agent(a) => a == self.agent,
            Address::Project(p) => Some(p) == self.project,
            Address::Task(t) => match self.store.get_task(t).await? {
                Some(task) => self.owns_or_created(&task),
                None => false,
            },
        };
        if !addressed {
            return Ok(false);
        }
        let from_self = matches!(
            self.store.get_message(id).await?.map(|m| m.from),
            Some(Actor::Agent(a, _)) if a == self.agent
        );
        Ok(!from_self)
    }

    async fn wants_task(&self, id: TaskId, project: ProjectId) -> crate::store::StoreResult<bool> {
        if Some(project) == self.project {
            return Ok(true);
        }
        let Some(task) = self.store.get_task(id).await? else {
            return Ok(false);
        };
        if self.owns_or_created(&task) {
            return Ok(true);
        }
        Ok(match self.project {
            Some(p) => self.store.task_projects(id).await?.contains(&p),
            None => false,
        })
    }

    fn owns_or_created(&self, task: &crate::domain::Task) -> bool {
        task.owner_agent_id == Some(self.agent)
            || matches!(task.created_by, Actor::Agent(a, _) if a == self.agent)
    }
}

// ---------------------------------------------------------------- inbox summary

#[derive(Debug, Deserialize)]
pub struct InboxQuery {
    pub session: SessionId,
    /// Page size (default 20, at most 100).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct InboxSummaryItem {
    pub id: MessageId,
    /// `human`, or the sending agent as `<project> · <host> · <harness>`.
    pub from: String,
    pub subject: Option<String>,
    /// The message's project key (where it was sent), for the `in <project>` label.
    pub project: Option<String>,
    pub importance: Importance,
    /// The body on one line, at most `PREVIEW_CHARS` characters.
    pub preview: String,
}

/// The session's unacked messages, oldest first, for hooks to inject as short summaries.
/// Like `fetch_inbox`, it marks the returned messages delivered.
async fn inbox_summary(
    State(state): State<AppState>,
    Extension(AuthToken(token)): Extension<AuthToken>,
    Query(q): Query<InboxQuery>,
) -> Result<Json<Vec<InboxSummaryItem>>, ApiError> {
    let r = session_for(&state, token, q.session).await?;
    let limit = q.limit.unwrap_or(SUMMARY_LIMIT).clamp(1, SUMMARY_MAX);
    let items = state
        .messages
        .inbox(
            &r,
            InboxFilter {
                limit: Some(limit),
                ..InboxFilter::default()
            },
        )
        .await
        .map_err(ApiError::internal)?;
    let mut labels: HashMap<AgentId, String> = HashMap::new();
    let mut projects: HashMap<ProjectId, Option<String>> = HashMap::new();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let m = item.message;
        let from = match m.from {
            Actor::Human => "human".to_string(),
            Actor::Agent(a, _) => match labels.get(&a) {
                Some(l) => l.clone(),
                None => {
                    let l = agent_label(&*state.store, a)
                        .await
                        .map_err(ApiError::internal)?;
                    labels.insert(a, l.clone());
                    l
                }
            },
        };
        let project = match m.project_id {
            None => None,
            Some(p) => match projects.get(&p) {
                Some(k) => k.clone(),
                None => {
                    let k = state
                        .store
                        .get_project(p)
                        .await
                        .map_err(ApiError::internal)?
                        .map(|p| p.key);
                    projects.insert(p, k.clone());
                    k
                }
            },
        };
        out.push(InboxSummaryItem {
            id: m.id,
            from,
            subject: m.subject,
            project,
            importance: m.importance,
            preview: preview(&m.body),
        });
    }
    Ok(Json(out))
}

/// `reyemtech/iac · mac · claude-code` (no project part for agents without a home project).
async fn agent_label(store: &dyn Store, id: AgentId) -> crate::store::StoreResult<String> {
    let Some(agent) = store.get_agent(id).await? else {
        return Ok(id.to_string());
    };
    let project = match agent.project_id {
        Some(p) => store.get_project(p).await?.map(|p| p.display_name),
        None => None,
    };
    Ok(match project {
        Some(p) => format!("{p} · {} · {}", agent.host, agent.harness),
        None => format!("{} · {}", agent.host, agent.harness),
    })
}

/// The body with whitespace collapsed, cut to `PREVIEW_CHARS` characters (ending in `…`
/// when cut).
fn preview(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PREVIEW_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(PREVIEW_CHARS - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::{PREVIEW_CHARS, preview};

    #[test]
    fn preview_flattens_and_cuts() {
        assert_eq!(preview("a\n  b\tc"), "a b c");
        let long = "é".repeat(300);
        let p = preview(&long);
        assert_eq!(p.chars().count(), PREVIEW_CHARS);
        assert!(p.ends_with('…'));
        assert_eq!(
            preview(&"x".repeat(PREVIEW_CHARS)).chars().count(),
            PREVIEW_CHARS
        );
    }
}
