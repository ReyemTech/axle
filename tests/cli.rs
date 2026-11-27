//! The `axl` admin commands and `serve` start-up checks, run as the real binary with a clean
//! environment.

use std::path::PathBuf;
use std::process::{Command, Output};

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("axl-cli-{name}-{suffix}"));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn db_url(&self) -> String {
        format!("sqlite://{}", self.0.join("axl.db").display())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `axl <args>` with only `vars` (and PATH) in the environment.
fn axl(args: &[&str], vars: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_axl"));
    cmd.args(args).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    for (k, v) in vars {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

#[test]
fn token_create_needs_no_admin_secret() {
    let dir = TempDir::new("token");
    let url = dir.db_url();
    let out = axl(
        &["token", "create", "--label", "ci"],
        &[("DATABASE_URL", &url)],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let token = String::from_utf8(out.stdout).unwrap();
    assert!(!token.trim().is_empty(), "the token is printed on stdout");
    assert!(!stderr.contains("AXL_ADMIN_SECRET"), "{stderr}");

    let out = axl(&["token", "list"], &[("DATABASE_URL", &url)]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\tci\t"));
}

#[test]
fn serve_off_loopback_without_ui_auth_refuses_with_the_ways_out() {
    let dir = TempDir::new("serve");
    let url = dir.db_url();
    let out = axl(
        &["serve"],
        &[("DATABASE_URL", &url), ("AXL_BIND", "0.0.0.0:0")],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    for way in ["AXL_ADMIN_SECRET", "AXL_TRUSTED_HEADER", "AXL_UI_AUTH=none"] {
        assert!(stderr.contains(way), "{stderr}");
    }
}
