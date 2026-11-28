use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axle::client::AxlDir;
use axle::install::opencode::{plan_install, plan_uninstall};
use axle::install::{EditError, Manifest, Plan, apply, render_diff};
use time::macros::datetime;

const AXL: &str = "/opt/axl/bin/axl";
const CASES: [&str; 5] = [
    "empty",
    "unrelated",
    "jsonc_comments",
    "handmade",
    "both_files",
];
const START: &str = "# >>> axle >>>";
const END: &str = "# <<< axle <<<";

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("axl-opencode-{suffix}"));
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
        .join("tests/fixtures/harness/opencode")
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

fn oc(home: &Path, file: &str) -> String {
    std::fs::read_to_string(home.join(".config/opencode").join(file)).unwrap()
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not valid JSON: {e}\n{text}"))
}

fn entry() -> serde_json::Value {
    serde_json::json!({"type": "local", "command": [AXL, "bridge", "--harness", "opencode"], "enabled": true})
}

#[test]
fn empty_creates_opencode_json() {
    let home = home_for("empty");
    let mut m = Manifest::default();
    install(home.path(), &mut m).unwrap();
    let text = oc(home.path(), "opencode.json");
    let v = json(&text);
    assert_eq!(v["$schema"], "https://opencode.ai/config.json");
    assert_eq!(v["mcp"]["axle"], entry());
    assert!(!text.contains("AXL_SERVER_URL") && !text.contains("token"));
    let agents = oc(home.path(), "AGENTS.md");
    assert!(agents.starts_with(START) && agents.contains(axle::install::INSTRUCTIONS));
    assert!(agents.trim_end().ends_with(END));
    assert_eq!(
        tree(home.path()).keys().cloned().collect::<Vec<_>>(),
        [
            ".config/opencode/AGENTS.md",
            ".config/opencode/opencode.json"
        ]
    );
}

#[test]
fn unrelated_mcp_servers_survive() {
    let home = home_for("unrelated");
    let before = oc(home.path(), "opencode.json");
    let agents_before = oc(home.path(), "AGENTS.md");
    let mut m = Manifest::default();
    install(home.path(), &mut m).unwrap();
    let after = oc(home.path(), "opencode.json");
    let v = json(&after);
    assert_eq!(v["mcp"]["axle"], entry());
    assert_eq!(v["mcp"]["docs"]["command"][0], "docs-mcp");
    assert_eq!(v["theme"], "tokyonight");
    assert_eq!(v["autoupdate"], false);
    // Key order: docs stays before axle, autoupdate stays last.
    let pos = |s: &str| after.find(s).unwrap();
    assert!(pos("\"theme\"") < pos("\"docs\"") && pos("\"docs\"") < pos("\"axle\""));
    assert!(pos("\"axle\"") < pos("\"autoupdate\""));
    assert!(before.contains("\"docs\": {"));
    assert!(oc(home.path(), "AGENTS.md").starts_with(&agents_before));
    assert!(
        tree(home.path())
            .keys()
            .all(|k| !k.contains("opencode.jsonc"))
    );
}

#[test]
fn jsonc_comments_preserved() {
    let home = home_for("jsonc_comments");
    let mut m = Manifest::default();
    install(home.path(), &mut m).unwrap();
    let after = oc(home.path(), "opencode.jsonc");
    for kept in [
        "// hand-edited OpenCode config\n",
        "/* keep this theme */",
        "\"theme\": \"tokyonight\", // trailing note",
        "// docs server",
        "\"enabled\": true, // on",
    ] {
        assert!(after.contains(kept), "lost {kept:?} in\n{after}");
    }
    let v: serde_json::Value = jsonc_parser::cst::CstRootNode::parse(&after, &Default::default())
        .unwrap()
        .to_serde_value()
        .unwrap();
    assert_eq!(v["mcp"]["axle"], entry());
    assert_eq!(v["mcp"]["docs"]["command"][0], "docs-mcp");
    assert!(!home.path().join(".config/opencode/opencode.json").exists());
}

#[test]
fn handmade_axle_adopted() {
    let home = home_for("handmade");
    let mut m = Manifest::default();
    let (plan, adopted) = install(home.path(), &mut m).unwrap();
    assert_eq!(adopted.len(), 1, "{adopted:?}");
    assert!(adopted[0].contains("mcp.axle"));
    assert!(
        plan.notes.iter().any(|n| n.contains("EXTRA")),
        "dropped environment is called out: {:?}",
        plan.notes
    );
    let cfg = axle::client::load_config(&AxlDir::new(home.path())).unwrap();
    assert_eq!(cfg.server_url.as_deref(), Some("http://100.64.0.10:7420"));
    let after = oc(home.path(), "opencode.json");
    assert!(!after.contains("AXL_SERVER_URL") && !after.contains("/old/path/axl"));
    let v = json(&after);
    assert_eq!(v["mcp"]["axle"], entry());
    assert_eq!(v["mcp"]["other"]["command"][0], "other-mcp");
    let orig = m
        .entries
        .iter()
        .find(|e| e.key == "mcp.axle")
        .and_then(|e| e.original.clone())
        .unwrap();
    assert_eq!(orig["command"][0], "/old/path/axl");
    assert_eq!(
        orig["environment"]["AXL_SERVER_URL"],
        "http://100.64.0.10:7420"
    );
}

