//! `axl install`: make sure there is a server (logging in or setting up local mode when
//! needed), then configure every detected harness, and print a summary table.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

use super::login::{LoginArgs, LoginHooks, login_with, normalise_url};
use crate::bridge::short_hostname;
use crate::client::{
    Api, AxlDir, ClientError, load_config, read_token, resolve_server_url_with_warning,
    save_config, write_token,
};
use crate::clock::{Clock, SystemClock};
use crate::daemon::{DEFAULT_PORT, NoServiceManager, ServiceControl, UnitSpec, install_unit};
use crate::install::{
    EditError, Harness, Manifest, ManifestEntry, Plan, apply, axl_path, claude_code, codex,
    opencode, render_diff,
};
use crate::service::auth::AuthService;
use crate::store::Store;
use crate::store::sqlite::SqliteStore;

/// How long local mode waits for the freshly installed service to answer `/readyz`.
const LOCAL_READY_TIMEOUT: Duration = Duration::from_secs(15);
const LOCAL_READY_POLL: Duration = Duration::from_millis(200);

const WHERE_QUESTION: &str = "Run Axle on this machine, or connect to a server?";
const WHERE_OPTIONS: [&str; 2] = ["This machine", "A server"];

/// Questions install asks on an interactive terminal. Tests script the answers.
pub trait Prompt {
    /// The index of the chosen option.
    fn choose(&self, question: &str, options: &[&str]) -> io::Result<usize>;
    /// A line of free text, trimmed.
    fn ask(&self, question: &str) -> io::Result<String>;
}

/// Asks on stdout and reads answers from stdin.
pub struct TerminalPrompt;

impl TerminalPrompt {
    fn read_line() -> io::Result<String> {
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "no answer (stdin closed); re-run with --yes, --local or --server URL",
            ));
        }
        Ok(line.trim().to_string())
    }
}

impl Prompt for TerminalPrompt {
    fn choose(&self, question: &str, options: &[&str]) -> io::Result<usize> {
        let mut stdout = io::stdout();
        writeln!(stdout, "{question}")?;
        for (i, o) in options.iter().enumerate() {
            writeln!(stdout, "  {}) {o}", i + 1)?;
        }
        loop {
            write!(stdout, "Choose 1-{} [1]: ", options.len())?;
            stdout.flush()?;
            let answer = Self::read_line()?;
            if answer.is_empty() {
                return Ok(0);
            }
            match answer.parse::<usize>() {
                Ok(n) if (1..=options.len()).contains(&n) => return Ok(n - 1),
                _ => writeln!(stdout, "Type a number from 1 to {}.", options.len())?,
            }
        }
    }

    fn ask(&self, question: &str) -> io::Result<String> {
        let mut stdout = io::stdout();
        write!(stdout, "{question} ")?;
        stdout.flush()?;
        Self::read_line()
    }
}

pub struct InstallArgs {
    pub server: Option<String>,
    pub code: Option<String>,
    /// Empty means every harness.
    pub harness: Vec<Harness>,
    pub local: bool,
    pub dry_run: bool,
    pub yes: bool,
}

/// Builds the service manager on demand, so runs that never need local mode never probe
/// launchd or systemd.
pub type ServiceFactory<'a> = &'a dyn Fn() -> Result<Box<dyn ServiceControl>, NoServiceManager>;

/// Everything install takes from the process, so tests can substitute each part.
pub struct InstallEnv<'a> {
    pub home: PathBuf,
    pub path_env: Option<OsString>,
    pub current_exe: PathBuf,
    /// Whether stdin is a terminal a person can answer on.
    pub interactive: bool,
    pub prompt: &'a dyn Prompt,
    pub out: &'a mut dyn Write,
    pub service: ServiceFactory<'a>,
    pub login_hooks: LoginHooks,
    /// Names this run's backups.
    pub now: OffsetDateTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Configured,
    AlreadyConfigured,
    /// What to do by hand (the editor's message, with the snippet).
    NeedsManual(String),
    NotFound,
}

