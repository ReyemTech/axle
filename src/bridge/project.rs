//! Project resolution for a bridge session (spec §6 "Projects"): `AXL_PROJECT`, then the
//! nearest `.axl` file, then the normalised git `origin`, else a local project
//! `local/<host>/<folder>` (docs/superpowers/specs/2026-11-28-local-projects-design.md).
//! Never an error: an unusable pin is logged once and treated as no project.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

/// What the bridge worked out for a session's working directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detected {
    /// The project key to send; `None` only for an unusable pin.
    pub key: Option<String>,
    /// `folder_hash` of the folder the key came from (the git top level, else the working
    /// directory). `None` for a pin (`AXL_PROJECT` or `.axl`), which never takes part in
    /// local-project matching or adoption.
    pub folder: Option<String>,
}

pub use crate::service::identity::LOCAL_PREFIX;

/// The session's project key for `cwd` on `host`, or `None`.
pub fn resolve_project(cwd: &Path, env: &Env, host: &str) -> Option<String> {
    detect(cwd, env, host).key
}

/// Resolution in order: `AXL_PROJECT`, the nearest `.axl`, the git `origin`, else
/// `local/<host>/<folder>` for the git top level (or `cwd` outside a repository).
pub fn detect(cwd: &Path, env: &Env, host: &str) -> Detected {
    if let Some(key) = pinned(cwd, env) {
        return Detected { key, folder: None };
    }
    let root = project_root(cwd);
    let folder = Some(folder_hash(&root));
    let key = git_origin(cwd)
        .and_then(|url| normalize_origin(&url))
        .or_else(|| local_key(host, &root));
    Detected { key, folder }
}

/// The folder a session's project belongs to: the git top level, else `cwd`.
pub fn project_root(cwd: &Path) -> PathBuf {
    git_toplevel(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

/// The pin for `cwd`: `Some(key)` when `AXL_PROJECT` or an `.axl` file decides it (`None`
/// inside when that pin is unusable); `None` when nothing pins it.
fn pinned(cwd: &Path, env: &Env) -> Option<Option<String>> {
    if let Some(key) = env.get("AXL_PROJECT") {
        return Some(usable(key, "AXL_PROJECT"));
    }
    let file = nearest_project_file(cwd)?;
    let source = file.display().to_string();
    Some(match read_project_file(&file) {
        Ok(key) => usable(key.trim(), &source),
        Err(e) => {
            eprintln!("axl: ignoring {source}: {e}; running with no project");
            None
        }
    })
}

/// The `project` value of an `.axl` file.
pub fn read_project_file(file: &Path) -> Result<String, String> {
    std::fs::read_to_string(file)
        .map_err(|e| e.to_string())
        .and_then(|text| toml::from_str::<ProjectFile>(&text).map_err(|e| e.to_string()))
        .map(|f| f.project)
}

/// `axl project pin <key>`: writes (or updates) `project = "<key>"` in the `.axl` of the
/// git top level, else of `cwd`, keeping any other content. Returns the file written.
pub fn pin(cwd: &Path, key: &str) -> Result<PathBuf, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("the project key is empty".to_string());
    }
    check_label("project key", key, PROJECT_KEY_MAX, true).map_err(|e| e.to_string())?;
    let file = project_root(cwd).join(PROJECT_FILE);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", file.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("cannot parse {}: {e}", file.display()))?;
    doc["project"] = toml_edit::value(key);
    std::fs::write(&file, doc.to_string())
        .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(file)
}

/// `axl project pin --show`: the key a session started in `cwd` would use, and why.
pub fn describe(cwd: &Path, env: &Env, host: &str) -> String {
    if env.get("AXL_PROJECT").is_some() {
        return match detect(cwd, env, host).key {
            Some(k) => format!("{k} (from AXL_PROJECT)"),
            None => "no project (AXL_PROJECT is not a usable key)".to_string(),
        };
    }
    if let Some(file) = nearest_project_file(cwd) {
        return match detect(cwd, env, host).key {
            Some(k) => format!("{k} (pinned by {})", file.display()),
            None => format!("no project ({} is not usable)", file.display()),
        };
    }
    match detect(cwd, env, host).key {
        Some(k) if k.starts_with(LOCAL_PREFIX) => format!(
            "{k} (local project: this folder has no git remote; pin a key with `axl project pin <key>`)"
        ),
        Some(k) => format!("{k} (from the git origin remote)"),
        None => "no project".to_string(),
    }
}

/// `local/<host>/<folder>`, both parts normalised by `local_segment`; `None` only if the
/// result is not a valid key.
pub fn local_key(host: &str, root: &Path) -> Option<String> {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let key = format!(
        "{LOCAL_PREFIX}{}/{}",
        local_segment(host, "host"),
        local_segment(&name, "folder")
    );
    check_label("project key", &key, PROJECT_KEY_MAX, true)
        .ok()
        .map(|()| key)
}

/// Longest folder (or host) segment of a local key, in bytes.
pub const LOCAL_SEGMENT_MAX: usize = 64;

/// A folder or host name as a key segment: lowercase ASCII letters, digits, `.`, `_` and
/// `-`; every other run of characters becomes one `-`; no leading or trailing `-` or `.`;
/// at most 64 bytes; `fallback` when nothing is left.
pub fn local_segment(raw: &str, fallback: &str) -> String {
    let mut out = String::new();
    for c in raw.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let mut out = out.trim_matches(['-', '.']).to_string();
    out.truncate(LOCAL_SEGMENT_MAX);
    let out = out.trim_end_matches(['-', '.']);
    if out.is_empty() {
        fallback.to_string()
    } else {
        out.to_string()
    }
}

/// The first 16 hex digits of SHA-256 of the folder's absolute path (symlinks resolved
/// when possible). Sent instead of the path, so the server can tell two folders apart
/// without learning where they are.
pub fn folder_hash(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let path = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Whether `value` looks like a `folder_hash` (16 lowercase hex digits).
pub fn is_folder_hash(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `git rev-parse --show-toplevel` in `cwd`, if git is installed and `cwd` is in a work tree.
fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let top = String::from_utf8(out.stdout).ok()?;
    let top = top.trim();
    (!top.is_empty()).then(|| PathBuf::from(top))
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

/// The `.axl` file that applies to `cwd`, if any.
pub fn nearest_project_file(cwd: &Path) -> Option<PathBuf> {
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
