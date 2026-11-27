//! `axl bridge` (spec §5, §6): the stdio MCP server each harness session spawns. It
//! serves the same tools as `/mcp` and forwards every call to the server with the bearer
//! token and the `X-Axl-*` identity headers, so the model never sees either.
//!
//! It fails open: with no token or no reachable server it still starts, still lists the
//! tools, and answers every call with `axle unavailable: <reason>` (an `isError` result),
//! while it retries registration every 30 s. Logs go to stderr only (stdout is the MCP
//! channel), each distinct problem once; the token is never logged.

pub mod project;
pub mod session_file;

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, StatusCode};
use futures_util::FutureExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    ContentBlock, Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::{RequestContext, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ErrorData, RoleClient, RoleServer, ServerHandler, ServiceExt};
use serde_json::json;
use tokio::sync::Mutex;

use self::project::{Env, resolve_project};
use self::session_file::{SessionFile, SessionInfo};
use crate::client::{AxlDir, read_token, resolve_server_url};
use crate::domain::SessionId;
use crate::mcp::{
    AxleMcp, HARNESS_HEADER, HOST_HEADER, INSTRUCTIONS, PROJECT_HEADER, SESSION_HEADER, UNKNOWN,
};
use crate::service::identity::NAME_MAX;

pub use crate::client::DEFAULT_SERVER_URL;
/// How often the bridge heartbeats (or, while unregistered, retries registering).
pub const HEARTBEAT: Duration = Duration::from_secs(30);
/// Overrides `HEARTBEAT`, in milliseconds. For tests and tuning only; not a user setting.
pub const HEARTBEAT_ENV: &str = "AXL_HEARTBEAT_MS";
/// Every tool result while the server cannot be used starts with this.
pub const UNAVAILABLE: &str = "axle unavailable";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything the bridge resolved at startup.
#[derive(Debug, Clone)]
pub struct Config {
    pub session_id: SessionId,
    pub harness: String,
    pub host: String,
    pub project_key: Option<String>,
    /// Without a trailing `/`.
    pub server_url: String,
    /// `HEARTBEAT`, unless `AXL_HEARTBEAT_MS` (tests and tuning only) sets another.
    pub heartbeat: Duration,
    /// `AXL_TOKEN`, if set; otherwise the token is read from `~/.axl/token` on use, so
    /// a token written after startup is picked up by the next retry.
    env_token: Option<String>,
    pub home: Option<PathBuf>,
}

impl Config {
    pub fn resolve(harness: &str, cwd: &Path, env: &Env) -> Self {
        let harness_label = sanitize_label(harness);
        if harness_label != harness {
            eprintln!("axl: harness name {harness:?} used as {harness_label:?}");
        }
        Self {
            session_id: SessionId::new(),
            harness: harness_label,
            host: sanitize_label(&short_hostname()),
            project_key: resolve_project(cwd, env),
            server_url: match env.get("HOME") {
                Some(home) => {
                    resolve_server_url(env.get("AXL_SERVER_URL"), &AxlDir::new(Path::new(home)))
                }
                None => env
                    .get("AXL_SERVER_URL")
                    .unwrap_or(DEFAULT_SERVER_URL)
                    .trim_end_matches('/')
                    .to_string(),
            },
            heartbeat: env
                .get(HEARTBEAT_ENV)
                .and_then(|ms| ms.parse::<u64>().ok())
                .filter(|&ms| ms > 0)
                .map_or(HEARTBEAT, Duration::from_millis),
            env_token: env.get("AXL_TOKEN").map(str::to_string),
            home: env.get("HOME").map(PathBuf::from),
        }
    }

    /// The bearer token, or why there is none.
    fn token(&self) -> Result<String, String> {
        let Some(home) = &self.home else {
            return match &self.env_token {
                Some(t) => Ok(t.clone()),
                None => Err("no token (set AXL_TOKEN or write ~/.axl/token)".to_string()),
            };
        };
        read_token(self.env_token.as_deref(), &AxlDir::new(home))
    }

    fn session_info(&self) -> SessionInfo {
        SessionInfo {
            session_id: self.session_id,
            project_key: self.project_key.clone(),
            server_url: self.server_url.clone(),
            harness: self.harness.clone(),
        }
    }
}

/// `raw` within the identity charset (`[A-Za-z0-9._:@-]`, at most 64 bytes): any other
/// character becomes `-`; empty becomes `unknown`.
pub fn sanitize_label(raw: &str) -> String {
    let mut label: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._:@-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    label.truncate(NAME_MAX);
    if label.is_empty() {
        UNKNOWN.to_string()
    } else {
        label
    }
}

/// This machine's host name up to the first `.` (`mac.local` → `mac`).
pub fn short_hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for `buf.len()` bytes; gethostname writes at most that.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return UNKNOWN.to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]);
    name.split('.').next().unwrap_or_default().to_string()
}

