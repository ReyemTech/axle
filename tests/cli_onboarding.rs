//! `axl install`: login inside install, local mode, harness setup, dry run and re-runs,
//! against temp HOMEs, a real in-process server, a fake prompt and a recording service.
//! The last test drives the real binary end to end.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axle::bridge::short_hostname;
use axle::cli::{
    HarnessRow, InstallArgs, InstallEnv, InstallReport, LoginArgs, LoginHooks, Outcome, Prompt,
    StatusReport, UninstallArgs, install, login, status, uninstall,
};
use axle::client::{Api, AxlDir, ClientConfig, load_config, save_config, write_token};
use axle::clock::{Clock, SystemClock};
use axle::config::ServerConfig;
use axle::daemon::{DaemonError, NoServiceManager, ServiceControl, ServiceState, UnitSpec};
use axle::http::AppState;
use axle::install::Harness;
use axle::store::Store;
use axle::store::sqlite::SqliteStore;
use time::OffsetDateTime;
use time::macros::datetime;

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("axle-install-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn mkdir(&self, rel: &str) -> &Self {
        std::fs::create_dir_all(self.0.join(rel)).unwrap();
        self
    }
    fn write(&self, rel: &str, text: &str) -> &Self {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
        self
    }
    /// A fake `axl` executable at `rel`, for `current_exe`.
    fn exe(&self, rel: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    state: AppState,
    base: String,
}

fn server_config() -> ServerConfig {
    ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some("test-secret-0123456789".into()),
        _ => None,
    })
    .unwrap()
}

async fn serve_store(store: Arc<dyn Store>, listener: tokio::net::TcpListener) -> Server {
    let state = AppState::new(store, Arc::new(SystemClock), server_config());
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axle::http::serve(
        listener,
        state.clone(),
        std::future::pending(),
    ));
    Server { state, base }
}