#[derive(Clone, Debug)]
pub struct HarnessRow {
    pub harness: Harness,
    pub outcome: Outcome,
    pub next_step: Option<String>,
}

#[derive(Debug)]
pub struct InstallReport {
    pub rows: Vec<HarnessRow>,
    pub server_warning: Option<String>,
}

impl InstallReport {
    /// 0 when at least one harness was configured or already was, else 1.
    pub fn exit_code(&self) -> ExitCode {
        let ok = self
            .rows
            .iter()
            .any(|r| matches!(r.outcome, Outcome::Configured | Outcome::AlreadyConfigured));
        if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }
}

fn usage(msg: impl Into<String>) -> ClientError {
    ClientError::Usage(msg.into())
}

pub(super) fn display_name(h: Harness) -> &'static str {
    match h {
        Harness::ClaudeCode => "Claude Code",
        Harness::Codex => "Codex",
        Harness::Opencode => "OpenCode",
    }
}

fn next_step(h: Harness) -> &'static str {
    match h {
        Harness::ClaudeCode => "restart Claude Code",
        Harness::Codex => "restart Codex and trust the new hooks",
        Harness::Opencode => "restart OpenCode",
    }
}

fn unreachable_warning(url: &str) -> String {
    format!("Could not reach {url}. Check the VPN or URL, run axl status, or run axl login again.")
}

pub async fn install(
    env: &mut InstallEnv<'_>,
    args: InstallArgs,
) -> Result<InstallReport, ClientError> {
    let dir = AxlDir::new(&env.home);
    // Read everything install depends on before changing anything: a corrupt manifest
    // or config must stop the run before a code is redeemed or a service installed.
    let mut manifest = Manifest::load(&dir).map_err(|e| usage(e.to_string()))?;
    load_config(&dir).map_err(|e| {
        usage(format!(
            "cannot read {}: {e}; fix or remove it, then re-run",
            dir.config_path().display()
        ))
    })?;

    let server_warning = ensure_server(env, &dir, &args).await?;

    let axl = axl_path(&env.current_exe, env.path_env.as_deref());
    let rows = configure_harnesses(env, &dir, &axl, &args, &mut manifest)?;
    print_summary(env.out, &rows, args.dry_run, server_warning.as_deref());
    Ok(InstallReport {
        rows,
        server_warning,
    })
}

/// Step 1. Returns a warning for the summary when the configured server is unreachable.
async fn ensure_server(
    env: &mut InstallEnv<'_>,
    dir: &AxlDir,
    args: &InstallArgs,
) -> Result<Option<String>, ClientError> {
    if args.local {
        return local_mode(env, dir, args.dry_run).await.map(|()| None);
    }
    let has_token = read_token(None, dir).is_ok();
    let configured_url = load_config(dir)?.server_url;
    let (current, _) = resolve_server_url_with_warning(None, dir);

    if let Some(server) = &args.server {
        let url = normalise_url(server)?;
        if has_token && current == url {
            if args.code.is_some() {
                let _ = writeln!(
                    env.out,
                    "Already connected to {url}; the code was not used."
                );
            }
            return Ok(check_server(&url).await);
        }
        return log_in(env, url, args.code.clone(), args.dry_run)
            .await
            .map(|()| None);
    }

    if has_token || configured_url.is_some() {
        let mut warning = check_server(&current).await;
        if !has_token {
            let no_token =
                format!("This machine has no token for {current}; run axl login {current}.");
            warning = Some(match warning {
                Some(w) => format!("{w} {no_token}"),
                None => no_token,
            });
        }
        return Ok(warning);
    }

    if env.interactive && !args.yes {
        let choice = env
            .prompt
            .choose(WHERE_QUESTION, &WHERE_OPTIONS)
            .map_err(ClientError::Io)?;
        if choice == 1 {
            let raw = env
                .prompt
                .ask("Server URL (for example https://axle.example.com):")
                .map_err(ClientError::Io)?;
            let url = normalise_url(&raw)?;
            return log_in(env, url, None, args.dry_run).await.map(|()| None);
        }
    }
    local_mode(env, dir, args.dry_run).await.map(|()| None)
}

