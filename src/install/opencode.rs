//! OpenCode: the `mcp.axle` local server in `~/.config/opencode/opencode.jsonc` (or
//! `opencode.json`) and the instructions block in `~/.config/opencode/AGENTS.md`.
//! OpenCode has no shell-hook system, so there are no hooks (wake-up comes later).
//!
//! The config is edited on the shared JSONC CST (`super::jsonc`), so comments, key order
//! and trailing commas outside the edited spot survive. A file the CST cannot parse, or
//! whose `mcp` is not an object, is never rewritten: the editor returns
//! `EditError::NeedsManual` with the exact snippet instead.

use std::path::{Path, PathBuf};

use jsonc_parser::cst::{CstInputValue, CstObject};
use serde_json::Value;

use super::claude_code::{plan_server_url, read};
use super::jsonc::{self, object, string, strings};
use super::{
    EditError, EntryKind, FileEdit, Harness, INSTRUCTIONS, Manifest, ManifestEntry, Plan,
    remove_block, upsert_block,
};
use crate::client::AxlDir;

const SCHEMA: &str = "https://opencode.ai/config.json";
const MCP_KEY: &str = "mcp.axle";

struct Paths {
    jsonc: PathBuf,
    json: PathBuf,
    agents: PathBuf,
    axl_config: PathBuf,
}

impl Paths {
    fn new(home: &Path) -> Self {
        let dir = home.join(".config").join("opencode");
        Self {
            jsonc: dir.join("opencode.jsonc"),
            json: dir.join("opencode.json"),
            agents: dir.join("AGENTS.md"),
            axl_config: AxlDir::new(home).config_path(),
        }
    }

    /// `opencode.jsonc` if it exists, else `opencode.json` (which install creates if need be).
    fn target(&self) -> &Path {
        if self.jsonc.exists() {
            &self.jsonc
        } else {
            &self.json
        }
    }
}

fn mcp_entry(axl: &str) -> CstInputValue {
    object(vec![
        ("type", string("local")),
        (
            "command",
            strings(&[axl, "bridge", "--harness", "opencode"]),
        ),
        ("enabled", CstInputValue::Bool(true)),
    ])
}

/// What to add by hand, with the bridge command on one line.
fn snippet(axl: &str) -> String {
    let axl = serde_json::to_string(axl).expect("string serializes");
    format!(
        "{{\n  \"mcp\": {{\n    \"axle\": {{\n      \"type\": \"local\",\n      \"command\": [{axl}, \"bridge\", \"--harness\", \"opencode\"],\n      \"enabled\": true\n    }}\n  }}\n}}\n"
    )
}

