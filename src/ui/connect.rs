//! Connecting machines (onboarding spec §2, §9): the "Connect a machine" panel on the tokens
//! page (one-time code), the pending-enrollment list, and the device-approval pages at
//! `/ui/connect`.
//!
//! The one-time code is rendered once, in the response to the POST that creates it: never
//! redirected to, never in a URL, and never listed again (only its label and expiry are).

use askama::Template;
use axum::Extension;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use time::OffsetDateTime;

use super::SessionCtx;
use super::admin;
use super::task_page::Stamp;
use super::views::{Pulse, UiError, UiResult, pulse_counts};
use crate::domain::{Enrollment, EnrollmentId, EnrollmentKind};
use crate::http::AppState;
use crate::service::enroll::EnrollError;

/// The device-approval page, and the only page a sign-in returns to.
pub const CONNECT_PATH: &str = "/ui/connect";
/// Longest label a person may give a machine.
pub const LABEL_MAX: usize = 64;

fn internal(e: EnrollError) -> UiError {
    UiError::Internal(e.to_string())
}

/// The Connect a machine panel's state: the form, or why there is none, and the issued code.
#[derive(Default)]
pub struct ConnectPanel {
    /// The server has no address to hand a machine (`AXL_PUBLIC_URL` is needed): the hint
    /// replaces the form.
    pub hint: Option<String>,
    /// A form error, shown beside the field.
    pub error: Option<String>,
    /// The label as typed, so a rejected form keeps it.
    pub label: String,
    /// The freshly issued code; present only in the response that issued it.
    pub issued: Option<IssuedPanel>,
}

pub struct IssuedPanel {
    pub label: String,
    pub code: String,
    pub install_cmd: String,
    pub login_cmd: String,
    /// When the code stops working (RFC 3339, UTC), and the seconds left now.
    pub expires_iso: String,
    pub left: i64,
    pub left_text: String,
}

/// One pending enrollment as the list shows it.
pub struct PendingRow {
    pub id: String,
    /// What it will be called: the label of a code, the host of a device login.
    pub title: String,
    /// `one-time code` or `device login`.
    pub kind: &'static str,
    pub ip: Option<String>,
    pub created: Stamp,
    /// Seconds until it expires, for the countdown.
    pub left: i64,
    /// `expires in 14:59`, the countdown's first frame (app.js keeps it ticking).
    pub left_text: String,
}

/// `m:ss` of `secs`, for the countdown text.
fn clock_text(secs: i64) -> String {
    let secs = secs.max(0);
    format!("expires in {}:{:02}", secs / 60, secs % 60)
}

pub fn pending_row(now: OffsetDateTime, e: Enrollment) -> PendingRow {
    let left = (e.expires_at - now).whole_seconds().max(0);
    let (title, kind) = match e.kind {
        EnrollmentKind::Code => (e.label, "one-time code"),
        EnrollmentKind::Device => (e.host, "device login"),
    };
    PendingRow {
        id: e.id.to_string(),
        title,
        kind,
        ip: e.requester_ip,
        created: Stamp::new(now, e.created_at),
        left,
        left_text: clock_text(left),
    }
}

pub async fn pending_rows(state: &AppState) -> UiResult<Vec<PendingRow>> {
    let now = state.clock.now();
    Ok(state
        .enroll
        .pending()
        .await
        .map_err(internal)?
        .into_iter()
        .map(|e| pending_row(now, e))
        .collect())
}

fn host_header(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|v| v.to_str().ok())
}

/// The panel for a page that issued nothing: the form, or the `AXL_PUBLIC_URL` hint.
pub fn idle_panel(state: &AppState, headers: &HeaderMap) -> ConnectPanel {
    ConnectPanel {
        hint: state
            .config
            .advertised_url(host_header(headers))
            .err()
            .map(|e| capitalise(&e.to_string())),
        ..ConnectPanel::default()
    }
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => format!(
            "{}{}.",
            c.to_uppercase(),
            chars.as_str().trim_end_matches('.')
        ),
        None => String::new(),
    }
}

/// A label as typed: trimmed, 1..=64 characters, no control characters.
fn valid_label(raw: &str) -> Result<String, String> {
    let label = raw.trim();
    let n = label.chars().count();
    if n == 0 || n > LABEL_MAX || label.chars().any(char::is_control) {
        return Err(format!(
            "Name the machine in 1 to {LABEL_MAX} characters, for example \"work laptop\"."
        ));
    }
    Ok(label.to_string())
}

#[derive(Deserialize)]
pub struct ConnectForm {
    #[serde(default)]
    label: String,
}

