//! `axl uninstall`: undo what `axl install` recorded in the manifest, and optionally
//! revoke this machine's token and remove the local service.

use std::io;
use std::path::Path;

use time::OffsetDateTime;

use super::install::{InstallEnv, display_name, redact};
use super::login::logout;
use crate::client::{AxlDir, ClientError};
use crate::daemon::purge_db;
use crate::install::{Harness, Manifest, Plan, apply, claude_code, codex, opencode};

pub struct UninstallArgs {
    /// Empty means every harness.
    pub harness: Vec<Harness>,
    /// Also stop and remove the local background service.
    pub service: bool,
    /// With `service`: also delete the local database.
    pub purge: bool,
    /// Revoke this machine's token on the server and forget it.
    pub revoke: bool,
}

fn usage(msg: impl Into<String>) -> ClientError {
    ClientError::Usage(msg.into())
}

/// Removes what install recorded, one harness at a time, saving the manifest after each so
/// a failure part-way leaves it matching the disk. Problems in one step do not stop the
/// later ones (a failed revoke should not leave the service running); the first is
/// returned at the end.
pub async fn uninstall(env: &mut InstallEnv<'_>, args: UninstallArgs) -> Result<(), ClientError> {
    if args.purge && !args.service {
        return Err(usage(
            "--purge deletes the local database and only applies with --service",
        ));
    }
    let dir = AxlDir::new(&env.home);
    // Never save over a manifest that could not be read: it is the only record.
    let mut manifest = Manifest::load(&dir).map_err(|e| usage(e.to_string()))?;

    let mut problems: Vec<String> = Vec::new();

    for h in Harness::ALL {
        if !args.harness.is_empty() && !args.harness.contains(&h) {
            continue;
        }
        match uninstall_harness(env, &dir, &mut manifest, h) {
            Ok(line) => {
                let _ = writeln!(env.out, "{}", redact(&line));
            }
            Err(msg) => {
                let msg = format!("{}: {msg}", display_name(h));
                let _ = writeln!(env.out, "{}", redact(&format!("needs manual step: {msg}")));
                problems.push(msg);
            }
        }
    }

    // Before the service goes: a local server must still be up to revoke its own token.
    if args.revoke
        && let Err(e) = logout(&env.home, true, env.out).await
    {
        problems.push(format!("could not revoke this machine's token: {e}"));
    }

    if args.service
        && let Err(e) = remove_service(env, &dir, args.purge)
    {
        problems.push(e);
    }

    match problems.into_iter().next() {
        None => Ok(()),
        Some(first) => Err(usage(redact(&first))),
    }
}

/// Plans, applies and un-records one harness. `Ok` is the summary line; `Err` the reason
/// nothing (or not everything) was removed, with the manifest entries left in place.
fn uninstall_harness(
    env: &InstallEnv<'_>,
    dir: &AxlDir,
    manifest: &mut Manifest,
    h: Harness,
) -> Result<String, String> {
    let plan = match h {
        Harness::ClaudeCode => claude_code::plan_uninstall(&env.home, manifest),
        Harness::Codex => codex::plan_uninstall(&env.home, manifest),
        Harness::Opencode => opencode::plan_uninstall(&env.home, manifest),
    }
    .map_err(|e| e.to_string())?;

    let changed = plan.edits.iter().any(|e| e.before != e.after)
        || plan.links.iter().any(|l| l.target != l.before);
    apply_removal(&plan, dir, env.now).map_err(|e| {
        format!(
            "failed: {e}. The manifest still lists what is left; fix the problem and re-run \
             ({})",
            dir.manifest_path().display()
        )
    })?;
    if h == Harness::ClaudeCode {
        prune_empty_dirs(&dir.integrations_dir().join("claude-code"));
        let _ = std::fs::remove_dir(dir.integrations_dir());
        for l in &plan.links {
            if let Some(parent) = l.path.parent() {
                // Only succeeds on an empty directory.
                let _ = std::fs::remove_dir(parent);
            }
        }
    }

    if manifest.remove_harness(h) > 0 {
        manifest.persist(dir).map_err(|e| {
            format!(
                "removed, but could not update {}: {e}",
                dir.manifest_path().display()
            )
        })?;
    }
    Ok(format!(
        "{}: {}",
        display_name(h),
        if changed {
            "removed"
        } else {
            "nothing to remove"
        }
    ))
}

/// Applies a plan. Files Axle wrote under `~/.axl/integrations` are deleted outright
/// (they are Axle's own, and a backup of them would only keep the directory alive);
/// everything else goes through `apply`, which backs up before changing.
fn apply_removal(plan: &Plan, dir: &AxlDir, now: OffsetDateTime) -> io::Result<()> {
    let integrations = dir.integrations_dir();
    let (own, rest): (Vec<_>, Vec<_>) = plan
        .edits
        .iter()
        .cloned()
        .partition(|e| e.after.is_none() && e.path.starts_with(&integrations));
    apply(
        &Plan {
            edits: rest,
            links: plan.links.clone(),
            notes: Vec::new(),
        },
        now,
    )?;
    for e in own {
        match std::fs::remove_file(&e.path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Removes `path` and every directory below it that is empty (never a symlink, never a
/// directory holding a file).
fn prune_empty_dirs(path: &Path) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            prune_empty_dirs(&e.path());
        }
    }
    let _ = std::fs::remove_dir(path);
}

fn remove_service(env: &mut InstallEnv<'_>, dir: &AxlDir, purge: bool) -> Result<(), String> {
    let service = (env.service)().map_err(|e| e.to_string())?;
    service
        .uninstall()
        .map_err(|e| format!("could not remove the service: {e}"))?;
    let _ = writeln!(env.out, "Service removed");
    if purge {
        purge_db(dir).map_err(|e| format!("could not delete the database: {e}"))?;
        let _ = writeln!(env.out, "Database deleted");
    } else {
        let _ = writeln!(
            env.out,
            "Database kept at {} (--purge deletes it)",
            dir.db_path().display()
        );
    }
    Ok(())
}
