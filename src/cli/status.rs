//! `axl status`: one read-only screen answering "is this machine connected, and to what?".
//! Every problem line is paired with the one command that fixes it. Nothing is written,
//! and the token is used only as a bearer credential, never printed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::install::{InstallEnv, display_name, redact};
use crate::bridge::session_file::SessionInfo;
use crate::client::{
    Api, AxlDir, ClientConfig, ClientError, load_config, read_token,
    resolve_server_url_with_warning, usage_off,
};
use crate::daemon::ServiceState;
use crate::install::{EditError, Harness, Manifest, Plan, axl_path, claude_code, codex, opencode};
use crate::usage::cursor;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusLine {
    pub label: String,
    pub value: String,
    /// The command that fixes the problem this line reports.
    pub fix: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StatusReport {
    pub lines: Vec<StatusLine>,
    /// False when any line carries a fix.
    pub ok: bool,
}

impl StatusReport {
    pub fn exit_code(&self) -> ExitCode {
        if self.ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }

    /// Aligned `label  value` lines, with `  fix: <command>` under each problem.
    pub fn render(&self) -> String {
        let width = self.lines.iter().map(|l| l.label.chars().count()).max();
        let width = width.unwrap_or(0);
        let mut out = String::new();
        for l in &self.lines {
            let pad = " ".repeat(width - l.label.chars().count());
            out.push_str(&format!("{}{pad}  {}\n", l.label, l.value));
            if let Some(fix) = &l.fix {
                out.push_str(&format!("{}  fix: {fix}\n", " ".repeat(width)));
            }
        }
        redact(&out)
    }
}

fn line(label: &str, value: impl Into<String>, fix: Option<String>) -> StatusLine {
    StatusLine {
        label: label.to_string(),
        value: value.into(),
        fix,
    }
}

/// Checks everything and prints the screen to `env.out`.
pub async fn run(env: &mut InstallEnv<'_>) -> StatusReport {
    let env_url = std::env::var("AXL_SERVER_URL").ok();
    let env_token = std::env::var("AXL_TOKEN").ok();
    let env_usage = std::env::var("AXL_USAGE").ok();
    let report = status_with(
        env,
        env_url.as_deref(),
        env_token.as_deref(),
        env_usage.as_deref(),
    )
    .await;
    let _ = write!(env.out, "{}", report.render());
    report
}

/// As [`status_with`], reading the server URL and token from `~/.axl` only.
pub async fn status(env: &InstallEnv<'_>) -> StatusReport {
    status_with(env, None, None, None).await
}

/// `env_url` / `env_token` / `env_usage` are the values of `AXL_SERVER_URL` / `AXL_TOKEN` /
/// `AXL_USAGE`, which win over the files.
pub async fn status_with(
    env: &InstallEnv<'_>,
    env_url: Option<&str>,
    env_token: Option<&str>,
    env_usage: Option<&str>,
) -> StatusReport {
    let dir = AxlDir::new(&env.home);
    let mut lines = Vec::new();

    let (url, warning) = resolve_server_url_with_warning(env_url, &dir);
    if let Some(w) = warning {
        lines.push(line(
            "config",
            w.trim_start_matches("axl: ").to_string(),
            Some(format!("fix or remove {}", dir.config_path().display())),
        ));
    }
    lines.push(line("server", url.clone(), None));
    let service = service_line(env);
    let stopped = service.as_ref().is_some_and(|l| l.fix.is_some());
    let token = read_token(env_token, &dir);
    server_lines(&mut lines, &url, token.clone(), service, stopped).await;
    let reachable = lines
        .iter()
        .any(|l| l.label == "reachable" && l.fix.is_none() && l.value.starts_with("yes"));
    if let (Ok(token), true) = (&token, reachable) {
        session_lines(&mut lines, &env.home, &url, token).await;
    }
    harness_lines(&mut lines, env, &dir);
    let cfg = load_config(&dir).map_err(|e| {
        (
            e.to_string(),
            format!("fix or remove {}", dir.config_path().display()),
        )
    });
    usage_lines(&mut lines, &env.home, cfg.as_ref(), env_usage);

    let ok = lines.iter().all(|l| l.fix.is_none());
    StatusReport { lines, ok }
}

fn is_local(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or("");
    let host = if host.starts_with('[') {
        host.split(']').next().unwrap_or("").trim_start_matches('[')
    } else {
        host.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// The HTTP status in a `readyz` error, when the server answered at all.
fn http_status(e: &ClientError) -> Option<u16> {
    let ClientError::Http(m) = e else { return None };
    let rest = m.split("(HTTP ").nth(1)?;
    rest.trim_end_matches(')').parse().ok()
}

async fn server_lines(
    lines: &mut Vec<StatusLine>,
    url: &str,
    token: Result<String, String>,
    service: Option<StatusLine>,
    service_stopped: bool,
) {
    let api = Api::new(url);
    let login = format!("axl login {url}");
    let local = is_local(url);
    let reachable = api.readyz().await;
    let mut service = service;
    match &reachable {
        Ok(()) => lines.push(line("reachable", "yes (/readyz)", None)),
        Err(e) => {
            // A stopped local service is the root cause: list it first, with the fix.
            if local && service_stopped {
                lines.extend(service.take());
            }
            let (value, fix) = match http_status(e) {
                Some(code) => (
                    format!("server at {url} returned {code}"),
                    if local {
                        "axl service restart".to_string()
                    } else {
                        "check the server's logs, then re-run axl status".to_string()
                    },
                ),
                None => (
                    format!("cannot reach {url}"),
                    if local {
                        "axl service start".to_string()
                    } else {
                        "check the VPN and the URL, then re-run axl status".to_string()
                    },
                ),
            };
            // Already pointed at the service above; don't repeat the same fix.
            let fix = (!(local && service_stopped)).then_some(fix);
            lines.push(line("reachable", value, fix));
        }
    }
    let token = match token {
        Ok(t) => t,
        Err(_) => {
            lines.push(line("token", "none for this server", Some(login)));
            lines.extend(service);
            return;
        }
    };
    if reachable.is_err() {
        lines.push(line("token", "not checked (server unreachable)", None));
        lines.extend(service);
        return;
    }
    match api.whoami(&token).await {
        Ok(me) => {
            lines.push(line("token", format!("valid ({})", me.label), None));
            if me.version == VERSION {
                lines.push(line(
                    "version",
                    format!("{VERSION} (server and axl match)"),
                    None,
                ));
            } else {
                let fix = if local {
                    "axl service restart".to_string()
                } else {
                    format!(
                        "install axl {} to match the server, or upgrade the server to {VERSION}",
                        me.version
                    )
                };
                lines.push(line(
                    "version",
                    format!("server runs {}, this axl is {VERSION}", me.version),
                    Some(fix),
                ));
            }
        }
        // Only a 401 means the token itself is bad.
        Err(ClientError::Server(m)) if m == "unauthorized" || m == "HTTP 401" => {
            lines.push(line(
                "token",
                "rejected by the server (unauthorized)",
                Some(login),
            ));
        }
        Err(ClientError::Server(why)) => {
            let fix = if local {
                "axl service restart".to_string()
            } else {
                "retry, or check the server's logs".to_string()
            };
            lines.push(line(
                "token",
                format!("server error checking the token ({why})"),
                Some(fix),
            ));
        }
        // The check never completed (timeout, dropped connection): not a success.
        Err(e) => {
            let fix = if local {
                "retry: axl status (or axl service restart)".to_string()
            } else {
                "retry: axl status, and check the network and the server".to_string()
            };
            lines.push(line("token", format!("could not check ({e})"), Some(fix)));
        }
    }
    lines.extend(service);
}

/// One `session` line per live bridge on this machine (a session file whose harness process
/// still runs): `<harness> · <project> · wake: <channel> · turn: <state>`, as its server
/// knows them. Informational only; a session the server cannot describe says so. Only
/// sessions of the configured `server` are asked; another server's are listed unqueried.
async fn session_lines(lines: &mut Vec<StatusLine>, home: &Path, server: &str, token: &str) {
    let dir = home.join(".axl").join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut live: Vec<(u32, SessionInfo)> = entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            if path.extension()? != "json" {
                return None;
            }
            let pid: u32 = path.file_stem()?.to_str()?.parse().ok()?;
            let info = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            alive(pid).then_some((pid, info))
        })
        .collect();
    live.sort_by_key(|(pid, _)| *pid);
    for (_, info) in live {
        let project = info.project_key.as_deref().unwrap_or("no project");
        // The token belongs to the configured server: never send it anywhere else.
        if !same_server(&info.server_url, server) {
            let value = format!(
                "{} · {project} · another server ({})",
                info.harness, info.server_url
            );
            lines.push(line("session", value, None));
            continue;
        }
        let state = Api::new(&info.server_url)
            .session_state(token, info.session_id)
            .await;
        let value = match state {
            Ok(s) => format!(
                "{} · {project} · wake: {} · turn: {}",
                info.harness, s.wake_channel, s.turn_state
            ),
            Err(e) => format!("{} · {project} · could not check ({e})", info.harness),
        };
        lines.push(line("session", value, None));
    }
}

/// Whether two server URLs name the same server: compared without surrounding whitespace
/// or trailing slashes, ignoring ASCII case.
fn same_server(a: &str, b: &str) -> bool {
    let norm = |u: &str| u.trim().trim_end_matches('/').to_ascii_lowercase();
    norm(a) == norm(b)
}

/// Whether process `pid` exists (a process of another user counts).
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 1 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists; nothing is sent.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn service_line(env: &InstallEnv<'_>) -> Option<StatusLine> {
    // No service manager means no local service to report on.
    let service = (env.service)().ok()?;
    match service.status() {
        Ok(ServiceState::Running) => Some(line("service", "running", None)),
        Ok(ServiceState::Stopped) => Some(line(
            "service",
            "installed but not running",
            Some("axl service start".into()),
        )),
        Ok(ServiceState::NotInstalled) => None,
        Err(e) => Some(line("service", format!("could not check ({e})"), None)),
    }
}

fn harness_lines(lines: &mut Vec<StatusLine>, env: &InstallEnv<'_>, dir: &AxlDir) {
    let manifest = match Manifest::load(dir) {
        Ok(m) => m,
        Err(e) => {
            lines.push(line(
                "manifest",
                e.to_string(),
                Some(format!(
                    "fix or move {} aside",
                    dir.manifest_path().display()
                )),
            ));
            return;
        }
    };
    let axl = axl_path(&env.current_exe, env.path_env.as_deref());
    for h in Harness::ALL {
        let label = display_name(h);
        let recorded = manifest.entries.iter().any(|e| e.harness == h);
        if !h.detect(&env.home, env.path_env.as_deref()) && !recorded {
            lines.push(line(label, "not found", None));
            continue;
        }
        let install_one = format!("axl install --harness {}", h.name());
        let plan = match plan_for(h, &env.home, &axl, &manifest) {
            Ok(p) => p,
            Err(EditError::NeedsManual { reason, .. }) => {
                lines.push(line(
                    label,
                    format!("not configured (needs a manual step: {reason})"),
                    Some(install_one),
                ));
                continue;
            }
            Err(EditError::Io(e)) => {
                lines.push(line(label, format!("could not check ({e})"), None));
                continue;
            }
        };
        let edits_differ = plan.edits.iter().any(|e| e.before != e.after);
        let links_differ = plan.links.iter().any(|l| l.target != l.before);
        if !edits_differ && !links_differ {
            lines.push(line(label, "configured", None));
            continue;
        }
        let missing = missing_axl_paths(&plan, &manifest, h);
        if let Some(path) = missing.first() {
            lines.push(line(
                label,
                format!("configured, but {} no longer exists", path.display()),
                Some("axl install".into()),
            ));
        } else if recorded && !edits_differ {
            lines.push(line(
                label,
                "configured, but the Claude Code plugin link is missing or points elsewhere",
                Some(install_one),
            ));
        } else if recorded {
            lines.push(line(
                label,
                "configured, but out of date (points at a different axl or lost an entry)",
                Some("axl install".into()),
            ));
        } else {
            lines.push(line(label, "not configured", Some(install_one)));
        }
    }
}

/// Whether usage reporting is on, the billing kind per harness (the config wins over what the
/// collector last recorded) and the last successful report. Information only, except when
/// config.toml does not parse: the collector then sends nothing, and the line says how to fix it.
fn usage_lines(
    lines: &mut Vec<StatusLine>,
    home: &Path,
    cfg: Result<&ClientConfig, &(String, String)>,
    env_usage: Option<&str>,
) {
    let default = ClientConfig::default();
    let (value, fix) = match (usage_off(env_usage, &default), cfg) {
        (Some(why), _) => (format!("off ({why})"), None),
        (None, Err((e, fix))) => (
            format!("off (config.toml unreadable: {e})"),
            Some(fix.clone()),
        ),
        (None, Ok(cfg)) => match usage_off(env_usage, cfg) {
            Some(why) => (format!("off ({why})"), None),
            None => ("on".to_string(), None),
        },
    };
    lines.push(line("usage", value, fix));
    let cfg = cfg.unwrap_or(&default);

    let status = cursor::load_status(home);
    let mut billing: BTreeMap<String, String> = status
        .billing
        .iter()
        .map(|(h, b)| (h.clone(), b.to_string()))
        .collect();
    if let Some(u) = &cfg.usage {
        for (h, raw) in &u.billing {
            let shown = match u.billing_for(h) {
                Some(b) => format!("{b} (from config)"),
                None => format!("{raw:?} (not a billing kind; ignored)"),
            };
            billing.insert(h.clone(), shown);
        }
    }
    let value = if billing.is_empty() {
        "not reported yet".to_string()
    } else {
        billing
            .iter()
            .map(|(h, b)| format!("{h} {b}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    lines.push(line("usage billing", value, None));

    let value = match status.last_success_at {
        Some(t) => format!("last sent {t}"),
        None => "none sent yet".to_string(),
    };
    lines.push(line("usage report", value, None));
}

fn plan_for(h: Harness, home: &Path, axl: &Path, manifest: &Manifest) -> Result<Plan, EditError> {
    match h {
        Harness::ClaudeCode => {
            claude_code::plan_install(home, axl, manifest, claude_code::MODE).map(|r| r.0)
        }
        Harness::Codex => codex::plan_install(home, axl, manifest).map(|r| r.0),
        Harness::Opencode => opencode::plan_install(home, axl, manifest).map(|r| r.0),
    }
}

/// Absolute `.../axl` paths in this harness's config files (and the files the manifest
/// recorded for it) that no longer exist.
fn missing_axl_paths(plan: &Plan, manifest: &Manifest, h: Harness) -> Vec<PathBuf> {
    let mut texts: Vec<String> = plan.edits.iter().filter_map(|e| e.before.clone()).collect();
    for e in manifest.entries.iter().filter(|e| e.harness == h) {
        if let Ok(t) = std::fs::read_to_string(&e.file) {
            texts.push(t);
        }
    }
    let mut missing: Vec<PathBuf> = Vec::new();
    for text in &texts {
        for tok in text.split(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '[' | ']' | '{' | '}' | '=' | '\\')
        }) {
            let p = Path::new(tok);
            let is_axl = p.is_absolute() && p.file_name().is_some_and(|n| n == "axl");
            if is_axl && !p.exists() && !missing.iter().any(|m| m == p) {
                missing.push(p.to_path_buf());
            }
        }
    }
    missing
}
