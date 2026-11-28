//! Shared foundation for the harness editors: which harnesses exist, the `axl` path to
//! write, planned edits with diffs and backups, marker blocks, and the manifest.

pub mod claude_code;
pub mod codex;
pub mod fsutil;
pub mod jsonc;
pub mod manifest;
pub mod opencode;
pub mod paths;

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub use fsutil::{
    BLOCK_END, BLOCK_START, BlockError, FileEdit, LinkEdit, Plan, apply, remove_block, render_diff,
    upsert_block,
};
pub use manifest::{EntryKind, Manifest, ManifestEntry};
pub use paths::axl_path;

/// The one source of the agent-facing instruction text (Claude Code's SKILL.md and the
/// Codex / OpenCode AGENTS.md blocks).
pub const INSTRUCTIONS: &str = include_str!("instructions.md");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    ClaudeCode,
    Codex,
    Opencode,
}

impl Harness {
    pub const ALL: [Harness; 3] = [Harness::ClaudeCode, Harness::Codex, Harness::Opencode];

    pub fn name(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
            Harness::Opencode => "opencode",
        }
    }

    /// True when the harness's config directory exists under `home`, or its binary is an
    /// executable file in a `path_env` directory.
    pub fn detect(self, home: &Path, path_env: Option<&OsStr>) -> bool {
        let (dir, bin) = match self {
            Harness::ClaudeCode => (home.join(".claude"), "claude"),
            Harness::Codex => (home.join(".codex"), "codex"),
            Harness::Opencode => (home.join(".config").join("opencode"), "opencode"),
        };
        if dir.is_dir() {
            return true;
        }
        path_env.is_some_and(|p| {
            std::env::split_paths(p).any(|d| {
                std::fs::metadata(d.join(bin))
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            })
        })
    }
}

/// Matches `axl hook <event>` and `<any path>/axl hook <event>` (path may be quoted).
pub fn is_axl_hook_command(cmd: &str) -> Option<crate::hook::Event> {
    let mut parts = cmd.split_whitespace().rev();
    let event = parts.next()?;
    if parts.next()? != "hook" {
        return None;
    }
    let rest = cmd.trim_end();
    let rest = rest[..rest.len() - event.len()].trim_end();
    let rest = rest[..rest.len() - "hook".len()].trim();
    let prog = rest.trim_matches(['"', '\'']);
    let base = prog.rsplit('/').next()?;
    if base != "axl" {
        return None;
    }
    crate::hook::Event::from_str(event, true).ok()
}

/// Why a harness editor could not produce a plan.
#[derive(Debug)]
pub enum EditError {
    /// The file is not in a shape the editor can change without rewriting what the user
    /// wrote. `snippet` is what to add by hand.
    NeedsManual {
        file: PathBuf,
        reason: String,
        snippet: String,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::NeedsManual {
                file,
                reason,
                snippet,
            } => write!(
                f,
                "manual step needed in {}: {reason}\nadd this yourself:\n{snippet}",
                file.display()
            ),
            EditError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EditError {}

impl From<std::io::Error> for EditError {
    fn from(e: std::io::Error) -> Self {
        EditError::Io(e)
    }
}