async fn server() -> Server {
    let store = Arc::new(SqliteStore::connect("sqlite::memory:").await.unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    serve_store(store, listener).await
}

/// Never answers: the tests that use it must not reach a question.
struct NoPrompt;

impl Prompt for NoPrompt {
    fn choose(&self, question: &str, _: &[&str]) -> io::Result<usize> {
        panic!("unexpected question: {question}")
    }
    fn ask(&self, question: &str) -> io::Result<String> {
        panic!("unexpected question: {question}")
    }
}

/// Scripted answers, recording the questions.
struct Answers {
    choice: usize,
    text: String,
    asked: Mutex<Vec<String>>,
}

impl Prompt for Answers {
    fn choose(&self, question: &str, options: &[&str]) -> io::Result<usize> {
        self.asked
            .lock()
            .unwrap()
            .push(format!("{question} {options:?}"));
        Ok(self.choice)
    }
    fn ask(&self, question: &str) -> io::Result<String> {
        self.asked.lock().unwrap().push(question.to_string());
        Ok(self.text.clone())
    }
}

/// A service manager that records calls and, on install, starts a real in-process server
/// on the unit's port with the unit's database, as launchd would start `axl serve`.
#[derive(Clone, Default)]
struct FakeService {
    calls: Arc<Mutex<Vec<String>>>,
    running: Arc<Mutex<bool>>,
    /// Serve a stand-in whose `/readyz` works and `/v1/whoami` answers 500.
    whoami_fails: bool,
    /// Report the unit as installed but not running.
    stopped: bool,
}

impl ServiceControl for FakeService {
    fn install(&self, spec: &UnitSpec) -> Result<(), DaemonError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("install {}", spec.port));
        let mut running = self.running.lock().unwrap();
        if !*running {
            *running = true;
            let db = format!("sqlite://{}/.axl/axl.db", spec.home.display());
            let port = spec.port;
            if self.whoami_fails {
                tokio::spawn(async move {
                    use axum::http::StatusCode;
                    let app = axum::Router::new()
                        .route("/readyz", axum::routing::get(|| async { "ok" }))
                        .route(
                            "/v1/whoami",
                            axum::routing::get(|| async {
                                (StatusCode::INTERNAL_SERVER_ERROR, "boom")
                            }),
                        );
                    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                        .await
                        .unwrap();
                    axum::serve(listener, app).await.unwrap();
                });
                return Ok(());
            }
            tokio::spawn(async move {
                let store = Arc::new(SqliteStore::connect(&db).await.unwrap());
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                    .await
                    .unwrap();
                let _ = serve_store(store, listener).await;
            });
        }
        Ok(())
    }
    fn uninstall(&self) -> Result<(), DaemonError> {
        self.calls.lock().unwrap().push("uninstall".into());
        Ok(())
    }
    fn start(&self) -> Result<(), DaemonError> {
        self.calls.lock().unwrap().push("start".into());
        Ok(())
    }
    fn stop(&self) -> Result<(), DaemonError> {
        self.calls.lock().unwrap().push("stop".into());
        Ok(())
    }
    fn restart(&self) -> Result<(), DaemonError> {
        self.calls.lock().unwrap().push("restart".into());
        Ok(())
    }
    fn status(&self) -> Result<ServiceState, DaemonError> {
        if self.stopped {
            return Ok(ServiceState::Stopped);
        }
        Ok(if *self.running.lock().unwrap() {
            ServiceState::Running
        } else {
            ServiceState::NotInstalled
        })
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn args() -> InstallArgs {
    InstallArgs {
        server: None,
        code: None,
        harness: Vec::new(),
        local: false,
        dry_run: false,
        yes: true,
        force: false,
        port: None,
    }
}

fn hooks() -> LoginHooks {
    LoginHooks {
        open: Box::new(|url| panic!("tests never open a browser ({url})")),
        interval: Some(Duration::from_millis(600)),
    }
}

const T1: OffsetDateTime = datetime!(2026-11-27 10:00:00 UTC);
const T2: OffsetDateTime = datetime!(2026-11-27 11:00:00 UTC);

/// Runs `install` with this home, a fake `axl` at `~/bin/axl`, no PATH, and `service`.
async fn run(
    home: &TempHome,
    a: InstallArgs,
    prompt: &dyn Prompt,
    interactive: bool,
    service: &FakeService,
    now: OffsetDateTime,
) -> (Result<InstallReport, axle::client::ClientError>, String) {
    let exe = home.exe("bin/axl");
    run_exe(home, a, prompt, interactive, service, now, exe, hooks()).await
}

#[allow(clippy::too_many_arguments)]
async fn run_exe(
    home: &TempHome,
    a: InstallArgs,
    prompt: &dyn Prompt,
    interactive: bool,
    service: &FakeService,
    now: OffsetDateTime,
    exe: PathBuf,
    login_hooks: LoginHooks,
) -> (Result<InstallReport, axle::client::ClientError>, String) {
    let mut out = Vec::new();
    let svc = service.clone();
    let factory =
        move || -> Result<Box<dyn ServiceControl>, NoServiceManager> { Ok(Box::new(svc.clone())) };
    let result = {
        let mut env = InstallEnv {
            home: home.path().to_path_buf(),
            path_env: Some(OsString::new()),
            current_exe: exe,
            interactive,
            prompt,
            out: &mut out,
            service: &factory,
            login_hooks,
            now,
        };
        install(&mut env, a).await
    };
    (result, String::from_utf8(out).unwrap())
}

fn row(report: &InstallReport, h: Harness) -> &HarnessRow {
    report
        .rows
        .iter()
        .find(|r| r.harness == h)
        .unwrap_or_else(|| panic!("no row for {h:?}: {:?}", report.rows))
}

/// Logs `home` in to `srv` with a fresh code, as `axl login` would.
async fn connect(home: &TempHome, srv: &Server) {
    let issued = srv.state.enroll.issue_code("test").await.unwrap();
    login(
        home.path(),
        LoginArgs {
            url: Some(srv.base.clone()),
            code: Some(issued.code),
            no_browser: true,
            force: false,
        },
        &mut Vec::new(),
    )
    .await
    .unwrap();
}

/// Every entry under `root`: relative path -> (mode, bytes or link target).
fn snapshot(root: &Path) -> BTreeMap<String, (u32, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (u32, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            let mode = meta.permissions().mode();
            if meta.file_type().is_symlink() {
                let t = std::fs::read_link(&p).unwrap();
                out.insert(rel, (mode, t.as_os_str().as_encoded_bytes().to_vec()));
            } else if meta.is_dir() {
                out.insert(rel, (mode, Vec::new()));
                walk(root, &p, out);
            } else {
                out.insert(rel, (mode, std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn backups(root: &Path) -> Vec<String> {
    snapshot(root)
        .into_keys()
        .filter(|k| k.contains(".axl-backup-"))
        .collect()
}

#[tokio::test]
async fn install_with_server_and_code_logs_in_then_configures() {
    let srv = server().await;
    let home = TempHome::new();
    home.mkdir(".claude");
    let issued = srv.state.enroll.issue_code("laptop").await.unwrap();
    let service = FakeService::default();
    let (report, out) = run(
        &home,
        InstallArgs {
            server: Some(srv.base.clone()),
            code: Some(issued.code),
            ..args()
        },
        &NoPrompt,
        false,
        &service,
        T1,
    )
    .await;
    let report = report.unwrap();
    let dir = AxlDir::new(home.path());
    let token = std::fs::read_to_string(dir.token_path()).unwrap();
    assert!(token.starts_with("axl_"));
    assert!(!out.contains(token.trim()), "token leaked: {out}");
    assert!(out.contains(&format!("Connected to {}", srv.base)), "{out}");
    assert_eq!(
        load_config(&dir).unwrap().server_url.as_deref(),
        Some(srv.base.as_str())
    );
    assert_eq!(
        row(&report, Harness::ClaudeCode).outcome,
        Outcome::Configured
    );
    assert_eq!(row(&report, Harness::Codex).outcome, Outcome::NotFound);
    assert_eq!(row(&report, Harness::Opencode).outcome, Outcome::NotFound);
    assert_eq!(
        row(&report, Harness::ClaudeCode).next_step.as_deref(),
        Some("restart Claude Code")
    );
    assert!(report.server_warning.is_none());
    assert_eq!(report.exit_code(), ExitCode::SUCCESS);
    assert!(service.calls.lock().unwrap().is_empty());
    assert!(
        std::fs::read_link(home.path().join(".claude/skills/axle")).is_ok(),
        "plugin not linked"
    );
    let mode = std::fs::metadata(&dir.root).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert!(out.contains("Harness"), "{out}");
    assert!(out.contains("Next step"), "{out}");
}

#[tokio::test]
async fn explicit_code_switches_servers_and_says_so() {
    let srv = server().await;
    let other = server().await;
    let home = TempHome::new();
    home.mkdir(".claude");
    connect(&home, &srv).await;
    let issued = other.state.enroll.issue_code("x").await.unwrap();
    let (report, out) = run(
        &home,
        InstallArgs {
            server: Some(other.base.clone()),
            code: Some(issued.code),
            ..args()
        },
        &NoPrompt,
        false,
        &FakeService::default(),
        T1,
    )
    .await;
    report.unwrap();
    assert!(
        out.contains(&format!("Switching from {} to {}", srv.base, other.base)),
        "{out}"
    );
    let cfg = load_config(&AxlDir::new(home.path())).unwrap();
    assert_eq!(cfg.server_url.as_deref(), Some(other.base.as_str()));
    assert!(home.path().join(".claude/skills/axle").exists());
}

#[tokio::test]
async fn revoked_saved_token_does_not_block_a_fresh_code() {
    let srv = server().await;
    let home = TempHome::new();
    connect(&home, &srv).await;
    let dir = AxlDir::new(home.path());
    let old = std::fs::read_to_string(dir.token_path()).unwrap();
    for t in srv.state.auth.list().await.unwrap() {
        srv.state.auth.revoke(t.id).await.unwrap();
    }
    let issued = srv.state.enroll.issue_code("again").await.unwrap();
    let (report, out) = run(
        &home,
        InstallArgs {
            server: Some(srv.base.clone()),
            code: Some(issued.code),
            ..args()
        },
        &NoPrompt,
        false,
        &FakeService::default(),
        T1,
    )
    .await;
    report.unwrap();
    let new = std::fs::read_to_string(dir.token_path()).unwrap();
    assert_ne!(new, old, "token not replaced: {out}");
    assert!(
        axle::client::Api::new(&srv.base)
            .whoami(new.trim())
            .await
            .is_ok(),
        "new token rejected: {out}"
    );
}

#[tokio::test]
async fn valid_saved_token_leaves_the_code_unused() {
    let srv = server().await;
    let home = TempHome::new();
    connect(&home, &srv).await;
    let dir = AxlDir::new(home.path());
    let old = std::fs::read_to_string(dir.token_path()).unwrap();
    let issued = srv.state.enroll.issue_code("spare").await.unwrap();
    let (report, out) = run(
        &home,
        InstallArgs {
            server: Some(srv.base.clone()),
            code: Some(issued.code.clone()),
            ..args()
        },
        &NoPrompt,
        false,
        &FakeService::default(),
        T1,
    )
    .await;
    report.unwrap();
    assert!(
        out.contains("Already connected") && out.contains("the code was not used"),
        "{out}"
    );
    assert_eq!(std::fs::read_to_string(dir.token_path()).unwrap(), old);
    let redeemed = srv.state.enroll.exchange(&issued.code, "x", None).await;
    assert!(redeemed.unwrap().is_some(), "the code was consumed");
}

#[tokio::test]
async fn interactive_switch_without_code_asks_and_declines_by_default() {
    let srv = server().await;
    let other = server().await;
    let home = TempHome::new();
    home.mkdir(".claude");
    connect(&home, &srv).await;
    let prompt = Answers {
        choice: 0,
        text: String::new(),
        asked: Mutex::new(Vec::new()),
    };
    let (report, _) = run(
        &home,
        InstallArgs {
            server: Some(other.base.clone()),
            yes: false,
            ..args()
        },
        &prompt,
        true,
        &FakeService::default(),
        T1,
    )
    .await;
    let asked = prompt.asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        [format!(
            "This machine is connected to {}. Switch to {}? [y/N]",
            srv.base, other.base
        )]
    );
    assert!(report.is_err(), "an empty answer means no");
    let cfg = load_config(&AxlDir::new(home.path())).unwrap();
    assert_eq!(cfg.server_url.as_deref(), Some(srv.base.as_str()));
    assert!(!home.path().join(".claude/skills/axle").exists());
}

#[tokio::test]
async fn non_interactive_switch_without_code_names_the_force_command() {
    let srv = server().await;
    let other = server().await;
    let home = TempHome::new();
    home.mkdir(".claude");
    connect(&home, &srv).await;
    let (report, _) = run(
        &home,
        InstallArgs {
            server: Some(other.base.clone()),
            ..args()
        },
        &NoPrompt,
        false,
        &FakeService::default(),
        T1,
    )
    .await;
    let err = report.unwrap_err().to_string();
    assert!(
        err.contains(&format!("axl install --server {} --force", other.base)),
        "{err}"
    );
    assert!(!home.path().join(".claude/skills/axle").exists());
}

#[tokio::test]
async fn force_flag_switches_servers() {
    let srv = server().await;
    let other = server().await;
    let home = TempHome::new();
    home.mkdir(".claude");
    connect(&home, &srv).await;
    let issued = other.state.enroll.issue_code("x").await.unwrap();
    let (report, out) = run(
        &home,
        InstallArgs {
            server: Some(other.base.clone()),
            code: Some(issued.code),
            force: true,
            ..args()
        },
        &NoPrompt,
        false,
        &FakeService::default(),
        T1,
    )
    .await;
    report.unwrap();
    assert!(out.contains("Switching from"), "{out}");
}

#[tokio::test]
async fn install_port_goes_to_the_service_and_busy_port_is_reported() {
    let home = TempHome::new();
    home.mkdir(".claude");
    let port = free_port();
    let service = FakeService::default();
    let (report, _) = run(
        &home,
        InstallArgs {
            port: Some(port),
            ..args()
        },
        &NoPrompt,
        false,
        &service,
        T1,
    )
    .await;
    report.unwrap();
    assert_eq!(
        service.calls.lock().unwrap().as_slice(),
        [format!("install {port}")]
    );
    let cfg = load_config(&AxlDir::new(home.path())).unwrap();
    assert_eq!(cfg.port, Some(port));

    let busy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let busy_port = busy.local_addr().unwrap().port();
    let home2 = TempHome::new();
    home2.mkdir(".claude");
    let (report, _) = run(
        &home2,
        InstallArgs {
            port: Some(busy_port),
            ..args()
        },
        &NoPrompt,
        false,
        &FakeService::default(),
        T1,
    )
    .await;
    let err = report.unwrap_err().to_string();
    assert!(err.contains("in use") && err.contains("--port"), "{err}");
}

#[tokio::test]
async fn unreachable_server_still_configures_with_warning() {
    let home = TempHome::new();
    home.mkdir(".claude");
    let dir = AxlDir::new(home.path());
    let url = format!("http://127.0.0.1:{}", free_port());
    save_config(
        &dir,
        &ClientConfig {
            server_url: Some(url.clone()),
            port: None,
            ..ClientConfig::default()
        },
    )
    .unwrap();
    write_token(&dir, "axl_not-a-real-token").unwrap();
    let service = FakeService::default();
    let (report, out) = run(&home, args(), &NoPrompt, true, &service, T1).await;
    let report = report.unwrap();
    let warning = report.server_warning.clone().expect("a warning");
    assert!(warning.contains(&url), "{warning}");
    assert!(warning.contains("axl status"), "{warning}");
    assert!(out.contains(&warning), "{out}");
    assert_eq!(
        row(&report, Harness::ClaudeCode).outcome,
        Outcome::Configured
    );
    assert!(service.calls.lock().unwrap().is_empty(), "went local");
    assert_eq!(
        load_config(&dir).unwrap().server_url.as_deref(),
        Some(url.as_str())
    );
    assert_eq!(report.exit_code(), ExitCode::SUCCESS);
}

#[tokio::test]
async fn no_server_non_interactive_goes_local() {
    let home = TempHome::new();
    home.mkdir(".claude");
    let dir = AxlDir::new(home.path());
    let port = free_port();
    // Saved by an earlier `axl service install --port`; must survive.
    save_config(
        &dir,
        &ClientConfig {
            server_url: None,
            port: Some(port),
            ..ClientConfig::default()
        },
    )
    .unwrap();
    std::fs::remove_file(dir.config_path()).ok();
    std::fs::write(dir.config_path(), format!("port = {port}\n")).unwrap();
    let service = FakeService::default();
    let (report, out) = run(&home, args(), &NoPrompt, true, &service, T1).await;
    let report = report.unwrap();
    assert_eq!(
        service.calls.lock().unwrap().as_slice(),
        [format!("install {port}")]
    );
    let url = format!("http://127.0.0.1:{port}");
    let cfg = load_config(&dir).unwrap();
    assert_eq!(cfg.port, Some(port));
    assert_eq!(cfg.server_url.as_deref(), Some(url.as_str()));
    let token = std::fs::read_to_string(dir.token_path()).unwrap();
    assert!(!out.contains(token.trim()), "token leaked: {out}");
    let who = Api::new(&url).whoami(token.trim()).await.unwrap();
    assert_eq!(who.label, format!("{} (local)", short_hostname()));
    assert_eq!(
        row(&report, Harness::ClaudeCode).outcome,
        Outcome::Configured
    );
    let mode = std::fs::metadata(&dir.root).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    for f in [dir.token_path(), dir.config_path()] {
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", f.display());
    }

    // A re-run reuses the running service and the token.
    let (again, _) = run(&home, args(), &NoPrompt, false, &service, T2).await;
    let again = again.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.token_path()).unwrap(),
        token,
        "minted a second token"
    );
    assert_eq!(
        row(&again, Harness::ClaudeCode).outcome,
        Outcome::AlreadyConfigured
    );
}

#[tokio::test]
async fn interactive_prompt_server_branch_runs_device_login() {
    let srv = server().await;
    let home = TempHome::new();
    home.mkdir(".codex");
    let prompt = Answers {
        choice: 1,
        text: srv.base.clone(),
        asked: Mutex::new(Vec::new()),
    };
    let opened = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorder = opened.clone();
    let login_hooks = LoginHooks {
        open: Box::new(move |u| recorder.lock().unwrap().push(u.to_string())),
        interval: Some(Duration::from_millis(600)),
    };
    let service = FakeService::default();
    let approve = async {
        loop {
            if let Some(e) = srv.state.enroll.pending().await.unwrap().first() {
                assert!(srv.state.enroll.decide(e.id, true).await.unwrap());
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let exe = home.exe("bin/axl");
    let ((report, out), ()) = tokio::join!(
        run_exe(
            &home,
            InstallArgs {
                yes: false,
                ..args()
            },
            &prompt,
            true,
            &service,
            T1,
            exe,
            login_hooks,
        ),
        approve
    );
    let report = report.unwrap();
    let asked = prompt.asked.lock().unwrap().clone();
    assert!(
        asked[0].contains("Run Axle on this machine, or connect to a server?"),
        "{asked:?}"
    );
    assert!(asked[0].contains("This machine") && asked[0].contains("A server"));
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(
        opened.lock().unwrap().as_slice(),
        [format!("{}/ui/connect", srv.base)]
    );
    assert!(out.contains("Connected to"), "{out}");
    assert!(!out.contains("axld_"), "device code leaked: {out}");
    let dir = AxlDir::new(home.path());
    assert!(dir.token_path().exists());
    assert!(service.calls.lock().unwrap().is_empty());
    assert_eq!(row(&report, Harness::Codex).outcome, Outcome::Configured);
    assert_eq!(
        row(&report, Harness::Codex).next_step.as_deref(),
        Some("restart Codex and trust the new hooks")
    );
}

#[tokio::test]
async fn dry_run_writes_nothing() {
    let srv = server().await;
    let home = TempHome::new();
    home.mkdir(".claude")
        .write(".claude/settings.json", "{\n  \"theme\": \"dark\"\n}\n")
        .write(".codex/config.toml", "model = \"o3\"\n")
        .write(".config/opencode/opencode.json", "{}\n");
    home.exe("bin/axl");
    let before = snapshot(home.path());
    let service = FakeService::default();

    // Nothing configured: would go local.
    let (report, out) = run(
        &home,
        InstallArgs {
            dry_run: true,
            ..args()
        },
        &NoPrompt,
        false,
        &service,
        T1,
    )
    .await;
    let report = report.unwrap();
    assert!(out.contains("would install the local service"), "{out}");
    assert!(out.contains("+++"), "{out}");
    assert!(service.calls.lock().unwrap().is_empty());
    assert_eq!(report.exit_code(), ExitCode::SUCCESS);
    assert_eq!(snapshot(home.path()), before, "dry run wrote something");

    // A server and a code: would log in, and the code stays unused.
    let issued = srv.state.enroll.issue_code("x").await.unwrap();
    let (report, out) = run(
        &home,
        InstallArgs {
            server: Some(srv.base.clone()),
            code: Some(issued.code.clone()),
            dry_run: true,
            ..args()
        },
        &NoPrompt,
        false,
        &service,
        T1,
    )
    .await;
    report.unwrap();
    assert!(
        out.contains(&format!("would log in to {}", srv.base)),
        "{out}"
    );
    assert!(out.contains("+++"), "{out}");
    assert_eq!(snapshot(home.path()), before, "dry run wrote something");
    // The code was not burned.
    let redeemed = srv.state.enroll.exchange(&issued.code, "x", None).await;
    assert!(redeemed.unwrap().is_some(), "dry run consumed the code");
}

/// Home with all three harnesses and existing files to back up, logged in to `srv`.
async fn full_home(srv: &Server) -> TempHome {
    let home = TempHome::new();
    home.write(".claude/settings.json", "{\n  \"theme\": \"dark\"\n}\n")
        .write(".claude.json", "{\n  \"numStartups\": 3\n}\n")
        .write(".codex/config.toml", "model = \"o3\"\n")
        .write(".codex/AGENTS.md", "# Mine\n")
        .write(
            ".config/opencode/opencode.json",
            "{\n  \"theme\": \"x\"\n}\n",
        );
    connect(&home, srv).await;
    home
}

#[tokio::test]
async fn rerun_is_noop() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let service = FakeService::default();
    let (first, _) = run(&home, args(), &NoPrompt, false, &service, T1).await;
    let first = first.unwrap();
    for h in Harness::ALL {
        assert_eq!(row(&first, h).outcome, Outcome::Configured, "{h:?}");
    }
    let tree = snapshot(home.path());
    let backups_before = backups(home.path());
    assert!(!backups_before.is_empty());

    let (second, out) = run(&home, args(), &NoPrompt, false, &service, T2).await;
    let second = second.unwrap();
    for h in Harness::ALL {
        assert_eq!(
            row(&second, h).outcome,
            Outcome::AlreadyConfigured,
            "{h:?}: {out}"
        );
        assert_eq!(row(&second, h).next_step, None);
    }
    assert_eq!(backups(home.path()), backups_before, "new backups");
    assert_eq!(snapshot(home.path()), tree, "a re-run changed files");
    assert_eq!(second.exit_code(), ExitCode::SUCCESS);
    assert!(service.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rerun_rewrites_moved_binary() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let service = FakeService::default();
    let old = home.exe("old/axl");
    let new = home.exe("new/axl");
    let (first, _) = run_exe(
        &home,
        args(),
        &NoPrompt,
        false,
        &service,
        T1,
        old.clone(),
        hooks(),
    )
    .await;
    first.unwrap();
    let tree1 = snapshot(home.path());

    let (second, _) = run_exe(
        &home,
        args(),
        &NoPrompt,
        false,
        &service,
        T2,
        new.clone(),
        hooks(),
    )
    .await;
    let second = second.unwrap();
    for h in Harness::ALL {
        assert_eq!(row(&second, h).outcome, Outcome::Configured, "{h:?}");
    }
    let tree2 = snapshot(home.path());
    let old_s = old.display().to_string();
    let new_s = new.display().to_string();
    let mut changed: Vec<String> = Vec::new();
    for (rel, (_, bytes)) in &tree2 {
        if rel.contains(".axl-backup-") || rel.ends_with("install-manifest.json") {
            continue;
        }
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains(&old_s), "{rel} still names the old binary");
        if let Some((_, b1)) = tree1.get(rel)
            && b1 != bytes
            && !b1.is_empty()
        {
            changed.push(rel.clone());
        }
    }
    assert!(
        tree2
            .values()
            .any(|(_, b)| String::from_utf8_lossy(b).contains(&new_s))
    );
    let stamp2 = ".axl-backup-20261127T110000Z";
    let mut new_backups: Vec<String> = backups(home.path())
        .into_iter()
        .filter(|b| b.ends_with(stamp2))
        .map(|b| b.trim_end_matches(stamp2).to_string())
        .collect();
    new_backups.sort();
    changed.retain(|c| {
        !std::fs::symlink_metadata(home.path().join(c))
            .unwrap()
            .is_dir()
    });
    changed.sort();
    assert_eq!(new_backups, changed, "one backup per changed file");
}

#[tokio::test]
async fn no_harness_found_exits_1() {
    let srv = server().await;
    let home = TempHome::new();
    connect(&home, &srv).await;
    let (report, out) = run(&home, args(), &NoPrompt, false, &FakeService::default(), T1).await;
    let report = report.unwrap();
    for h in Harness::ALL {
        assert_eq!(row(&report, h).outcome, Outcome::NotFound);
    }
    assert!(out.contains("not found"), "{out}");
    assert_eq!(report.exit_code(), ExitCode::FAILURE);
}

#[tokio::test]
async fn harness_filter_limits_run() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let claude_before = snapshot(&home.path().join(".claude"));
    let (report, _) = run(
        &home,
        InstallArgs {
            harness: vec![Harness::Codex],
            ..args()
        },
        &NoPrompt,
        false,
        &FakeService::default(),
        T1,
    )
    .await;
    let report = report.unwrap();
    assert_eq!(report.rows.len(), 1, "{:?}", report.rows);
    assert_eq!(row(&report, Harness::Codex).outcome, Outcome::Configured);
    assert_eq!(snapshot(&home.path().join(".claude")), claude_before);
    assert!(!home.path().join(".config/opencode/AGENTS.md").exists());
}

#[tokio::test]
async fn needs_manual_harness_does_not_stop_the_others() {
    let srv = server().await;
    let home = full_home(&srv).await;
    // An orphan start marker: the Codex editor cannot place its block.
    home.write(".codex/AGENTS.md", "# Mine\n# >>> axle >>>\nhalf a block\n");
    let (report, out) = run(&home, args(), &NoPrompt, false, &FakeService::default(), T1).await;
    let report = report.unwrap();
    assert!(
        matches!(
            row(&report, Harness::Codex).outcome,
            Outcome::NeedsManual(_)
        ),
        "{:?}",
        report.rows
    );
    assert!(out.contains("needs manual step"), "{out}");
    assert_eq!(
        row(&report, Harness::ClaudeCode).outcome,
        Outcome::Configured
    );
    assert_eq!(row(&report, Harness::Opencode).outcome, Outcome::Configured);
    assert_eq!(report.exit_code(), ExitCode::SUCCESS);
}

#[tokio::test]
async fn corrupt_manifest_stops_before_writing() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let dir = AxlDir::new(home.path());
    std::fs::write(dir.manifest_path(), "{ not json").unwrap();
    home.exe("bin/axl");
    let before = snapshot(home.path());
    let (report, _) = run(&home, args(), &NoPrompt, false, &FakeService::default(), T1).await;
    let err = report.unwrap_err().to_string();
    assert!(err.contains("install-manifest.json"), "{err}");
    assert_eq!(snapshot(home.path()), before);
}

#[tokio::test]
async fn adopted_entries_are_listed() {
    let srv = server().await;
    let home = full_home(&srv).await;
    home.write(
        ".claude.json",
        "{\n  \"mcpServers\": {\n    \"axle\": {\n      \"command\": \"axl\",\n      \"args\": [\"bridge\"],\n      \"env\": { \"AXL_SERVER_URL\": \"http://old:7420\" }\n    }\n  }\n}\n",
    );
    let (report, out) = run(&home, args(), &NoPrompt, false, &FakeService::default(), T1).await;
    report.unwrap();
    assert!(
        out.contains("adopted hand-made mcpServers.axle in"),
        "{out}"
    );
}

#[tokio::test]
async fn local_token_check_failure_does_not_mint() {
    let home = TempHome::new();
    home.mkdir(".claude");
    let dir = AxlDir::new(home.path());
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    save_config(
        &dir,
        &ClientConfig {
            server_url: Some(url.clone()),
            port: Some(port),
            ..ClientConfig::default()
        },
    )
    .unwrap();
    write_token(&dir, "axl_existing").unwrap();
    let service = FakeService {
        whoami_fails: true,
        ..FakeService::default()
    };
    let (report, _) = run(
        &home,
        InstallArgs {
            local: true,
            ..args()
        },
        &NoPrompt,
        false,
        &service,
        T1,
    )
    .await;
    let err = report.unwrap_err().to_string();
    assert!(err.contains("cannot check this machine's token"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dir.token_path()).unwrap(),
        "axl_existing"
    );
    // No database was opened to mint into.
    assert!(!dir.db_path().exists());
}

#[tokio::test]
async fn failed_apply_still_records_what_landed() {
    let srv = server().await;
    let home = full_home(&srv).await;
    // Codex's directory cannot be written: its backup and edit fail, after Claude Code's.
    let codex = home.path().join(".codex");
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o555)).unwrap();
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    let _restore = Restore(codex.clone());
    let (report, _) = run(&home, args(), &NoPrompt, false, &FakeService::default(), T1).await;
    let err = report.unwrap_err().to_string();
    assert!(err.contains("Codex"), "{err}");
    assert!(err.contains("install-manifest.json"), "{err}");

    let dir = AxlDir::new(home.path());
    let manifest = axle::install::Manifest::load(&dir).unwrap();
    assert!(
        manifest
            .entries
            .iter()
            .any(|e| e.harness == Harness::ClaudeCode && e.kind == axle::install::EntryKind::Link),
        "{:?}",
        manifest.entries
    );
    assert!(
        manifest
            .entries
            .iter()
            .all(|e| e.harness != Harness::Codex || !e.file.starts_with(&codex)),
        "recorded Codex edits that never landed: {:?}",
        manifest.entries
    );
    let link = home.path().join(".claude/skills/axle");
    assert!(std::fs::read_link(&link).is_ok());

    // Uninstall finds what was recorded and removes it.
    let plan = axle::install::claude_code::plan_uninstall(home.path(), &manifest).unwrap();
    axle::install::apply(&plan, T2).unwrap();
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "link left behind"
    );
    assert!(
        !dir.integrations_dir()
            .join("claude-code/.mcp.json")
            .exists()
    );
}

