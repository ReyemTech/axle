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
use axum::response::{IntoResponse, Response};
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
use crate::service::usage::UsageError;
use crate::store::{Store, TaskFilter};
use crate::usage::report::{IngestOutcome, MAX_BODY_BYTES, MAX_RECORDS, UsageReport};

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
        .route("/leases", get(leases))
        .route("/project", get(session_project))
        .route("/usage", post(usage))
        .route("/whoami", get(whoami))
        .route("/token/revoke", post(revoke_self))
}

// ---------------------------------------------------------------- whoami

/// The calling token's label and the server version (what `axl doctor` checks).
async fn whoami(
    State(state): State<AppState>,
    Extension(AuthToken(token)): Extension<AuthToken>,
) -> Result<Response, ApiError> {
    let tokens = state.auth.list().await.map_err(ApiError::internal)?;
    let label = tokens
        .into_iter()
        .find(|t| t.id == token && t.revoked_at.is_none())
        .map(|t| t.label)
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized"))?;
    Ok(super::enroll::no_store(
        Json(serde_json::json!({ "label": label, "version": env!("CARGO_PKG_VERSION") }))
            .into_response(),
    )
    .await)
}

/// Revokes the calling token (`axl logout --revoke`); no other token is touched.
async fn revoke_self(
    State(state): State<AppState>,
    Extension(AuthToken(token)): Extension<AuthToken>,
) -> Result<Response, ApiError> {
    state.auth.revoke(token).await.map_err(ApiError::internal)?;
    Ok(super::enroll::no_store(Json(serde_json::json!({ "revoked": true })).into_response()).await)
}

// ---------------------------------------------------------------- register

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub session_id: SessionId,
    pub host: String,
    pub harness: String,
    #[serde(default)]
    pub project_key: Option<String>,
    /// The bridge's folder hash (see `bridge::project::folder_hash`).
    #[serde(default)]
    pub folder: Option<String>,
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
            folder: req.folder,
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

// ---------------------------------------------------------------- project

/// The session's home project, for the SessionStart hook: `null` with no project.
#[derive(Debug, Serialize)]
pub struct SessionProject {
    pub key: String,
    pub display_name: String,
    /// The machine, for a local project (a folder without a git remote).
    pub local_host: Option<String>,
    /// The git key the folder now has, until the human moves the project to it.
    pub adopt_key: Option<String>,
}

async fn session_project(
    State(state): State<AppState>,
    Extension(AuthToken(token)): Extension<AuthToken>,
    Query(q): Query<SessionQuery>,
) -> Result<Json<Option<SessionProject>>, ApiError> {
    let r = session_for(&state, token, q.session).await?;
    Ok(Json(r.project.map(|p| SessionProject {
        key: p.key,
        display_name: p.display_name,
        local_host: p.local_host,
        adopt_key: p.adopt_key,
    })))
}

// ---------------------------------------------------------------- usage

/// Stores a session's token-usage records. The body is read by hand so an oversize one is a
/// 400 like the other envelope errors. Not a heartbeat: renews no lease, publishes no event.
async fn usage(
    State(state): State<AppState>,
    Extension(AuthToken(token)): Extension<AuthToken>,
    body: axum::body::Body,
) -> Result<Json<IngestOutcome>, ApiError> {
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "body over 256 KB"))?;
    let report: UsageReport = serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("bad request: {e}")))?;
    if report.records.len() > MAX_RECORDS {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "more than 500 records",
        ));
    }
    let r = session_for(&state, token, report.session_id).await?;
    match state.usage.ingest(&r, &report).await {
        Ok(outcome) => Ok(Json(outcome)),
        Err(UsageError::Envelope(m)) => Err(ApiError::new(StatusCode::BAD_REQUEST, m)),
        Err(UsageError::Store(e)) => Err(ApiError::internal(e)),
    }
}

// ---------------------------------------------------------------- heartbeat

#[derive(Debug, Deserialize)]
pub struct HeartbeatRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatResponse {
    pub unread: u32,
    /// Tasks this session holds (`planning`, `claimed`, `blocked` or `review`).
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
    let leases = held(&state, &r).await?.into_iter().map(|t| t.id).collect();
    Ok(Json(HeartbeatResponse { unread, leases }))
}

