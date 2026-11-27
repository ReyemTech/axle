//! Projects (editable display names) and tokens (last use, revoke), spec §12 / §7.

use askama::Template;
use axum::Extension;
use axum::extract::State;
use axum::response::Response;

use super::SessionCtx;
use super::task_page::Stamp;
use super::views::{UiError, UiResult, html};
use crate::domain::DISPLAY_NAME_MAX;
use crate::http::AppState;

pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub key: String,
    pub created: Stamp,
}

#[derive(Template)]
#[template(path = "projects.html")]
struct ProjectsPage {
    csrf: Option<String>,
    nav: &'static str,
    projects: Vec<ProjectRow>,
    max: usize,
}

pub async fn projects(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
) -> UiResult<Response> {
    let now = state.clock.now();
    let mut projects: Vec<ProjectRow> = state
        .store
        .list_projects()
        .await?
        .into_iter()
        .map(|p| ProjectRow {
            id: p.id.to_string(),
            name: p.display_name,
            key: p.key,
            created: Stamp::new(now, p.created_at),
        })
        .collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.key.cmp(&b.key)));
    html(&ProjectsPage {
        csrf: Some(session.csrf),
        nav: "projects",
        projects,
        max: DISPLAY_NAME_MAX,
    })
}

pub struct TokenRow {
    pub id: String,
    pub label: String,
    pub created: Stamp,
    pub last_used: Option<Stamp>,
    pub revoked: Option<Stamp>,
}

#[derive(Template)]
#[template(path = "tokens.html")]
struct TokensPage {
    csrf: Option<String>,
    nav: &'static str,
    tokens: Vec<TokenRow>,
}

pub async fn tokens(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
) -> UiResult<Response> {
    let now = state.clock.now();
    let mut tokens: Vec<TokenRow> = state
        .auth
        .list()
        .await
        .map_err(|e| UiError::Internal(e.to_string()))?
        .into_iter()
        .map(|t| TokenRow {
            id: t.id.to_string(),
            label: t.label,
            created: Stamp::new(now, t.created_at),
            last_used: t.last_used_at.map(|u| Stamp::new(now, u)),
            revoked: t.revoked_at.map(|r| Stamp::new(now, r)),
        })
        .collect();
    // Live tokens first, newest first within each group.
    tokens.reverse();
    tokens.sort_by_key(|t| t.revoked.is_some());
    html(&TokensPage {
        csrf: Some(session.csrf),
        nav: "tokens",
        tokens,
    })
}