// ---- end to end, real binary ----

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn axl(home: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_axl"));
    cmd.env("HOME", home).env("PATH", "/usr/bin:/bin");
    for k in [
        "AXL_SERVER_URL",
        "AXL_TOKEN",
        "AXL_UI_AUTH",
        "AXL_ADMIN_SECRET",
        "AXL_TRUSTED_HEADER",
        "AXL_PUBLIC_URL",
        "AXL_ALLOWED_HOSTS",
        "AXL_BIND",
        "DATABASE_URL",
    ] {
        cmd.env_remove(k);
    }
    cmd
}

#[test]
fn e2e_serve_login_install_dry_run() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let srv_dir = TempHome::new();
    let home = TempHome::new();
    home.mkdir(".claude");
    let db = format!("sqlite://{}/server.db", srv_dir.path().display());
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let _server = Child(
        axl(srv_dir.path())
            .args(["serve", "--bind", &format!("127.0.0.1:{port}")])
            .env("DATABASE_URL", &db)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    rt.block_on(async {
        for _ in 0..200 {
            if Api::new(&base).readyz().await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("axl serve did not become ready");
    });
    let code = rt.block_on(async {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::connect(&db).await.unwrap());
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let bus = axle::events::EventBus::new(store.clone(), clock.clone());
        axle::service::enroll::EnrollService::new(store, clock, bus)
            .issue_code("e2e")
            .await
            .unwrap()
            .code
    });

    let check = |out: &std::process::Output, what: &str| -> String {
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "{what} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(!stdout.contains("axl_"), "{what} leaked a token: {stdout}");
        assert!(!stderr.contains("axl_"), "{what} leaked a token: {stderr}");
        stdout
    };

    let login = axl(home.path())
        .args(["login", &base, "--code", &code])
        .output()
        .unwrap();
    let out = check(&login, "login");
    assert!(out.contains(&format!("Connected to {base}")), "{out}");
    let dir = AxlDir::new(home.path());
    assert!(dir.token_path().is_file());
    assert!(dir.config_path().is_file());

    let claude_before = snapshot(&home.path().join(".claude"));
    let home_before = snapshot(home.path());
    let dry = axl(home.path())
        .args(["install", "--dry-run", "--yes"])
        .output()
        .unwrap();
    let out = check(&dry, "install --dry-run");
    assert!(out.contains("+++"), "{out}");
    assert_eq!(snapshot(&home.path().join(".claude")), claude_before);
    assert_eq!(snapshot(home.path()), home_before);

    let real = axl(home.path())
        .args(["install", "--yes"])
        .output()
        .unwrap();
    let out = check(&real, "install");
    assert!(out.contains("Claude Code"), "{out}");
    let link = std::fs::read_link(home.path().join(".claude/skills/axle")).unwrap();
    assert_eq!(link, dir.integrations_dir().join("claude-code"));
    let mcp =
        std::fs::read_to_string(dir.integrations_dir().join("claude-code/.mcp.json")).unwrap();
    assert!(mcp.contains(env!("CARGO_BIN_EXE_axl")), "{mcp}");
}

