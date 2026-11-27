//! The axum server: shared state, bearer-token auth, health probes and the `/v1` API.

pub mod v1;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::clock::Clock;
use crate::config::ServerConfig;
use crate::domain::TokenId;
use crate::events::EventBus;
use crate::service::auth::AuthService;
use crate::service::messages::MessageService;
use crate::service::tasks::TaskService;
use crate::store::Store;

/// Everything a request handler needs; cheap to clone.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub clock: Arc<dyn Clock>,
    pub config: Arc<ServerConfig>,
    pub bus: EventBus,
    pub auth: Arc<AuthService>,
    pub messages: Arc<MessageService>,
    pub tasks: Arc<TaskService>,
}

impl AppState {
    /// Builds the services over `store`, with message and task changes published on one bus.
    pub fn new(store: Arc<dyn Store>, clock: Arc<dyn Clock>, config: ServerConfig) -> Self {
        let bus = EventBus::new(store.clone(), clock.clone());
        Self {
            auth: Arc::new(AuthService::new(store.clone(), clock.clone())),
            messages: Arc::new(
                MessageService::new(store.clone(), clock.clone()).with_events(bus.clone()),
            ),
            tasks: Arc::new(
                TaskService::from_config(store.clone(), clock.clone(), &config)
                    .with_events(bus.clone()),
            ),
            bus,
            config: Arc::new(config),
            store,
            clock,
        }
    }
}

/// The authenticated caller's token, set by `require_token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthToken(pub TokenId);

/// The full server: unauthenticated probes plus the token-protected `/v1` API.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .nest(
            "/v1",
            v1::routes().route_layer(middleware::from_fn_with_state(state.clone(), require_token)),
        )
        .with_state(state)
}

/// Bearer-token middleware: a missing, unknown or revoked token is a 401
/// `{"error":"unauthorized"}`; otherwise the request carries an `AuthToken` extension.
pub async fn require_token(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer);
    let Some(presented) = presented else {
        return unauthorized();
    };
    match state.auth.verify(presented, state.clock.now()).await {
        Ok(Some(token)) => {
            req.extensions_mut().insert(AuthToken(token));
            next.run(req).await
        }
        Ok(None) => unauthorized(),
        Err(e) => ApiError::internal(e).into_response(),
    }
}

/// The token in an `Authorization: Bearer <token>` value (scheme case-insensitive).
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

fn unauthorized() -> Response {
    ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

/// Process liveness: answers whenever the server is up.
async fn healthz() -> &'static str {
    "ok"
}

/// Readiness: the store answers a trivial query.
async fn readyz(State(state): State<AppState>) -> Response {
    match state.store.ping().await {
        Ok(()) => "ok".into_response(),
        Err(e) => {
            eprintln!("coord: readiness check failed: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, "store unavailable").into_response()
        }
    }
}

/// A JSON error response: `{"error": "<message>"}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// A 500 whose detail goes to the server log, not to the client.
    pub fn internal(e: impl std::fmt::Display) -> Self {
        eprintln!("coord: internal error: {e}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::bearer;

    #[test]
    fn parses_bearer_header() {
        assert_eq!(bearer("Bearer coord_abc"), Some("coord_abc"));
        assert_eq!(bearer("bearer  coord_abc "), Some("coord_abc"));
        assert_eq!(bearer("Basic Zm9v"), None);
        assert_eq!(bearer("Bearer "), None);
        assert_eq!(bearer("coord_abc"), None);
    }
}
