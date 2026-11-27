//! Unauthenticated enrollment endpoints (spec §2): redeem a one-time code, or run the device
//! login. They sit outside bearer auth and are limited per client IP instead. Responses never
//! carry anything but the token itself, and always `Cache-Control: no-store`.

use std::net::SocketAddr;

use axum::extract::State;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{ApiError, AppState};
use crate::service::enroll::PollReply;

/// Requests per minute per client IP, across `/enroll` and `/login/*`.
pub const ENROLL_PER_MINUTE: u32 = 10;

const INVALID_CODE: &str = "invalid or expired code";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/enroll", post(enroll))
        .route("/login/start", post(login_start))
        .route("/login/poll", post(login_poll))
        .layer(middleware::map_response(no_store))
}

/// Marks a response as uncacheable.
pub async fn no_store(mut res: Response) -> Response {
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// The client IP the per-IP limits are keyed by. With `trusted` set (the header a proxy
/// fills in), its right-most non-empty comma-separated entry, the one the nearest proxy
/// appended; a client cannot forge that one. Otherwise, or when the header is absent, the
/// socket address; `unknown` when the server was not started with connection info (tests).
pub fn client_ip(headers: &HeaderMap, extensions: &Extensions, trusted: Option<&str>) -> String {
    let forwarded = trusted
        .and_then(|name| headers.get(name))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').map(str::trim).find(|e| !e.is_empty()));
    match forwarded {
        Some(ip) => ip.to_string(),
        None => extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map_or_else(|| "unknown".to_string(), |ci| ci.0.ip().to_string()),
    }
}

/// Takes one request from the caller's per-IP bucket; the 429 response when it is empty.
fn limit(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> Result<String, Box<Response>> {
    let ip = client_ip(
        headers,
        extensions,
        state.config.trusted_proxy_header.as_deref(),
    );
    match state.enroll_limit.check(ip.clone(), state.clock.now()) {
        Ok(()) => Ok(ip),
        Err(retry_after) => Err(Box::new(super::rate_limited(retry_after))),
    }
}

/// The address to hand the client, or the 503 naming `AXL_PUBLIC_URL`.
fn advertised(state: &AppState, headers: &HeaderMap) -> Result<String, Box<Response>> {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    state.config.advertised_url(host).map_err(|e| {
        Box::new(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response())
    })
}

fn bad_request(error: &str) -> Response {
    ApiError::new(StatusCode::BAD_REQUEST, error).into_response()
}

#[derive(Deserialize)]
struct EnrollRequest {
    #[serde(default)]
    code: String,
    #[serde(default)]
    host: String,
}

async fn enroll(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Json(req): Json<EnrollRequest>,
) -> Response {
    let ip = match limit(&state, &headers, &extensions) {
        Ok(ip) => ip,
        Err(res) => return *res,
    };
    // Before the store: a code is never consumed without being able to say where to connect.
    let server_url = match advertised(&state, &headers) {
        Ok(url) => url,
        Err(res) => return *res,
    };
    match state.enroll.exchange(&req.code, &req.host, Some(&ip)).await {
        Ok(Some(token)) => {
            Json(json!({ "token": token, "server_url": server_url })).into_response()
        }
        Ok(None) => bad_request(INVALID_CODE),
        Err(e) => ApiError::internal(e).into_response(),
    }
}

#[derive(Deserialize)]
struct StartRequest {
    #[serde(default)]
    host: String,
}

async fn login_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Json(req): Json<StartRequest>,
) -> Response {
    let ip = match limit(&state, &headers, &extensions) {
        Ok(ip) => ip,
        Err(res) => return *res,
    };
    let server_url = match advertised(&state, &headers) {
        Ok(url) => url,
        Err(res) => return *res,
    };
    match state.enroll.start_device(&req.host, Some(&ip)).await {
        Ok(start) => Json(json!({
            "device_code": start.device_code,
            "user_code": start.user_code,
            "verify_url": format!("{server_url}/ui/connect"),
            "interval": start.interval,
            "expires_in": start.expires_in,
        }))
        .into_response(),
        Err(e) => ApiError::internal(e).into_response(),
    }
}

#[derive(Deserialize)]
struct PollRequest {
    #[serde(default)]
    device_code: String,
}

async fn login_poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Json(req): Json<PollRequest>,
) -> Response {
    if let Err(res) = limit(&state, &headers, &extensions) {
        return *res;
    }
    let server_url = match advertised(&state, &headers) {
        Ok(url) => url,
        Err(res) => return *res,
    };
    match state.enroll.poll(&req.device_code).await {
        Ok(PollReply::Token { token, .. }) => {
            Json(json!({ "token": token, "server_url": server_url })).into_response()
        }
        Ok(PollReply::Pending) => bad_request("authorization_pending"),
        Ok(PollReply::SlowDown) => bad_request("slow_down"),
        Ok(PollReply::Denied) => bad_request("denied"),
        Ok(PollReply::Expired) => bad_request("expired"),
        Err(e) => ApiError::internal(e).into_response(),
    }
}
