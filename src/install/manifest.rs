//! What install changed, so uninstall can undo exactly that. Lives at `~/.axl/install-manifest.json`.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::Harness;
use super::fsutil::write_atomic;
use crate::client::AxlDir;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    Added,
    Replaced,
    Adopted,
    Block,
    Link,
    CreatedFile,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub harness: Harness,
    pub file: PathBuf,
    pub kind: EntryKind,
    pub key: String,
    pub original: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub entries: Vec<ManifestEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
        }
    }
}

impl Manifest {
    /// A missing manifest is an empty one. An unreadable, corrupt or newer-version
    /// manifest is an error naming the file: replacing it would lose the record of what
    /// install changed, so callers must stop rather than save over it.
    pub fn load(dir: &AxlDir) -> io::Result<Manifest> {
        let path = dir.manifest_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Manifest::default()),
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("cannot read {}: {e}", path.display()),
                ));
            }
        };
        let bad = |why: String| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is {why}; fix or move it aside (it records what `axl install` changed), then re-run",
                    path.display()
                ),
            )
        };
        let m: Manifest =
            serde_json::from_str(&text).map_err(|e| bad(format!("not a valid manifest ({e})")))?;
        if m.version != 1 {
            return Err(bad(format!(
                "manifest version {}, which this axl does not understand (it supports 1); upgrade axl",
                m.version
            )));
        }
        Ok(m)
    }

    pub fn save(&self, dir: &AxlDir) -> io::Result<()> {
        dir.ensure()?;
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        write_atomic(&dir.manifest_path(), text.as_bytes(), 0o600)
    }

    /// Records an entry unless one with the same `(harness, file, key)` exists. The first
    /// record wins: it holds the pre-install `original`, which a re-run must not overwrite
    /// with already-edited state.
    pub fn record(&mut self, e: ManifestEntry) {
        let dup = self
            .entries
            .iter()
            .any(|x| x.harness == e.harness && x.file == e.file && x.key == e.key);
        if !dup {
            self.entries.push(e);
        }
    }
}
