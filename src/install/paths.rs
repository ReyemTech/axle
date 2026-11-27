//! Which `axl` path harness configs should point at.
//!
//! Symlinks are never resolved: Homebrew's `/opt/homebrew/bin/axl` points into
//! `Cellar/axle/<version>/`, which `brew upgrade` deletes.

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// The first `axl` on PATH that is the same file as `current_exe` (same dev and inode),
/// else `current_exe` as reported. A versioned `*/Cellar/axle/<ver>/bin/axl` result maps
/// to `<prefix>/bin/axl`.
pub fn axl_path(current_exe: &Path, path_env: Option<&OsStr>) -> PathBuf {
    let chosen = same_file_on_path(current_exe, path_env).unwrap_or_else(|| current_exe.into());
    let chosen = if chosen.is_absolute() {
        chosen
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&chosen))
            .unwrap_or(chosen)
    };
    cellar_to_prefix(&chosen).unwrap_or(chosen)
}

fn same_file_on_path(current_exe: &Path, path_env: Option<&OsStr>) -> Option<PathBuf> {
    let want = std::fs::metadata(current_exe).ok()?;
    let key = (want.dev(), want.ino());
    std::env::split_paths(path_env?)
        .filter(|d| d.is_absolute())
        .map(|d| d.join("axl"))
        .find(|cand| {
            std::fs::metadata(cand)
                .map(|m| (m.dev(), m.ino()) == key)
                .unwrap_or(false)
        })
}

fn cellar_to_prefix(path: &Path) -> Option<PathBuf> {
    let comps: Vec<Component> = path.components().collect();
    let n = comps.len();
    // .../Cellar/axle/<ver>/bin/axl
    if n < 6 {
        return None;
    }
    let tail = &comps[n - 5..];
    let is = |c: &Component, s: &str| c.as_os_str() == OsStr::new(s);
    if is(&tail[0], "Cellar") && is(&tail[1], "axle") && is(&tail[3], "bin") && is(&tail[4], "axl")
    {
        let prefix: PathBuf = comps[..n - 5].iter().collect();
        return Some(prefix.join("bin").join("axl"));
    }
    None
}
