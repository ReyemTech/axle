//! `~/.coord/sessions/<harness-pid>.json` (spec §6): how hooks and adapters, which are also
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

/// `<home>/.coord/sessions/<harness_pid>.json`.
pub fn path(home: &Path, harness_pid: u32) -> PathBuf {
    home.join(".coord")
        .join("sessions")
        .join(format!("{harness_pid}.json"))
}

/// A written session file; `remove` (or drop) deletes it if it is still ours.
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
        if ours && let Err(e) = std::fs::remove_file(&self.path) {
            eprintln!("coord: cannot remove {}: {e}", self.path.display());
        }
    }
}

impl Drop for SessionFile {
    fn drop(&mut self) {
        self.remove();
    }
}
