use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use axle::client::AxlDir;
use axle::install::{
    EntryKind, FileEdit, Harness, INSTRUCTIONS, LinkEdit, Manifest, ManifestEntry, Plan, apply,
    axl_path, is_axl_hook_command, remove_block, render_diff, upsert_block,
};
use time::macros::datetime;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("axl-install-{suffix}"));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn exe_file(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn backups(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.contains(".axl-backup-"))
        .collect();
    v.sort();
    v
}

#[test]
fn axl_path_prefers_path_entry_with_same_inode() {
    let t = Temp::new();
    let real = t.path().join("real/axl-real");
    exe_file(&real);
    let bin = t.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&real, bin.join("axl")).unwrap();
    let other = t.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    exe_file(&other.join("axl")); // different file: must be skipped
    let path_env = std::env::join_paths([&other, &bin]).unwrap();
    assert_eq!(axl_path(&real, Some(&path_env)), bin.join("axl"));
}

#[test]
fn axl_path_falls_back_to_current_exe_uncanonicalised() {
    let t = Temp::new();
    let real = t.path().join("real/axl");
    exe_file(&real);
    let link = t.path().join("link-to-axl");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let empty = t.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert_eq!(axl_path(&link, Some(empty.as_os_str())), link);
    assert_eq!(axl_path(&link, None), link);
}

#[test]
fn cellar_path_maps_to_prefix_bin() {
    assert_eq!(
        axl_path(Path::new("/opt/homebrew/Cellar/axle/0.1.1/bin/axl"), None),
        PathBuf::from("/opt/homebrew/bin/axl")
    );
    assert_eq!(
        axl_path(
            Path::new("/home/linuxbrew/.linuxbrew/Cellar/axle/0.2.0/bin/axl"),
            None
        ),
        PathBuf::from("/home/linuxbrew/.linuxbrew/bin/axl")
    );
    // Not a versioned axle Cellar path: left alone.
    assert_eq!(
        axl_path(Path::new("/opt/homebrew/Cellar/other/1/bin/axl"), None),
        PathBuf::from("/opt/homebrew/Cellar/other/1/bin/axl")
    );
}

#[test]
fn block_upsert_is_idempotent_and_removable() {
    let orig = "# mine\nkeep = 1\n";
    let once = upsert_block(orig, "x");
    assert!(once.contains("# >>> axle >>>\nx\n# <<< axle <<<\n"));
    assert_eq!(upsert_block(&once, "x"), once);
    assert_eq!(remove_block(&once), orig);
}

#[test]
fn block_replaced_in_place_and_empty_file() {
    let text = "a\n\n# >>> axle >>>\nold\n# <<< axle <<<\nz\n";
    assert_eq!(
        upsert_block(text, "new\n"),
        "a\n\n# >>> axle >>>\nnew\n# <<< axle <<<\nz\n"
    );
    let from_empty = upsert_block("", "x");
    assert_eq!(from_empty, "# >>> axle >>>\nx\n# <<< axle <<<\n");
    assert_eq!(remove_block(&from_empty), "");
    assert_eq!(remove_block("no block\n"), "no block\n");
}

#[test]
fn apply_backs_up_once_and_skips_unchanged() {
    let t = Temp::new();
    let f = t.path().join("config.toml");
    std::fs::write(&f, "old\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
    let plan = Plan {
        edits: vec![
            FileEdit {
                path: f.clone(),
                before: Some("old\n".into()),
                after: Some("new\n".into()),
            },
            FileEdit {
                path: t.path().join("untouched"),
                before: Some("same".into()),
                after: Some("same".into()),
            },
        ],
        ..Plan::default()
    };
    apply(&plan, datetime!(2026-11-27 09:00:00 UTC)).unwrap();
    apply(&plan, datetime!(2026-11-27 09:00:00 UTC)).unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "new\n");
    assert_eq!(
        std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        backups(t.path()),
        vec!["config.toml.axl-backup-20261127T090000Z"]
    );
    let b = t.path().join("config.toml.axl-backup-20261127T090000Z");
    assert_eq!(std::fs::read_to_string(b).unwrap(), "old\n");
    assert!(!t.path().join("untouched").exists());
}

#[test]
fn apply_creates_deletes_and_backs_up_once_per_file_per_run() {
    let t = Temp::new();
    let f = t.path().join("sub/new.json");
    let g = t.path().join("gone.json");
    std::fs::write(&g, "x").unwrap();
    let h = t.path().join("twice.md");
    std::fs::write(&h, "1").unwrap();
    let plan = Plan {
        edits: vec![
            FileEdit {
                path: f.clone(),
                before: None,
                after: Some("{}".into()),
            },
            FileEdit {
                path: g.clone(),
                before: Some("x".into()),
                after: None,
            },
            FileEdit {
                path: h.clone(),
                before: Some("1".into()),
                after: Some("2".into()),
            },
            FileEdit {
                path: h.clone(),
                before: Some("2".into()),
                after: Some("3".into()),
            },
        ],
        ..Plan::default()
    };
    apply(&plan, datetime!(2026-11-27 09:00:00 UTC)).unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "{}");
    assert!(!g.exists());
    assert_eq!(std::fs::read_to_string(&h).unwrap(), "3");
    // New file: no backup. Deleted and edited files: one each, holding the original.
    assert_eq!(
        backups(t.path()),
        vec![
            "gone.json.axl-backup-20261127T090000Z",
            "twice.md.axl-backup-20261127T090000Z"
        ]
    );
    let b = t.path().join("twice.md.axl-backup-20261127T090000Z");
    assert_eq!(std::fs::read_to_string(b).unwrap(), "1");
}

