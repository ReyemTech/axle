use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::TcpListener;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::sync::{Arc, Mutex};

use axle::cli::{ServiceAction, service};
use axle::client::{AxlDir, ClientConfig, load_config, save_config};
use axle::daemon::{
    DaemonError, Launchd, Runner, ServiceControl, Systemd, UnitSpec, install_unit, launchd_plist,
    platform_for, plist_path, systemd_unit, unit_path,
};

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("axl-daemon-{suffix}"));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Script = Arc<Mutex<HashMap<&'static str, VecDeque<bool>>>>;
type Calls = Arc<Mutex<Vec<Vec<String>>>>;

/// Records every command; nothing real runs. Programs in `fail` exit 1 for any args that
/// contain one of the given words (`fail_on`), or always (`fail_all`).
#[derive(Clone, Default)]
struct Recorder {
    calls: Calls,
    fail_on: Vec<&'static str>,
    fail_all: bool,
    stdout: Vec<(&'static str, &'static str)>,
    /// Per-subcommand scripted outcomes (true = exit 0), consumed in order.
    script: Script,
}

impl Recorder {
    fn script(self, key: &'static str, outcomes: &[bool]) -> Self {
        self.script
            .lock()
            .unwrap()
            .insert(key, outcomes.iter().copied().collect());
        self
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl Runner for Recorder {
    fn run(&self, prog: &str, args: &[&str]) -> io::Result<Output> {
        let mut call = vec![prog.to_string()];
        call.extend(args.iter().map(|a| a.to_string()));
        self.calls.lock().unwrap().push(call);
        let scripted = {
            let mut map = self.script.lock().unwrap();
            args.iter()
                .find_map(|a| map.get_mut(*a).and_then(|q| q.pop_front()))
        };
        let failed = match scripted {
            Some(ok) => !ok,
            None => self.fail_all || args.iter().any(|a| self.fail_on.contains(a)),
        };
        let stdout = self
            .stdout
            .iter()
            .find(|(k, _)| args.contains(k))
            .map(|(_, v)| v.as_bytes().to_vec())
            .unwrap_or_default();
        Ok(Output {
            status: ExitStatus::from_raw(if failed { 1 << 8 } else { 0 }),
            stdout,
            stderr: Vec::new(),
        })
    }
}

fn spec(home: &Path) -> UnitSpec {
    UnitSpec {
        axl: PathBuf::from("/opt/homebrew/bin/axl"),
        home: home.to_path_buf(),
        port: free_port(),
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn golden(name: &str, home: &str, axl: &str) -> String {
    std::fs::read_to_string(Path::new("tests/fixtures/daemon").join(name))
        .unwrap()
        .replace("{{HOME}}", home)
        .replace("{{AXL}}", axl)
}

fn default_spec(home: &str) -> UnitSpec {
    UnitSpec {
        axl: PathBuf::from("/opt/homebrew/bin/axl"),
        home: PathBuf::from(home),
        port: 7420,
    }
}

#[test]
fn plist_matches_golden() {
    assert_eq!(
        launchd_plist(&default_spec("/Users/tester")),
        golden(
            "com.reyemtech.axle.plist",
            "/Users/tester",
            "/opt/homebrew/bin/axl"
        )
    );
}

#[test]
fn unit_matches_golden() {
    assert_eq!(
        systemd_unit(&default_spec("/home/tester")),
        golden("axle.service", "/home/tester", "/opt/homebrew/bin/axl")
    );
}

#[test]
fn paths_with_spaces_are_escaped() {
    let mut s = default_spec("/Users/A & B <x>");
    s.axl = PathBuf::from("/Users/A & B <x>/my bin/axl");
    let plist = launchd_plist(&s);
    assert!(plist.contains("<string>/Users/A &amp; B &lt;x&gt;/my bin/axl</string>"));
    assert!(!plist.contains("A & B"));

    let mut s = default_spec("/home/a b");
    s.axl = PathBuf::from("/home/a b/bin/axl");
    let unit = systemd_unit(&s);
    assert!(unit.contains("ExecStart=\"/home/a b/bin/axl\" serve\n"));
    assert!(unit.contains("Environment=\"DATABASE_URL=sqlite:///home/a b/.axl/axl.db\"\n"));
}

#[test]
fn launchd_install_runs_bootstrap_for_uid() {
    let home = TempHome::new();
    let rec = Recorder {
        fail_on: vec!["print"], // not loaded yet
        ..Default::default()
    };
    let ctl = Launchd::new(home.path(), Box::new(rec.clone()), 501);
    ctl.install(&spec(home.path())).unwrap();
    let plist = plist_path(home.path());
    assert!(plist.starts_with(home.path()));
    assert!(plist.is_file());
    assert_eq!(
        rec.calls().last().unwrap(),
        &vec![
            "launchctl".to_string(),
            "bootstrap".into(),
            "gui/501".into(),
            plist.display().to_string()
        ]
    );
}

#[test]
fn systemd_install_enables_now() {
    let home = TempHome::new();
    let rec = Recorder::default();
    let ctl = Systemd::new(home.path(), Box::new(rec.clone()));
    ctl.install(&spec(home.path())).unwrap();
    assert!(unit_path(home.path()).is_file());
    let calls = rec.calls();
    let enable: Vec<String> = ["systemctl", "--user", "enable", "--now", "axle.service"]
        .map(String::from)
        .to_vec();
    assert!(calls.contains(&enable), "{calls:?}");
    let reload = calls
        .iter()
        .position(|c| c.contains(&"daemon-reload".to_string()))
        .unwrap();
    assert!(reload < calls.iter().position(|c| *c == enable).unwrap());
}

#[test]
fn no_systemd_prints_serve_instructions() {
    let home = TempHome::new();
    let rec = Recorder {
        fail_all: true,
        ..Default::default()
    };
    let err = match platform_for("linux", home.path(), Box::new(rec)) {
        Err(e) => e,
        Ok(_) => panic!("expected no service manager"),
    };
    let text = err.to_string();
    assert!(text.contains("axl serve"), "{text}");
    assert!(text.contains(&format!(
        "DATABASE_URL=sqlite://{}/.axl/axl.db",
        home.path().display()
    )));
    assert!(text.contains("AXL_BIND=127.0.0.1:7420"));
}

#[test]
fn taken_port_is_refused_with_hint() {
    let home = TempHome::new();
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let rec = Recorder {
        stdout: vec![],
        ..Default::default()
    };
    let ctl = Systemd::new(home.path(), Box::new(rec.clone()));
    let mut s = spec(home.path());
    s.port = port;
    let err = install_unit(&ctl, &AxlDir::new(home.path()), &s).unwrap_err();
    assert!(matches!(err, DaemonError::PortInUse(p) if p == port));
    assert!(err.to_string().contains("--port"));
    assert!(err.to_string().contains(&port.to_string()));
    assert!(
        !unit_path(home.path()).exists(),
        "nothing written on refusal"
    );
    assert!(
        rec.calls()
            .iter()
            .all(|c| !c.contains(&"enable".to_string()))
    );
}

fn running_recorder() -> Recorder {
    Recorder {
        stdout: vec![("is-active", "active\n")],
        ..Default::default()
    }
}

fn restarted(rec: &Recorder) -> bool {
    rec.calls()
        .iter()
        .any(|c| c.contains(&"restart".to_string()))
}

#[test]
fn running_service_skips_port_check_for_its_own_port() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    let held = TcpListener::bind("127.0.0.1:0").unwrap(); // the service itself holds it
    let port = held.local_addr().unwrap().port();
    save_config(
        &dir,
        &ClientConfig {
            server_url: None,
            port: Some(port),
            ..ClientConfig::default()
        },
    )
    .unwrap();
    let rec = running_recorder();
    let ctl = Systemd::new(home.path(), Box::new(rec.clone()));
    let mut s = spec(home.path());
    s.port = port;
    install_unit(&ctl, &dir, &s).unwrap();
    assert!(restarted(&rec));
}

#[test]
fn running_service_refuses_a_changed_occupied_port_untouched() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    let held = TcpListener::bind("127.0.0.1:0").unwrap(); // someone else holds it
    let port = held.local_addr().unwrap().port();
    let unit = unit_path(home.path());
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "ORIGINAL").unwrap();
    let rec = running_recorder();
    let ctl = Systemd::new(home.path(), Box::new(rec.clone()));
    let mut s = spec(home.path());
    s.port = port;
    let err = install_unit(&ctl, &dir, &s).unwrap_err();
    assert!(matches!(err, DaemonError::PortInUse(p) if p == port));
    assert_eq!(std::fs::read_to_string(&unit).unwrap(), "ORIGINAL");
    assert!(!restarted(&rec));
    assert!(
        rec.calls()
            .iter()
            .all(|c| !c.contains(&"enable".to_string()))
    );
}

#[test]
fn running_service_moves_to_a_free_port() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let rec = running_recorder();
    let ctl = Systemd::new(home.path(), Box::new(rec.clone()));
    let mut s = spec(home.path());
    s.port = port;
    install_unit(&ctl, &dir, &s).unwrap();
    assert!(restarted(&rec));
    assert_eq!(load_config(&dir).unwrap().port, Some(port));
}