async fn check_server(url: &str) -> Option<String> {
    match Api::new(url).readyz().await {
        Ok(()) => None,
        Err(_) => Some(unreachable_warning(url)),
    }
}

/// The `axl login` flow (code exchange, or the device flow without a code). Its
/// "already connected; pass --force" refusal comes back unchanged.
async fn log_in(
    env: &mut InstallEnv<'_>,
    url: String,
    code: Option<String>,
    dry_run: bool,
) -> Result<(), ClientError> {
    if dry_run {
        let _ = writeln!(env.out, "would log in to {url}");
        return Ok(());
    }
    let hooks = std::mem::take(&mut env.login_hooks);
    login_with(
        &env.home,
        LoginArgs {
            url: Some(url),
            code,
            no_browser: false,
            force: false,
        },
        env.out,
        hooks,
    )
    .await
    .map(|_| ())
}

/// Installs the background service, waits for it, and connects this machine to it with a
/// token minted straight from the local database.
async fn local_mode(
    env: &mut InstallEnv<'_>,
    dir: &AxlDir,
    dry_run: bool,
) -> Result<(), ClientError> {
    let cfg = load_config(dir)?;
    let port = cfg.port.unwrap_or(DEFAULT_PORT);
    let url = format!("http://127.0.0.1:{port}");
    let token = read_token(None, dir).ok();
    let (current, _) = resolve_server_url_with_warning(None, dir);
    if token.is_some() && current != url {
        return Err(usage(format!(
            "already connected to {current}; run `axl logout` first to switch this machine \
             to a local server"
        )));
    }
    if dry_run {
        let _ = writeln!(env.out, "would install the local service on {url}");
        return Ok(());
    }

    let service = (env.service)().map_err(|e| usage(e.to_string()))?;
    let spec = UnitSpec {
        axl: axl_path(&env.current_exe, env.path_env.as_deref()),
        home: env.home.clone(),
        port,
    };
    install_unit(service.as_ref(), dir, &spec).map_err(|e| usage(e.to_string()))?;
    let _ = writeln!(env.out, "Local service installed on {url}");
    wait_ready(&url).await?;

    if let Some(t) = &token {
        match Api::new(&url).whoami(t).await {
            Ok(_) => {
                let _ = writeln!(env.out, "Already connected to {url}");
                return Ok(());
            }
            // The local server does not know this token (say, after `--purge`): mint anew.
            Err(ClientError::Server(m)) if m == "unauthorized" || m == "HTTP 401" => {}
            // Anything else would mint a token on every re-run; say what went wrong.
            Err(e) => {
                return Err(usage(format!(
                    "cannot check this machine's token with {url}: {e}; see `axl service logs`"
                )));
            }
        }
    }
    let host = short_hostname();
    let db = format!("sqlite://{}", dir.db_path().display());
    let store: Arc<dyn Store> = Arc::new(SqliteStore::connect(&db).await.map_err(|e| {
        usage(format!(
            "cannot open the local database {}: {e}",
            dir.db_path().display()
        ))
    })?);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (_, token) = AuthService::new(store, clock)
        .create(&format!("{host} (local)"))
        .await
        .map_err(|e| usage(format!("cannot create a token: {e}")))?;
    write_token(dir, &token)?;
    // Load, modify, save: the port install_unit recorded stays.
    let mut cfg = load_config(dir)?;
    cfg.server_url = Some(url.clone());
    save_config(dir, &cfg)?;
    let _ = writeln!(env.out, "Connected to {url} as {host}");
    Ok(())
}