#[test]
fn apply_writes_through_symlinked_file() {
    let t = Temp::new();
    let real = t.path().join("dotfiles/AGENTS.md");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(&real, "a").unwrap();
    let link = t.path().join("AGENTS.md");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let plan = Plan {
        edits: vec![FileEdit {
            path: link.clone(),
            before: Some("a".into()),
            after: Some("b".into()),
        }],
        ..Plan::default()
    };
    apply(&plan, datetime!(2026-11-27 09:00:00 UTC)).unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "b");
}

#[test]
fn links_are_created_replaced_and_removed() {
    let t = Temp::new();
    let link = t.path().join("skills/axle");
    let one = t.path().join("one");
    let two = t.path().join("two");
    let mk = |target: Option<&PathBuf>, before: Option<&PathBuf>| Plan {
        links: vec![LinkEdit {
            path: link.clone(),
            target: target.cloned(),
            before: before.cloned(),
        }],
        ..Plan::default()
    };
    let now = datetime!(2026-11-27 09:00:00 UTC);
    apply(&mk(Some(&one), None), now).unwrap();
    assert_eq!(std::fs::read_link(&link).unwrap(), one);
    apply(&mk(Some(&two), Some(&one)), now).unwrap();
    assert_eq!(std::fs::read_link(&link).unwrap(), two);
    let diff = render_diff(&mk(Some(&two), Some(&one)));
    assert_eq!(
        diff,
        format!("link {} -> {}\n", link.display(), two.display())
    );
    apply(&mk(None, Some(&two)), now).unwrap();
    assert!(std::fs::symlink_metadata(&link).is_err());
}

#[test]
fn dry_run_diff_writes_nothing() {
    let t = Temp::new();
    let f = t.path().join("c.toml");
    std::fs::write(&f, "a = 1\n").unwrap();
    let plan = Plan {
        edits: vec![FileEdit {
            path: f.clone(),
            before: Some("a = 1\n".into()),
            after: Some("a = 2\n".into()),
        }],
        notes: vec!["restart the harness".into()],
        ..Plan::default()
    };
    let diff = render_diff(&plan);
    assert!(diff.contains("-a = 1\n"), "{diff}");
    assert!(diff.contains("+a = 2\n"), "{diff}");
    assert!(diff.contains("note: restart the harness"), "{diff}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "a = 1\n");
    assert!(backups(t.path()).is_empty());
}

#[test]
fn hook_command_matching() {
    assert!(is_axl_hook_command("axl hook stop").is_some());
    assert!(is_axl_hook_command("/Users/x/.cargo/bin/axl hook post-tool-use").is_some());
    assert!(is_axl_hook_command("\"/Users/x y/bin/axl\" hook session-start").is_some());
    assert!(is_axl_hook_command("other hook stop").is_none());
    assert!(is_axl_hook_command("axl hook bogus").is_none());
    assert!(is_axl_hook_command("axl serve").is_none());
    assert!(is_axl_hook_command("/x/notaxl hook stop").is_none());
}

#[test]
fn detect_by_dir_or_binary() {
    let t = Temp::new();
    let home = t.path().join("home");
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    assert!(Harness::Codex.detect(&home, None));
    assert!(!Harness::ClaudeCode.detect(&home, None));
    assert!(!Harness::Opencode.detect(&home, None));
    let shims = t.path().join("shims");
    exe_file(&shims.join("opencode"));
    let path_env: OsString = shims.clone().into_os_string();
    assert!(Harness::Opencode.detect(&home, Some(&path_env)));
    assert!(!Harness::ClaudeCode.detect(&home, Some(&path_env)));
    // A non-executable file is not a binary.
    std::fs::write(shims.join("claude"), "").unwrap();
    assert!(!Harness::ClaudeCode.detect(&home, Some(&path_env)));
    std::fs::create_dir_all(home.join(".config/opencode")).unwrap();
    assert!(Harness::Opencode.detect(&home, None));
}

#[test]
fn harness_names_and_serde() {
    assert_eq!(Harness::ClaudeCode.name(), "claude-code");
    assert_eq!(
        serde_json::to_string(&Harness::ClaudeCode).unwrap(),
        "\"claude-code\""
    );
    assert_eq!(
        serde_json::to_string(&Harness::Opencode).unwrap(),
        "\"opencode\""
    );
}

#[test]
fn manifest_roundtrip_dedupes_and_is_private() {
    let t = Temp::new();
    let dir = AxlDir::new(t.path());
    assert!(Manifest::load(&dir).entries.is_empty());
    let entry = |kind, original: Option<serde_json::Value>| ManifestEntry {
        harness: Harness::Codex,
        file: PathBuf::from("/h/.codex/config.toml"),
        kind,
        key: "mcp_servers.axle".into(),
        original,
    };
    let mut m = Manifest::default();
    assert_eq!(m.version, 1);
    m.record(entry(
        EntryKind::Replaced,
        Some(serde_json::json!({"url": "old"})),
    ));
    m.record(entry(EntryKind::Added, None)); // same key: first record wins
    m.save(&dir).unwrap();
    let loaded = Manifest::load(&dir);
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].kind, EntryKind::Replaced);
    assert_eq!(
        loaded.entries[0].original,
        Some(serde_json::json!({"url": "old"}))
    );
    let mode = std::fs::metadata(dir.manifest_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn instructions_text_is_shared_source() {
    assert!(INSTRUCTIONS.starts_with("## Axle\n"));
    assert!(INSTRUCTIONS.contains("`whoami`"));
}
