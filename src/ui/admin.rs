//! Projects (editable display names) and tokens (last use, revoke), spec §12 / §7.

use askama::Template;
use axum::Extension;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;

use super::SessionCtx;
use super::connect::{self, ConnectPanel, PendingRow};
use super::task_page::Stamp;
use super::views::{Pulse, UiError, UiResult, html, line_colours, pulse_counts};
use crate::domain::DISPLAY_NAME_MAX;
use crate::http::AppState;

/// `https://host/path` for a project key shaped `host[:port]/seg/seg…`, else `None`.
/// Keys are agent-supplied, so this is the security boundary for the link: a strict
/// allow-list of host and path characters, no scheme, userinfo, query, fragment or `..`.
pub fn repo_url(key: &str) -> Option<String> {
    // Local projects (`local/<host>/<folder>`) have no repository to link to.
    if key.starts_with(crate::service::identity::LOCAL_PREFIX) {
        return None;
    }
    let (host, path) = key.split_once('/')?;
    let (name, port) = match host.split_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (host, None),
    };
    let label_ok = |l: &str| {
        !l.is_empty()
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if !name.contains('.') || !name.split('.').all(label_ok) {
        return None;
    }
    if let Some(p) = port
        && (p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let seg_ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
    };
    if !path.split('/').all(seg_ok) {
        return None;
    }
    Some(format!("https://{key}"))
}

pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub key: String,
    /// The repository page for `key`, when the key is a plain `host/path`.
    pub repo_url: Option<String>,
    /// The project's line colour (see `views::line_colours`).
    pub line: u8,
    pub created: Stamp,
    /// The machine of a local project (a folder without a git remote).
    pub local_host: Option<String>,
    /// The git key this local project's folder now has, waiting to be adopted.
    pub adopt_key: Option<String>,
    /// Whether a project with `adopt_key` exists already (adopting merges into it).
    pub adopt_exists: bool,
}

#[derive(Template)]
#[template(path = "projects.html")]
struct ProjectsPage {
    csrf: Option<String>,
    logout: bool,
    /// The top bar's counts (`None` when signed out).
    pulse: Option<Pulse>,
    nav: &'static str,
    projects: Vec<ProjectRow>,
    max: usize,
}

pub async fn projects(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
) -> UiResult<Response> {
    let now = state.clock.now();
    let all = state.store.list_projects().await?;
    let lines = line_colours(&all);
    let keys: std::collections::HashSet<String> = all.iter().map(|p| p.key.clone()).collect();
    let mut projects: Vec<ProjectRow> = all
        .into_iter()
        .map(|p| ProjectRow {
            id: p.id.to_string(),
            line: lines.get(&p.id).copied().unwrap_or(0),
            name: p.display_name,
            repo_url: repo_url(&p.key),
            key: p.key,
            created: Stamp::new(now, p.created_at),
            adopt_exists: p.adopt_key.as_ref().is_some_and(|k| keys.contains(k)),
            local_host: p.local_host,
            adopt_key: p.adopt_key,
        })
        .collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.key.cmp(&b.key)));
    html(&ProjectsPage {
        logout: session.logout,
        csrf: Some(session.csrf),
        pulse: Some(pulse_counts(&state).await?),
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
    logout: bool,
    /// The top bar's counts (`None` when signed out).
    pulse: Option<Pulse>,
    nav: &'static str,
    tokens: Vec<TokenRow>,
    /// Enrollments waiting to be used or approved.
    pending: Vec<PendingRow>,
    connect: ConnectPanel,
}

pub async fn tokens(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    headers: HeaderMap,
) -> UiResult<Response> {
    let panel = connect::idle_panel(&state, &headers);
    tokens_page(&state, session, panel, StatusCode::OK).await
}

/// The tokens page with `connect` as the Connect a machine panel (idle, rejected or issued).
pub(super) async fn tokens_page(
    state: &AppState,
    session: SessionCtx,
    connect: ConnectPanel,
    status: StatusCode,
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
    let mut res = html(&TokensPage {
        logout: session.logout,
        csrf: Some(session.csrf),
        pulse: Some(pulse_counts(state).await?),
        nav: "tokens",
        tokens,
        pending: connect::pending_rows(state).await?,
        connect,
    })?;
    *res.status_mut() = status;
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::repo_url;

    #[test]
    fn links_plain_hosts() {
        let u = |k| repo_url(k).unwrap();
        assert_eq!(u("github.com/acme/web"), "https://github.com/acme/web");
        assert_eq!(u("gitlab.com/a/b/c"), "https://gitlab.com/a/b/c");
        assert_eq!(
            u("git.example.org:8443/team/repo.name"),
            "https://git.example.org:8443/team/repo.name"
        );
    }

    #[test]
    fn rejects_everything_else() {
        for k in [
            "javascript:alert(1)",
            "github.com/a/../b",
            "github.com/a b",
            "local",
            "github.com",
            "github.com/a?x",
            "user@host:repo",
            "github.com/a/%2e%2e",
            "github.com/a//b",
            "github.com/a/",
            "github.com/a#x",
            "github.com/a\\b",
            "user@github.com/a",
            "localhost/a",
            "-x.com/a",
            "github.com:/a",
            "github.com:80a/a",
            "https://github.com/a",
            "local/mac/axl-saas",
            "local/build.example.com/web",
        ] {
            assert_eq!(repo_url(k), None, "{k}");
        }
    }
}