async fn wait_ready(url: &str) -> Result<(), ClientError> {
    let api = Api::new(url);
    let deadline = Instant::now() + LOCAL_READY_TIMEOUT;
    loop {
        if api.readyz().await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(usage(format!(
                "the local service did not answer on {url} within {} s; see `axl service logs`",
                LOCAL_READY_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(LOCAL_READY_POLL).await;
    }
}

/// One harness's planned part of the run.
struct Planned {
    harness: Harness,
    plan: Plan,
    entries: Vec<ManifestEntry>,
    adopted: Vec<String>,
}

fn plan_for(
    h: Harness,
    home: &Path,
    axl: &Path,
    manifest: &Manifest,
) -> Result<(Plan, Vec<ManifestEntry>, Vec<String>), EditError> {
    match h {
        Harness::ClaudeCode => claude_code::plan_install(home, axl, manifest, claude_code::MODE),
        Harness::Codex => codex::plan_install(home, axl, manifest),
        Harness::Opencode => opencode::plan_install(home, axl, manifest),
    }
}

fn changes_something(plan: &Plan) -> bool {
    plan.edits.iter().any(|e| e.before != e.after)
        || plan.links.iter().any(|l| l.target != l.before)
}

/// Steps 2 and 3: detect, plan each harness, merge, then print (dry run) or apply.
fn configure_harnesses(
    env: &mut InstallEnv<'_>,
    dir: &AxlDir,
    axl: &Path,
    args: &InstallArgs,
    manifest: &mut Manifest,
) -> Result<Vec<HarnessRow>, ClientError> {
    let mut rows = Vec::new();
    let mut planned: Vec<Planned> = Vec::new();
    let mut manual: Vec<String> = Vec::new();
    for h in Harness::ALL {
        if !args.harness.is_empty() && !args.harness.contains(&h) {
            continue;
        }
        if !h.detect(&env.home, env.path_env.as_deref()) {
            rows.push(HarnessRow {
                harness: h,
                outcome: Outcome::NotFound,
                next_step: None,
            });
            continue;
        }
        match plan_for(h, &env.home, axl, manifest) {
            Ok((plan, entries, adopted)) => planned.push(Planned {
                harness: h,
                plan,
                entries,
                adopted,
            }),
            Err(e) => {
                manual.push(format!("{}: {e}", display_name(h)));
                rows.push(HarnessRow {
                    harness: h,
                    outcome: Outcome::NeedsManual(redact(&e.to_string())),
                    next_step: Some("make the change shown above, then re-run".into()),
                });
            }
        }
    }

    // Merge in the order Claude Code, Codex, OpenCode. When two editors change the same
    // file (the adopted AXL_SERVER_URL moving into ~/.axl/config.toml), the first wins and
    // the later edit, with its manifest entries for that file, is dropped.
    let mut merged = Plan::default();
    let mut changed_paths: HashSet<PathBuf> = HashSet::new();
    for p in &mut planned {
        let mut dropped: Vec<PathBuf> = Vec::new();
        for e in std::mem::take(&mut p.plan.edits) {
            if e.before != e.after && !changed_paths.insert(e.path.clone()) {
                dropped.push(e.path);
                continue;
            }
            p.plan.edits.push(e);
        }
        p.entries.retain(|en| !dropped.contains(&en.file));
        merged.edits.extend(p.plan.edits.iter().cloned());
        merged.links.extend(p.plan.links.iter().cloned());
        merged.notes.extend(p.plan.notes.iter().cloned());
    }

    // Everything below can quote the user's files (a hand-made entry may carry a token),
    // so it all goes through `redact`.
    for m in &manual {
        let _ = writeln!(env.out, "{}", redact(&format!("needs manual step: {m}")));
    }
    for p in &planned {
        for a in &p.adopted {
            let _ = writeln!(env.out, "{}", redact(&format!("adopted hand-made {a}")));
        }
    }

    if args.dry_run {
        let diff = render_diff(&merged);
        if !diff.is_empty() {
            let _ = write!(env.out, "{}", redact(&diff));
        }
    } else {
        for n in &merged.notes {
            let _ = writeln!(env.out, "{}", redact(&format!("note: {n}")));
        }
        // Also tightens an existing ~/.axl to 0700, and must precede apply, which would
        // otherwise create it with the default mode.
        dir.ensure()?;
        apply_and_record(&planned, dir, manifest, env.now)?;
    }

    for p in &planned {
        let changed = changes_something(&p.plan);
        rows.push(HarnessRow {
            harness: p.harness,
            outcome: if changed {
                Outcome::Configured
            } else {
                Outcome::AlreadyConfigured
            },
            next_step: changed.then(|| next_step(p.harness).to_string()),
        });
    }
    rows.sort_by_key(|r| Harness::ALL.iter().position(|h| *h == r.harness));
    Ok(rows)
}

/// Applies each harness's plan one edit at a time, then records the manifest entries for
/// every file that is now in its planned state. When an edit fails, the entries for what
/// already landed are still saved, so uninstall can find them, and the error names the
/// harness.
fn apply_and_record(
    planned: &[Planned],
    dir: &AxlDir,
    manifest: &mut Manifest,
    now: OffsetDateTime,
) -> Result<(), ClientError> {
    let mut done: HashSet<PathBuf> = HashSet::new();
    let mut failure: Option<(Harness, io::Error)> = None;
    'harnesses: for p in planned {
        for e in &p.plan.edits {
            let one = Plan {
                edits: vec![e.clone()],
                ..Plan::default()
            };
            if let Err(err) = apply(&one, now) {
                failure = Some((p.harness, err));
                break 'harnesses;
            }
            done.insert(e.path.clone());
        }
        for l in &p.plan.links {
            let one = Plan {
                links: vec![l.clone()],
                ..Plan::default()
            };
            if let Err(err) = apply(&one, now) {
                failure = Some((p.harness, err));
                break 'harnesses;
            }
            done.insert(l.path.clone());
        }
    }
    let before = manifest.entries.len();
    for p in planned {
        for e in p.entries.iter().filter(|e| done.contains(&e.file)) {
            manifest.record(e.clone());
        }
    }
    if manifest.entries.len() != before {
        manifest.save(dir)?;
    }
    match failure {
        None => Ok(()),
        Some((h, err)) => Err(usage(redact(&format!(
            "configuring {} failed: {err}. What was already changed is recorded in {}; \
             fix the problem and re-run",
            display_name(h),
            dir.manifest_path().display()
        )))),
    }
}

/// Key names whose values are never printed.
const SENSITIVE_KEYS: [&str; 4] = ["TOKEN", "SECRET", "AUTHORIZATION", "PASSWORD"];

/// Hides secrets in text that may quote the user's files: Axle tokens and device codes
/// (`axl_…`, `axld_…`) anywhere, and the value after any key whose name contains TOKEN,
/// SECRET, AUTHORIZATION or PASSWORD (`KEY = "v"`, `"KEY": "v"`, inline tables alike).
pub fn redact(text: &str) -> String {
    let text = redact_prefixed(text);
    text.split_inclusive('\n').map(redact_keys).collect()
}

fn redact_prefixed(text: &str) -> String {
    const MIN_SECRET_CHARS: usize = 8;
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        let prefix = ["axld_", "axl_"]
            .into_iter()
            .find(|p| b[i..].starts_with(p.as_bytes()));
        if let Some(p) = prefix {
            let start = i + p.len();
            let end = start
                + b[start..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric() || **c == b'-')
                    .count();
            if end - start >= MIN_SECRET_CHARS {
                out.push_str(&text[copied..i]);
                out.push_str("<redacted>");
                copied = end;
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    out
}

fn redact_keys(line: &str) -> String {
    let b = line.as_bytes();
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    let is_blank = |c: u8| c == b' ' || c == b'\t';
    let mut out = String::with_capacity(line.len());
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        if !is_ident(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_ident(b[i]) {
            i += 1;
        }
        let word = line[start..i].to_ascii_uppercase();
        if !SENSITIVE_KEYS.iter().any(|k| word.contains(k)) {
            continue;
        }
        let mut j = i;
        if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
            j += 1;
        }
        while j < b.len() && is_blank(b[j]) {
            j += 1;
        }
        if j >= b.len() || (b[j] != b'=' && b[j] != b':') {
            continue;
        }
        j += 1;
        while j < b.len() && is_blank(b[j]) {
            j += 1;
        }
        let value_start = j;
        let value_end = match b.get(j) {
            Some(&q) if q == b'"' || q == b'\'' => {
                let mut k = j + 1;
                while k < b.len() && b[k] != q {
                    k += if b[k] == b'\\' { 2 } else { 1 };
                }
                (k + 1).min(b.len())
            }
            // An object or array: the keys inside get their own turn.
            Some(b'{') | Some(b'[') | None => continue,
            Some(_) => {
                let mut k = j;
                while k < b.len()
                    && !matches!(b[k], b',' | b'}' | b']' | b'\n' | b'\r')
                    && !is_blank(b[k])
                {
                    k += 1;
                }
                k
            }
        };
        if value_end > value_start {
            out.push_str(&line[copied..value_start]);
            out.push_str("\"<redacted>\"");
            copied = value_end.min(b.len());
            i = copied;
        }
    }
    out.push_str(&line[copied..]);
    out
}

fn print_summary(out: &mut dyn Write, rows: &[HarnessRow], dry_run: bool, warning: Option<&str>) {
    let result = |o: &Outcome| -> &'static str {
        match (o, dry_run) {
            (Outcome::Configured, false) => "configured",
            (Outcome::Configured, true) => "would configure",
            (Outcome::AlreadyConfigured, _) => "already configured",
            (Outcome::NeedsManual(_), _) => "needs manual step",
            (Outcome::NotFound, _) => "not found",
        }
    };
    let table: Vec<[String; 3]> = rows
        .iter()
        .map(|r| {
            [
                display_name(r.harness).to_string(),
                result(&r.outcome).to_string(),
                r.next_step.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    let header = ["Harness", "Result", "Next step"].map(String::from);
    let width = |i: usize| {
        table
            .iter()
            .chain(std::iter::once(&header))
            .map(|r| r[i].len())
            .max()
            .unwrap_or(0)
    };
    let (w0, w1) = (width(0), width(1));
    let _ = writeln!(out);
    if dry_run {
        let _ = writeln!(out, "Dry run: nothing was written.");
    }
    for r in std::iter::once(&header).chain(table.iter()) {
        let _ = writeln!(out, "{:<w0$} | {:<w1$} | {}", r[0], r[1], r[2]);
    }
    if let Some(w) = warning {
        let _ = writeln!(out);
        let _ = writeln!(out, "{}", redact(&format!("WARNING: {w}")));
    }
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redact_hides_tokens_and_sensitive_values() {
        let hex = "0123456789abcdef0123456789abcdef";
        let cases = [
            (
                format!("+    \"AXL_TOKEN\": \"axl_{hex}\",\n"),
                "+    \"AXL_TOKEN\": \"<redacted>\",\n",
            ),
            (
                format!("-env = {{ AXL_SERVER_URL = \"http://h\", AXL_TOKEN = \"axl_{hex}\" }}\n"),
                "-env = { AXL_SERVER_URL = \"http://h\", AXL_TOKEN = \"<redacted>\" }\n",
            ),
            (
                "api_secret = hunter2\n".to_string(),
                "api_secret = \"<redacted>\"\n",
            ),
            (
                format!("Authorization: Bearer axl_{hex}"),
                "Authorization: \"<redacted>\" <redacted>",
            ),
            (format!("see axld_{hex} here"), "see <redacted> here"),
            ("\"tokens\": {\n".to_string(), "\"tokens\": {\n"),
            (
                "command = \"/bin/axl\"\n".to_string(),
                "command = \"/bin/axl\"\n",
            ),
            ("is axl_hook short\n".to_string(), "is axl_hook short\n"),
            (
                "héllo TOKEN='é\\'x' ok\n".to_string(),
                "héllo TOKEN=\"<redacted>\" ok\n",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(redact(&input), want, "{input}");
        }
    }
}
