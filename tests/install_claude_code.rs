use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axle::client::AxlDir;
use axle::install::claude_code::{ClaudeMode, MODE, plan_install, plan_uninstall};
use axle::install::{EditError, Manifest, apply, render_diff};
use time::macros::datetime;

const AXL: &str = "/opt/axl/bin/axl";
const CASES: [&str; 4] = ["empty", "unrelated", "comments", "handmade"];

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("axl-claude-{suffix}"));
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

fn fixture(case: &str, sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/harness/claude-code")
        .join(case)
        .join(sub)
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    if !src.is_dir() {
        return;
    }
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        if name == ".gitkeep" {
            continue;
        }
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dst.join(&name));
        } else {
            std::fs::copy(e.path(), dst.join(&name)).unwrap();
        }
    }
}

/// Every regular file under `root` (symlinks and backups skipped), relative path -> text.
fn tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        if !dir.is_dir() {
            return;
        }
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            let ty = e.file_type().unwrap();
            if ty.is_symlink() || name.contains(".axl-backup-") || name == ".gitkeep" {
                continue;
            }
            if ty.is_dir() {
                walk(root, &e.path(), out);
            } else {
                let rel = e.path().strip_prefix(root).unwrap().display().to_string();
                out.insert(rel, std::fs::read_to_string(e.path()).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn expand(text: &str) -> String {
    text.replace("{{AXL}}", AXL)
        .replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
}

fn contract(text: &str) -> String {
    text.replace(AXL, "{{AXL}}")
        .replace(env!("CARGO_PKG_VERSION"), "{{VERSION}}")
}

fn expected(case: &str, sub: &str) -> BTreeMap<String, String> {
    tree(&fixture(case, sub))
        .into_iter()
        .map(|(k, v)| (k, expand(&v)))
        .collect()
}

fn home_for(case: &str) -> Temp {
    let t = Temp::new();
    copy_tree(&fixture(case, "before"), t.path());
    t
}

fn install(
    home: &Path,
    manifest: &mut Manifest,
    mode: ClaudeMode,
) -> Result<(axle::install::Plan, Vec<String>), EditError> {
    let (plan, entries, adopted) = plan_install(home, Path::new(AXL), manifest, mode)?;
    apply(&plan, datetime!(2026-11-27 12:00:00 UTC)).unwrap();
    for e in entries {
        manifest.record(e);
    }
    Ok((plan, adopted))
}

fn after_dir(mode: ClaudeMode) -> &'static str {
    match mode {
        ClaudeMode::Plugin => "after",
        ClaudeMode::UserScope => "after-userscope",
    }
}

/// `AXL_BLESS_FIXTURES=1` rewrites the `after*` trees from the current output; review the diff.
fn check_golden(case: &str, mode: ClaudeMode) {
    let home = home_for(case);
    let mut m = Manifest::default();
    install(home.path(), &mut m, mode).unwrap();
    let got = tree(home.path());
    let dir = fixture(case, after_dir(mode));
    if std::env::var_os("AXL_BLESS_FIXTURES").is_some() {
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, text) in &got {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, contract(text)).unwrap();
        }
        return;
    }
    let want = expected(case, after_dir(mode));
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>(),
        "{case} {mode:?}: file set differs"
    );
    for (rel, text) in &want {
        assert_eq!(&got[rel], text, "{case} {mode:?}: {rel} differs");
    }
}

fn is_plugin_link(home: &Path) -> bool {
    std::fs::read_link(home.join(".claude/skills/axle")).ok()
        == Some(AxlDir::new(home).integrations_dir().join("claude-code"))
}

#[test]
fn mode_is_plugin() {
    assert_eq!(MODE, ClaudeMode::Plugin);
}

