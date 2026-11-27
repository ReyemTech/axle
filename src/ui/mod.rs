//! The web UI (spec §12, §7): admin-secret login with a server-side cookie session, CSRF on
//! every POST, the overview, the board, task pages, the unified inbox, projects and tokens,
//! the human's actions, and a live SSE stream that re-renders changed cards.
//!
//! UI routes do not use bearer tokens. A request is signed in by the `coord_ui` cookie (an
//! in-memory session; the MVP runs one replica) or, in trusted-header mode, by the header an
//! auth proxy sets. Every non-GET request must carry the session's CSRF token, as the form
//! field `csrf` or the `X-CSRF-Token` header (what htmx requests send).

pub mod actions;
pub mod admin;
pub mod inbox;
pub mod task_page;
pub mod views;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Mutex;

use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{Form, Path, Request, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};
use tokio::sync::broadcast::error::RecvError;

use crate::http::AppState;
use crate::http::rate_limit::RateLimiter;

/// The session cookie.
pub const COOKIE: &str = "coord_ui";
/// How long a UI session lasts after login.
pub const SESSION_TTL: Duration = Duration::hours(12);
/// Login attempts allowed per client IP per minute.
pub const LOGIN_PER_MINUTE: u32 = 10;
/// How often an idle live stream re-checks that its session is still signed in.
const LIVE_SESSION_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);
/// Largest form body a UI POST may send.
const FORM_LIMIT: usize = 256 * 1024;

/// Server-side UI sessions and the login rate limit.
pub struct UiState {
    sessions: Mutex<HashMap<String, UiSession>>,
    login_limit: RateLimiter<String>,
}

#[derive(Debug, Clone)]
struct UiSession {
    csrf: String,
    expires_at: OffsetDateTime,
}

/// The signed-in request's session, for handlers (the CSRF token goes into every page).
#[derive(Debug, Clone)]
pub struct SessionCtx {
    pub id: String,
    pub csrf: String,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

impl UiState {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            login_limit: RateLimiter::per_minute(LOGIN_PER_MINUTE),
        }
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, UiSession>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts a session expiring `SESSION_TTL` after `now` (dropping expired ones).
    fn create(&self, now: OffsetDateTime) -> SessionCtx {
        let ctx = SessionCtx {
            id: random_hex(),
            csrf: random_hex(),
        };
        let mut sessions = self.sessions();
        sessions.retain(|_, s| s.expires_at > now);
        sessions.insert(
            ctx.id.clone(),
            UiSession {
                csrf: ctx.csrf.clone(),
                expires_at: now + SESSION_TTL,
            },
        );
        ctx
    }

    /// The live session `id`, if any.
    fn get(&self, id: &str, now: OffsetDateTime) -> Option<SessionCtx> {
        let mut sessions = self.sessions();
        match sessions.get(id) {
            Some(s) if s.expires_at > now => Some(SessionCtx {
                id: id.to_string(),
                csrf: s.csrf.clone(),
            }),
            Some(_) => {
                sessions.remove(id);
                None
            }
            None => None,
        }
    }

    fn remove(&self, id: &str) {
        self.sessions().remove(id);
    }
}

/// 32 random bytes as hex.
fn random_hex() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random source is available");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Equality that takes the same time wherever the inputs differ (both sides are hashed
/// first, so length differences do not leak either).
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

pub fn routes(state: AppState) -> Router<AppState> {
    let signed_in = Router::new()
        .route("/ui/", get(views::overview))
        .route("/ui/overview/panel", get(views::overview_panel))
        .route("/ui/board", get(views::board))
        .route("/ui/board/columns", get(views::board_columns))
        .route("/ui/tasks", post(actions::create_task))
        .route("/ui/tasks/{id}", get(task_page::task_page))
        .route("/ui/tasks/{id}/children", get(views::children))
        .route("/ui/tasks/{id}/post", post(actions::post_to_thread))
        .route("/ui/tasks/{id}/force-release", post(actions::force_release))
        .route("/ui/tasks/{id}/cancel", post(actions::cancel))
        .route("/ui/inbox", get(inbox::inbox))
        .route("/ui/inbox/{id}/archive", post(actions::archive))
        .route("/ui/messages", post(actions::send_message))
        .route("/ui/projects", get(admin::projects))
        .route("/ui/projects/{id}/rename", post(actions::rename_project))
        .route("/ui/tokens", get(admin::tokens))
        .route("/ui/tokens/{id}/revoke", post(actions::revoke_token))
        .route("/ui/live", get(live))
        .route("/ui/logout", post(logout))
        .route_layer(middleware::from_fn_with_state(state, require_session));
    Router::new()
        .route("/ui", get(|| async { Redirect::to("/ui/") }))
        .route("/ui/login", get(login_page).post(login))
        .route("/ui/static/{file}", get(asset))
        .merge(signed_in)
        .route_layer(middleware::from_fn(security_headers))
}

