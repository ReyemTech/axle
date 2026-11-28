//! Per-transcript cursors and the status file under `<home>/.axl/sessions/usage`.
//! Written atomically (tmp + rename), files 0600, directory 0700. Nothing here holds the token.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::config::write_private;
use crate::domain::{Billing, SessionId};
use crate::usage::parsers::ParserState;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub offset: u64,
    pub parser: String,
    pub state: ParserState,
    pub billing: Option<Billing>,
    pub session_id: Option<SessionId>,
    pub server_url: Option<String>,
    /// RFC 3339: records older than this are dropped (the session file's mtime at creation).
    pub floor: Option<String>,
    /// Discard bytes up to the next newline before parsing (a line over the size cap).
    #[serde(default)]
    pub skip_partial: bool,
    /// The "does not recognise this log" line was printed.
    #[serde(default)]
    pub warned: bool,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub last_success_at: Option<String>,
    #[serde(default)]
    pub billing: BTreeMap<String, Billing>,
}

pub fn dir(home: &Path) -> PathBuf {
    home.join(".axl").join("sessions").join("usage")
}

pub fn path_for(home: &Path, transcript: &Path) -> PathBuf {
    let digest = Sha256::digest(transcript.as_os_str().as_encoded_bytes());
    let mut name = String::with_capacity(70);
    for b in digest {
        let _ = write!(name, "{b:02x}");
    }
    name.push_str(".json");
    dir(home).join(name)
}

pub fn status_path(home: &Path) -> PathBuf {
    dir(home).join("status.json")
}

/// Missing or corrupt: None.
pub fn load(home: &Path, transcript: &Path) -> Option<Cursor> {
    serde_json::from_slice(&std::fs::read(path_for(home, transcript)).ok()?).ok()
}

pub fn save(home: &Path, c: &Cursor) -> io::Result<()> {
    write_in_dir(home, &path_for(home, &c.path), c)
}

pub fn load_status(home: &Path) -> Status {
    std::fs::read(status_path(home))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_status(home: &Path, s: &Status) -> io::Result<()> {
    write_in_dir(home, &status_path(home), s)
}

fn write_in_dir<T: Serialize>(home: &Path, path: &Path, value: &T) -> io::Result<()> {
    let d = dir(home);
    std::fs::create_dir_all(&d)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
    }
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    write_private(path, &bytes)
}

/// Deletes cursor files whose mtime is older than `max_age`; returns how many. The status
/// file and anything that is not a `.json` file are left alone.
pub fn prune(home: &Path, now: SystemTime, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir(home)) else {
        return 0;
    };
    let status = status_path(home);
    let mut removed = 0;
    for e in entries.flatten() {
        let p = e.path();
        if p == status || p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > max_age);
        if old && std::fs::remove_file(&p).is_ok() {
            removed += 1;
        }
    }
    removed
}
