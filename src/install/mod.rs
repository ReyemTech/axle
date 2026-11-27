//! Shared foundation for the harness editors: which harnesses exist, the `axl` path to
//! write, planned edits with diffs and backups, marker blocks, and the manifest.

pub mod fsutil;
pub mod manifest;
pub mod paths;

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub use fsutil::{
    BLOCK_END, BLOCK_START, FileEdit, LinkEdit, Plan, apply, remove_block, render_diff,
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
