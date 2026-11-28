//! `axl service install | uninstall | start | stop | restart | status | logs`.

use std::io::Write;
use std::path::Path;

use crate::client::{AxlDir, load_config};
use crate::daemon::{
    self, DEFAULT_PORT, DaemonError, NoServiceManager, Runner, ServiceControl, ServiceState,
    UnitSpec, install_unit, purge_db,
};

pub enum ServiceAction {
    Install { port: Option<u16> },
    Uninstall { purge: bool },
    Start,
    Stop,
    Restart,
    Status,
    Logs,
}

/// Lines `axl service logs` prints.
const LOG_TAIL_LINES: usize = 100;

pub fn run(
    home: &Path,
    axl: &Path,
    os: &str,
    runner: Box<dyn Runner>,
    action: ServiceAction,
    out: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = AxlDir::new(home);
    if let ServiceAction::Logs = action {
        return logs(&dir, out);
    }
    let port = match &action {
        ServiceAction::Install { port } => port.or(load_config(&dir)?.port).unwrap_or(DEFAULT_PORT),
        _ => DEFAULT_PORT,
    };
    let ctl = daemon::platform_for(os, home, runner).map_err(|e| NoServiceManager { port, ..e })?;
    match action {
        ServiceAction::Install { .. } => {
            let spec = UnitSpec {
                axl: axl.to_path_buf(),
                home: home.to_path_buf(),
                port,
            };
            install_unit(ctl.as_ref(), &dir, &spec)?;
            writeln!(
                out,
                "axl: service installed and started on http://127.0.0.1:{port}"
            )?;
            writeln!(out, "axl: logs at {}", spec.log_path().display())?;
        }
        ServiceAction::Uninstall { purge } => {
            ctl.uninstall()?;
            writeln!(out, "axl: service removed")?;
            if purge {
                purge_db(&dir)?;
                writeln!(out, "axl: database deleted")?;
            } else {
                writeln!(
                    out,
                    "axl: database kept at {} (--purge deletes it)",
                    dir.db_path().display()
                )?;
            }
        }
        ServiceAction::Start => simple(ctl.as_ref(), |c| c.start(), "started", out)?,
        ServiceAction::Stop => simple(ctl.as_ref(), |c| c.stop(), "stopped", out)?,
        ServiceAction::Restart => simple(ctl.as_ref(), |c| c.restart(), "restarted", out)?,
        ServiceAction::Status => {
            let word = match ctl.status()? {
                ServiceState::Running => "running",
                ServiceState::Stopped => "installed but not running",
                ServiceState::NotInstalled => "not installed (run `axl service install`)",
            };
            writeln!(out, "axl: service {word}")?;
        }
        ServiceAction::Logs => unreachable!("handled above"),
    }
    Ok(())
}

fn simple(
    ctl: &dyn ServiceControl,
    f: impl Fn(&dyn ServiceControl) -> Result<(), DaemonError>,
    done: &str,
    out: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    f(ctl)?;
    writeln!(out, "axl: service {done}")?;
    Ok(())
}

fn logs(dir: &AxlDir, out: &mut dyn Write) -> Result<(), Box<dyn std::error::Error>> {
    let path = dir.logs_dir().join("axl.log");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            for l in &lines[lines.len().saturating_sub(LOG_TAIL_LINES)..] {
                writeln!(out, "{l}")?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            writeln!(out, "axl: no log yet at {}", path.display())?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}