#[test]
fn prefers_jsonc_when_both_exist() {
    let home = home_for("both_files");
    let json_before = oc(home.path(), "opencode.json");
    let mut m = Manifest::default();
    install(home.path(), &mut m).unwrap();
    assert_eq!(oc(home.path(), "opencode.json"), json_before);
    let after = oc(home.path(), "opencode.jsonc");
    assert!(after.starts_with("// preferred file\n"));
    assert!(after.contains("\"axle\""));
    let plan = plan_uninstall(home.path(), &m).unwrap();
    apply(&plan, datetime!(2026-11-27 12:10:00 UTC)).unwrap();
    assert_eq!(tree(home.path()), tree(&fixture("both_files")));
}

#[test]
fn rerun_produces_empty_plan() {
    for case in CASES {
        let home = home_for(case);
        let mut m = Manifest::default();
        install(home.path(), &mut m).unwrap();
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
    let cfg = oc(home.path(), "opencode.json");
    assert!(!cfg.contains(AXL), "{cfg}");
    assert_eq!(
        json(&cfg)["mcp"]["axle"]["command"][0],
        "/usr/local/bin/axl"
    );
}

#[test]
fn uninstall_restores_original() {
    for case in ["empty", "unrelated", "jsonc_comments"] {
        let home = home_for(case);
        let mut m = Manifest::default();
        install(home.path(), &mut m).unwrap();
        let plan = plan_uninstall(home.path(), &m).unwrap();
        apply(&plan, datetime!(2026-11-27 12:10:00 UTC)).unwrap();
        assert_eq!(tree(home.path()), tree(&fixture(case)), "{case}");
    }
}

#[test]
fn uninstall_after_adoption_removes_managed_form_only() {
    let home = home_for("handmade");
    let mut m = Manifest::default();
    install(home.path(), &mut m).unwrap();
    let plan = plan_uninstall(home.path(), &m).unwrap();
    apply(&plan, datetime!(2026-11-27 12:10:00 UTC)).unwrap();
    let v = json(&oc(home.path(), "opencode.json"));
    assert!(v["mcp"].get("axle").is_none());
    assert_eq!(v["mcp"]["other"]["command"][0], "other-mcp");
}

#[test]
fn opencode_unusual_shape_unparseable_jsonc_needs_manual() {
    for (file, text) in [
        ("opencode.jsonc", "// c\n{ \"mcp\": { \"x\": \n"),
        ("opencode.json", "[1, 2]\n"),
        ("opencode.json", "{ \"mcp\": 3 }\n"),
        (
            "opencode.jsonc",
            "{ \"mcp\": [] } // arrays are not servers\n",
        ),
    ] {
        let home = Temp::new();
        std::fs::create_dir_all(home.path().join(".config/opencode")).unwrap();
        std::fs::write(home.path().join(".config/opencode").join(file), text).unwrap();
        let err = plan_install(home.path(), Path::new(AXL), &Manifest::default()).unwrap_err();
        match err {
            EditError::NeedsManual {
                file: f, snippet, ..
            } => {
                assert!(f.ends_with(file), "{text:?}");
                assert!(
                    snippet.contains("\"bridge\", \"--harness\", \"opencode\""),
                    "{snippet}"
                );
            }
            e => panic!("{text:?}: {e}"),
        }
        assert_eq!(oc(home.path(), file), text, "never rewritten");
    }
}

#[test]
fn orphan_marker_in_agents_md_needs_a_manual_step() {
    let home = Temp::new();
    std::fs::create_dir_all(home.path().join(".config/opencode")).unwrap();
    let text = format!("rules\n{START}\nhalf a block\n");
    std::fs::write(home.path().join(".config/opencode/AGENTS.md"), &text).unwrap();
    let err = plan_install(home.path(), Path::new(AXL), &Manifest::default()).unwrap_err();
    let EditError::NeedsManual { reason, .. } = err else {
        panic!("expected manual step")
    };
    assert!(reason.contains("no matching"), "{reason}");
    assert_eq!(oc(home.path(), "AGENTS.md"), text);
}