#[tokio::test]
async fn adopted_server_url_from_first_harness_wins() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    // A token from the old docs' setup, and no server_url in config.toml yet.
    write_token(&dir, "axl_not-a-real-token").unwrap();
    home.write(
        ".claude.json",
        "{\n  \"mcpServers\": {\n    \"axle\": {\n      \"command\": \"axl\",\n      \"args\": [\"bridge\"],\n      \"env\": { \"AXL_SERVER_URL\": \"http://claude-host:7420\" }\n    }\n  }\n}\n",
    )
    .mkdir(".claude")
    .write(
        ".codex/config.toml",
        "[mcp_servers.axle]\ncommand = \"axl\"\nargs = [\"bridge\"]\nenv = { AXL_SERVER_URL = \"http://codex-host:7420\" }\n",
    );
    let (report, out) = run(&home, args(), &NoPrompt, false, &FakeService::default(), T1).await;
    let report = report.unwrap();
    assert_eq!(
        row(&report, Harness::ClaudeCode).outcome,
        Outcome::Configured
    );
    assert_eq!(row(&report, Harness::Codex).outcome, Outcome::Configured);
    assert_eq!(
        load_config(&dir).unwrap().server_url.as_deref(),
        Some("http://claude-host:7420"),
        "{out}"
    );
    let manifest = std::fs::read_to_string(dir.manifest_path()).unwrap();
    let m: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    let url_entries: Vec<&serde_json::Value> = m["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["file"].as_str().unwrap().ends_with(".axl/config.toml"))
        .collect();
    assert_eq!(url_entries.len(), 1, "{manifest}");
    assert_eq!(url_entries[0]["harness"], "claude-code");
}

