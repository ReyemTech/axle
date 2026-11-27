//! macOS: a LaunchAgent at `~/Library/LaunchAgents/com.reyemtech.axle.plist`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{DaemonError, LABEL, Runner, ServiceControl, ServiceState, UnitSpec, run_ok};
use crate::install::fsutil::write_atomic;

pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

pub fn launchd_plist(spec: &UnitSpec) -> String {
    let log = xml_escape(&spec.log_path().display().to_string());
    let mut env = String::new();
    for (k, v) in spec.env() {
        env.push_str(&format!(
            "    <key>{k}</key>\n    <string>{}</string>\n",
            xml_escape(&v)
        ));
    }
    let axl = xml_escape(&spec.axl.display().to_string());
    let head = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
        "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\">\n",
        "<dict>\n",
    );
    format!(
        "{head}  <key>Label</key>\n  <string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{axl}</string>\n    <string>serve</string>\n  </array>\n  <key>EnvironmentVariables</key>\n  <dict>\n{env}  </dict>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n  <key>StandardOutPath</key>\n  <string>{log}</string>\n  <key>StandardErrorPath</key>\n  <string>{log}</string>\n</dict>\n</plist>\n"
    )
}

/// How long to wait for a booted-out job to disappear, and how often to look.
const UNLOAD_POLLS: u32 = 25;
const UNLOAD_POLL_EVERY: Duration = Duration::from_millis(200);
/// `bootstrap` right after `bootout` can fail with "Input/output error"; retry a few times.
const BOOTSTRAP_ATTEMPTS: u32 = 5;
const BOOTSTRAP_RETRY_EVERY: Duration = Duration::from_millis(500);

pub struct Launchd {
    home: PathBuf,
    runner: Box<dyn Runner>,
    uid: u32,
    sleep: Box<dyn Fn(Duration)>,
}

impl Launchd {
    pub fn new(home: &Path, runner: Box<dyn Runner>, uid: u32) -> Self {
        Self {
            home: home.to_path_buf(),
            runner,
            uid,
            sleep: Box::new(std::thread::sleep),
        }
    }

    /// Replaces the delay between polls and retries, so tests do not wait.
    pub fn with_sleep(mut self, sleep: Box<dyn Fn(Duration)>) -> Self {
        self.sleep = sleep;
        self
    }

    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn target(&self) -> String {
        format!("gui/{}/{LABEL}", self.uid)
    }

    fn plist(&self) -> String {
        plist_path(&self.home).display().to_string()
    }

    fn loaded(&self) -> bool {
        self.runner
            .run("launchctl", &["print", &self.target()])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn bootstrap(&self) -> Result<(), DaemonError> {
        let mut last = None;
        for attempt in 0..BOOTSTRAP_ATTEMPTS {
            if attempt > 0 {
                (self.sleep)(BOOTSTRAP_RETRY_EVERY);
            }
            match run_ok(
                self.runner.as_ref(),
                "launchctl",
                &["bootstrap", &self.domain(), &self.plist()],
            ) {
                Ok(_) => return Ok(()),
                Err(e) => last = Some(e),
            }
        }
        Err(last.expect("at least one attempt"))
    }

    /// Waits (bounded) until launchd no longer reports the job.
    fn wait_unloaded(&self) {
        for _ in 0..UNLOAD_POLLS {
            if !self.loaded() {
                return;
            }
            (self.sleep)(UNLOAD_POLL_EVERY);
        }
    }
}

impl ServiceControl for Launchd {
    fn install(&self, spec: &UnitSpec) -> Result<(), DaemonError> {
        let path = plist_path(&self.home);
        write_atomic(&path, launchd_plist(spec).as_bytes(), 0o644)?;
        // Bootstrapping an already loaded label fails, so unload a previous copy first;
        // that also makes a re-install pick up a changed binary or port.
        if self.loaded() {
            let _ = self.runner.run("launchctl", &["bootout", &self.target()]);
            self.wait_unloaded();
        }
        self.bootstrap()
    }

    fn uninstall(&self) -> Result<(), DaemonError> {
        if self.loaded() {
            run_ok(
                self.runner.as_ref(),
                "launchctl",
                &["bootout", &self.target()],
            )?;
        }
        match std::fs::remove_file(plist_path(&self.home)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn start(&self) -> Result<(), DaemonError> {
        if !self.loaded() {
            self.bootstrap()?;
        }
        run_ok(
            self.runner.as_ref(),
            "launchctl",
            &["kickstart", &self.target()],
        )
        .map(|_| ())
    }

    fn stop(&self) -> Result<(), DaemonError> {
        run_ok(
            self.runner.as_ref(),
            "launchctl",
            &["bootout", &self.target()],
        )
        .map(|_| ())
    }

    fn restart(&self) -> Result<(), DaemonError> {
        if !self.loaded() {
            self.bootstrap()?;
        }
        run_ok(
            self.runner.as_ref(),
            "launchctl",
            &["kickstart", "-k", &self.target()],
        )
        .map(|_| ())
    }

    fn status(&self) -> Result<ServiceState, DaemonError> {
        let out = self.runner.run("launchctl", &["print", &self.target()])?;
        if !out.status.success() {
            return Ok(if plist_path(&self.home).exists() {
                ServiceState::Stopped
            } else {
                ServiceState::NotInstalled
            });
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(if text.lines().any(|l| l.trim() == "state = running") {
            ServiceState::Running
        } else {
            ServiceState::Stopped
        })
    }
}
