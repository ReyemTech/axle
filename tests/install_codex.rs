use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axle::client::AxlDir;
use axle::install::codex::{plan_install, plan_uninstall};
use axle::install::{EditError, Manifest, Plan, apply, render_diff};
use time::macros::datetime;

const AXL: &str = "/opt/axl/bin/axl";
const CASES: [&str; 5] = ["empty", "unrelated", "comments", "handmade", "inline_table"];
const START: &str = "# >>> axle >>>";
const END: &str = "# <<< axle <<<";

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("axl-codex-{suffix}"));
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

fn fixture(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/harness/codex")
        .join(case)
        .join("before")
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
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

/// Every regular file under `root` (backups skipped), relative path -> text.
fn tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        if !dir.is_dir() {
            return;
        }
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            if name.contains(".axl-backup-") || name == ".gitkeep" {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
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

fn home_for(case: &str) -> Temp {
    let t = Temp::new();
    copy_tree(&fixture(case), t.path());
    t
}

fn install_with(
    home: &Path,
    axl: &str,
    manifest: &mut Manifest,
) -> Result<(Plan, Vec<String>), EditError> {
    let (plan, entries, adopted) = plan_install(home, Path::new(axl), manifest)?;
    apply(&plan, datetime!(2026-11-27 12:00:00 UTC)).unwrap();
    for e in entries {
        manifest.record(e);
    }
    Ok((plan, adopted))
}

fn install(home: &Path, manifest: &mut Manifest) -> Result<(Plan, Vec<String>), EditError> {
    install_with(home, AXL, manifest)
}

fn config(home: &Path) -> String {
    std::fs::read_to_string(home.join(".codex/config.toml")).unwrap()
}

fn parsed(text: &str) -> toml::Value {
    toml::from_str(text).unwrap_or_else(|e| panic!("not valid TOML: {e}\n{text}"))
}

#[test]
fn empty_creates_config_and_agents_md() {
    let home = home_for("empty");
    let mut m = Manifest::default();
    install(home.path(), &mut m).unwrap();
    let cfg = config(home.path());
    assert!(
        cfg.starts_with(&format!(
            "[mcp_servers.axle]\ncommand = \"{AXL}\"\nargs = [\"bridge\", \"--harness\", \"codex\"]\n\n{START}\n"
        )),
        "{cfg}"
    );
    let v = parsed(&cfg);
    assert_eq!(v["mcp_servers"]["axle"]["command"].as_str(), Some(AXL));
    // No token or server URL in Codex config.
    assert!(
        !cfg.contains("AXL_SERVER_URL") && !cfg.contains("token"),
        "{cfg}"
    );
    let agents = std::fs::read_to_string(home.path().join(".codex/AGENTS.md")).unwrap();
    assert!(agents.starts_with(START) && agents.contains(axle::install::INSTRUCTIONS));
    assert!(agents.trim_end().ends_with(END));
    // The only files are the two Codex files.
    assert_eq!(
        tree(home.path()).keys().cloned().collect::<Vec<_>>(),
        [".codex/AGENTS.md", ".codex/config.toml"]
    );
}

#[test]
fn unrelated_tables_and_comments_survive() {
    let home = home_for("unrelated");
    let before = config(home.path());
    let agents_before = std::fs::read_to_string(home.path().join(".codex/AGENTS.md")).unwrap();
    let mut m = Manifest::default();
    install(home.path(), &mut m).unwrap();
    let after = config(home.path());
    // Everything the user wrote is byte-identical and in the same place: dropping what
    // axl added gives the original back.
    let added_table = format!(
        "[mcp_servers.axle]\ncommand = \"{AXL}\"\nargs = [\"bridge\", \"--harness\", \"codex\"]\n\n"
    );
    assert!(after.contains(&added_table), "{after}");
    let start = after.find(START).unwrap();
    let without = after[..start].trim_end().to_string() + "\n";
    assert_eq!(without.replacen(&added_table, "", 1), before);
    let agents = std::fs::read_to_string(home.path().join(".codex/AGENTS.md")).unwrap();
    assert!(agents.starts_with(&agents_before));
    let backups: Vec<_> = std::fs::read_dir(home.path().join(".codex"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".axl-backup-")
        })
        .collect();
    assert_eq!(backups.len(), 2, "one backup per changed existing file");

    let home = home_for("comments");
    let before = config(home.path());
    let mut m = Manifest::default();
    install(home.path(), &mut m).unwrap();
    let after = config(home.path());
    for kept in [
        "# hand-edited Codex config\nmodel = \"gpt-5\"   # keep this one\n",
        "# --- servers ---\n[mcp_servers.docs]\ncommand = \"docs-mcp\"   # docs server\nargs = [\"--stdio\"]\n",
        "# notify me\nnotify = [\"notify-send\", \"codex\"]\n",
        "[features]\nweb_search = true  # trailing comment\n",
    ] {
        assert!(after.contains(kept), "lost {kept:?} in\n{after}");
        assert!(before.contains(kept));
    }
    // The key order outside the axle table is unchanged.
    let pos = |t: &str, s: &str| t.find(s).unwrap_or_else(|| panic!("{s} in {t}"));
    assert!(pos(&after, "model =") < pos(&after, "notify ="));
    assert!(pos(&after, "notify =") < pos(&after, "[mcp_servers.docs]"));
    assert!(pos(&after, "[mcp_servers.docs]") < pos(&after, "[features]"));
}

#[test]
fn handmade_axle_table_adopted_and_url_moved() {
    let home = home_for("handmade");
    let mut m = Manifest::default();
    let (plan, adopted) = install(home.path(), &mut m).unwrap();
    let joined = adopted.join("\n");
    assert!(
        adopted.iter().any(|a| a.contains("mcp_servers.axle")),
        "{joined}"
    );
    assert!(adopted.iter().any(|a| a.contains("hooks.SessionStart")));
    assert!(adopted.iter().any(|a| a.contains("hooks.Stop")));
    assert_eq!(adopted.len(), 3, "{joined}");
    assert!(
        plan.notes.iter().any(|n| n.contains("EXTRA")),
        "dropped env is called out: {:?}",
        plan.notes
    );
    let cfg = axle::client::load_config(&AxlDir::new(home.path())).unwrap();
    assert_eq!(cfg.server_url.as_deref(), Some("http://100.64.0.10:7420"));
    let after = config(home.path());
    assert!(!after.contains("AXL_SERVER_URL"), "{after}");
    assert!(!after.contains("/old/path/axl"));
    assert!(after.contains("[mcp_servers.other]\ncommand = \"other-mcp\"\n"));
    assert!(after.contains("echo my own hook"), "user hook kept");
    assert!(after.contains("matcher = \"startup\""));
    assert!(!after.contains("command = \"axl hook session-start\""));
    assert!(!after.contains("/usr/local/bin/axl hook stop"));
    assert_eq!(
        after.matches("hook session-start --harness codex").count(),
        1
    );
    assert_eq!(after.matches("hook stop --harness codex").count(), 1);
    let v = parsed(&after);
    assert_eq!(v["mcp_servers"]["axle"]["command"].as_str(), Some(AXL));
    // The originals are in the manifest.
    let orig = m
        .entries
        .iter()
        .find(|e| e.key == "mcp_servers.axle")
        .and_then(|e| e.original.clone())
        .unwrap();
    assert_eq!(orig["env"]["AXL_SERVER_URL"], "http://100.64.0.10:7420");
    assert_eq!(orig["command"], "/old/path/axl");
    let hook = m
        .entries
        .iter()
        .find(|e| e.key == "hooks.Stop")
        .and_then(|e| e.original.clone())
        .unwrap();
    assert_eq!(hook[0]["command"], "/usr/local/bin/axl hook stop");

    // Comments case: the adopted table's comments go, the rest stays.
    let home = home_for("comments");
    let mut m = Manifest::default();
    install(home.path(), &mut m).unwrap();
    let after = config(home.path());
    assert!(!after.contains("AXL_SERVER_URL") && !after.contains("old hand-made"));
    assert!(after.contains("# notify me") && after.contains("# docs server"));
    let cfg = axle::client::load_config(&AxlDir::new(home.path())).unwrap();
    assert_eq!(cfg.server_url.as_deref(), Some("http://100.64.0.10:7420"));
    assert_eq!(
        parsed(&after)["mcp_servers"]["axle"]["command"].as_str(),
        Some(AXL)
    );
}

#[test]
fn hooks_block_written() {
    let home = home_for("unrelated");
    let mut m = Manifest::default();
    let (plan, _) = install(home.path(), &mut m).unwrap();
    let cfg = config(home.path());
    let block: String = {
        let start = cfg.find(START).expect("start marker");
        let end = cfg.find(END).expect("end marker");
        cfg[start..end + END.len()].to_string()
    };
    assert_eq!(cfg.matches(START).count(), 1);
    assert_eq!(cfg.matches(END).count(), 1);
    let v = parsed(&cfg);
    for (event, arg) in [
        ("SessionStart", "session-start"),
        ("PostToolUse", "post-tool-use"),
        ("Stop", "stop"),
    ] {
        assert!(block.contains(&format!("[[hooks.{event}]]")), "{event}");
        let groups = v["hooks"][event].as_array().unwrap();
        assert_eq!(groups.len(), 1, "{event}");
        let h = &groups[0]["hooks"].as_array().unwrap()[0];
        assert_eq!(h["type"].as_str(), Some("command"));
        assert_eq!(
            h["command"].as_str().unwrap(),
            format!("{AXL} hook {arg} --harness codex")
        );
        assert_eq!(h["timeout"].as_integer(), Some(10));
    }
    assert_eq!(v["hooks"].as_table().unwrap().len(), 3, "no other events");
    assert!(
        plan.notes
            .iter()
            .any(|n| n.contains("Codex asks you to trust new hooks on its next start")),
        "{:?}",
        plan.notes
    );

    // Idempotent: a re-run changes nothing and adds no note.
    let before = tree(home.path());
    let (plan, entries, adopted) = plan_install(home.path(), Path::new(AXL), &m).unwrap();
    assert_eq!(render_diff(&plan), "");
    assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    assert!(adopted.is_empty());
    for e in entries {
        m.record(e);
    }
    apply(&plan, datetime!(2026-11-27 12:05:00 UTC)).unwrap();
    assert_eq!(tree(home.path()), before);

    // Uninstall removes the block byte-exactly.
    let plan = plan_uninstall(home.path(), &m).unwrap();
    apply(&plan, datetime!(2026-11-27 12:10:00 UTC)).unwrap();
    assert_eq!(
        std::fs::read_to_string(home.path().join(".codex/config.toml")).unwrap(),
        std::fs::read_to_string(fixture("unrelated").join(".codex/config.toml")).unwrap()
    );
}

#[test]
fn rerun_produces_empty_plan() {
    for case in CASES {
        let home = home_for(case);
        let mut m = Manifest::default();
        let first = install(home.path(), &mut m);
        let Ok(_) = first else {
            // inline_table may ask for a manual step; then nothing was written.
            assert_eq!(case, "inline_table");
            continue;
        };
        let before = tree(home.path());
        let (plan, entries, adopted) = plan_install(home.path(), Path::new(AXL), &m).unwrap();
        assert_eq!(render_diff(&plan), "", "{case}");
        assert!(adopted.is_empty(), "{case}: {adopted:?}");
        for e in entries {
            m.record(e);
        }
        apply(&plan, datetime!(2026-11-27 12:05:00 UTC)).unwrap();
        assert_eq!(tree(home.path()), before, "{case}");
    }
}

#[test]
fn moved_binary_is_rewritten_in_place() {
    let home = home_for("unrelated");
    let mut m = Manifest::default();
    install(home.path(), &mut m).unwrap();
    let (_, adopted) = install_with(home.path(), "/usr/local/bin/axl", &mut m).unwrap();
    assert!(adopted.is_empty(), "our own entries are not adoptions");
    let cfg = config(home.path());
    assert!(!cfg.contains(AXL), "old path left behind:\n{cfg}");
    assert_eq!(cfg.matches("/usr/local/bin/axl").count(), 4);
    assert!(cfg.trim_end().ends_with(END), "block stayed last:\n{cfg}");
    let (again, _, _) = plan_install(home.path(), Path::new("/usr/local/bin/axl"), &m).unwrap();
    assert_eq!(render_diff(&again), "");
}

#[test]
fn awkward_binary_path_is_quoted_for_the_shell() {
    let home = home_for("empty");
    let mut m = Manifest::default();
    install_with(home.path(), "/Users/a b/bin/axl", &mut m).unwrap();
    let v = parsed(&config(home.path()));
    assert_eq!(
        v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str(),
        Some("'/Users/a b/bin/axl' hook stop --harness codex")
    );
    assert_eq!(
        v["mcp_servers"]["axle"]["command"].as_str(),
        Some("/Users/a b/bin/axl")
    );
}

#[test]
fn uninstall_restores_original() {
    for case in ["empty", "unrelated"] {
        let home = home_for(case);
        let mut m = Manifest::default();
        install(home.path(), &mut m).unwrap();
        let plan = plan_uninstall(home.path(), &m).unwrap();
        apply(&plan, datetime!(2026-11-27 12:10:00 UTC)).unwrap();
        let want: BTreeMap<String, String> = tree(&fixture(case));
        assert_eq!(tree(home.path()), want, "{case}");
    }
}

#[test]
fn uninstall_after_adoption_removes_managed_form_only() {
    let home = home_for("handmade");
    let mut m = Manifest::default();
    install(home.path(), &mut m).unwrap();
    let plan = plan_uninstall(home.path(), &m).unwrap();
    apply(&plan, datetime!(2026-11-27 12:10:00 UTC)).unwrap();
    let after = config(home.path());
    assert!(!after.contains("axle") && !after.contains(START), "{after}");
    assert!(!after.contains("axl hook"));
    assert!(after.contains("[mcp_servers.other]") && after.contains("echo my own hook"));
    assert!(after.contains("model = \"gpt-5\""));
    parsed(&after);
}

#[test]
fn codex_unusual_shape_inline_table() {
    let home = home_for("inline_table");
    let before = config(home.path());
    let mut m = Manifest::default();
    match install(home.path(), &mut m) {
        Ok(_) => {
            let after = config(home.path());
            let v = parsed(&after);
            assert_eq!(
                v["mcp_servers"]["other"]["command"].as_str(),
                Some("other-mcp")
            );
            assert_eq!(v["mcp_servers"]["other"]["args"][0].as_str(), Some("a"));
            assert_eq!(v["mcp_servers"]["axle"]["command"].as_str(), Some(AXL));
            assert!(after.contains("model = \"gpt-5\""));
            let plan = plan_uninstall(home.path(), &m).unwrap();
            apply(&plan, datetime!(2026-11-27 12:10:00 UTC)).unwrap();
            let v = parsed(&config(home.path()));
            assert!(v["mcp_servers"].get("axle").is_none());
            assert_eq!(
                v["mcp_servers"]["other"]["command"].as_str(),
                Some("other-mcp")
            );
        }
        Err(EditError::NeedsManual { .. }) => {
            assert_eq!(config(home.path()), before, "nothing written");
        }
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn hooks_shape_conflicts_need_a_manual_step() {
    for text in [
        "hooks = { SessionStart = [] }\n",
        "[hooks]\nSessionStart = [{ hooks = [] }]\n",
        "mcp_servers = 3\n",
        "this is = not toml [\n",
    ] {
        let home = Temp::new();
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(home.path().join(".codex/config.toml"), text).unwrap();
        let err = plan_install(home.path(), Path::new(AXL), &Manifest::default()).unwrap_err();
        match err {
            EditError::NeedsManual { file, snippet, .. } => {
                assert!(file.ends_with(".codex/config.toml"), "{text:?}");
                assert!(!snippet.is_empty());
            }
            e => panic!("{text:?}: {e}"),
        }
        assert_eq!(config(home.path()), text, "never rewritten");
    }
}

#[test]
fn orphan_marker_needs_a_manual_step() {
    for file in ["config.toml", "AGENTS.md"] {
        let home = Temp::new();
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        let text = format!("model = \"x\"\n{START}\nhalf a block\n");
        let other = if file == "config.toml" {
            "AGENTS.md"
        } else {
            "config.toml"
        };
        let _ = other;
        std::fs::write(home.path().join(".codex").join(file), &text).unwrap();
        let err = plan_install(home.path(), Path::new(AXL), &Manifest::default()).unwrap_err();
        let EditError::NeedsManual { reason, .. } = err else {
            panic!("{file}")
        };
        assert!(reason.contains("no matching"), "{reason}");
        assert_eq!(
            std::fs::read_to_string(home.path().join(".codex").join(file)).unwrap(),
            text
        );
    }
}