/// No framing, no inline script or style, and nothing loaded from other origins.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    res
}

// ---------------------------------------------------------------- session middleware

/// The `coord_ui` cookie's value, if sent.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value)
}

fn session_set_cookie(state: &AppState, id: &str) -> HeaderValue {
    let secure = if state.config.cookie_secure {
        "; Secure"
    } else {
        ""
    };
    let value = format!(
        "{COOKIE}={id}; Path=/ui; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        SESSION_TTL.whole_seconds()
    );
    HeaderValue::from_str(&value).expect("hex session ids are valid header values")
}

fn clear_cookie(state: &AppState) -> HeaderValue {
    let secure = if state.config.cookie_secure {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{COOKIE}=; Path=/ui; HttpOnly; SameSite=Strict; Max-Age=0{secure}"
    ))
    .expect("static cookie is a valid header value")
}

/// Whether the request carries the trusted-header-mode header (non-empty).
fn trusted(state: &AppState, headers: &HeaderMap) -> bool {
    state.config.trusted_header.as_deref().is_some_and(|name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !v.trim().is_empty())
    })
}

/// Signed-in routes: loads the session (or starts one for a trusted-header request), sends
/// anyone else to the login page, and rejects a non-GET request without the session's CSRF
/// token with 403.
async fn require_session(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let now = state.clock.now();
    let mut session = session_cookie(req.headers()).and_then(|id| state.ui.get(id, now));
    let mut new_cookie = None;
    if session.is_none() && trusted(&state, req.headers()) {
        let ctx = state.ui.create(now);
        new_cookie = Some(session_set_cookie(&state, &ctx.id));
        session = Some(ctx);
    }
    let Some(session) = session else {
        return to_login(req.headers());
    };
    let mut req = req;
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        let (ok, rebuilt) = csrf_ok(req, &session.csrf).await;
        if !ok {
            return views::error_page(StatusCode::FORBIDDEN, "Missing or invalid CSRF token.");
        }
        req = rebuilt;
    }
    req.extensions_mut().insert(session);
    let mut res = next.run(req).await;
    if let Some(cookie) = new_cookie {
        res.headers_mut().append(header::SET_COOKIE, cookie);
    }
    res
}

/// Not signed in: a 303 to the login page, or for htmx requests a 401 with `HX-Redirect`
/// (so a fragment swap does not paste the login page into the board).
fn to_login(headers: &HeaderMap) -> Response {
    if headers.contains_key("hx-request") {
        let mut res = StatusCode::UNAUTHORIZED.into_response();
        res.headers_mut()
            .insert("hx-redirect", HeaderValue::from_static("/ui/login"));
        return res;
    }
    Redirect::to("/ui/login").into_response()
}

/// Checks the `X-CSRF-Token` header, else the `csrf` field of a urlencoded form body (the
/// body is buffered and put back for the handler).
async fn csrf_ok(req: Request, expected: &str) -> (bool, Request) {
    if let Some(token) = req
        .headers()
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
    {
        let ok = constant_time_eq(token, expected);
        return (ok, req);
    }
    let is_form = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));
    if !is_form {
        return (false, req);
    }
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, FORM_LIMIT).await else {
        return (false, Request::from_parts(parts, Body::empty()));
    };
    let token = serde_urlencoded::from_bytes::<Vec<(String, String)>>(&bytes)
        .ok()
        .and_then(|fields| fields.into_iter().find(|(k, _)| k == "csrf"))
        .map(|(_, v)| v);
    let ok = token.is_some_and(|t| constant_time_eq(&t, expected));
    (ok, Request::from_parts(parts, Body::from(bytes)))
}

// ---------------------------------------------------------------- login / logout

async fn login_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let now = state.clock.now();
    if session_cookie(&headers).is_some_and(|id| state.ui.get(id, now).is_some()) {
        return Redirect::to("/ui/").into_response();
    }
    views::login_page(StatusCode::OK, None)
}

#[derive(Deserialize)]
struct LoginForm {
    #[serde(default)]
    secret: String,
}

/// The client IP the login limit is keyed by (`unknown` when the server was not started
/// with connection info, e.g. in tests).
fn client_ip(extensions: &Extensions) -> String {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map_or_else(|| "unknown".to_string(), |ci| ci.0.ip().to_string())
}

async fn login(
    State(state): State<AppState>,
    extensions: Extensions,
    Form(form): Form<LoginForm>,
) -> Response {
    let now = state.clock.now();
    if let Err(retry_after) = state.ui.login_limit.check(client_ip(&extensions), now) {
        let mut res = views::login_page(
            StatusCode::TOO_MANY_REQUESTS,
            Some("Too many login attempts. Wait a minute and try again."),
        );
        res.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(retry_after));
        return res;
    }
    if !constant_time_eq(&form.secret, &state.config.admin_secret) {
        return views::login_page(StatusCode::UNAUTHORIZED, Some("Wrong admin secret."));
    }
    let ctx = state.ui.create(now);
    let mut res = Redirect::to("/ui/").into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, session_set_cookie(&state, &ctx.id));
    res
}