/// Issues a one-time code and shows it, with the commands that use it, in this response.
pub async fn connect_machine(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    headers: HeaderMap,
    Form(form): Form<ConnectForm>,
) -> UiResult<Response> {
    // No address to put in the command: say so, issue nothing.
    let url = match state.config.advertised_url(host_header(&headers)) {
        Ok(url) => url,
        Err(e) => {
            let panel = ConnectPanel {
                hint: Some(capitalise(&e.to_string())),
                ..ConnectPanel::default()
            };
            return admin::tokens_page(&state, session, panel, StatusCode::OK).await;
        }
    };
    let label = match valid_label(&form.label) {
        Ok(label) => label,
        Err(error) => {
            let panel = ConnectPanel {
                error: Some(error),
                label: form.label,
                ..ConnectPanel::default()
            };
            return admin::tokens_page(&state, session, panel, StatusCode::UNPROCESSABLE_ENTITY)
                .await;
        }
    };
    let issued = state.enroll.issue_code(&label).await.map_err(internal)?;
    let panel = ConnectPanel {
        issued: Some(IssuedPanel {
            install_cmd: format!("curl -fsSL {url}/install.sh | sh -s -- {}", issued.code),
            login_cmd: login_command(&url, &issued.code),
            expires_iso: issued
                .expires_at
                .to_offset(time::UtcOffset::UTC)
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            left: (issued.expires_at - state.clock.now())
                .whole_seconds()
                .max(0),
            left_text: clock_text((issued.expires_at - state.clock.now()).whole_seconds()),
            code: issued.code,
            label,
        }),
        ..ConnectPanel::default()
    };
    let mut res = admin::tokens_page(&state, session, panel, StatusCode::OK).await?;
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

#[derive(Template)]
#[template(path = "_pending.html")]
struct PendingFragment {
    csrf: Option<String>,
    pending: Vec<PendingRow>,
}

/// The pending list alone, for the live refresh on the `tokens` frame.
pub async fn pending_fragment(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
) -> UiResult<Response> {
    super::views::html(&PendingFragment {
        csrf: Some(session.csrf),
        pending: pending_rows(&state).await?,
    })
}

/// Revokes a pending enrollment (it becomes `denied`, so its code stops working).
pub async fn revoke(State(state): State<AppState>, Path(id): Path<String>) -> UiResult<Response> {
    let id: EnrollmentId = id
        .parse()
        .map_err(|_| UiError::NotFound("No such enrollment."))?;
    state.enroll.cancel(id).await.map_err(internal)?;
    Ok(Redirect::to("/ui/tokens#connect").into_response())
}

// ---------------------------------------------------------------- device approval

/// The request a person is asked to approve.
pub struct RequestCard {
    pub id: String,
    pub host: String,
    pub ip: Option<String>,
    pub requested: Stamp,
    pub left_text: String,
    pub left: i64,
}

#[derive(Template)]
#[template(path = "connect.html")]
struct ConnectPage {
    csrf: Option<String>,
    logout: bool,
    pulse: Option<Pulse>,
    nav: &'static str,
    error: Option<String>,
    request: Option<RequestCard>,
    /// A decision just made: `true` approved, `false` denied.
    outcome: Option<bool>,
    stale: bool,
}

struct View {
    error: Option<String>,
    request: Option<RequestCard>,
    outcome: Option<bool>,
    stale: bool,
}

impl View {
    fn form() -> Self {
        Self {
            error: None,
            request: None,
            outcome: None,
            stale: false,
        }
    }
}

async fn render(
    state: &AppState,
    session: SessionCtx,
    status: StatusCode,
    view: View,
) -> UiResult<Response> {
    let page = ConnectPage {
        logout: session.logout,
        csrf: Some(session.csrf),
        pulse: Some(pulse_counts(state).await?),
        nav: "tokens",
        error: view.error,
        request: view.request,
        outcome: view.outcome,
        stale: view.stale,
    };
    let mut res = super::views::html(&page)?;
    *res.status_mut() = status;
    Ok(res)
}

/// Where the person types the code their machine shows.
pub async fn connect_page(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
) -> UiResult<Response> {
    render(&state, session, StatusCode::OK, View::form()).await
}

#[derive(Deserialize)]
pub struct LookupForm {
    #[serde(default)]
    user_code: String,
}

/// Finds the pending login for a typed code and asks for confirmation.
pub async fn lookup(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    Form(form): Form<LookupForm>,
) -> UiResult<Response> {
    let Some(found) = state
        .enroll
        .lookup_user_code(&form.user_code)
        .await
        .map_err(internal)?
    else {
        let view = View {
            error: Some(
                "No pending request with that code. Check the code on the machine, and that it \
                 has not expired."
                    .to_string(),
            ),
            ..View::form()
        };
        return render(&state, session, StatusCode::NOT_FOUND, view).await;
    };
    let now = state.clock.now();
    let row = pending_row(now, found.clone());
    let view = View {
        request: Some(RequestCard {
            id: row.id,
            host: row.title,
            ip: row.ip,
            requested: row.created,
            left_text: row.left_text,
            left: row.left,
        }),
        ..View::form()
    };
    render(&state, session, StatusCode::OK, view).await
}

async fn decide(
    state: AppState,
    session: SessionCtx,
    id: String,
    approve: bool,
) -> UiResult<Response> {
    let id: EnrollmentId = id
        .parse()
        .map_err(|_| UiError::NotFound("No such request."))?;
    let done = state.enroll.decide(id, approve).await.map_err(internal)?;
    let view = if done {
        View {
            outcome: Some(approve),
            ..View::form()
        }
    } else {
        View {
            stale: true,
            ..View::form()
        }
    };
    render(&state, session, StatusCode::OK, view).await
}

pub async fn approve(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    Path(id): Path<String>,
) -> UiResult<Response> {
    decide(state, session, id, true).await
}

pub async fn deny(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
    Path(id): Path<String>,
) -> UiResult<Response> {
    decide(state, session, id, false).await
}

/// The command the connect panel tells a person to paste (`axl login`'s positional URL).
pub fn login_command(url: &str, code: &str) -> String {
    format!("axl login {url} --code {code}")
}
