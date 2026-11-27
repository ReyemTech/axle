//! Linux: a user unit at `~/.config/systemd/user/axle.service`.

use std::path::{Path, PathBuf};

use super::{DaemonError, Runner, ServiceControl, ServiceState, UNIT_NAME, UnitSpec, run_ok};
use crate::install::fsutil::write_atomic;

pub fn unit_path(home: &Path) -> PathBuf {
    home.join(".config")
        .join("systemd")
        .join("user")
        .join(UNIT_NAME)
}

/// A value inside double quotes: `\`, `"` and `%` (a specifier) escaped. `$` is doubled
/// only when `expand_dollar` (command lines expand it; `Environment=` values do not).
fn quote(s: &str, expand_dollar: bool) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' if expand_dollar => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Bare when it has nothing systemd would split or expand, else quoted.
fn exec_word(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+@:,=".contains(c))
    {
        s.to_string()
    } else {
        quote(s, true)
    }
}

pub fn systemd_unit(spec: &UnitSpec) -> String {
    let mut env = String::new();
    for (k, v) in spec.env() {
        env.push_str(&format!(
            "Environment={}\n",
            quote(&format!("{k}={v}"), false)
        ));
    }
    let log = spec.log_path().display().to_string().replace('%', "%%");
    format!(
        "[Unit]\n\
Description=Axle coordination server\n\
After=network.target\n\
\n\
[Service]\n\
ExecStart={axl} serve\n\
{env}\
Restart=on-failure\n\
StandardOutput=append:{log}\n\
StandardError=append:{log}\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        axl = exec_word(&spec.axl.display().to_string()),
    )
}

pub struct Systemd {
    home: PathBuf,
    runner: Box<dyn Runner>,
}

impl Systemd {
    pub fn new(home: &Path, runner: Box<dyn Runner>) -> Self {
        Self {
            home: home.to_path_buf(),
            runner,
        }
    }

    /// Whether a systemd user session answers.
    pub fn available(runner: &dyn Runner) -> bool {
        runner
            .run("systemctl", &["--user", "show-environment"])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn ctl(&self, args: &[&str]) -> Result<(), DaemonError> {
        let mut full = vec!["--user"];
        full.extend_from_slice(args);
        run_ok(self.runner.as_ref(), "systemctl", &full).map(|_| ())
    }
}

impl ServiceControl for Systemd {
    fn install(&self, spec: &UnitSpec) -> Result<(), DaemonError> {
        let was_running = matches!(self.status(), Ok(ServiceState::Running));
        write_atomic(&unit_path(&self.home), systemd_unit(spec).as_bytes(), 0o644)?;
        self.ctl(&["daemon-reload"])?;
        self.ctl(&["enable", "--now", UNIT_NAME])?;
        if was_running {
            self.ctl(&["restart", UNIT_NAME])?; // pick up a changed binary or port
        }
        Ok(())
    }

    fn uninstall(&self) -> Result<(), DaemonError> {
        let path = unit_path(&self.home);
        if path.exists() {
            let _ = self.ctl(&["disable", "--now", UNIT_NAME]);
            std::fs::remove_file(&path)?;
            let _ = self.ctl(&["daemon-reload"]);
        }
        Ok(())
    }

    fn start(&self) -> Result<(), DaemonError> {
        self.ctl(&["start", UNIT_NAME])
    }

    fn stop(&self) -> Result<(), DaemonError> {
        self.ctl(&["stop", UNIT_NAME])
    }

    fn restart(&self) -> Result<(), DaemonError> {
        self.ctl(&["restart", UNIT_NAME])
    }

    fn status(&self) -> Result<ServiceState, DaemonError> {
        let out = self
            .runner
            .run("systemctl", &["--user", "is-active", UNIT_NAME])?;
        if String::from_utf8_lossy(&out.stdout).trim() == "active" {
            return Ok(ServiceState::Running);
        }
        Ok(if unit_path(&self.home).exists() {
            ServiceState::Stopped
        } else {
            ServiceState::NotInstalled
        })
    }
}
