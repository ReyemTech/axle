//! What install changed, so uninstall can undo exactly that. Lives at `~/.axl/manifest.json`.

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
    /// A missing or unreadable manifest is an empty one.
    pub fn load(dir: &AxlDir) -> Manifest {
        std::fs::read_to_string(dir.manifest_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
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
