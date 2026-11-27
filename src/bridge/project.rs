//! Project resolution for a bridge session (spec §6 "Projects"): `AXL_PROJECT`, then the
//! nearest `.axl` file, then the normalised git `origin`, else no project. Never an
//! error: anything unusable is logged once and treated as no project.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::service::identity::{PROJECT_KEY_MAX, check_label};

/// The file that pins a directory tree to a project.
pub const PROJECT_FILE: &str = ".axl";

/// The environment variables the bridge reads, captured once (tests build their own).
#[derive(Debug, Clone, Default)]
pub struct Env(HashMap<String, String>);

impl Env {
    pub fn from_process() -> Self {
        Self(std::env::vars().collect())
    }

    pub fn from_pairs<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }

    /// The variable's trimmed value; `None` when unset or blank.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(|v| v.trim()).filter(|v| !v.is_empty())
    }
}

#[derive(Debug, Deserialize)]
struct ProjectFile {
    project: String,
    #[allow(dead_code)]
    display_name: Option<String>,
}

/// The session's project key for `cwd`, or `None`.
pub fn resolve_project(cwd: &Path, env: &Env) -> Option<String> {
    if let Some(key) = env.get("AXL_PROJECT") {
        return usable(key, "AXL_PROJECT");
    }
    if let Some(file) = nearest_project_file(cwd) {
        let source = file.display().to_string();
        let parsed = std::fs::read_to_string(&file)
            .map_err(|e| e.to_string())
            .and_then(|text| toml::from_str::<ProjectFile>(&text).map_err(|e| e.to_string()));
        return match parsed {
            Ok(f) => usable(f.project.trim(), &source),
            Err(e) => {
                eprintln!("axl: ignoring {source}: {e}; running with no project");
                None
            }
        };
    }
    git_origin(cwd).and_then(|url| normalize_origin(&url))
}

/// `key` if it is a valid project key; else logs why and gives `None`.
fn usable(key: &str, source: &str) -> Option<String> {
    match check_label("project key", key, PROJECT_KEY_MAX, true) {
        Ok(()) if !key.is_empty() => Some(key.to_string()),
        Ok(()) => {
            eprintln!("axl: {source} names an empty project; running with no project");
            None
        }
        Err(e) => {
            eprintln!("axl: ignoring project from {source}: {e}; running with no project");
            None
        }
    }
}

fn nearest_project_file(cwd: &Path) -> Option<std::path::PathBuf> {
    cwd.ancestors()
        .map(|dir| dir.join(PROJECT_FILE))
        .find(|p| p.is_file())
}

/// `git config --get remote.origin.url` in `cwd`, if git is installed and there is one.
fn git_origin(cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["config", "--get", "remote.origin.url"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8(out.stdout).ok()?;
    let url = url.trim();
    (!url.is_empty()).then(|| url.to_string())
}

/// A git remote URL as `host/path`: lowercase, no user, no port, no `.git` suffix. SSH
/// (`git@host:owner/repo`), `ssh://`, `https://` and `git://` forms of one repository
/// collapse to the same key. Local paths and anything unparsable give `None`.
pub fn normalize_origin(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() || url.chars().any(char::is_whitespace) {
        return None;
    }
    let (authority, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            if scheme.eq_ignore_ascii_case("file") {
                return None;
            }
            rest.split_once('/')?
        }
        // scp-like `[user@]host:path`; a `/` before the `:` means a local path.
        None => {
            let (authority, path) = url.split_once(':')?;
            if authority.contains('/') {
                return None;
            }
            (authority, path)
        }
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split_once(':').map_or(host, |(h, _)| h);
    // Lowercase before stripping, so `.GIT` collapses like `.git`.
    let path = path.to_lowercase();
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    if host.is_empty() || path.is_empty() || path.split('/').any(str::is_empty) {
        return None;
    }
    let key = format!("{host}/{path}").to_lowercase();
    check_label("project key", &key, PROJECT_KEY_MAX, true)
        .ok()
        .map(|()| key)
}