#[test]
fn e2e_hand_made_token_is_never_printed() {
    let home = TempHome::new();
    let secret = format!("axl_{}", "0123456789abcdef".repeat(4));
    home.write(
        ".claude.json",
        &format!(
            "{{\n  \"mcpServers\": {{\n    \"axle\": {{\n      \"command\": \"axl\",\n      \"args\": [\"bridge\"],\n      \"env\": {{ \"AXL_SERVER_URL\": \"http://127.0.0.1:1\", \"AXL_TOKEN\": \"{secret}\" }}\n    }}\n  }}\n}}\n"
        ),
    )
    .mkdir(".claude")
    .write(
        ".codex/config.toml",
        &format!(
            "[mcp_servers.axle]\ncommand = \"axl\"\nargs = [\"bridge\"]\nenv = {{ AXL_SERVER_URL = \"http://127.0.0.1:1\", AXL_TOKEN = \"{secret}\" }}\n"
        ),
    );
    // Configured but unreachable: no login, no local service.
    let dir = AxlDir::new(home.path());
    save_config(
        &dir,
        &ClientConfig {
            server_url: Some("http://127.0.0.1:1".into()),
            port: None,
            ..ClientConfig::default()
        },
    )
    .unwrap();
    write_token(&dir, &secret).unwrap();
    for argv in [
        &["install", "--dry-run", "--yes"][..],
        &["install", "--yes"][..],
    ] {
        let out = axl(home.path()).args(argv).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{argv:?}\n{stdout}\n{stderr}");
        assert!(
            !stdout.contains("axl_"),
            "{argv:?} printed a token:\n{stdout}"
        );
        assert!(
            !stderr.contains("axl_"),
            "{argv:?} printed a token:\n{stderr}"
        );
        assert!(stdout.contains("<redacted>") || argv.len() == 2, "{stdout}");
    }
}

// ---- axl uninstall ----

fn uninstall_args() -> UninstallArgs {
    UninstallArgs {
        harness: Vec::new(),
        service: false,
        purge: false,
        revoke: false,
    }
}

async fn run_uninstall(
    home: &TempHome,
    a: UninstallArgs,
    service: &FakeService,
    now: OffsetDateTime,
) -> (Result<(), axle::client::ClientError>, String) {
    let mut out = Vec::new();
    let svc = service.clone();
    let factory =
        move || -> Result<Box<dyn ServiceControl>, NoServiceManager> { Ok(Box::new(svc.clone())) };
    let result = {
        let mut env = InstallEnv {
            home: home.path().to_path_buf(),
            path_env: Some(OsString::new()),
            current_exe: home.exe("bin/axl"),
            interactive: false,
            prompt: &NoPrompt,
            out: &mut out,
            service: &factory,
            login_hooks: hooks(),
            now,
        };
        uninstall(&mut env, a).await
    };
    (result, String::from_utf8(out).unwrap())
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).unwrap();
        }
    }
}

