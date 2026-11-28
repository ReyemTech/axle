//! The bridge's wake listener (wake-up spec, bridge side). It subscribes to the server's
//! `/v1/events` stream for this session; on a `WakeRequested` it hands the text to the
//! harness's `WakeAdapter` and reports how that went to `POST /v1/wake/result`.
//!
//! It fails open like the rest of the bridge: a server outage, a bad stream or a panicking
//! round only logs once and retries after the heartbeat interval; stdio proxying is never
//! touched.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::FutureExt;
use serde::Deserialize;
use serde_json::json;

use super::sse::SseParser;
use super::{Bridge, Config, HTTP_TIMEOUT, Rejected};
use crate::domain::{TaskId, TurnState, WakeChannel};

/// The longest one poke may take before it counts as failed.
const POKE_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest a stream may stay silent. The server's keep-alive comes every 15 s, so a
/// longer silence means a dead connection.
const STREAM_IDLE: Duration = Duration::from_secs(90);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest outcome detail sent back, in characters.
const DETAIL_MAX: usize = 300;

/// How a poke turned out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PokeOutcome {
    /// The text reached the harness.
    Sent,
    /// The harness could not take it now (a prompt is open, say). The reason follows.
    Held(String),
    /// The harness declined it.
    Refused(String),
    /// Nothing reached the harness.
    Failed(String),
}

/// One harness's way of getting text in front of an idle session.
#[async_trait]
pub trait WakeAdapter: Send + Sync {
    /// The channel name reported to the server.
    fn channel(&self) -> WakeChannel;
    /// Whether the channel works right now; the heartbeat carries `channel()` only when it
    /// does.
    async fn check(&self) -> bool;
    /// The harness's own view of the turn, when it has one.
    async fn turn_state(&self) -> Option<TurnState>;
    /// Delivers `text` to the session.
    async fn poke(&self, text: &str) -> PokeOutcome;
}

/// The adapter for a harness with no wake channel: never wakeable.
pub struct NoneAdapter;

#[async_trait]
impl WakeAdapter for NoneAdapter {
    fn channel(&self) -> WakeChannel {
        WakeChannel::None
    }

    async fn check(&self) -> bool {
        false
    }

    async fn turn_state(&self) -> Option<TurnState> {
        None
    }

    async fn poke(&self, _text: &str) -> PokeOutcome {
        PokeOutcome::Failed("no wake channel".to_string())
    }
}

/// The adapter for this bridge's harness. Only `NoneAdapter` exists so far.
pub fn select_adapter(_config: &Config) -> Box<dyn WakeAdapter> {
    Box::new(NoneAdapter)
}

/// What a `WakeRequested` event carries that the bridge uses.
#[derive(Debug, Deserialize)]
struct WakeData {
    session: crate::domain::SessionId,
    task: Option<TaskId>,
    text: String,
}

/// Listens for wakes until the process ends; a failing (even panicking) round never ends
/// the loop. A round that finds the adapter unusable opens no stream at all.
pub async fn wake_loop(bridge: Bridge, adapter: Arc<dyn WakeAdapter>) {
    let delay = bridge.inner.config.heartbeat;
    let clients = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .and_then(|stream| {
            reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .map(|post| (stream, post))
        });
    let (stream_http, post_http) = match clients {
        Ok(c) => c,
        Err(e) => {
            bridge.problem(&format!("cannot build an HTTP client for wakes: {e}"));
            return;
        }
    };
    loop {
        let round = AssertUnwindSafe(wake_round(
            &bridge,
            adapter.as_ref(),
            &stream_http,
            &post_http,
        ))
        .catch_unwind()
        .await;
        match round {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => bridge.problem(&reason),
            Err(_) => bridge.problem("wake listener failed unexpectedly; retrying"),
        }
        tokio::time::sleep(delay).await;
    }
}

/// One subscription: re-checks the adapter, then reads the stream until it ends or fails.
async fn wake_round(
    bridge: &Bridge,
    adapter: &dyn WakeAdapter,
    stream_http: &reqwest::Client,
    post_http: &reqwest::Client,
) -> Result<(), String> {
    let usable = adapter.check().await;
    bridge.set_wake_channel(if usable {
        adapter.channel()
    } else {
        WakeChannel::None
    });
    if !usable {
        return Ok(());
    }
    let config = &bridge.inner.config;
    let token = config.token()?;
    let url = format!(
        "{}/v1/events?session={}",
        config.server_url, config.session_id
    );
    let mut response = stream_http
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("wake stream: cannot reach the server at {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("wake stream rejected ({})", response.status()));
    }
    let mut parser = SseParser::default();
    loop {
        let chunk = tokio::time::timeout(STREAM_IDLE, response.chunk())
            .await
            .map_err(|_| "wake stream went silent; reconnecting".to_string())?
            .map_err(|e| format!("wake stream dropped: {e}"))?;
        let Some(chunk) = chunk else {
            return Ok(());
        };
        for (event, data) in parser.push(&chunk) {
            if event == "WakeRequested" {
                handle(bridge, adapter, post_http, &data).await;
            }
        }
    }
}

/// Pokes the adapter for one `WakeRequested` and reports the outcome.
async fn handle(bridge: &Bridge, adapter: &dyn WakeAdapter, http: &reqwest::Client, data: &str) {
    let Ok(wake) = serde_json::from_str::<WakeData>(data) else {
        bridge.problem("a wake event could not be read; ignoring it");
        return;
    };
    // The server only sends a session its own wakes; this guards a confused one.
    if wake.session != bridge.inner.config.session_id {
        return;
    }
    let outcome = tokio::time::timeout(POKE_TIMEOUT, adapter.poke(&wake.text))
        .await
        .unwrap_or_else(|_| PokeOutcome::Failed("the wake did not finish within 30 s".into()));
    let (name, detail) = match outcome {
        PokeOutcome::Sent => ("sent", None),
        PokeOutcome::Held(d) => ("held", Some(d)),
        PokeOutcome::Refused(d) => ("refused", Some(d)),
        PokeOutcome::Failed(d) => ("failed", Some(d)),
    };
    let detail = detail.map(|d| d.chars().take(DETAIL_MAX).collect::<String>());
    let body = json!({
        "session_id": wake.session,
        "task_id": wake.task,
        "outcome": name,
        "detail": detail,
    });
    match bridge.post(http, "wake/result", body).await {
        Ok(()) => {}
        Err(Rejected::NotFound) => bridge.problem("wake result: the server has no such route"),
        Err(Rejected::Other(reason)) => bridge.problem(&reason),
    }
}