async fn logout(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
) -> Response {
    state.ui.remove(&session.id);
    let mut res = Redirect::to("/ui/login").into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, clear_cookie(&state));
    res
}

// ---------------------------------------------------------------- static assets

/// The vendored and own static files, embedded in the binary.
async fn asset(Path(file): Path<String>, headers: HeaderMap) -> Response {
    let (body, content_type): (&'static str, &'static str) = match file.as_str() {
        "htmx.min.js" => (
            include_str!("../../static/htmx.min.js"),
            "text/javascript; charset=utf-8",
        ),
        "htmx-ext-sse.js" => (
            include_str!("../../static/htmx-ext-sse.js"),
            "text/javascript; charset=utf-8",
        ),
        "app.js" => (
            include_str!("../../static/app.js"),
            "text/javascript; charset=utf-8",
        ),
        "app.css" => (
            include_str!("../../static/app.css"),
            "text/css; charset=utf-8",
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // A content hash, so browsers revalidate exactly when an asset changes.
    let digest = Sha256::digest(body.as_bytes());
    let etag: String = std::iter::once("\"".to_string())
        .chain(digest[..12].iter().map(|b| format!("{b:02x}")))
        .chain(std::iter::once("\"".to_string()))
        .collect();
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag);
    let cache = [
        (header::CACHE_CONTROL, "no-cache".to_string()),
        (header::ETAG, etag),
    ];
    if fresh {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    (cache, [(header::CONTENT_TYPE, content_type)], body).into_response()
}

// ---------------------------------------------------------------- live stream

/// SSE for every open UI page. Per bus event it sends (see `views::live_frames`):
/// `task-<id>` with the re-rendered card of the affected top-level task (htmx `sse-swap`s it
/// in place), `board` when a top-level task may have changed column (the board re-fetches
/// its columns) and `overview` for anything (the overview re-fetches its panel). A lagging
/// subscriber gets `board` and `overview`. The stream ends when the server shuts down, and
/// when its session is logged out or expires (checked before every event and every
/// `LIVE_SESSION_RECHECK`).
async fn live(
    State(state): State<AppState>,
    Extension(session): Extension<SessionCtx>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    // Subscribe before responding, so nothing published after this request is missed.
    let rx = state.bus.subscribe();
    let shutdown = state.shutdown.clone();
    let mut recheck = tokio::time::interval(LIVE_SESSION_RECHECK);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let init = (rx, recheck, state, session.id);
    let stream = futures_util::stream::unfold(init, move |(mut rx, mut recheck, state, id)| {
        let shutdown = shutdown.clone();
        async move {
            loop {
                let received = tokio::select! {
                    received = rx.recv() => received,
                    _ = recheck.tick() => {
                        // Ends the stream once the session is gone.
                        state.ui.get(&id, state.clock.now())?;
                        continue;
                    }
                    () = shutdown.wait() => return None,
                };
                // Logged out or expired since the stream opened: send nothing more.
                state.ui.get(&id, state.clock.now())?;
                let frames = match received {
                    Ok(event) => views::live_frames(&state, &event).await,
                    Err(RecvError::Lagged(_)) => views::refresh_frames(),
                    Err(RecvError::Closed) => return None,
                };
                return Some((frames, (rx, recheck, state, id)));
            }
        }
    })
    .flat_map(|frames| {
        futures_util::stream::iter(
            frames
                .into_iter()
                .map(|(name, data)| Ok(SseEvent::default().event(name).data(data))),
        )
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_compares_values() {
        assert!(constant_time_eq("s3cret", "s3cret"));
        assert!(!constant_time_eq("s3cret", "s3creT"));
        assert!(!constant_time_eq("s3cret", "s3cre"));
        assert!(!constant_time_eq("", "x"));
    }

    #[test]
    fn reads_the_session_cookie() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; coord_ui=abc; b=2"),
        );
        assert_eq!(session_cookie(&h), Some("abc"));
        h.insert(header::COOKIE, HeaderValue::from_static("coord_uix=abc"));
        assert_eq!(session_cookie(&h), None);
    }

    #[test]
    fn sessions_expire() {
        let ui = UiState::new();
        let t0 = time::macros::datetime!(2026-11-27 9:00 UTC);
        let ctx = ui.create(t0);
        assert_eq!(ctx.id.len(), 64);
        assert_ne!(ctx.id, ctx.csrf);
        assert!(
            ui.get(&ctx.id, t0 + SESSION_TTL - Duration::SECOND)
                .is_some()
        );
        assert!(ui.get(&ctx.id, t0 + SESSION_TTL).is_none());
        assert!(ui.get(&ctx.id, t0).is_none(), "an expired session is gone");
    }
}
