//! `axl status`: one read-only screen answering "is this machine connected, and to what?".
//! Every problem line is paired with the one command that fixes it. Nothing is written,
//! and the token is used only as a bearer credential, never printed.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::install::{InstallEnv, display_name, redact};
use crate::client::{Api, AxlDir, ClientError, read_token, resolve_server_url_with_warning};
use crate::daemon::ServiceState;
use crate::install::{EditError, Harness, Manifest, Plan, axl_path, claude_code, codex, opencode};

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
    let report = status_with(env, env_url.as_deref(), env_token.as_deref()).await;
    let _ = write!(env.out, "{}", report.render());
    report
}

/// As [`status_with`], reading the server URL and token from `~/.axl` only.
pub async fn status(env: &InstallEnv<'_>) -> StatusReport {
    status_with(env, None, None).await
}

/// `env_url` / `env_token` are the values of `AXL_SERVER_URL` / `AXL_TOKEN`, which win over
/// the files.
pub async fn status_with(
    env: &InstallEnv<'_>,
    env_url: Option<&str>,
    env_token: Option<&str>,
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
    server_lines(&mut lines, &url, read_token(env_token, &dir)).await;
    service_line(&mut lines, env);
    harness_lines(&mut lines, env, &dir);

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

async fn server_lines(lines: &mut Vec<StatusLine>, url: &str, token: Result<String, String>) {
    let api = Api::new(url);
    let login = format!("axl login {url}");
    let reachable = api.readyz().await;
    match &reachable {
        Ok(()) => lines.push(line("reachable", "yes (/readyz)", None)),
        Err(e) => lines.push(line(
            "reachable",
            format!("no ({e})"),
            Some(format!("{login}   (or check the VPN and the URL)")),
        )),
    }
    let token = match token {
        Ok(t) => t,
        Err(_) => {
            lines.push(line("token", "none for this server", Some(login)));
            return;
        }
    };
    if reachable.is_err() {
        lines.push(line("token", "not checked (server unreachable)", None));
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
                let fix = if is_local(url) {
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
        Err(ClientError::Server(why)) => {
            lines.push(line(
                "token",
                format!("rejected by the server ({why})"),
                Some(login),
            ));
        }
        Err(e) => lines.push(line("token", format!("could not check ({e})"), None)),
    }
}

fn service_line(lines: &mut Vec<StatusLine>, env: &InstallEnv<'_>) {
    // No service manager means no local service to report on.
    let Ok(service) = (env.service)() else {
        return;
    };
    match service.status() {
        Ok(ServiceState::Running) => lines.push(line("service", "running", None)),
        Ok(ServiceState::Stopped) => lines.push(line(
            "service",
            "installed but not running",
            Some("axl service start".into()),
        )),
        Ok(ServiceState::NotInstalled) => {}
        Err(e) => lines.push(line("service", format!("could not check ({e})"), None)),
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
