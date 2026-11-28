//! `axl login` (one-time code or browser device flow) and `axl logout`.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::bridge::short_hostname;
use crate::client::{
    Api, AxlDir, ClientConfig, ClientError, PollStatus, load_config, read_token, remove_token,
    resolve_server_url, resolve_server_url_with_warning, save_config, write_token,
};

/// Added to the poll interval each time the server says `slow_down` (RFC 8628).
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// Poll interval when the server names none.
const DEFAULT_POLL: Duration = Duration::from_secs(7);
/// Bounds on what a server may tell us: the device flow lasts at most this long (the server
/// uses 10 minutes) and polls between these intervals.
const MAX_EXPIRES: Duration = Duration::from_secs(15 * 60);
const MIN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_INTERVAL: Duration = Duration::from_secs(60);

/// The poll interval to use for a server-supplied `interval` in seconds.
fn clamp_interval(secs: u64) -> Duration {
    if secs == 0 {
        DEFAULT_POLL
    } else {
        Duration::from_secs(secs).clamp(MIN_INTERVAL, MAX_INTERVAL)
    }
}

/// How long the device flow may last for a server-supplied `expires_in` in seconds.
fn clamp_expires(secs: u64) -> Duration {
    Duration::from_secs(secs).clamp(Duration::from_secs(1), MAX_EXPIRES)
}

/// Consecutive transport failures tolerated while polling.
const POLL_FAILURES: u32 = 5;

pub struct LoginArgs {
    pub url: Option<String>,
    pub code: Option<String>,
    pub no_browser: bool,
    pub force: bool,
}

#[derive(Debug)]
pub struct LoginOutcome {
    pub server_url: String,
    pub host: String,
}

/// Injection points, so tests neither open a browser nor wait seconds between polls.
pub struct LoginHooks {
    /// Opens a URL in the user's browser.
    pub open: Box<dyn Fn(&str) + Send + Sync>,
    /// Overrides the server's poll interval.
    pub interval: Option<Duration>,
}

impl Default for LoginHooks {
    fn default() -> Self {
        Self {
            open: Box::new(open_browser),
            interval: None,
        }
    }
}

/// `open` on macOS, `xdg-open` elsewhere. Best effort: the URL is printed regardless.
fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|mut child| {
            // Reap it without blocking the login.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        });
}

pub async fn login(
    home: &Path,
    args: LoginArgs,
    out: &mut dyn Write,
) -> Result<LoginOutcome, ClientError> {
    login_with(home, args, out, LoginHooks::default()).await
}

fn usage(msg: impl Into<String>) -> ClientError {
    ClientError::Usage(msg.into())
}

/// A server URL as the user typed it, without trailing slashes; must be http(s).
pub(crate) fn normalise_url(raw: &str) -> Result<String, ClientError> {
    let url = raw.trim().trim_end_matches('/');
    let ok = ["http://", "https://"]
        .iter()
        .any(|p| url.len() > p.len() && url[..p.len()].eq_ignore_ascii_case(p));
    if ok {
        Ok(url.to_string())
    } else {
        Err(usage(format!(
            "server URL must start with http:// or https://, got {raw:?}"
        )))
    }
}

pub async fn login_with(
    home: &Path,
    args: LoginArgs,
    out: &mut dyn Write,
    hooks: LoginHooks,
) -> Result<LoginOutcome, ClientError> {
    let dir = AxlDir::new(home);
    let url = match &args.url {
        Some(u) => normalise_url(u)?,
        None => resolve_server_url(std::env::var("AXL_SERVER_URL").ok().as_deref(), &dir),
    };

    // Refuse to quietly repoint a machine that already has a token for another server.
    if !args.force && read_token(None, &dir).is_ok() {
        let (current, _) = resolve_server_url_with_warning(None, &dir);
        if current != url {
            return Err(usage(format!(
                "already connected to {current}; pass --force to switch"
            )));
        }
    }

    // Read the config before anything is redeemed: a malformed file must not burn a code.
    let old = load_config(&dir).map_err(|e| {
        usage(format!(
            "cannot read {}: {e}; fix or remove it, then run `axl login` again",
            dir.config_path().display()
        ))
    })?;
    let host = short_hostname();
    let api = Api::new(&url);
    api.readyz().await?;

    let token = match args.code.as_deref() {
        Some(code) => api.enroll(code.trim(), &host).await?,
        None => device_flow(&api, &host, args.no_browser, out, &hooks).await?,
    };

    // The URL the user typed is what we saved, not whatever the server advertises.
    let cfg = ClientConfig {
        server_url: Some(url.clone()),
        port: old.port,
    };
    save_config(&dir, &cfg)?;
    if let Err(e) = write_token(&dir, &token) {
        let _ = save_config(&dir, &old);
        return Err(e);
    }
    let _ = writeln!(out, "Connected to {url} as {host}");
    Ok(LoginOutcome {
        server_url: url,
        host,
    })
}