#[test]
fn empty_home_installs_plugin() {
    let home = home_for("empty");
    let mut m = Manifest::default();
    let (_, adopted) = install(home.path(), &mut m, ClaudeMode::Plugin).unwrap();
    assert!(adopted.is_empty());
    assert!(is_plugin_link(home.path()));
    check_golden("empty", ClaudeMode::Plugin);
    // Valid JSON with the pinned shape.
    let dir = AxlDir::new(home.path())
        .integrations_dir()
        .join("claude-code");
    let plugin: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(plugin["name"], "axle");
    assert_eq!(plugin["version"], env!("CARGO_PKG_VERSION"));
    let mcp: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(mcp["mcpServers"]["axle"]["type"], "stdio");
    assert_eq!(mcp["mcpServers"]["axle"]["command"], AXL);
    assert_eq!(
        mcp["mcpServers"]["axle"]["args"],
        serde_json::json!(["bridge", "--harness", "claude-code"])
    );
    assert!(mcp["mcpServers"]["axle"].get("env").is_none());
    let hooks: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("hooks/hooks.json")).unwrap())
            .unwrap();
    for (ev, arg) in [
        ("SessionStart", "session-start"),
        ("UserPromptSubmit", "user-prompt-submit"),
        ("PostToolUse", "post-tool-use"),
        ("Stop", "stop"),
        ("SessionEnd", "session-end"),
    ] {
        let h = &hooks["hooks"][ev][0]["hooks"][0];
        assert_eq!(h["command"], format!("{AXL} hook {arg}"));
        let want = if ev == "SessionEnd" { 3 } else { 5 };
        assert_eq!(h["timeout"], want, "{ev}");
    }
    let skill = std::fs::read_to_string(dir.join("skills/axle/SKILL.md")).unwrap();
    assert!(skill.starts_with("---\nname: axle\n"));
    assert!(skill.contains(axle::install::INSTRUCTIONS));
}

