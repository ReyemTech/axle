//! Planned edits and how they are shown and applied: unified diffs, one backup per file
//! per run, atomic writes that keep the original mode, and `# >>> axle >>>` blocks.

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use similar::TextDiff;
use time::OffsetDateTime;

pub const BLOCK_START: &str = "# >>> axle >>>";
pub const BLOCK_END: &str = "# <<< axle <<<";

/// One file's change. `None` means absent (before) or delete (after).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEdit {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// A symlink to create or replace (`target: Some`) or remove (`target: None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkEdit {
    pub path: PathBuf,
    pub target: Option<PathBuf>,
    pub before: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub edits: Vec<FileEdit>,
    pub links: Vec<LinkEdit>,
    pub notes: Vec<String>,
}

/// Unified diff of every edit that changes something, then link lines, then notes.
pub fn render_diff(plan: &Plan) -> String {
    let mut out = String::new();
    for e in plan.edits.iter().filter(|e| e.before != e.after) {
        let name = e.path.display().to_string();
        let old = if e.before.is_some() {
            format!("a/{name}")
        } else {
            "/dev/null".to_string()
        };
        let new = if e.after.is_some() {
            format!("b/{name}")
        } else {
            "/dev/null".to_string()
        };
        let diff = TextDiff::from_lines(
            e.before.as_deref().unwrap_or(""),
            e.after.as_deref().unwrap_or(""),
        );
        out.push_str(&format!(
            "{}",
            diff.unified_diff().context_radius(3).header(&old, &new)
        ));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    for l in plan.links.iter().filter(|l| l.target != l.before) {
        match &l.target {
            Some(t) => out.push_str(&format!("link {} -> {}\n", l.path.display(), t.display())),
            None => out.push_str(&format!("unlink {}\n", l.path.display())),
        }
    }
    for n in &plan.notes {
        out.push_str(&format!("note: {n}\n"));
    }
    out
}

/// Writes the plan. Edits whose before and after match, or whose file already holds the
/// wanted content, are skipped. An existing file is copied once per run to
/// `<file>.axl-backup-<YYYYMMDDTHHMMSSZ>` before its first change.
pub fn apply(plan: &Plan, now: OffsetDateTime) -> io::Result<()> {
    let stamp = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let mut backed_up: HashSet<PathBuf> = HashSet::new();
    for e in &plan.edits {
        if e.before == e.after {
            continue;
        }
        let exists = e.path.symlink_metadata().is_ok();
        if let Some(after) = &e.after {
            if exists && std::fs::read_to_string(&e.path).ok().as_deref() == Some(after) {
                continue;
            }
        } else if !exists {
            continue;
        }
        if exists && backed_up.insert(e.path.clone()) {
            let mut name = e.path.as_os_str().to_owned();
            name.push(format!(".axl-backup-{stamp}"));
            let backup = PathBuf::from(name);
            if backup.symlink_metadata().is_err() {
                std::fs::copy(&e.path, &backup)?;
            }
        }
        match &e.after {
            Some(after) => write_atomic(&e.path, after.as_bytes(), 0o644)?,
            None => std::fs::remove_file(&e.path)?,
        }
    }
    for l in &plan.links {
        apply_link(l)?;
    }
    Ok(())
}

fn apply_link(l: &LinkEdit) -> io::Result<()> {
    let current = std::fs::read_link(&l.path).ok();
    match &l.target {
        Some(target) => {
            if current.as_ref() == Some(target) {
                return Ok(());
            }
            if l.path.symlink_metadata().is_ok() {
                std::fs::remove_file(&l.path)?;
            }
            if let Some(parent) = l.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(target, &l.path)
        }
        None => {
            if current.is_some() {
                std::fs::remove_file(&l.path)?;
            }
            Ok(())
        }
    }
}

/// Writes through a symlinked path to its target (dotfile managers), via a temp file in the
/// same directory and a rename. An existing file keeps its mode; a new one gets
/// `default_mode`.
pub fn write_atomic(path: &Path, bytes: &[u8], default_mode: u32) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let real = if path
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        std::fs::canonicalize(path)?
    } else {
        path.to_path_buf()
    };
    if let Some(parent) = real.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mode = std::fs::metadata(&real)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(default_mode);
    let mut tmp = real.as_os_str().to_owned();
    tmp.push(".axl-tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        // `mode()` is masked by umask; restore the exact original.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, &real)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn block_range(lines: &[&str]) -> Option<(usize, usize)> {
    let start = lines
        .iter()
        .position(|l| l.trim_end_matches(['\r', '\n']) == BLOCK_START)?;
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_end_matches(['\r', '\n']) == BLOCK_END)?
        + start
        + 1;
    Some((start, end))
}

/// Puts `body` between the axle markers: replaced in place when a block exists, otherwise
/// appended after one blank line. Idempotent.
pub fn upsert_block(text: &str, body: &str) -> String {
    let block = format!(
        "{BLOCK_START}\n{}\n{BLOCK_END}\n",
        body.trim_end_matches('\n')
    );
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if let Some((start, end)) = block_range(&lines) {
        let mut out: String = lines[..start].concat();
        out.push_str(&block);
        out.push_str(&lines[end + 1..].concat());
        return out;
    }
    let mut out = text.to_string();
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(&block);
    out
}

/// Removes the axle block and the one blank separator line before it. Undoes
/// `upsert_block` exactly when the original text ended in a newline (or was empty).
pub fn remove_block(text: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some((mut start, end)) = block_range(&lines) else {
        return text.to_string();
    };
    if start > 0 && lines[start - 1].trim_end_matches(['\r', '\n']).is_empty() {
        start -= 1;
    }
    let mut out: String = lines[..start].concat();
    out.push_str(&lines[end + 1..].concat());
    out
}
