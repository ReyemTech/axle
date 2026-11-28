//! `~/.axl/config.toml` and `~/.axl/token`: paths, private modes, and precedence.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::Billing;

pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:7420";

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("toml: {0}")]
    Toml(String),
    #[error("http: {0}")]
    Http(String),
    #[error("server: {0}")]
    Server(String),
    /// A problem on this machine or with what the user asked for, not the server's doing.
    #[error("{0}")]
    Usage(String),
}

/// The `~/.axl` directory and the files in it.
#[derive(Debug, Clone)]
pub struct AxlDir {
    pub root: PathBuf,
}

impl AxlDir {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".axl"),
        }
    }

    pub fn token_path(&self) -> PathBuf {
        self.root.join("token")
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("install-manifest.json")
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join("axl.db")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn integrations_dir(&self) -> PathBuf {
        self.root.join("integrations")
    }

    /// Creates the directory with mode 0700, and tightens an existing looser one.
    pub fn ensure(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&self.root)?.permissions().mode() & 0o777;
            if mode != 0o700 {
                std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Clone)]
pub struct UsageConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Harness name -> billing kind as written; a known kind always wins over detection. Kept
    /// as text and parsed per entry ([`UsageConfig::billing_for`]) so a typo here is ignored
    /// rather than failing the whole of config.toml (which would also lose `enabled = false`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub billing: std::collections::BTreeMap<String, String>,
}

impl UsageConfig {
    /// The configured billing kind for `harness`, or None when unset or not a billing kind.
    pub fn billing_for(&self, harness: &str) -> Option<Billing> {
        self.billing
            .get(harness)
            .and_then(|v| v.trim().parse().ok())
    }
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
pub struct ClientConfig {
    pub server_url: Option<String>,
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageConfig>,
}

/// Why usage collection is off: `Some("AXL_USAGE=off")`, `Some("[usage] enabled = false")`,
/// or None (on). `AXL_USAGE` is off for "off", "0", "false", "no" (ASCII case-insensitive).
pub fn usage_off(axl_usage: Option<&str>, cfg: &ClientConfig) -> Option<&'static str> {
    if let Some(v) = axl_usage {
        let v = v.trim();
        if ["off", "0", "false", "no"]
            .iter()
            .any(|o| v.eq_ignore_ascii_case(o))
        {
            return Some("AXL_USAGE=off");
        }
    }
    match cfg.usage.as_ref().and_then(|u| u.enabled) {
        Some(false) => Some("[usage] enabled = false"),
        _ => None,
    }
}

/// A missing file is the default config; a malformed one is an error.
pub fn load_config(dir: &AxlDir) -> Result<ClientConfig, ClientError> {
    match std::fs::read_to_string(dir.config_path()) {
        Ok(text) => toml::from_str(&text).map_err(|e| ClientError::Toml(e.to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ClientConfig::default()),
        Err(e) => Err(e.into()),
    }
}

pub fn save_config(dir: &AxlDir, cfg: &ClientConfig) -> Result<(), ClientError> {
    let text = toml::to_string(cfg).map_err(|e| ClientError::Toml(e.to_string()))?;
    dir.ensure()?;
    write_private(&dir.config_path(), text.as_bytes())?;
    Ok(())
}

/// `AXL_SERVER_URL` (when not blank), then `config.toml`'s `server_url`, then the default.
/// Never ends in `/`. An unreadable or malformed config fails open to the default, with a
/// one-line warning on stderr (stdout is the bridge's MCP channel).
pub fn resolve_server_url(env_url: Option<&str>, dir: &AxlDir) -> String {
    let (url, warning) = resolve_server_url_with_warning(env_url, dir);
    if let Some(w) = warning {
        eprintln!("{w}");
    }
    url
}

/// As `resolve_server_url`, but returns the warning instead of printing it. The config is
/// only read (and so only warned about) when the environment does not decide the URL.
pub fn resolve_server_url_with_warning(
    env_url: Option<&str>,
    dir: &AxlDir,
) -> (String, Option<String>) {
    let non_blank = |s: &str| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    let mut warning = None;
    let url = env_url
        .and_then(non_blank)
        .or_else(|| match load_config(dir) {
            Ok(c) => c.server_url.as_deref().and_then(non_blank),
            Err(e) => {
                warning = Some(format!("axl: ignoring ~/.axl/config.toml: {e}"));
                None
            }
        })
        .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
    (url.trim_end_matches('/').to_string(), warning)
}

/// `AXL_TOKEN` (when not blank), then `~/.axl/token`.
pub fn read_token(env_token: Option<&str>, dir: &AxlDir) -> Result<String, String> {
    if let Some(t) = env_token.map(str::trim).filter(|t| !t.is_empty()) {
        return Ok(t.to_string());
    }
    let missing = "no token (set AXL_TOKEN or write ~/.axl/token)".to_string();
    match std::fs::read_to_string(dir.token_path()) {
        Ok(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
        Ok(_) => Err(missing),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(missing),
        Err(e) => Err(format!("cannot read ~/.axl/token: {e}")),
    }
}

pub fn write_token(dir: &AxlDir, token: &str) -> Result<(), ClientError> {
    dir.ensure()?;
    write_private(&dir.token_path(), token.as_bytes())?;
    Ok(())
}

/// Whether a token file existed.
pub fn remove_token(dir: &AxlDir) -> Result<bool, ClientError> {
    match std::fs::remove_file(dir.token_path()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Writes `<path>.tmp` with mode 0600, then renames it over `path`.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // A name of its own per write (process id and a per-process counter), so concurrent hooks
    // writing the same status.json or cursor never share, delete or rename each other's file.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.{seq}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::write_private;

    /// Concurrent hooks write the same status.json or cursor: every write lands whole and none
    /// fails, because each one writes its own temporary file before the rename.
    #[test]
    fn concurrent_private_writes_do_not_race() {
        let dir = std::env::temp_dir().join(format!("axl-wp-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("status.json");
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let body = format!("writer{i} ").repeat(4096);
                    for _ in 0..25 {
                        write_private(&path, body.as_bytes()).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("a write failed");
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let first = text.split(' ').next().unwrap().to_string();
        assert_eq!(text, format!("{first} ").repeat(4096), "a torn write");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "temporary files left behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