/// The tasks this session holds (`planning`, `claimed`, `blocked` or `review`).
async fn held(state: &AppState, r: &Resolved) -> Result<Vec<crate::domain::Task>, ApiError> {
    Ok(state
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
                    TaskStatus::Planning
                        | TaskStatus::Claimed
                        | TaskStatus::Blocked
                        | TaskStatus::Review
                )
        })
        .collect())
}

// ---------------------------------------------------------------- leases

#[derive(Debug, Serialize)]
pub struct LeaseItem {
    pub id: TaskId,
    /// Peer-written and not framed: clients frame it with the three labels below, the way
    /// the MCP tools frame task titles.
    pub title: String,
    pub status: TaskStatus,
    /// `human` or the author's agent label.
    pub created_by: String,
    /// Who last replaced the title, when that is not its author.
    pub edited_by: Option<String>,
    /// The task's project key.
    pub project: String,
}

/// The tasks this session holds, with titles, for the hooks. A plain read: unlike the
/// heartbeat it renews nothing.
async fn leases(
    State(state): State<AppState>,
    Extension(AuthToken(token)): Extension<AuthToken>,
    Query(q): Query<SessionQuery>,
) -> Result<Json<Vec<LeaseItem>>, ApiError> {
    let r = session_for(&state, token, q.session).await?;
    let mut labels = crate::mcp::Labels::new(&*state.store);
    let mut out = Vec::new();
    for t in held(&state, &r).await? {
        let (created_by, edited_by, project) = labels
            .title_frame(&t)
            .await
            .map_err(|_| ApiError::internal("cannot label a held task"))?;
        out.push(LeaseItem {
            id: t.id,
            title: t.title,
            status: t.status,
            created_by,
            edited_by,
            project,
        });
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------- events

#[derive(Debug, Deserialize)]
pub struct SessionQuery {
    pub session: SessionId,
}

/// SSE stream of the events relevant to one session. Event name = `Event` variant, data =
/// its JSON. A subscriber that falls behind gets a `Lagged` event (data: how many were
/// dropped) and should re-read its inbox; the server re-resolves the session's agent and
/// project at that point, in case a dropped event was a merge. The stream ends when the server shuts down.
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
    let stream = futures_util::stream::unfold((rx, watcher), move |(mut rx, mut w)| {
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
                        // A dropped `ProjectMerged` would leave the watcher on retired ids, so
                        // re-resolve the session rather than rely on having seen the hint.
                        if let Err(e) = w.reload().await {
                            eprintln!("axl: reloading session {} after lag: {e}", w.session);
                        }
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
    async fn wants(&mut self, event: &Event) -> bool {
        let result = match *event {
            Event::MessageCreated { id, to, .. } => self.wants_message(id, to).await,
            Event::TaskChanged { id, project_id, .. } => self.wants_task(id, project_id).await,
            // The human's inbox is not an agent's business.
            Event::HumanArchived { .. } | Event::EnrollmentChanged { .. } => Ok(false),
            // This session's project was merged: its agent and project may have new ids.
            // Re-resolve them, and pass the event on so the client re-reads its inbox.
            Event::ProjectMerged { from, .. } if Some(from) == self.project => self.reload().await,
            Event::ProjectMerged { .. } => Ok(false),
        };
        result.unwrap_or_else(|e| {
            eprintln!("axl: dropping event for session {}: {e}", self.session);
            false
        })
    }

    /// Re-reads the session's agent and home project (after a merge folded or moved them).
    async fn reload(&mut self) -> crate::store::StoreResult<bool> {
        match IdentityService::load(&*self.store, self.session).await {
            Ok(Some(r)) => {
                self.agent = r.agent.id;
                self.project = r.agent.project_id;
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(IdentityError::Store(e)) => Err(e),
            Err(e) => Err(crate::store::StoreError::Corrupt(e.to_string())),
        }
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
    /// Only messages at least this important (`high` or `urgent`), filtered before the page
    /// limit so they are never hidden behind older ordinary mail.
    #[serde(default)]
    pub min_importance: Option<Importance>,
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
                min_importance: q.min_importance,
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

/// `acme/platform-infra · mac · claude-code` (no project part for agents without a home project).
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