/// A home holding the `before` trees of the three harnesses' `variant` fixtures.
fn fixture_home(variant: &str) -> TempHome {
    let home = TempHome::new();
    for h in ["claude-code", "codex", "opencode"] {
        let before = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/harness")
            .join(h)
            .join(variant)
            .join("before");
        copy_tree(&before, home.path());
    }
    // Claude Code's `empty` fixture is only a .gitkeep: give it its config directory.
    home.mkdir(".claude");
    // The fake `axl` install runs from.
    home.exe("bin/axl");
    home
}

/// The tree minus Axle's own directory and the backups it made.
fn user_tree(root: &Path) -> BTreeMap<String, (u32, Vec<u8>)> {
    snapshot(root)
        .into_iter()
        .filter(|(k, _)| !k.contains(".axl-backup-") && k != ".axl" && !k.starts_with(".axl/"))
        .collect()
}

#[tokio::test]
async fn install_then_uninstall_restores_tree() {
    for variant in ["unrelated", "empty"] {
        let srv = server().await;
        let home = fixture_home(variant);
        connect(&home, &srv).await;
        let before = user_tree(home.path());
        let service = FakeService::default();
        let (installed, out) = run(&home, args(), &NoPrompt, false, &service, T1).await;
        installed.unwrap();
        assert_ne!(
            user_tree(home.path()),
            before,
            "{variant}: install did nothing: {out}"
        );

        let (result, out) = run_uninstall(&home, uninstall_args(), &service, T2).await;
        result.unwrap();
        assert_eq!(user_tree(home.path()), before, "{variant}: {out}");
        // Axle's own tree: no integrations left, no manifest, the login kept.
        let dir = AxlDir::new(home.path());
        assert!(!dir.integrations_dir().exists(), "{variant}");
        assert!(!dir.manifest_path().exists(), "{variant}");
        assert!(
            dir.token_path().exists(),
            "{variant}: token must stay without --revoke"
        );
        assert!(service.calls.lock().unwrap().is_empty(), "{variant}");
    }
}

#[tokio::test]
async fn uninstall_one_harness_keeps_others() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let service = FakeService::default();
    run(&home, args(), &NoPrompt, false, &service, T1)
        .await
        .0
        .unwrap();

    let (result, out) = run_uninstall(
        &home,
        UninstallArgs {
            harness: vec![Harness::Codex],
            ..uninstall_args()
        },
        &service,
        T2,
    )
    .await;
    result.unwrap();
    let codex = std::fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
    assert_eq!(codex, "model = \"o3\"\n", "{out}");
    let opencode =
        std::fs::read_to_string(home.path().join(".config/opencode/opencode.json")).unwrap();
    assert!(opencode.contains("axle"), "opencode lost its entry");
    assert!(
        std::fs::read_link(home.path().join(".claude/skills/axle")).is_ok(),
        "claude code link removed"
    );
    // The manifest keeps only the other harnesses.
    let manifest = std::fs::read_to_string(AxlDir::new(home.path()).manifest_path()).unwrap();
    assert!(!manifest.contains("\"codex\""), "{manifest}");
    assert!(manifest.contains("claude-code") && manifest.contains("opencode"));

    // A second run of the same command is a no-op.
    let tree = snapshot(home.path());
    let (again, _) = run_uninstall(
        &home,
        UninstallArgs {
            harness: vec![Harness::Codex],
            ..uninstall_args()
        },
        &service,
        T2,
    )
    .await;
    again.unwrap();
    assert_eq!(snapshot(home.path()), tree);
}

#[tokio::test]
async fn uninstall_ignores_unrecorded_user_edits() {
    let srv = server().await;
    let home = TempHome::new();
    home.write(
        ".config/opencode/opencode.json",
        "{\n  \"theme\": \"x\"\n}\n",
    );
    connect(&home, &srv).await;
    let service = FakeService::default();
    run(&home, args(), &NoPrompt, false, &service, T1)
        .await
        .0
        .unwrap();
    // The user adds a server of their own next to `axle`.
    let path = home.path().join(".config/opencode/opencode.json");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"axle\""), "{text}");
    let edited = text.replacen(
        "\"axle\"",
        "\"mine\": {\"type\": \"local\", \"command\": [\"/usr/bin/mine\"]},\n    \"axle\"",
        1,
    );
    std::fs::write(&path, edited).unwrap();

    let (result, out) = run_uninstall(&home, uninstall_args(), &service, T2).await;
    result.unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("/usr/bin/mine"), "{after}\n{out}");
    assert!(after.contains("\"theme\""), "{after}");
    assert!(!after.contains("\"axle\""), "{after}");
}

#[tokio::test]
async fn uninstall_service_keeps_db_without_purge() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let dir = AxlDir::new(home.path());
    home.write(".axl/axl.db", "db")
        .write(".axl/axl.db-wal", "wal")
        .write(".axl/axl.db.bak", "bak");
    let service = FakeService::default();

    let (result, out) = run_uninstall(
        &home,
        UninstallArgs {
            service: true,
            ..uninstall_args()
        },
        &service,
        T2,
    )
    .await;
    result.unwrap();
    assert!(
        service
            .calls
            .lock()
            .unwrap()
            .contains(&"uninstall".to_string())
    );
    assert!(dir.db_path().exists(), "{out}");
    assert!(out.contains("--purge"), "{out}");

    let (result, _) = run_uninstall(
        &home,
        UninstallArgs {
            service: true,
            purge: true,
            ..uninstall_args()
        },
        &service,
        T2,
    )
    .await;
    result.unwrap();
    assert!(!dir.db_path().exists());
    assert!(!dir.root.join("axl.db-wal").exists());
    assert!(
        dir.root.join("axl.db.bak").exists(),
        "purge deleted a user file"
    );
}

#[tokio::test]
async fn purge_without_service_is_refused_before_anything_changes() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let service = FakeService::default();
    run(&home, args(), &NoPrompt, false, &service, T1)
        .await
        .0
        .unwrap();
    let tree = snapshot(home.path());
    let (result, _) = run_uninstall(
        &home,
        UninstallArgs {
            purge: true,
            ..uninstall_args()
        },
        &service,
        T2,
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("--service"));
    assert_eq!(snapshot(home.path()), tree);
}

#[tokio::test]
async fn uninstall_revoke_invalidates_token() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let dir = AxlDir::new(home.path());
    let token = std::fs::read_to_string(dir.token_path()).unwrap();
    let api = Api::new(&srv.base);
    api.whoami(token.trim()).await.unwrap();

    let service = FakeService::default();
    let (result, out) = run_uninstall(
        &home,
        UninstallArgs {
            revoke: true,
            ..uninstall_args()
        },
        &service,
        T2,
    )
    .await;
    result.unwrap();
    assert!(
        api.whoami(token.trim()).await.is_err(),
        "token still valid: {out}"
    );
    assert!(!dir.token_path().exists());
    assert!(!out.contains(token.trim()), "token printed: {out}");
}

#[tokio::test]
async fn corrupt_manifest_stops_uninstall() {
    let srv = server().await;
    let home = full_home(&srv).await;
    home.write(".axl/install-manifest.json", "{ not json");
    let service = FakeService::default();
    let (result, _) = run_uninstall(
        &home,
        UninstallArgs {
            service: true,
            revoke: true,
            ..uninstall_args()
        },
        &service,
        T2,
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("install-manifest.json")
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join(".axl/install-manifest.json")).unwrap(),
        "{ not json"
    );
    assert!(service.calls.lock().unwrap().is_empty());
    assert!(AxlDir::new(home.path()).token_path().exists());
}

// ---- axl status ----

async fn run_status(
    home: &TempHome,
    service: &FakeService,
    exe: PathBuf,
) -> (StatusReport, String) {
    let mut out = Vec::new();
    let svc = service.clone();
    let factory =
        move || -> Result<Box<dyn ServiceControl>, NoServiceManager> { Ok(Box::new(svc.clone())) };
    let env = InstallEnv {
        home: home.path().to_path_buf(),
        path_env: Some(OsString::new()),
        current_exe: exe,
        interactive: false,
        prompt: &NoPrompt,
        out: &mut out,
        service: &factory,
        login_hooks: hooks(),
        now: T2,
    };
    let report = status(&env).await;
    (report.clone(), report.render())
}