async fn device_flow(
    api: &Api,
    host: &str,
    no_browser: bool,
    out: &mut dyn Write,
    hooks: &LoginHooks,
) -> Result<String, ClientError> {
    let start = api.login_start(host).await?;
    let _ = writeln!(
        out,
        "To connect this machine, open {} and enter the code {}",
        start.verify_url, start.user_code
    );
    let web = start.verify_url.to_ascii_lowercase();
    if !no_browser && (web.starts_with("http://") || web.starts_with("https://")) {
        (hooks.open)(&start.verify_url);
    }
    let _ = writeln!(out, "Waiting for approval...");
    let _ = out.flush();

    let mut interval = hooks
        .interval
        .unwrap_or_else(|| clamp_interval(start.interval));
    let deadline = Instant::now() + clamp_expires(start.expires_in);
    let mut failures = 0;
    loop {
        tokio::time::sleep(interval).await;
        if Instant::now() >= deadline {
            return Err(usage("login timed out; run `axl login` again"));
        }
        match api.login_poll(&start.device_code).await {
            Ok(PollStatus::Token(token)) => return Ok(token),
            Ok(PollStatus::Pending) => failures = 0,
            Ok(PollStatus::SlowDown) => {
                failures = 0;
                interval = (interval + SLOW_DOWN_STEP).min(MAX_INTERVAL);
            }
            Ok(PollStatus::RateLimited(wait)) => {
                failures = 0;
                let wait = wait
                    .map_or(Duration::ZERO, |s| Duration::from_secs(s).min(MAX_INTERVAL))
                    .min(deadline.saturating_duration_since(Instant::now()));
                tokio::time::sleep(wait).await;
                interval = (interval + SLOW_DOWN_STEP.min(interval)).min(MAX_INTERVAL);
            }
            Ok(PollStatus::Denied) => return Err(usage("login was denied")),
            Ok(PollStatus::Expired) => {
                return Err(usage("login code expired; run `axl login` again"));
            }
            Err(e @ ClientError::Http(_)) => {
                failures += 1;
                if failures >= POLL_FAILURES {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

pub async fn logout(home: &Path, revoke: bool, out: &mut dyn Write) -> Result<(), ClientError> {
    let dir = AxlDir::new(home);
    let token = read_token(None, &dir).ok();
    if revoke && let Some(token) = &token {
        let url = resolve_server_url(std::env::var("AXL_SERVER_URL").ok().as_deref(), &dir);
        match Api::new(&url).revoke_self(token).await {
            Ok(()) => {
                let _ = writeln!(out, "Revoked this machine's token on {url}");
            }
            // Already dead on the server: nothing left to revoke.
            Err(ClientError::Server(m)) if m == "unauthorized" => {}
            // Keep the local token so the user can retry.
            Err(e) => return Err(e),
        }
    }
    if remove_token(&dir)? {
        let _ = writeln!(out, "Logged out");
    } else {
        let _ = writeln!(out, "Not logged in");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_supplied_timings_are_clamped() {
        assert_eq!(clamp_interval(0), DEFAULT_POLL);
        assert_eq!(clamp_interval(1), Duration::from_secs(1));
        assert_eq!(clamp_interval(7), Duration::from_secs(7));
        assert_eq!(clamp_interval(u64::MAX), MAX_INTERVAL);
        assert_eq!(clamp_expires(0), Duration::from_secs(1));
        assert_eq!(clamp_expires(600), Duration::from_secs(600));
        assert_eq!(clamp_expires(u64::MAX), MAX_EXPIRES);
        // Adding a clamped value to Instant::now() cannot overflow.
        let _ = Instant::now() + clamp_expires(u64::MAX);
    }
}