// ---------------------------------------------------------------- the proxy

type Upstream = RunningService<RoleClient, ClientConfig>;

/// The stdio MCP server; cheap to clone.
#[derive(Clone)]
pub struct Bridge {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    upstream: Mutex<Option<Arc<Upstream>>>,
    /// The last problem logged, so each distinct one is logged once.
    last_problem: StdMutex<Option<String>>,
}

impl Bridge {
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                upstream: Mutex::new(None),
                last_problem: StdMutex::new(None),
            }),
        }
    }

    /// Logs `reason` unless it is the problem logged last.
    fn problem(&self, reason: &str) {
        let mut last = self.inner.last_problem.lock().expect("log lock");
        if last.as_deref() != Some(reason) {
            eprintln!("{UNAVAILABLE}: {reason}");
            *last = Some(reason.to_string());
        }
    }

    /// Logs that the server is usable again, after a logged problem.
    fn recovered(&self) {
        let mut last = self.inner.last_problem.lock().expect("log lock");
        if last.take().is_some() {
            eprintln!("axl: connected to {}", self.inner.config.server_url);
        }
    }

    fn identity_headers(&self) -> HashMap<HeaderName, HeaderValue> {
        let c = &self.inner.config;
        let mut headers = HashMap::new();
        let mut put = |name: &'static str, value: &str| {
            if let Ok(v) = HeaderValue::from_str(value) {
                headers.insert(HeaderName::from_static(name), v);
            }
        };
        put(HARNESS_HEADER, &c.harness);
        put(HOST_HEADER, &c.host);
        put(SESSION_HEADER, &c.session_id.to_string());
        if let Some(p) = &c.project_key {
            put(PROJECT_HEADER, p);
        }
        headers
    }

    /// The live MCP client to `/mcp`, connecting first if there is none.
    async fn upstream(&self) -> Result<Arc<Upstream>, String> {
        let token = self.inner.config.token()?;
        let mut slot = self.inner.upstream.lock().await;
        if let Some(c) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(c.clone());
        }
        let url = format!("{}/mcp", self.inner.config.server_url);
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(url.as_str())
                .auth_header(token)
                .custom_headers(self.identity_headers()),
        );
        let client = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("axle-bridge", env!("CARGO_PKG_VERSION")),
        );
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, client.serve(transport))
            .await
            .map_err(|_| format!("server at {url} did not answer within 10 s"))?
            .map_err(|e| format!("cannot reach the server at {url}: {e}"))?;
        let connected = Arc::new(connected);
        *slot = Some(connected.clone());
        Ok(connected)
    }

    /// Drops `client` as the cached connection (the next call reconnects).
    async fn discard(&self, client: &Arc<Upstream>) {
        let mut slot = self.inner.upstream.lock().await;
        if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, client)) {
            *slot = None;
        }
    }

    /// Forwards one call. `Err` (the server is unusable) only for transport failures and
    /// timeouts, which also drop the connection.
    async fn forward(&self, request: CallToolRequestParams) -> Result<CallToolResult, String> {
        let client = self.upstream().await?;
        match tokio::time::timeout(CALL_TIMEOUT, client.call_tool(request)).await {
            Ok(Ok(result)) => Ok(result),
            // The server answered with a JSON-RPC error (an unknown tool, say): that is its
            // answer to this call, and the connection is fine.
            Ok(Err(ServiceError::McpError(e))) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(
                    e.message.to_string(),
                )]))
            }
            Ok(Err(e)) => {
                self.discard(&client).await;
                Err(format!("call to the server failed: {e}"))
            }
            Err(_) => {
                self.discard(&client).await;
                Err("the server did not answer within 60 s".to_string())
            }
        }
    }

    // ------------------------------------------------------------ presence

    /// Registers the session, then heartbeats every `heartbeat` (30 s by default); while
    /// unregistered (no token, server down, rejected) it retries on the same schedule. A
    /// failing (even panicking) round never ends the loop; it runs until the process ends.
    pub async fn presence_loop(self) {
        let http = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
            Ok(h) => h,
            Err(e) => {
                self.problem(&format!("cannot build an HTTP client: {e}"));
                return;
            }
        };
        let mut registered = false;
        let mut tick = tokio::time::interval(self.inner.config.heartbeat);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            registered = match AssertUnwindSafe(self.presence_round(&http, registered))
                .catch_unwind()
                .await
            {
                Ok(registered) => registered,
                Err(_) => {
                    self.problem("presence round failed unexpectedly; retrying");
                    false
                }
            };
        }
    }

    /// One heartbeat (when `registered`) or registration; whether the session is now
    /// registered.
    async fn presence_round(&self, http: &reqwest::Client, registered: bool) -> bool {
        let c = &self.inner.config;
        if registered {
            match self
                .post(http, "heartbeat", json!({ "session_id": c.session_id }))
                .await
            {
                Ok(()) => return true,
                // The server forgot the session (e.g. a fresh database): register again.
                Err(Rejected::NotFound) => {}
                Err(Rejected::Other(reason)) => {
                    self.problem(&reason);
                    return true;
                }
            }
        }
        let body = json!({
            "session_id": c.session_id,
            "host": c.host,
            "harness": c.harness,
            "project_key": c.project_key,
        });
        match self.post(http, "register", body).await {
            Ok(()) => {
                self.recovered();
                true
            }
            Err(Rejected::NotFound) => {
                self.problem("register: the server has no /v1/register");
                false
            }
            Err(Rejected::Other(reason)) => {
                self.problem(&reason);
                false
            }
        }
    }

    async fn post(
        &self,
        http: &reqwest::Client,
        what: &str,
        body: serde_json::Value,
    ) -> Result<(), Rejected> {
        let token = self.inner.config.token().map_err(Rejected::Other)?;
        let url = format!("{}/v1/{what}", self.inner.config.server_url);
        let response = http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|e| Rejected::Other(format!("cannot reach the server at {url}: {e}")))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        if status == StatusCode::NOT_FOUND {
            return Err(Rejected::NotFound);
        }
        let body = response.text().await.unwrap_or_default();
        let detail: String = body.chars().take(200).collect();
        Err(Rejected::Other(format!(
            "{what} rejected ({status}): {detail}"
        )))
    }
}