fn value_of<'a>(report: &'a StatusReport, label: &str) -> &'a axle::cli::StatusLine {
    report
        .lines
        .iter()
        .find(|l| l.label == label)
        .unwrap_or_else(|| panic!("no {label} line: {:?}", report.lines))
}

#[tokio::test]
async fn status_shows_usage_lines() {
    use axle::client::{UsageConfig, load_config, save_config};
    use axle::domain::Billing;
    use axle::usage::cursor::{Status, save_status};

    let srv = server().await;
    let home = full_home(&srv).await;
    let service = FakeService::default();
    let (installed, _) = run(&home, args(), &NoPrompt, false, &service, T1).await;
    installed.unwrap();

    let (report, text) = run_status(&home, &service, home.exe("bin/axl")).await;
    assert!(report.ok, "{text}");
    assert_eq!(value_of(&report, "usage").value, "on");
    assert_eq!(value_of(&report, "usage report").value, "none sent yet");
    assert_eq!(value_of(&report, "usage billing").value, "not reported yet");
    for l in ["usage", "usage billing", "usage report"] {
        assert_eq!(value_of(&report, l).fix, None);
    }

    let mut status = Status {
        last_success_at: Some("2026-11-28T10:00:00Z".into()),
        ..Status::default()
    };
    status
        .billing
        .insert("claude-code".into(), Billing::Subscription);
    save_status(home.path(), &status).unwrap();
    let dir = AxlDir::new(home.path());
    let mut cfg = load_config(&dir).unwrap();
    let mut usage = UsageConfig::default();
    usage.billing.insert("codex".into(), "api".into());
    cfg.usage = Some(usage.clone());
    save_config(&dir, &cfg).unwrap();
    let (report, text) = run_status(&home, &service, home.exe("bin/axl")).await;
    assert!(report.ok, "{text}");
    assert_eq!(
        value_of(&report, "usage billing").value,
        "claude-code subscription, codex api (from config)"
    );
    assert_eq!(
        value_of(&report, "usage report").value,
        "last sent 2026-11-28T10:00:00Z"
    );

    usage.enabled = Some(false);
    cfg.usage = Some(usage);
    save_config(&dir, &cfg).unwrap();
    let (report, _) = run_status(&home, &service, home.exe("bin/axl")).await;
    assert_eq!(
        value_of(&report, "usage").value,
        "off ([usage] enabled = false)"
    );
    assert!(report.ok);

    // A billing typo is ignored (and said so); it never hides `enabled = false`.
    std::fs::write(
        dir.config_path(),
        "[usage]\nenabled = false\n[usage.billing]\ncodex = \"apii\"\n",
    )
    .unwrap();
    let (report, _) = run_status(&home, &service, home.exe("bin/axl")).await;
    assert_eq!(
        value_of(&report, "usage").value,
        "off ([usage] enabled = false)"
    );
    assert_eq!(
        value_of(&report, "usage billing").value,
        "claude-code subscription, codex \"apii\" (not a billing kind; ignored)"
    );

    // A config that does not parse turns usage off, and status says why.
    std::fs::write(dir.config_path(), "server_url = [not toml\n").unwrap();
    let (report, _) = run_status(&home, &service, home.exe("bin/axl")).await;
    let usage = value_of(&report, "usage");
    assert!(
        usage.value.starts_with("off (config.toml unreadable: "),
        "{}",
        usage.value
    );
    assert!(usage.fix.is_some());
}

#[tokio::test]
async fn status_all_good() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let service = FakeService::default();
    let (installed, _) = run(&home, args(), &NoPrompt, false, &service, T1).await;
    installed.unwrap();
    let token = std::fs::read_to_string(AxlDir::new(home.path()).token_path()).unwrap();

    let (report, text) = run_status(&home, &service, home.exe("bin/axl")).await;
    assert!(report.ok, "{text}");
    assert_eq!(report.exit_code(), ExitCode::SUCCESS);
    assert!(text.contains(&srv.base), "{text}");
    assert!(value_of(&report, "reachable").value.starts_with("yes"));
    assert!(
        value_of(&report, "token").value.starts_with("valid ("),
        "{text}"
    );
    for name in ["Claude Code", "Codex", "OpenCode"] {
        assert_eq!(value_of(&report, name).value, "configured", "{text}");
    }
    assert!(!text.contains("fix:"), "{text}");
    assert!(!text.contains(token.trim()), "token leaked: {text}");

    // A harness that is not on this machine is reported, not failed.
    let bare = TempHome::new();
    bare.mkdir(".claude");
    connect(&bare, &srv).await;
    let (report, text) = run_status(&bare, &FakeService::default(), bare.exe("bin/axl")).await;
    assert_eq!(value_of(&report, "Codex").value, "not found", "{text}");
    assert_eq!(value_of(&report, "OpenCode").value, "not found", "{text}");
    assert_eq!(
        value_of(&report, "Claude Code").value.split(',').next(),
        Some("not configured")
    );
}

#[tokio::test]
async fn status_reports_missing_token_with_fix() {
    let srv = server().await;
    let home = TempHome::new();
    home.mkdir(".claude");
    save_config(
        &AxlDir::new(home.path()),
        &ClientConfig {
            server_url: Some(srv.base.clone()),
            port: None,
            ..ClientConfig::default()
        },
    )
    .unwrap();
    let (report, text) = run_status(&home, &FakeService::default(), home.exe("bin/axl")).await;
    assert!(!report.ok);
    assert_eq!(report.exit_code(), ExitCode::FAILURE);
    assert!(
        text.contains(&format!("fix: axl login {}", srv.base)),
        "{text}"
    );
    assert!(
        value_of(&report, "Claude Code")
            .fix
            .as_deref()
            .is_some_and(|f| f == "axl install --harness claude-code"),
        "{text}"
    );

    // An unreachable server gets the same login fix; a stopped service its own.
    let dead = format!("http://127.0.0.1:{}", free_port());
    let home = TempHome::new();
    save_config(
        &AxlDir::new(home.path()),
        &ClientConfig {
            server_url: Some(dead.clone()),
            port: None,
            ..ClientConfig::default()
        },
    )
    .unwrap();
    write_token(&AxlDir::new(home.path()), "axl_0123456789abcdef").unwrap();
    let service = FakeService {
        stopped: true,
        ..FakeService::default()
    };
    let (report, text) = run_status(&home, &service, home.exe("bin/axl")).await;
    assert!(
        !text.contains("axl login"),
        "unreachable is not a login problem: {text}"
    );
    assert!(
        value_of(&report, "reachable")
            .value
            .starts_with("cannot reach"),
        "{text}"
    );
    // Root cause first: the stopped local service, with its fix, before "reachable".
    let pos = |l: &str| report.lines.iter().position(|x| x.label == l).unwrap();
    assert!(pos("service") < pos("reachable"), "{text}");
    assert_eq!(
        value_of(&report, "service").fix.as_deref(),
        Some("axl service start")
    );
    assert!(!text.contains("axl_0123456789abcdef"), "{text}");
}

#[tokio::test]
async fn status_reports_stale_binary_path() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let service = FakeService::default();
    let old = home.exe("bin/axl");
    let (installed, _) = run(&home, args(), &NoPrompt, false, &service, T1).await;
    installed.unwrap();
    let new = home.exe("newbin/axl");
    std::fs::remove_file(&old).unwrap();

    let (report, text) = run_status(&home, &service, new).await;
    assert!(!report.ok, "{text}");
    for name in ["Claude Code", "Codex", "OpenCode"] {
        let l = value_of(&report, name);
        assert!(l.value.contains("no longer exists"), "{name}: {text}");
        assert!(
            l.value.contains(&old.display().to_string()),
            "{name}: {text}"
        );
        assert_eq!(l.fix.as_deref(), Some("axl install"), "{name}: {text}");
    }
    assert!(text.contains("fix: axl install\n"), "{text}");
}

