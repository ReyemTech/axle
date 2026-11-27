//! `GET /install.sh`: the served script, and the script's own guard rails.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use askama::Template;
use axle::clock::TestClock;
use axle::config::ServerConfig;
use axle::http::install_script::InstallScript;
use axle::http::{AppState, router};
use axle::store::sqlite::SqliteStore;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use time::macros::datetime;
use tower::ServiceExt;

async fn get_install(
    extra: &'static [(&'static str, &'static str)],
) -> (StatusCode, String, String) {
    let opts: SqliteConnectOptions = "sqlite::memory:".parse().unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts.foreign_keys(true))
        .await
        .unwrap();
    let store = Arc::new(SqliteStore::from_pool(pool).await.unwrap());
    let clock = Arc::new(TestClock::new(datetime!(2026-11-27 9:00 UTC)));
    let config = ServerConfig::from_lookup(|k| match k {
        "DATABASE_URL" => Some("sqlite::memory:".into()),
        "AXL_ADMIN_SECRET" => Some("test-secret-0123456789".into()),
        _ => extra
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string()),
    })
    .unwrap();
    let app = router(AppState::new(store, clock, config));
    let res = app
        .oneshot(Request::get("/install.sh").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let ctype = res
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, ctype, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn install_sh_bakes_url_and_version() {
    let (status, ctype, body) = get_install(&[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("text/x-shellscript"), "{ctype}");
    assert!(
        body.contains("axl install --server 'http://127.0.0.1:7420'"),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            "releases/download/v{}/axle-mcp-installer.sh",
            env!("CARGO_PKG_VERSION")
        )),
        "{body}"
    );
}

#[tokio::test]
async fn install_sh_uses_the_public_url() {
    let (status, _, body) = get_install(&[
        ("AXL_BIND", "0.0.0.0:7420"),
        ("AXL_PUBLIC_URL", "https://axle.example.com"),
    ])
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("axl install --server 'https://axle.example.com'"),
        "{body}"
    );
}

#[tokio::test]
async fn install_sh_503_without_public_url_off_loopback() {
    let (status, _, body) = get_install(&[("AXL_BIND", "0.0.0.0:7420")]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("AXL_PUBLIC_URL"), "{body}");
    assert!(!body.contains("axl install"), "{body}");
}

/// A scratch directory under the system temp dir, removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "axl-install-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn render_for(server_url: &str) -> String {
    InstallScript {
        server_url: server_url.into(),
        version: "0.1.1",
    }
    .render()
    .unwrap()
}

fn render() -> String {
    render_for("https://axle.example.com")
}

fn write_script(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("install.sh");
    std::fs::write(&path, render()).unwrap();
    path
}

#[test]
fn rendered_script_passes_shellcheck() {
    if Command::new("shellcheck")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: shellcheck not installed");
        return;
    }
    let dir = TempDir::new();
    let path = write_script(dir.path());
    let out = Command::new("shellcheck")
        .args(["-s", "sh"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Run {
    ok: bool,
    stderr: String,
    /// Lines the stub `axl` appended, one per invocation.
    axl_log: String,
}

/// Runs the script piped into `sh` (as `curl | sh` does) with `id`, `uname` and a stub `axl`
/// first on PATH, in a session with no controlling terminal.
fn run_script(server_url: &str, uid: &str, os: &str, allow_root: bool, args: &[&str]) -> Run {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let dir = TempDir::new();
    let shims = dir.path().join("bin");
    std::fs::create_dir(&shims).unwrap();
    let shim = |name: &str, body: &str| {
        let p = shims.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    shim("id", &format!("echo {uid}"));
    shim(
        "uname",
        &format!("case \"$1\" in -s) echo {os};; -m) echo x86_64;; esac"),
    );
    // Reads all of its stdin, like an interactive login would, then logs the call.
    shim(
        "axl",
        "if [ \"$1\" = --version ]; then echo 'axl 99.0.0'; exit 0; fi\n\
         cat >/dev/null\n\
         echo \"$1\" >>\"$HOME/axl.log\"",
    );
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut cmd = Command::new("sh");
    cmd.args(["-s", "--"])
        .args(args)
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", shims.display()))
        .env("HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if allow_root {
        cmd.env("AXL_ALLOW_ROOT", "1");
    }
    // SAFETY: setsid is async-signal-safe; it drops the controlling terminal so /dev/tty is
    // unavailable whether or not the tests run from a terminal.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // The script, then a trailing line a stdin-reading child would swallow or that would run
    // in its place.
    let script = format!("{}\necho TRAILER-RAN >&2\n", render_for(server_url));
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(script.as_bytes());
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    Run {
        ok: out.status.success(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        axl_log: std::fs::read_to_string(home.join("axl.log")).unwrap_or_default(),
    }
}

#[test]
fn script_refuses_root_and_unknown_os() {
    let r = run_script("https://axle.example.com", "0", "Linux", false, &[]);
    assert!(!r.ok);
    assert!(r.stderr.contains("AXL_ALLOW_ROOT"), "{}", r.stderr);

    let r = run_script("https://axle.example.com", "1000", "FreeBSD", false, &[]);
    assert!(!r.ok);
    assert!(r.stderr.contains("unsupported"), "{}", r.stderr);

    // AXL_ALLOW_ROOT lifts the root refusal (it then stops at the OS check).
    let r = run_script("https://axle.example.com", "0", "FreeBSD", true, &[]);
    assert!(!r.ok);
    assert!(r.stderr.contains("unsupported"), "{}", r.stderr);
}

#[test]
fn axl_reading_stdin_does_not_swallow_the_script() {
    let r = run_script(
        "https://axle.example.com",
        "1000",
        "Linux",
        false,
        &["CODE-1"],
    );
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.axl_log, "install\nstatus\n", "{}", r.stderr);
}

#[test]
fn plain_http_warns_unless_the_host_is_exactly_loopback() {
    for url in [
        "http://localhost.evil.com",
        "http://127.0.0.1.evil.com:7420",
        "http://localhostx:7420/path",
        "http://10.0.0.5:7420",
    ] {
        let r = run_script(url, "1000", "Linux", false, &[]);
        assert!(r.stderr.contains("plain http"), "{url}: {}", r.stderr);
        assert!(!r.ok, "{url}");
        assert!(
            r.axl_log.is_empty(),
            "{url}: installed without confirmation"
        );
    }
    for url in [
        "http://localhost:7420",
        "http://localhost",
        "http://127.0.0.1:7420",
        "http://[::1]:7420",
    ] {
        let r = run_script(url, "1000", "Linux", false, &[]);
        assert!(!r.stderr.contains("plain http"), "{url}: {}", r.stderr);
        assert!(r.ok, "{url}: {}", r.stderr);
        assert_eq!(r.axl_log, "install\nstatus\n", "{url}");
    }
}