#[test]
fn install_saves_non_default_port_and_keeps_other_keys() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    let ctl = Systemd::new(home.path(), Box::new(Recorder::default()));
    let s = spec(home.path());
    install_unit(&ctl, &dir, &s).unwrap();
    let cfg = load_config(&dir).unwrap();
    assert_eq!(cfg.port, Some(s.port));
    assert_eq!(cfg.server_url, Some(format!("http://127.0.0.1:{}", s.port)));
    assert!(dir.logs_dir().is_dir());

    // Back to the default: the saved port goes, a remote server URL would stay.
    save_config(
        &dir,
        &ClientConfig {
            server_url: Some("https://axle.example.com".into()),
            port: Some(9),
            ..ClientConfig::default()
        },
    )
    .unwrap();
    // (default port 7420 may be taken on this machine, so go through save path via spec)
    let mut d = spec(home.path());
    d.port = 7420;
    if axle::daemon::port_free(7420) {
        install_unit(&ctl, &dir, &d).unwrap();
        let cfg = load_config(&dir).unwrap();
        assert_eq!(cfg.port, None);
        assert_eq!(cfg.server_url.as_deref(), Some("https://axle.example.com"));
    }
}

#[test]
fn install_port_keeps_a_remote_server_url_and_moves_a_local_one() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    let ctl = Systemd::new(home.path(), Box::new(Recorder::default()));

    // A remote server stays; only the port is saved.
    save_config(
        &dir,
        &ClientConfig {
            server_url: Some("https://axle.example.com".into()),
            port: None,
            ..ClientConfig::default()
        },
    )
    .unwrap();
    let s = spec(home.path());
    install_unit(&ctl, &dir, &s).unwrap();
    let cfg = load_config(&dir).unwrap();
    assert_eq!(cfg.port, Some(s.port));
    assert_eq!(cfg.server_url.as_deref(), Some("https://axle.example.com"));

    // A previous local URL (loopback or localhost) follows the new port.
    for old in ["http://127.0.0.1:9", "http://localhost:9"] {
        save_config(
            &dir,
            &ClientConfig {
                server_url: Some(old.into()),
                port: Some(9),
                ..ClientConfig::default()
            },
        )
        .unwrap();
        let s = spec(home.path());
        install_unit(&ctl, &dir, &s).unwrap();
        let cfg = load_config(&dir).unwrap();
        assert_eq!(cfg.server_url, Some(format!("http://127.0.0.1:{}", s.port)));
    }
}