#[tokio::test]
async fn status_flags_version_mismatch() {
    use axum::{Json, Router, routing::get};
    let app = Router::new()
        .route("/readyz", get(|| async { "ok" }))
        .route(
            "/v1/whoami",
            get(|| async { Json(serde_json::json!({"label": "stub", "version": "0.0.1"})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    save_config(
        &dir,
        &ClientConfig {
            server_url: Some(base),
            port: None,
            ..ClientConfig::default()
        },
    )
    .unwrap();
    write_token(&dir, "axl_0123456789abcdef").unwrap();
    let (report, text) = run_status(&home, &FakeService::default(), home.exe("bin/axl")).await;
    let v = value_of(&report, "version");
    assert!(v.value.contains("0.0.1"), "{text}");
    assert!(v.value.contains(env!("CARGO_PKG_VERSION")), "{text}");
    assert_eq!(v.fix.as_deref(), Some("axl service restart"), "{text}");
    assert!(!report.ok);
    assert!(value_of(&report, "token").value.contains("stub"));
}

#[tokio::test]
async fn status_is_read_only() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let service = FakeService::default();
    let (installed, _) = run(&home, args(), &NoPrompt, false, &service, T1).await;
    installed.unwrap();
    // Make it a messy home: a stale path, a missing config entry.
    let old = home.exe("bin/axl");
    let new = home.exe("newbin/axl");
    std::fs::remove_file(&old).unwrap();
    std::fs::write(home.path().join(".codex/config.toml"), "model = \"o3\"\n").unwrap();

    let before = snapshot(home.path());
    let stopped = FakeService {
        stopped: true,
        ..FakeService::default()
    };
    let (report, _) = run_status(&home, &stopped, new).await;
    assert!(!report.ok);
    assert_eq!(
        snapshot(home.path()),
        before,
        "status changed the home tree"
    );
    assert!(service.calls.lock().unwrap().is_empty());
}

/// A stub server with a fixed `/readyz` and `/v1/whoami` status, and a home logged in to it.
async fn stub_home(readyz: u16, whoami: u16) -> (TempHome, String) {
    use axum::{Router, http::StatusCode, routing::get};
    let code = |c: u16| StatusCode::from_u16(c).unwrap();
    let app = Router::new()
        .route("/readyz", get(move || async move { (code(readyz), "x") }))
        .route(
            "/v1/whoami",
            get(move || async move {
                let body = if whoami == 401 {
                    r#"{"error":"unauthorized"}"#
                } else {
                    r#"{"error":"boom"}"#
                };
                (code(whoami), body)
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    save_config(
        &dir,
        &ClientConfig {
            server_url: Some(base.clone()),
            port: None,
            ..ClientConfig::default()
        },
    )
    .unwrap();
    write_token(&dir, "axl_0123456789abcdef").unwrap();
    (home, base)
}

#[tokio::test]
async fn status_whoami_401_means_login_but_500_does_not() {
    let (home, base) = stub_home(200, 401).await;
    let (report, text) = run_status(&home, &FakeService::default(), home.exe("bin/axl")).await;
    assert!(
        value_of(&report, "token").value.contains("rejected"),
        "{text}"
    );
    assert_eq!(
        value_of(&report, "token").fix.as_deref(),
        Some(format!("axl login {base}").as_str())
    );

    let (home, _) = stub_home(200, 500).await;
    let (report, text) = run_status(&home, &FakeService::default(), home.exe("bin/axl")).await;
    let token = value_of(&report, "token");
    assert!(token.value.contains("server error"), "{text}");
    assert!(
        !text.contains("axl login") && !text.contains("rejected"),
        "{text}"
    );
    assert!(token.fix.is_some() && !report.ok);
}

/// `/readyz` answers 200; `/v1/whoami` gets its connection dropped without a reply.
#[tokio::test]
async fn status_whoami_transport_failure_is_unsuccessful_with_a_retry() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            if String::from_utf8_lossy(&buf[..n]).starts_with("GET /readyz") {
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\nconnection: close\r\n\r\nx",
                    )
                    .await;
            }
            // Anything else: drop the socket.
        }
    });
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    save_config(
        &dir,
        &ClientConfig {
            server_url: Some(base),
            port: None,
            ..ClientConfig::default()
        },
    )
    .unwrap();
    write_token(&dir, "axl_0123456789abcdef").unwrap();
    let (report, text) = run_status(&home, &FakeService::default(), home.exe("bin/axl")).await;
    let token = value_of(&report, "token");
    assert!(token.value.contains("could not check"), "{text}");
    assert!(
        token
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("axl status")),
        "{text}"
    );
    assert!(!report.ok, "{text}");
}

#[tokio::test]
async fn status_readyz_500_is_a_server_error_not_unreachable() {
    let (home, base) = stub_home(503, 200).await;
    let (report, text) = run_status(&home, &FakeService::default(), home.exe("bin/axl")).await;
    let r = value_of(&report, "reachable");
    assert_eq!(r.value, format!("server at {base} returned 503"), "{text}");
    assert!(
        !text.contains("axl login") && !text.contains("cannot reach"),
        "{text}"
    );
    assert!(r.fix.is_some());
}

#[tokio::test]
async fn status_shows_wake_channel_and_turn() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let service = FakeService::default();
    let (installed, _) = run(&home, args(), &NoPrompt, false, &service, T1).await;
    installed.unwrap();
    let token = std::fs::read_to_string(AxlDir::new(home.path()).token_path()).unwrap();
    let token = token.trim();

    // A live bridge session (this process is its harness) that reported a channel and a turn.
    let session = axle::domain::SessionId::new();
    let http = reqwest::Client::new();
    let post = |path: &str, body: serde_json::Value| {
        http.post(format!("{}/v1/{path}", srv.base))
            .bearer_auth(token)
            .json(&body)
            .send()
    };
    let res = post(
        "register",
        serde_json::json!({"session_id": session, "host": "mac", "harness": "claude-code",
            "project_key": "github.com/acme/platform-infra"}),
    )
    .await
    .unwrap();
    assert!(res.status().is_success());
    post(
        "heartbeat",
        serde_json::json!({"session_id": session, "wake_channel": "claude-socket"}),
    )
    .await
    .unwrap();
    let res = post(
        "turn",
        serde_json::json!({"session_id": session, "state": "busy"}),
    )
    .await
    .unwrap();
    assert_eq!(res.status().as_u16(), 204);
    let info = serde_json::json!({"session_id": session,
        "project_key": "github.com/acme/platform-infra",
        "server_url": srv.base, "harness": "claude-code"})
    .to_string();
    home.write(&format!(".axl/sessions/{}.json", std::process::id()), &info);
    // A session file whose harness is gone is not a live session.
    let dead = serde_json::json!({"session_id": axle::domain::SessionId::new(),
        "project_key": null, "server_url": srv.base, "harness": "codex"})
    .to_string();
    home.write(".axl/sessions/999999.json", &dead);

    let (report, text) = run_status(&home, &service, home.exe("bin/axl")).await;
    assert!(report.ok, "{text}");
    let sessions: Vec<_> = report
        .lines
        .iter()
        .filter(|l| l.label == "session")
        .collect();
    assert_eq!(sessions.len(), 1, "{text}");
    assert_eq!(
        sessions[0].value,
        "claude-code · github.com/acme/platform-infra · wake: claude-socket · turn: busy",
        "{text}"
    );
    assert!(sessions[0].fix.is_none());
}

#[tokio::test]
async fn status_never_sends_the_token_to_another_server() {
    let srv = server().await;
    let home = full_home(&srv).await;
    let service = FakeService::default();
    let (installed, _) = run(&home, args(), &NoPrompt, false, &service, T1).await;
    installed.unwrap();

    // Another server that records every request it gets.
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counted = hits.clone();
    let app = axum::Router::new().fallback(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async { "{}" }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app).into_future());

    // A live session (this process is its harness) whose bridge talks to the other server.
    let info = serde_json::json!({"session_id": axle::domain::SessionId::new(),
        "project_key": "github.com/acme/elsewhere",
        "server_url": other, "harness": "codex"})
    .to_string();
    home.write(&format!(".axl/sessions/{}.json", std::process::id()), &info);

    let (report, text) = run_status(&home, &service, home.exe("bin/axl")).await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0, "{text}");
    let sessions: Vec<_> = report
        .lines
        .iter()
        .filter(|l| l.label == "session")
        .collect();
    assert_eq!(sessions.len(), 1, "{text}");
    assert_eq!(
        sessions[0].value,
        format!("codex · github.com/acme/elsewhere · another server ({other})"),
        "{text}"
    );
    assert!(sessions[0].fix.is_none());
}