/// The `environment` keys of an old entry other than the server URL, which the managed
/// entry does not carry.
fn dropped_env(entry: &Value) -> Vec<String> {
    entry
        .get("environment")
        .and_then(Value::as_object)
        .map(|env| {
            env.keys()
                .filter(|k| *k != "AXL_SERVER_URL")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Plans an install. Returns the plan, the manifest entries to record, and a description
/// of each hand-made entry that was adopted.
pub fn plan_install(
    home: &Path,
    axl: &Path,
    manifest: &Manifest,
) -> Result<(Plan, Vec<ManifestEntry>, Vec<String>), EditError> {
    let paths = Paths::new(home);
    let axl = axl.to_string_lossy().into_owned();
    let mut plan = Plan::default();
    let mut entries: Vec<ManifestEntry> = Vec::new();
    let mut adopted: Vec<String> = Vec::new();
    let push = |entries: &mut Vec<ManifestEntry>,
                file: &Path,
                kind: EntryKind,
                key: &str,
                original: Option<Value>| {
        entries.push(ManifestEntry {
            harness: Harness::Opencode,
            file: file.to_path_buf(),
            kind,
            key: key.to_string(),
            original,
        });
    };

    // opencode.json[c]: the axle server.
    let file = paths.target().to_path_buf();
    let before = read(&file)?;
    let snip = snippet(&axl);
    let manual = |reason: &str| EditError::NeedsManual {
        file: file.clone(),
        reason: reason.into(),
        snippet: snip.clone(),
    };
    let desired = mcp_entry(&axl);
    let mut url: Option<String> = None;
    let after = if before.is_none() {
        jsonc::render(object(vec![
            ("$schema", string(SCHEMA)),
            ("mcp", object(vec![("axle", desired)])),
        ]))
    } else {
        let (root, obj) = jsonc::parse_object(&file, before.as_deref().unwrap_or(""), &snip)?;
        let servers = match obj.get("mcp") {
            None => None,
            Some(p) => Some(
                p.value()
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| manual("`mcp` is not an object"))?,
            ),
        };
        match servers.as_ref().and_then(|s| s.get("axle")) {
            Some(prop) => {
                let current = prop
                    .value()
                    .and_then(|v| v.to_serde_value())
                    .unwrap_or(Value::Null);
                if current == jsonc::to_value(&desired) {
                    push(&mut entries, &file, EntryKind::Added, MCP_KEY, None);
                } else {
                    url = current
                        .get("environment")
                        .and_then(|e| e.get("AXL_SERVER_URL"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let extra = dropped_env(&current);
                    if !extra.is_empty() {
                        plan.notes.push(format!(
                            "the old axle entry set environment {}; the managed entry does not (originals are in the manifest and the backup)",
                            extra.join(", ")
                        ));
                    }
                    prop.set_value(desired);
                    let known = manifest.entries.iter().any(|e| {
                        e.harness == Harness::Opencode && e.file == file && e.key == MCP_KEY
                    });
                    if !known {
                        adopted.push(format!("{MCP_KEY} in {}", file.display()));
                    }
                    push(
                        &mut entries,
                        &file,
                        EntryKind::Adopted,
                        MCP_KEY,
                        Some(current),
                    );
                }
            }
            None => {
                obj.object_value_or_create("mcp")
                    .ok_or_else(|| manual("`mcp` is not an object"))?
                    .append("axle", desired);
                push(&mut entries, &file, EntryKind::Added, MCP_KEY, None);
            }
        }
        root.to_string()
    };
    if before.is_none() {
        push(&mut entries, &file, EntryKind::CreatedFile, "file", None);
        push(&mut entries, &file, EntryKind::Added, MCP_KEY, None);
    }
    plan.edits.push(FileEdit {
        path: file,
        before,
        after: Some(after),
    });

    // AGENTS.md: the instructions block.
    let file = paths.agents.clone();
    let before = read(&file)?;
    let after = upsert_block(before.as_deref().unwrap_or(""), INSTRUCTIONS).map_err(|e| {
        EditError::NeedsManual {
            reason: e.describe(&file),
            snippet: INSTRUCTIONS.to_string(),
            file: file.clone(),
        }
    })?;
    if before.is_none() {
        push(&mut entries, &file, EntryKind::CreatedFile, "file", None);
    }
    push(&mut entries, &file, EntryKind::Block, "block", None);
    plan.edits.push(FileEdit {
        path: file,
        before,
        after: Some(after),
    });

    // An adopted AXL_SERVER_URL moves into ~/.axl/config.toml.
    if let Some(url) = url
        && plan_server_url(&paths.axl_config, &url, &mut plan)?
    {
        push(
            &mut entries,
            &paths.axl_config,
            EntryKind::Added,
            "server_url",
            None,
        );
    }
    Ok((plan, entries, adopted))
}

/// Plans the removal of what install added: the `axle` MCP entry and the instructions
/// block. Adopted hand-made entries are not restored (the backups keep them). A file
/// install created is deleted once nothing else is left in it.
pub fn plan_uninstall(home: &Path, manifest: &Manifest) -> Result<Plan, EditError> {
    let paths = Paths::new(home);
    let mine: Vec<&ManifestEntry> = manifest
        .entries
        .iter()
        .filter(|e| e.harness == Harness::Opencode)
        .collect();
    let created = |file: &Path| {
        mine.iter()
            .any(|e| e.file == file && e.kind == EntryKind::CreatedFile)
    };
    let mut plan = Plan::default();

    // The file the entry was written to, whichever of the two it was.
    let mut files: Vec<&Path> = Vec::new();
    for e in mine.iter().filter(|e| e.key == MCP_KEY) {
        if !files.contains(&e.file.as_path()) {
            files.push(&e.file);
        }
    }
    for file in files {
        let Some(before) = read(file)? else { continue };
        let (root, obj) = jsonc::parse_object(file, &before, "")?;
        remove_axle(file, &obj)?;
        let only_schema = {
            let props = obj.properties();
            props.is_empty()
                || (props.len() == 1 && props[0].decoded_name().as_deref() == Some("$schema"))
        };
        let gone = created(file) && only_schema;
        plan.edits.push(FileEdit {
            path: file.to_path_buf(),
            before: Some(before),
            after: (!gone).then(|| root.to_string()),
        });
    }

    let agents = &paths.agents;
    if mine.iter().any(|e| e.file == *agents && e.key == "block")
        && let Some(before) = read(agents)?
    {
        let after = remove_block(&before).map_err(|e| EditError::NeedsManual {
            reason: e.describe(agents),
            snippet: "remove the `# >>> axle >>>` block by hand".into(),
            file: agents.clone(),
        })?;
        let gone = created(agents) && after.trim().is_empty();
        plan.edits.push(FileEdit {
            path: agents.clone(),
            before: Some(before),
            after: (!gone).then_some(after),
        });
    }
    Ok(plan)
}

/// Removes `mcp.axle`, and `mcp` itself when that leaves it empty.
fn remove_axle(file: &Path, obj: &CstObject) -> Result<(), EditError> {
    let Some(mcp) = obj.get("mcp") else {
        return Ok(());
    };
    let servers =
        mcp.value()
            .and_then(|v| v.as_object())
            .ok_or_else(|| EditError::NeedsManual {
                file: file.to_path_buf(),
                reason: "`mcp` is not an object".into(),
                snippet: "remove the `axle` entry under `mcp` by hand".into(),
            })?;
    if let Some(p) = servers.get("axle") {
        p.remove();
        if servers.properties().is_empty() {
            mcp.remove();
        }
    }
    Ok(())
}