#[test]
fn uninstall_keeps_db_unless_purge() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    dir.ensure().unwrap();
    for name in ["axl.db", "axl.db-wal", "axl.db-shm"] {
        std::fs::write(dir.root.join(name), b"x").unwrap();
    }
    std::fs::write(dir.root.join("token"), b"t").unwrap();
    let run = |purge: bool| {
        let mut out = Vec::new();
        service(
            home.path(),
            Path::new("/opt/homebrew/bin/axl"),
            "linux",
            Box::new(Recorder::default()),
            ServiceAction::Uninstall { purge },
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    };
    let text = run(false);
    assert!(dir.db_path().exists() && text.contains("--purge"));
    run(true);
    assert!(!dir.db_path().exists());
    assert!(!dir.root.join("axl.db-wal").exists());
    assert!(!dir.root.join("axl.db-shm").exists());
    assert!(
        dir.root.join("token").exists(),
        "purge touches only the database"
    );
}

type SleepFn = Box<dyn Fn(std::time::Duration)>;

fn no_sleep() -> (SleepFn, Arc<Mutex<u32>>) {
    let n = Arc::new(Mutex::new(0));
    let n2 = n.clone();
    (Box::new(move |_| *n2.lock().unwrap() += 1), n)
}

fn verbs(calls: &[Vec<String>]) -> Vec<&str> {
    calls.iter().map(|c| c[1].as_str()).collect()
}

