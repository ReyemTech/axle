//! A small client for the server endpoints `axl login` and `axl logout` use. Secrets (the
//! token, the device code) travel only in request bodies or the `Authorization` header, and
//! never appear in `Debug` output or error text.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::ClientError;

const TIMEOUT: Duration = Duration::from_secs(15);

pub struct Api {
    base: String,
    client: reqwest::Client,
}

#[derive(Deserialize)]
pub struct DeviceStartReply {
    pub device_code: String,
    pub user_code: String,
    pub verify_url: String,
    pub interval: u64,
    pub expires_in: u64,
}

impl fmt::Debug for DeviceStartReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceStartReply")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verify_url", &self.verify_url)
            .field("interval", &self.interval)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(PartialEq, Eq)]
pub enum PollStatus {
    Pending,
    SlowDown,
    /// HTTP 429; wait this many seconds (the `Retry-After`, when the server sent one).
    RateLimited(Option<u64>),
    Denied,
    Expired,
    Token(String),
}

impl fmt::Debug for PollStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => f.write_str("Pending"),
            Self::SlowDown => f.write_str("SlowDown"),
            Self::RateLimited(s) => write!(f, "RateLimited({s:?})"),
            Self::Denied => f.write_str("Denied"),
            Self::Expired => f.write_str("Expired"),
            Self::Token(_) => f.write_str("Token(<redacted>)"),
        }
    }
}

/// `GET /v1/session`: what Axle knows about one session's turn and how to wake it.
#[derive(Deserialize, Debug)]
pub struct SessionState {
    pub wake_channel: crate::domain::WakeChannel,
    pub turn_state: crate::domain::TurnState,
}

#[derive(Deserialize, Debug)]
pub struct WhoAmI {
    pub label: String,
    pub version: String,
}

impl Api {
    pub fn new(base: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            base: base.trim_end_matches('/').to_string(),
            client,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, ClientError> {
        let res = req.send().await.map_err(|e| {
            // Without the URL: reqwest errors embed it, and this keeps one stable message.
            let why = if e.is_timeout() {
                "timed out"
            } else if e.is_connect() {
                "connection failed"
            } else {
                "request failed"
            };
            ClientError::Http(format!("{why} talking to {}", self.base))
        })?;
        if res.status().is_redirection() {
            // Redirects are off (a token must not follow one), so say where it points.
            let to = res
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("another address");
            // A scheme or host move keeps the path; the user wants the base URL.
            let path = res.url().path();
            let to = to.strip_suffix(path).unwrap_or(to);
            return Err(ClientError::Http(format!(
                "{} redirects to {to}; use that URL instead",
                self.base
            )));
        }
        Ok(res)
    }

    pub async fn readyz(&self) -> Result<(), ClientError> {
        let res = self.send(self.client.get(self.url("/readyz"))).await?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(ClientError::Http(format!(
                "{} is not ready (HTTP {})",
                self.base,
                res.status().as_u16()
            )))
        }
    }

    /// Redeems a one-time code; the token.
    pub async fn enroll(&self, code: &str, host: &str) -> Result<String, ClientError> {
        let res = self
            .send(
                self.client
                    .post(self.url("/v1/enroll"))
                    .json(&json!({ "code": code, "host": host })),
            )
            .await?;
        let body = ok_body(res).await?;
        token_of(&body)
    }

    pub async fn login_start(&self, host: &str) -> Result<DeviceStartReply, ClientError> {
        let res = self
            .send(
                self.client
                    .post(self.url("/v1/login/start"))
                    .json(&json!({ "host": host })),
            )
            .await?;
        let body = ok_body(res).await?;
        serde_json::from_value(body)
            .map_err(|_| ClientError::Server("malformed login/start reply".into()))
    }

    pub async fn login_poll(&self, device_code: &str) -> Result<PollStatus, ClientError> {
        let res = self
            .send(
                self.client
                    .post(self.url("/v1/login/poll"))
                    .json(&json!({ "device_code": device_code })),
            )
            .await?;
        let status = res.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let wait = res
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok());
            return Ok(PollStatus::RateLimited(wait));
        }
        let body = body_json(res).await;
        if status.is_success() {
            return Ok(PollStatus::Token(token_of(&body)?));
        }
        match error_text(&body, status).as_str() {
            "authorization_pending" => Ok(PollStatus::Pending),
            "slow_down" => Ok(PollStatus::SlowDown),
            "denied" => Ok(PollStatus::Denied),
            "expired" => Ok(PollStatus::Expired),
            other => Err(ClientError::Server(other.to_string())),
        }
    }

    pub async fn whoami(&self, token: &str) -> Result<WhoAmI, ClientError> {
        let res = self
            .send(self.client.get(self.url("/v1/whoami")).bearer_auth(token))
            .await?;
        let body = ok_body(res).await?;
        serde_json::from_value(body)
            .map_err(|_| ClientError::Server("malformed whoami reply".into()))
    }

    /// One of this token's sessions: its wake channel and turn state.
    pub async fn session_state(
        &self,
        token: &str,
        session: crate::domain::SessionId,
    ) -> Result<SessionState, ClientError> {
        let res = self
            .send(
                self.client
                    .get(self.url(&format!("/v1/session?session={session}")))
                    .bearer_auth(token),
            )
            .await?;
        let body = ok_body(res).await?;
        serde_json::from_value(body)
            .map_err(|_| ClientError::Server("malformed session reply".into()))
    }

    /// Revokes the calling token on the server.
    pub async fn revoke_self(&self, token: &str) -> Result<(), ClientError> {
        let res = self
            .send(
                self.client
                    .post(self.url("/v1/token/revoke"))
                    .bearer_auth(token)
                    .json(&json!({})),
            )
            .await?;
        ok_body(res).await.map(|_| ())
    }
}

async fn body_json(res: reqwest::Response) -> Value {
    res.json().await.unwrap_or(Value::Null)
}

/// The JSON body of a success reply; the server's `error` text otherwise.
async fn ok_body(res: reqwest::Response) -> Result<Value, ClientError> {
    let status = res.status();
    let body = body_json(res).await;
    if status.is_success() {
        Ok(body)
    } else {
        Err(ClientError::Server(error_text(&body, status)))
    }
}

fn error_text(body: &Value, status: StatusCode) -> String {
    body.get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()))
}

fn token_of(body: &Value) -> Result<String, ClientError> {
    body.get("token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ClientError::Server("reply carried no token".into()))
}