#[test]
fn unrelated_config_untouched() {
    let home = home_for("unrelated");
    let mut m = Manifest::default();
    install(home.path(), &mut m, ClaudeMode::Plugin).unwrap();
    for rel in [".claude.json", ".claude/settings.json"] {
        assert_eq!(
            std::fs::read(home.path().join(rel)).unwrap(),
            std::fs::read(fixture("unrelated", "before").join(rel)).unwrap(),
            "{rel} changed"
        );
        let name = Path::new(rel).file_name().unwrap().to_string_lossy();
        assert!(
            std::fs::read_dir(home.path().join(rel).parent().unwrap())
                .unwrap()
                .all(|e| !e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{name}.axl-backup-"))),
            "no backup for an untouched file"
        );
    }
    check_golden("unrelated", ClaudeMode::Plugin);
}

#[test]
fn comments_preserved() {
    let home = home_for("comments");
    let mut m = Manifest::default();
    install(home.path(), &mut m, ClaudeMode::Plugin).unwrap();
    let cj = std::fs::read_to_string(home.path().join(".claude.json")).unwrap();
    assert!(cj.starts_with("// user config, hand edited\n"));
    assert!(cj.contains("// do not touch") && cj.contains("// docs server"));
    assert!(cj.contains(
        "\t\t\"docs\": { \"type\": \"http\", \"url\": \"https://docs.example.com/mcp\" }"
    ));
    assert!(!cj.contains("axle"));
    let st = std::fs::read_to_string(home.path().join(".claude/settings.json")).unwrap();
    assert!(st.contains("   // permissions I tuned") && st.contains("/* mail for this machine */"));
    assert!(st.contains("notify-send done") && st.contains("echo hi"));
    assert!(!st.contains("axl hook"));
    let toml = std::fs::read_to_string(home.path().join(".axl/config.toml")).unwrap();
    assert!(toml.starts_with("# my axl config\nport = 7420\n"));
    assert!(toml.contains("server_url = \"https://axle.example.com\""));
    check_golden("comments", ClaudeMode::Plugin);
}

#[test]
fn handmade_entries_adopted_and_url_moved() {
    use std::os::unix::fs::PermissionsExt;
    let home = home_for("handmade");
    let mut m = Manifest::default();
    let (_, adopted) = install(home.path(), &mut m, ClaudeMode::Plugin).unwrap();
    let joined = adopted.join("\n");
    assert!(
        adopted.iter().any(|a| a.contains("mcpServers.axle")),
        "{joined}"
    );
    for ev in ["SessionStart", "UserPromptSubmit", "PostToolUse", "Stop"] {
        assert!(
            adopted.iter().any(|a| a.contains(&format!("hooks.{ev}"))),
            "{ev}: {joined}"
        );
    }
    assert_eq!(adopted.len(), 5, "{joined}");
    let cfg = axle::client::load_config(&AxlDir::new(home.path())).unwrap();
    assert_eq!(cfg.server_url.as_deref(), Some("http://100.64.0.10:7420"));
    let mode = std::fs::metadata(home.path().join(".axl/config.toml"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    // The originals are in the manifest and the backups.
    let orig = m
        .entries
        .iter()
        .find(|e| e.key == "mcpServers.axle")
        .and_then(|e| e.original.clone())
        .unwrap();
    assert_eq!(orig["env"]["AXL_SERVER_URL"], "http://100.64.0.10:7420");
    let backups = std::fs::read_dir(home.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".claude.json.axl-backup-")
        })
        .count();
    assert_eq!(backups, 1);
    check_golden("handmade", ClaudeMode::Plugin);
}

#[test]
fn rerun_produces_empty_plan() {
    for mode in [ClaudeMode::Plugin, ClaudeMode::UserScope] {
        for case in CASES {
            let home = home_for(case);
            let mut m = Manifest::default();
            install(home.path(), &mut m, mode).unwrap();
            let before = tree(home.path());
            let (plan, entries, adopted) =
                plan_install(home.path(), Path::new(AXL), &m, mode).unwrap();
            assert_eq!(render_diff(&plan), "", "{case} {mode:?}");
            assert!(adopted.is_empty(), "{case} {mode:?}: {adopted:?}");
            for e in entries {
                m.record(e);
            }
            apply(&plan, datetime!(2026-11-27 12:05:00 UTC)).unwrap();
            assert_eq!(tree(home.path()), before, "{case} {mode:?}");
        }
    }
}

#[test]
fn moved_binary_is_rewritten() {
    for mode in [ClaudeMode::Plugin, ClaudeMode::UserScope] {
        let home = home_for("unrelated");
        let mut m = Manifest::default();
        install(home.path(), &mut m, mode).unwrap();
        let (plan, entries, adopted) =
            plan_install(home.path(), Path::new("/usr/local/bin/axl"), &m, mode).unwrap();
        assert!(
            adopted.is_empty(),
            "{mode:?}: our own entry is not an adoption"
        );
        apply(&plan, datetime!(2026-11-27 12:05:00 UTC)).unwrap();
        for e in entries {
            m.record(e);
        }
        let all: String = tree(home.path()).values().cloned().collect();
        assert!(all.contains("/usr/local/bin/axl hook stop"), "{mode:?}");
        assert!(!all.contains(AXL), "{mode:?}: old path left behind");
        let (again, _, _) =
            plan_install(home.path(), Path::new("/usr/local/bin/axl"), &m, mode).unwrap();
        assert_eq!(render_diff(&again), "", "{mode:?}");
    }
}

#[test]
fn uninstall_restores_original_for_added_files() {
    for mode in [ClaudeMode::Plugin, ClaudeMode::UserScope] {
        for case in ["empty", "unrelated"] {
            let home = home_for(case);
            let mut m = Manifest::default();
            install(home.path(), &mut m, mode).unwrap();
            let plan = plan_uninstall(home.path(), &m).unwrap();
            apply(&plan, datetime!(2026-11-27 12:10:00 UTC)).unwrap();
            assert_eq!(
                tree(home.path()),
                tree(&fixture(case, "before")),
                "{case} {mode:?}"
            );
            assert!(
                std::fs::symlink_metadata(home.path().join(".claude/skills/axle")).is_err(),
                "{case} {mode:?}: link left"
            );
        }
    }
}

#[test]
fn uninstall_after_adoption_removes_managed_form_only() {
    for mode in [ClaudeMode::Plugin, ClaudeMode::UserScope] {
        let home = home_for("handmade");
        let mut m = Manifest::default();
        install(home.path(), &mut m, mode).unwrap();
        if mode == ClaudeMode::UserScope {
            let st = std::fs::read_to_string(home.path().join(".claude/settings.json")).unwrap();
            assert!(
                st.contains("hook session-end"),
                "installed before uninstall"
            );
        }
        let plan = plan_uninstall(home.path(), &m).unwrap();
        apply(&plan, datetime!(2026-11-27 12:10:00 UTC)).unwrap();
        let cj = std::fs::read_to_string(home.path().join(".claude.json")).unwrap();
        assert!(
            !cj.contains("\"axle\""),
            "{mode:?}: handmade entry restored"
        );
        assert!(cj.contains("\"other\""), "{mode:?}");
        let st = std::fs::read_to_string(home.path().join(".claude/settings.json")).unwrap();
        assert!(!st.contains("axl hook"), "{mode:?}: hooks restored");
        assert!(!st.contains("session-end"), "{mode:?}: SessionEnd removed");
        assert!(st.contains("\"theme\": \"dark\""), "{mode:?}");
        assert!(
            !home
                .path()
                .join(".axl/integrations/claude-code/.mcp.json")
                .exists()
        );
        assert!(std::fs::symlink_metadata(home.path().join(".claude/skills/axle")).is_err());
        // The moved URL stays: config.toml is the user's.
        let cfg = axle::client::load_config(&AxlDir::new(home.path())).unwrap();
        assert_eq!(cfg.server_url.as_deref(), Some("http://100.64.0.10:7420"));
    }
}

#[test]
fn userscope_fallback_matches_golden() {
    for case in CASES {
        check_golden(case, ClaudeMode::UserScope);
    }
}

#[test]
fn userscope_has_no_plugin_and_runs_things_once() {
    let home = home_for("handmade");
    let mut m = Manifest::default();
    install(home.path(), &mut m, ClaudeMode::UserScope).unwrap();
    assert!(!AxlDir::new(home.path()).integrations_dir().exists());
    let st = std::fs::read_to_string(home.path().join(".claude/settings.json")).unwrap();
    assert_eq!(st.matches("axl hook").count(), 6);
    let cj = std::fs::read_to_string(home.path().join(".claude.json")).unwrap();
    assert_eq!(cj.matches("\"axle\"").count(), 1);
    assert!(!cj.contains("env"));
    let md = std::fs::read_to_string(home.path().join(".claude/CLAUDE.md")).unwrap();
    assert!(md.starts_with("# >>> axle >>>\n") && md.trim_end().ends_with("# <<< axle <<<"));
}

#[test]
fn claude_unusual_shape_trailing_commas() {
    let home = Temp::new();
    let text = "{\n  \"numStartups\": 1,\n  \"mcpServers\": {\n    \"keep\": { \"command\": \"k\", },\n    \"axle\": { \"command\": \"axl\", \"args\": [\"bridge\",], },\n  },\n}\n";
    std::fs::write(home.path().join(".claude.json"), text).unwrap();
    let mut m = Manifest::default();
    let r = install(home.path(), &mut m, ClaudeMode::Plugin);
    match r {
        Ok(_) => {
            let got = std::fs::read_to_string(home.path().join(".claude.json")).unwrap();
            assert!(
                got.contains("\"keep\": { \"command\": \"k\", },"),
                "unrelated text reformatted:\n{got}"
            );
            assert!(!got.contains("axle"));
            assert!(got.trim_end().ends_with("}"));
        }
        Err(EditError::NeedsManual { snippet, .. }) => {
            assert!(snippet.contains("mcpServers"));
            assert_eq!(
                std::fs::read_to_string(home.path().join(".claude.json")).unwrap(),
                text
            );
        }
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn claude_unparseable_json_is_manual_and_untouched() {
    for mode in [ClaudeMode::Plugin, ClaudeMode::UserScope] {
        let home = Temp::new();
        let text = "{ \"mcpServers\": { \"axle\": ";
        std::fs::write(home.path().join(".claude.json"), text).unwrap();
        let m = Manifest::default();
        let err = plan_install(home.path(), Path::new(AXL), &m, mode).unwrap_err();
        match err {
            EditError::NeedsManual { file, snippet, .. } => {
                assert!(file.ends_with(".claude.json"));
                assert!(snippet.contains("\"mcpServers\""));
            }
            e => panic!("{e}"),
        }
        assert_eq!(
            std::fs::read_to_string(home.path().join(".claude.json")).unwrap(),
            text
        );
    }
}

#[test]
fn orphan_marker_in_claude_md_is_manual() {
    for mode in [ClaudeMode::Plugin, ClaudeMode::UserScope] {
        let home = Temp::new();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        let text = "# mine\n# >>> axle >>>\nhalf a block\n";
        std::fs::write(home.path().join(".claude/CLAUDE.md"), text).unwrap();
        let err =
            plan_install(home.path(), Path::new(AXL), &Manifest::default(), mode).unwrap_err();
        match err {
            EditError::NeedsManual {
                reason, snippet, ..
            } => {
                assert!(reason.contains("CLAUDE.md:2"), "{mode:?}: {reason}");
                assert!(snippet.contains(axle::install::INSTRUCTIONS.trim_end()));
            }
            e => panic!("{e}"),
        }
        assert_eq!(
            std::fs::read_to_string(home.path().join(".claude/CLAUDE.md")).unwrap(),
            text,
            "{mode:?}"
        );
    }
}

fn block() -> String {
    format!(
        "# >>> axle >>>\n{}\n# <<< axle <<<\n",
        axle::install::INSTRUCTIONS.trim_end()
    )
}

#[test]
fn plugin_mode_creates_claude_md_with_the_rules_block() {
    let home = home_for("empty");
    let mut m = Manifest::default();
    let (plan, _, _) = plan_install(home.path(), Path::new(AXL), &m, ClaudeMode::Plugin).unwrap();
    // Dry run: the diff names the file and shows the block; nothing is written yet.
    let diff = render_diff(&plan);
    assert!(diff.contains(".claude/CLAUDE.md"), "{diff}");
    assert!(diff.contains("# >>> axle >>>"), "{diff}");
    assert!(!home.path().join(".claude/CLAUDE.md").exists());

    install(home.path(), &mut m, ClaudeMode::Plugin).unwrap();
    let md = std::fs::read_to_string(home.path().join(".claude/CLAUDE.md")).unwrap();
    assert_eq!(md, block());
    let md_path = home.path().join(".claude/CLAUDE.md");
    let kinds: Vec<_> = m
        .entries
        .iter()
        .filter(|e| e.file == md_path)
        .map(|e| (e.kind, e.key.clone()))
        .collect();
    assert!(
        kinds.contains(&(axle::install::EntryKind::Block, "block".to_string())),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&(axle::install::EntryKind::CreatedFile, "file".to_string())),
        "{kinds:?}"
    );

    // Uninstall removes the file it created.
    let plan = plan_uninstall(home.path(), &m).unwrap();
    apply(&plan, datetime!(2026-11-27 12:10:00 UTC)).unwrap();
    assert!(!md_path.exists());
}

#[test]
fn plugin_mode_appends_block_to_existing_claude_md_with_backup() {
    let home = home_for("unrelated");
    let original = std::fs::read_to_string(home.path().join(".claude/CLAUDE.md")).unwrap();
    let mut m = Manifest::default();
    install(home.path(), &mut m, ClaudeMode::Plugin).unwrap();
    let md = std::fs::read_to_string(home.path().join(".claude/CLAUDE.md")).unwrap();
    assert_eq!(md, format!("{original}\n{}", block()));
    let backups: Vec<String> = std::fs::read_dir(home.path().join(".claude"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("CLAUDE.md.axl-backup-"))
        .collect();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(
        std::fs::read_to_string(home.path().join(".claude").join(&backups[0])).unwrap(),
        original
    );

    // Uninstall removes exactly the block.
    let plan = plan_uninstall(home.path(), &m).unwrap();
    apply(&plan, datetime!(2026-11-27 12:10:00 UTC)).unwrap();
    assert_eq!(
        std::fs::read_to_string(home.path().join(".claude/CLAUDE.md")).unwrap(),
        original
    );
}

#[test]
fn reinstall_adds_the_block_to_an_older_install() {
    let home = home_for("unrelated");
    let mut m = Manifest::default();
    install(home.path(), &mut m, ClaudeMode::Plugin).unwrap();
    // An install from before the block existed: no block in the file or the manifest.
    let md_path = home.path().join(".claude/CLAUDE.md");
    let original =
        std::fs::read_to_string(fixture("unrelated", "before").join(".claude/CLAUDE.md")).unwrap();
    std::fs::write(&md_path, &original).unwrap();
    m.entries.retain(|e| e.file != md_path);

    let (plan, entries, adopted) =
        plan_install(home.path(), Path::new(AXL), &m, ClaudeMode::Plugin).unwrap();
    assert!(adopted.is_empty());
    let diff = render_diff(&plan);
    assert!(diff.contains("# >>> axle >>>"), "{diff}");
    apply(&plan, datetime!(2026-11-27 12:05:00 UTC)).unwrap();
    for e in entries {
        m.record(e);
    }
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert_eq!(md.matches("# >>> axle >>>").count(), 1);
    // Re-running again changes nothing.
    let (plan, _, _) = plan_install(home.path(), Path::new(AXL), &m, ClaudeMode::Plugin).unwrap();
    assert_eq!(render_diff(&plan), "");
}

#[test]
fn real_directory_at_link_path_is_manual() {
    let home = Temp::new();
    std::fs::create_dir_all(home.path().join(".claude/skills/axle")).unwrap();
    let err = plan_install(
        home.path(),
        Path::new(AXL),
        &Manifest::default(),
        ClaudeMode::Plugin,
    )
    .unwrap_err();
    assert!(matches!(err, EditError::NeedsManual { .. }));
}

#[test]
fn config_files_contain_no_token_or_url() {
    for mode in [ClaudeMode::Plugin, ClaudeMode::UserScope] {
        for case in CASES {
            let home = home_for(case);
            let mut m = Manifest::default();
            install(home.path(), &mut m, mode).unwrap();
            for (rel, text) in tree(home.path()) {
                if rel.ends_with("config.toml") {
                    continue; // server_url belongs here, and only here
                }
                // Untouched user files may say anything; only files install wrote matter.
                let before = tree(&fixture(case, "before"));
                if before.get(&rel).is_some_and(|b| *b == text) {
                    continue;
                }
                assert!(
                    !text.contains("axl_"),
                    "{case} {mode:?} {rel}: token-like text"
                );
                assert!(!text.contains("AXL_SERVER_URL"), "{case} {mode:?} {rel}");
                assert!(!text.contains("http://"), "{case} {mode:?} {rel}: a URL");
            }
        }
    }
}
