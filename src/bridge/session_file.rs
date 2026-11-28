//! `~/.axl/sessions/<harness-pid>.json` (spec §6): how hooks and adapters, which are also
//! children of the harness, find the bridge's session without asking the model.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::SessionId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub project_key: Option<String>,
    pub server_url: String,
    pub harness: String,
}

/// `<home>/.axl/sessions/<harness_pid>.json`.
pub fn path(home: &Path, harness_pid: u32) -> PathBuf {
    home.join(".axl")
        .join("sessions")
        .join(format!("{harness_pid}.json"))
}

/// `<pid>.seen`: message ids the hooks already surfaced for whatever session this pid runs.
pub fn seen_path(session_file: &Path) -> PathBuf {
    session_file.with_extension("seen")
}

/// `<pid>.<session>.nudged`: the hooks gave this session its one-time "no task" reminder.
pub fn nudged_path(session_file: &Path, session: SessionId) -> PathBuf {
    session_file.with_extension(format!("{session}.nudged"))
}

/// `<pid>.<session>.leases`: written when this session was seen holding a lease (or its
/// server has no lease list); the hooks skip the lookup while it is fresh.
pub fn lease_cache_path(session_file: &Path, session: SessionId) -> PathBuf {
    session_file.with_extension(format!("{session}.leases"))
}

/// A written session file; `remove` (or drop) deletes it, and the hooks' files for it, if
/// it is still ours.
#[derive(Debug)]
pub struct SessionFile {
    path: PathBuf,
    session_id: SessionId,
}

impl SessionFile {
    /// Writes `info` to `path` (mode 0600, parent directories 0700), atomically replacing
    /// any file a previous bridge of the same harness left.
    pub fn write(path: PathBuf, info: &SessionInfo) -> std::io::Result<Self> {
        let dir = path
            .parent()
            .ok_or_else(|| std::io::Error::other("session file path has no parent"))?;
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let tmp = dir.join(format!(".{}.tmp", info.session_id));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(info).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(Self {
            path,
            session_id: info.session_id,
        })
    }

    /// Deletes the file unless another bridge has since replaced it with its own session.
    pub fn remove(&self) {
        let ours = std::fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SessionInfo>(&b).ok())
            .is_some_and(|info| info.session_id == self.session_id);
        if !ours {
            return;
        }
        if let Err(e) = std::fs::remove_file(&self.path) {
            eprintln!("axl: cannot remove {}: {e}", self.path.display());
        }
        for companion in [
            seen_path(&self.path),
            nudged_path(&self.path, self.session_id),
            lease_cache_path(&self.path, self.session_id),
        ] {
            // Absent is the common case; a leftover is harmless, so no line for it.
            let _ = std::fs::remove_file(companion);
        }
    }
}

impl Drop for SessionFile {
    fn drop(&mut self) {
        self.remove();
    }
}
