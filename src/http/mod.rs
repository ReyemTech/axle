//! The axum server: shared state, bearer-token auth, health probes, the `/v1` API, the MCP
//! endpoint and the web UI.

pub mod enroll;
pub mod install_script;
pub mod rate_limit;
pub mod v1;

use std::future::{Future, IntoFuture};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tokio::sync::watch;

use crate::clock::Clock;
use crate::config::ServerConfig;
use crate::domain::TokenId;
use crate::events::EventBus;
use crate::http::rate_limit::RateLimiter;
use crate::service::auth::AuthService;
use crate::service::enroll::EnrollService;
use crate::service::messages::MessageService;
use crate::service::tasks::TaskService;
use crate::service::usage::UsageService;
use crate::store::Store;
use crate::ui::UiState;
use crate::usage::prices::PriceTable;

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
    pub enroll: Arc<EnrollService>,
    pub usage: Arc<UsageService>,
    /// Fired when the server starts shutting down; long-lived responses (SSE) end on it.
    pub shutdown: Shutdown,
    pub rate_limit: Arc<RateLimiter>,
    /// Per-client-IP limit on the unauthenticated `/v1/enroll` and `/v1/login/*` endpoints.
    pub enroll_limit: Arc<RateLimiter<std::net::IpAddr>>,
    /// Web UI sessions and login limits.
    pub ui: Arc<UiState>,
}

/// A one-way "the server is stopping" flag that any number of tasks can wait on.
#[derive(Clone)]
pub struct Shutdown(Arc<watch::Sender<bool>>);

impl Shutdown {
    pub fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    pub fn trigger(&self) {
        self.0.send_replace(true);
    }

    /// Resolves once `trigger` has been called (immediately if it already was).
    pub async fn wait(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|stopping| *stopping).await;
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

/// How long `serve` waits for in-flight requests after shutdown starts before returning.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

/// Serves `router(state)` on `listener` until `signal` resolves. Then it fires
/// `state.shutdown` (ending SSE streams), stops accepting connections, and waits at most
/// `SHUTDOWN_DRAIN` for in-flight requests before returning anyway.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    signal: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let shutdown = state.shutdown.clone();
    let trigger = shutdown.clone();
    let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            signal.await;
            trigger.trigger();
        })
        .into_future();
    tokio::select! {
        result = server => result,
        () = async {
            shutdown.wait().await;
            tokio::time::sleep(SHUTDOWN_DRAIN).await;
        } => {
            eprintln!(
                "axl: in-flight requests still open after {}s, exiting anyway",
                SHUTDOWN_DRAIN.as_secs()
            );
            Ok(())
        }
    }
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
            enroll: Arc::new(EnrollService::new(
                store.clone(),
                clock.clone(),
                bus.clone(),
            )),
            usage: Arc::new(UsageService::new(
                store.clone(),
                clock.clone(),
                Arc::new(PriceTable::builtin()),
            )),
            bus,
            shutdown: Shutdown::new(),
            rate_limit: Arc::new(RateLimiter::new(
                config.rate_limit_per_sec,
                config.rate_limit_burst,
            )),
            enroll_limit: Arc::new(RateLimiter::per_minute(enroll::ENROLL_PER_MINUTE)),
            ui: Arc::new(UiState::new()),
            config: Arc::new(config),
            store,
            clock,
        }
    }

    /// The owner's "Move tasks and history to it" (see `IdentityService::adopt`). A merge
    /// publishes `ProjectMerged`, so open event streams of the merged project's sessions
    /// pick up their surviving agent and project ids.
    pub async fn adopt_project(
        &self,
        id: crate::domain::ProjectId,
    ) -> Result<crate::domain::Adoption, crate::store::StoreError> {
        let out = crate::service::identity::IdentityService::adopt(&*self.store, &*self.clock, id)
            .await?;
        if let crate::domain::Adoption::Merged(into) = &out {
            self.bus
                .emit(crate::events::Event::ProjectMerged {
                    from: id,
                    into: into.id,
                })
                .await;
        }
        Ok(out)
    }

    /// Uses `prices` instead of the built-in table.
    pub fn with_prices(mut self, prices: PriceTable) -> Self {
        self.usage = Arc::new(UsageService::new(
            self.store.clone(),
            self.clock.clone(),
            Arc::new(prices),
        ));
        self
    }
}

/// The authenticated caller's token, set by `require_token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthToken(pub TokenId);

/// The full server: unauthenticated probes, the token-protected `/v1` API and MCP endpoint
/// (`/mcp`, Streamable HTTP), and the cookie-session web UI under `/ui`. Must be called
/// inside a Tokio runtime.
pub fn router(state: AppState) -> Router {
    let mcp = Router::new()
        .nest_service("/mcp", crate::mcp::service(state.clone()))
        .layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/install.sh", get(install_script::install_sh))
        .nest(
            "/v1",
            v1::routes()
                .route_layer(middleware::from_fn_with_state(state.clone(), require_token))
                .merge(enroll::routes()),
        )
        .merge(crate::ui::routes(state.clone()))
        .merge(mcp)
        .with_state(state)
}

/// Bearer-token middleware: a missing, unknown or revoked token is a 401
/// `{"error":"unauthorized"}`; a token over its rate limit is a 429
/// `{"error":"rate_limited"}` with `Retry-After` (opening an SSE stream counts as one
/// request); otherwise the request carries an `AuthToken` extension.
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
            if let Err(retry_after) = state.rate_limit.check(token, state.clock.now()) {
                return rate_limited(retry_after);
            }
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

/// 429 `{"error":"rate_limited"}` with `Retry-After` in whole seconds.
pub(crate) fn rate_limited(retry_after: u64) -> Response {
    let mut res = ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited").into_response();
    res.headers_mut()
        .insert(header::RETRY_AFTER, header::HeaderValue::from(retry_after));
    res
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
            eprintln!("axl: readiness check failed: {e}");
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
        eprintln!("axl: internal error: {e}");
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
        assert_eq!(bearer("Bearer axl_abc"), Some("axl_abc"));
        assert_eq!(bearer("bearer  axl_abc "), Some("axl_abc"));
        assert_eq!(bearer("Basic Zm9v"), None);
        assert_eq!(bearer("Bearer "), None);
        assert_eq!(bearer("axl_abc"), None);
    }
}
