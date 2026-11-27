//! Running `axl serve` as a per-user background service: a launchd agent on macOS, a
//! systemd user unit on Linux. Every command goes through a [`Runner`], so tests record
//! what would run instead of touching the real service manager.

pub mod launchd;
pub mod systemd;

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::client::{AxlDir, ClientError, load_config, save_config};

pub use launchd::{Launchd, launchd_plist, plist_path};
pub use systemd::{Systemd, systemd_unit, unit_path};

pub const DEFAULT_PORT: u16 = 7420;
pub const LABEL: &str = "com.reyemtech.axle";
pub const UNIT_NAME: &str = "axle.service";

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("port {0} is in use; pick another with --port")]
    PortInUse(u16),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Client(#[from] ClientError),
    /// A service-manager command that ran and failed.
    #[error("`{command}` failed: {detail}")]
    Command { command: String, detail: String },
}

/// What the service manager says about the unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    Running,
}

/// Everything a unit file needs.
#[derive(Clone, Debug)]
pub struct UnitSpec {
    pub axl: PathBuf,
    pub home: PathBuf,
    pub port: u16,
}

impl UnitSpec {
    pub fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "DATABASE_URL",
                format!("sqlite://{}/.axl/axl.db", self.home.display()),
            ),
            ("AXL_BIND", format!("127.0.0.1:{}", self.port)),
            ("AXL_UI_AUTH", "auto".to_string()),
        ]
    }

    pub fn log_path(&self) -> PathBuf {
        self.home.join(".axl").join("logs").join("axl.log")
    }
}

/// Runs an external program. Tests substitute a recorder.
pub trait Runner {
    fn run(&self, prog: &str, args: &[&str]) -> io::Result<Output>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, prog: &str, args: &[&str]) -> io::Result<Output> {
        Command::new(prog).args(args).output()
    }
}

pub trait ServiceControl {
    fn install(&self, spec: &UnitSpec) -> Result<(), DaemonError>;
    fn uninstall(&self) -> Result<(), DaemonError>;
    fn start(&self) -> Result<(), DaemonError>;
    fn stop(&self) -> Result<(), DaemonError>;
    fn restart(&self) -> Result<(), DaemonError>;
    fn status(&self) -> Result<ServiceState, DaemonError>;
}

/// No launchd and no usable systemd user session: say how to run in the foreground.
#[derive(Debug)]
pub struct NoServiceManager {
    pub home: PathBuf,
    pub port: u16,
}

impl std::fmt::Display for NoServiceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no service manager found (launchd, or a systemd user session). \
             Run the server in the foreground instead:\n  \
             DATABASE_URL=sqlite://{}/.axl/axl.db AXL_BIND=127.0.0.1:{} axl serve",
            self.home.display(),
            self.port
        )
    }
}

impl std::error::Error for NoServiceManager {}

/// The service manager for this machine.
pub fn platform(
    home: &Path,
    runner: Box<dyn Runner>,
) -> Result<Box<dyn ServiceControl>, NoServiceManager> {
    platform_for(std::env::consts::OS, home, runner)
}

/// As [`platform`], for a named OS (`"macos"`, `"linux"`), so tests need not run on it.
pub fn platform_for(
    os: &str,
    home: &Path,
    runner: Box<dyn Runner>,
) -> Result<Box<dyn ServiceControl>, NoServiceManager> {
    match os {
        "macos" => Ok(Box::new(Launchd::new(home, runner, uid()))),
        "linux" if Systemd::available(runner.as_ref()) => Ok(Box::new(Systemd::new(home, runner))),
        _ => Err(NoServiceManager {
            home: home.to_path_buf(),
            port: DEFAULT_PORT,
        }),
    }
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// True when nothing is listening on `127.0.0.1:<port>`.
pub fn port_free(port: u16) -> bool {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).is_ok()
}

/// Installs and starts the unit. The one place that checks the port (skipped when the
/// service is already running, since it is then the one holding it) and records a
/// non-default port in `config.toml`.
pub fn install_unit(
    ctl: &dyn ServiceControl,
    dir: &AxlDir,
    spec: &UnitSpec,
) -> Result<(), DaemonError> {
    let running = matches!(ctl.status(), Ok(ServiceState::Running));
    if !running && !port_free(spec.port) {
        return Err(DaemonError::PortInUse(spec.port));
    }
    dir.ensure()?;
    std::fs::create_dir_all(dir.logs_dir())?;
    ctl.install(spec)?;
    save_port(dir, spec.port)?;
    Ok(())
}

/// Load-modify-save, so a server URL the user chose for a remote server survives.
fn save_port(dir: &AxlDir, port: u16) -> Result<(), ClientError> {
    let mut cfg = load_config(dir)?;
    if port != DEFAULT_PORT {
        cfg.port = Some(port);
        cfg.server_url = Some(format!("http://127.0.0.1:{port}"));
    } else if let Some(old) = cfg.port.take() {
        if cfg.server_url.as_deref() == Some(&format!("http://127.0.0.1:{old}")) {
            cfg.server_url = None;
        }
    } else {
        return Ok(());
    }
    save_config(dir, &cfg)
}

/// Deletes `axl.db` and its `-wal` / `-shm` / `-journal` companions.
pub fn purge_db(dir: &AxlDir) -> io::Result<()> {
    let Ok(entries) = std::fs::read_dir(&dir.root) else {
        return Ok(());
    };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with("axl.db") {
            std::fs::remove_file(e.path())?;
        }
    }
    Ok(())
}

/// Runs a command and turns a spawn error or non-zero exit into a [`DaemonError`].
pub(crate) fn run_ok(
    runner: &dyn Runner,
    prog: &str,
    args: &[&str],
) -> Result<Output, DaemonError> {
    let out = runner.run(prog, args)?;
    if out.status.success() {
        Ok(out)
    } else {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(DaemonError::Command {
            command: format!("{prog} {}", args.join(" ")),
            detail: if detail.is_empty() {
                out.status.to_string()
            } else {
                detail
            },
        })
    }
}