#[test]
fn launchd_reinstall_boots_out_polls_then_bootstraps() {
    let home = TempHome::new();
    // print: loaded (install check), loaded (poll 1), loaded (poll 2), gone (poll 3).
    let rec = Recorder::default().script("print", &[true, true, true, false]);
    let (sleep, slept) = no_sleep();
    let ctl = Launchd::new(home.path(), Box::new(rec.clone()), 501).with_sleep(sleep);
    ctl.install(&spec(home.path())).unwrap();
    assert_eq!(
        verbs(&rec.calls()),
        ["print", "bootout", "print", "print", "print", "bootstrap"]
    );
    assert_eq!(
        *slept.lock().unwrap(),
        2,
        "slept between the two still-loaded polls"
    );
}

#[test]
fn launchd_bootstrap_is_retried_after_io_error() {
    let home = TempHome::new();
    let rec = Recorder::default()
        .script("print", &[false])
        .script("bootstrap", &[false, false, true]);
    let (sleep, slept) = no_sleep();
    let ctl = Launchd::new(home.path(), Box::new(rec.clone()), 501).with_sleep(sleep);
    ctl.install(&spec(home.path())).unwrap();
    assert_eq!(
        verbs(&rec.calls()),
        ["print", "bootstrap", "bootstrap", "bootstrap"]
    );
    assert_eq!(*slept.lock().unwrap(), 2);

    // Gives up (with the error) after a bounded number of attempts.
    let rec = Recorder::default()
        .script("print", &[false])
        .script("bootstrap", &[false; 10]);
    let (sleep, _) = no_sleep();
    let ctl = Launchd::new(home.path(), Box::new(rec.clone()), 501).with_sleep(sleep);
    assert!(ctl.install(&spec(home.path())).is_err());
    assert_eq!(
        rec.calls().iter().filter(|c| c[1] == "bootstrap").count(),
        5
    );
}

#[test]
fn dollar_in_home_is_literal_in_environment_but_doubled_in_exec_start() {
    let mut s = default_spec("/home/we$ird");
    s.axl = PathBuf::from("/home/we$ird/bin/axl");
    let unit = systemd_unit(&s);
    assert!(unit.contains("Environment=\"DATABASE_URL=sqlite:///home/we$ird/.axl/axl.db\"\n"));
    assert!(unit.contains("ExecStart=\"/home/we$$ird/bin/axl\" serve\n"));
}

#[test]
fn purge_leaves_other_files_alike_axl_db_bak() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    dir.ensure().unwrap();
    std::fs::write(dir.root.join("axl.db"), b"x").unwrap();
    std::fs::write(dir.root.join("axl.db.bak"), b"keep").unwrap();
    axle::daemon::purge_db(&dir).unwrap();
    assert!(!dir.db_path().exists());
    assert!(dir.root.join("axl.db.bak").exists());
}