enum Rejected {
    NotFound,
    Other(String),
}

fn unavailable(reason: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("{UNAVAILABLE}: {reason}"))])
}

impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("axle", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let remote = match self.upstream().await {
            Ok(client) => match tokio::time::timeout(CALL_TIMEOUT, client.list_all_tools()).await {
                Ok(Ok(tools)) => Some(tools),
                _ => {
                    self.discard(&client).await;
                    None
                }
            },
            Err(reason) => {
                self.problem(&reason);
                None
            }
        };
        Ok(ListToolsResult::with_all_items(
            remote.unwrap_or_else(AxleMcp::tool_definitions),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let result = match self.forward(request).await {
            Ok(result) => {
                self.recovered();
                result
            }
            Err(reason) => {
                self.problem(&reason);
                unavailable(&reason)
            }
        };
        Ok(result.into())
    }
}

// ---------------------------------------------------------------- run

/// Runs the bridge until stdin closes or `stop` resolves, then removes the session file.
/// Never fails: every problem is logged and the bridge keeps serving (or exits cleanly).
pub async fn run(harness: &str, stop: impl Future<Output = ()>) {
    let env = Env::from_process();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let config = Config::resolve(harness, &cwd, &env);
    let session_file = match &config.home {
        None => {
            eprintln!("axl: HOME is not set; not writing a session file");
            None
        }
        Some(home) => {
            let path = session_file::path(home, std::os::unix::process::parent_id());
            match SessionFile::write(path, &config.session_info()) {
                Ok(f) => Some(f),
                Err(e) => {
                    eprintln!("axl: cannot write the session file: {e}");
                    None
                }
            }
        }
    };
    let bridge = Bridge::new(config);
    let presence = tokio::spawn(bridge.clone().presence_loop());

    tokio::select! {
        served = serve_stdio(bridge) => {
            if let Err(e) = served {
                eprintln!("axl: stdio MCP session ended: {e}");
            }
        }
        () = stop => {}
    }
    presence.abort();
    drop(session_file);
}

async fn serve_stdio(bridge: Bridge) -> Result<(), String> {
    let service = bridge
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| e.to_string())?;
    service.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Self {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).unwrap();
            let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let dir = std::env::temp_dir().join(format!("axl-bridge-cfg-{suffix}"));
            std::fs::create_dir_all(dir.join(".axl")).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolve_reads_server_url_from_config_toml() {
        let home = TempHome::new();
        std::fs::write(
            home.0.join(".axl").join("config.toml"),
            "server_url = \"https://axle.example.com/\"\n",
        )
        .unwrap();
        let home_str = home.0.to_str().unwrap();
        let env = Env::from_pairs([("HOME", home_str)]);
        let cfg = Config::resolve("claude", &home.0, &env);
        assert_eq!(cfg.server_url, "https://axle.example.com");

        let env = Env::from_pairs([
            ("HOME", home_str),
            ("AXL_SERVER_URL", "http://10.0.0.5:7420/"),
        ]);
        let cfg = Config::resolve("claude", &home.0, &env);
        assert_eq!(cfg.server_url, "http://10.0.0.5:7420");
    }
}
