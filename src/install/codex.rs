//! Codex: the `[mcp_servers.axle]` bridge, a marker block of `[[hooks.*]]` tables and the
//! instructions block, all user-level under `~/.codex/`. `config.toml` is edited with
//! `toml_edit`, so comments, key order and blank lines outside what Axle owns survive.
//!
//! Hooks follow the Codex hooks docs (https://learn.chatgpt.com/docs/hooks.md, read
//! 2026-11-27): inline `[[hooks.<Event>]]` groups in `config.toml`, each with a
//! `[[hooks.<Event>.hooks]]` handler (`type = "command"`, `command`, `timeout` in seconds).
//! The commands are `axl hook <event> --harness codex`; Codex says non-managed hooks must
//! be reviewed and trusted before they run, so the summary tells the user it will ask.
//!
//! Hand-made setups are adopted (spec §5): an existing `[mcp_servers.axle]` in any shape,
//! and any `axl hook <event>` handler at any path for the events Axle manages, are
//! replaced by the managed form and recorded. An `axl hook` handler for an event Axle does
//! not register in Codex (user-prompt-submit) is left alone.

use std::path::{Path, PathBuf};

use serde_json::Value;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table};

use super::claude_code::{plan_server_url, read, shell_word};
use super::{
    BLOCK_END, BLOCK_START, EditError, EntryKind, FileEdit, Harness, INSTRUCTIONS, Manifest,
    ManifestEntry, Plan, is_axl_hook_command, remove_block, upsert_block,
};
use crate::client::AxlDir;

/// Codex event name, the `axl hook` argument for it and its timeout in seconds.
const EVENTS: [(&str, &str, i64); 4] = [
    ("SessionStart", "session-start", HOOK_TIMEOUT_SECS),
    ("PostToolUse", "post-tool-use", HOOK_TIMEOUT_SECS),
    ("Stop", "stop", HOOK_TIMEOUT_SECS),
    ("SessionEnd", "session-end", 3),
];

const BRIDGE_ARGS: [&str; 3] = ["bridge", "--harness", "codex"];
const MCP_KEY: &str = "mcp_servers.axle";
/// Codex kills a hook after this many seconds; `axl hook` gives up after 5 s on its own, so
/// it always finishes first. SessionEnd uses a shorter 3 s: the session is already closing.
const HOOK_TIMEOUT_SECS: i64 = 10;

struct Paths {
    config: PathBuf,
    agents: PathBuf,
    axl_config: PathBuf,
}

impl Paths {
    fn new(home: &Path) -> Self {
        Self {
            config: home.join(".codex").join("config.toml"),
            agents: home.join(".codex").join("AGENTS.md"),
            axl_config: AxlDir::new(home).config_path(),
        }
    }
}

fn toml_string(s: &str) -> String {
    toml_edit::Value::from(s).to_string().trim().to_string()
}

fn hook_command(axl: &str, arg: &str) -> String {
    format!("{} hook {arg} --harness codex", shell_word(axl))
}

/// The text between the markers in `config.toml`.
fn hooks_body(axl: &str) -> String {
    EVENTS
        .iter()
        .map(|(event, arg, timeout)| {
            format!(
                "[[hooks.{event}]]\n[[hooks.{event}.hooks]]\ntype = \"command\"\ncommand = {}\ntimeout = {timeout}\n",
                toml_string(&hook_command(axl, arg))
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn mcp_snippet(axl: &str) -> String {
    format!(
        "[mcp_servers.axle]\ncommand = {}\nargs = [\"bridge\", \"--harness\", \"codex\"]\n",
        toml_string(axl)
    )
}

fn snippet(axl: &str) -> String {
    format!(
        "{}\n{BLOCK_START}\n{}{BLOCK_END}\n",
        mcp_snippet(axl),
        hooks_body(axl)
    )
}

fn mcp_table(axl: &str) -> Table {
    let mut t = Table::new();
    t.insert("command", toml_edit::value(axl));
    t.insert("args", toml_edit::value(bridge_args()));
    t
}

fn mcp_inline(axl: &str) -> InlineTable {
    let mut t = InlineTable::new();
    t.insert("command", toml_edit::Value::from(axl));
    t.insert("args", toml_edit::Value::Array(bridge_args()));
    t
}

fn bridge_args() -> Array {
    let mut a = Array::new();
    for s in BRIDGE_ARGS {
        a.push(s);
    }
    a
}

/// `item` as JSON, for the manifest's record of what was replaced.
fn item_json(item: &Item) -> Value {
    let mut doc = DocumentMut::new();
    doc.insert("x", item.clone());
    toml::from_str::<Value>(&doc.to_string())
        .ok()
        .and_then(|v| v.get("x").cloned())
        .unwrap_or(Value::Null)
}

struct Ctx<'a> {
    manifest: &'a Manifest,
    paths: Paths,
    axl: String,
    plan: Plan,
    entries: Vec<ManifestEntry>,
    adopted: Vec<String>,
    urls: Vec<String>,
}

impl Ctx<'_> {
    fn known(&self, file: &Path, key: &str) -> bool {
        self.manifest
            .entries
            .iter()
            .any(|e| e.harness == Harness::Codex && e.file == file && e.key == key)
    }

    fn push(&mut self, file: &Path, kind: EntryKind, key: &str, original: Option<Value>) {
        self.entries.push(ManifestEntry {
            harness: Harness::Codex,
            file: file.to_path_buf(),
            kind,
            key: key.to_string(),
            original,
        });
    }

    fn adopt(&mut self, file: &Path, key: &str, what: String, original: Value) {
        if !self.known(file, key) {
            self.adopted.push(what);
        }
        self.push(file, EntryKind::Adopted, key, Some(original));
    }

    fn manual(&self, file: &Path, reason: impl Into<String>) -> EditError {
        EditError::NeedsManual {
            file: file.to_path_buf(),
            reason: reason.into(),
            snippet: snippet(&self.axl),
        }
    }
}

/// Plans an install. Returns the plan, the manifest entries to record, and a description
/// of each hand-made entry that was adopted.
pub fn plan_install(
    home: &Path,
    axl: &Path,
    manifest: &Manifest,
) -> Result<(Plan, Vec<ManifestEntry>, Vec<String>), EditError> {
    let mut cx = Ctx {
        manifest,
        paths: Paths::new(home),
        axl: axl.to_string_lossy().into_owned(),
        plan: Plan::default(),
        entries: Vec::new(),
        adopted: Vec::new(),
        urls: Vec::new(),
    };
    plan_config(&mut cx)?;
    plan_agents(&mut cx)?;
    if let Some(url) = cx.urls.first().cloned() {
        let file = cx.paths.axl_config.clone();
        if plan_server_url(&file, &url, &mut cx.plan)? {
            cx.push(&file, EntryKind::Added, "server_url", None);
        }
    }
    Ok((cx.plan, cx.entries, cx.adopted))
}

fn plan_config(cx: &mut Ctx) -> Result<(), EditError> {
    let file = cx.paths.config.clone();
    let before = read(&file)?;
    let text = before.clone().unwrap_or_default();
    remove_block(&text).map_err(|e| cx.manual(&file, e.describe(&file)))?;
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        cx.manual(
            &file,
            format!("it is not valid TOML ({e}); axl will not rewrite it"),
        )
    })?;
    edit_mcp(cx, &file, &mut doc)?;
    adopt_hooks(cx, &file, &mut doc)?;

    let body = hooks_body(&cx.axl);
    let after = upsert_block(&doc.to_string(), &body).expect("orphan markers checked above");
    // A `hooks` table written in a shape that cannot take more `[[hooks.*]]` entries makes
    // the result invalid; then nothing is changed.
    if let Err(e) = toml::from_str::<toml::Table>(&after) {
        return Err(cx.manual(
            &file,
            format!("the result would not be valid TOML ({e}); is `hooks` defined in a form axl cannot extend?"),
        ));
    }
    let block = format!("{BLOCK_START}\n{body}{BLOCK_END}\n");
    if !text.contains(&block) {
        cx.plan.notes.push(
            "Codex asks you to trust new hooks on its next start (review them with /hooks)"
                .to_string(),
        );
    }
    if before.is_none() {
        cx.push(&file, EntryKind::CreatedFile, "file", None);
    }
    cx.push(&file, EntryKind::Block, "block", None);
    cx.plan.edits.push(FileEdit {
        path: file,
        before,
        after: Some(after),
    });
    Ok(())
}

/// Sets `[mcp_servers.axle]` to the managed table, adopting whatever was there.
fn edit_mcp(cx: &mut Ctx, file: &Path, doc: &mut DocumentMut) -> Result<(), EditError> {
    let axl = cx.axl.clone();
    let desired = serde_json::json!({"command": axl, "args": BRIDGE_ARGS});
    let existing: Option<Value> = match doc.get("mcp_servers") {
        None => None,
        Some(Item::Table(t)) => t.get("axle").map(item_json),
        Some(Item::Value(toml_edit::Value::InlineTable(t))) => {
            t.get("axle").map(|v| item_json(&Item::Value(v.clone())))
        }
        Some(_) => return Err(cx.manual(file, "`mcp_servers` is not a table")),
    };
    match &existing {
        Some(current) if *current == desired => {}
        Some(current) => {
            if let Some(u) = current
                .get("env")
                .and_then(|e| e.get("AXL_SERVER_URL"))
                .and_then(Value::as_str)
            {
                cx.urls.push(u.to_string());
            }
            if let Some(env) = current.get("env").and_then(Value::as_object) {
                let extra: Vec<&str> = env
                    .keys()
                    .map(String::as_str)
                    .filter(|k| *k != "AXL_SERVER_URL")
                    .collect();
                if !extra.is_empty() {
                    cx.plan.notes.push(format!(
                        "the old axle entry set env {}; the managed entry does not (originals are in the manifest and the backup)",
                        extra.join(", ")
                    ));
                }
            }
        }
        None => {}
    }
    if existing.as_ref() != Some(&desired) {
        match doc.get_mut("mcp_servers") {
            None => {
                let mut servers = Table::new();
                servers.set_implicit(true);
                servers.insert("axle", Item::Table(mcp_table(&axl)));
                doc.insert("mcp_servers", Item::Table(servers));
            }
            Some(Item::Table(t)) => {
                t.insert("axle", Item::Table(mcp_table(&axl)));
            }
            Some(Item::Value(toml_edit::Value::InlineTable(t))) => {
                t.insert("axle", toml_edit::Value::InlineTable(mcp_inline(&axl)));
            }
            Some(_) => unreachable!("shape checked above"),
        }
    }
    match existing {
        Some(current) if current != desired => cx.adopt(
            file,
            MCP_KEY,
            format!("{MCP_KEY} in {}", file.display()),
            current,
        ),
        _ => cx.push(file, EntryKind::Added, MCP_KEY, None),
    }
    Ok(())
}

/// Removes every `axl hook <event>` handler (any path) for the managed events, along with
/// groups and event arrays it empties, and records each event as adopted.
fn adopt_hooks(cx: &mut Ctx, file: &Path, doc: &mut DocumentMut) -> Result<(), EditError> {
    match doc.get("hooks") {
        None => return Ok(()),
        Some(Item::Table(_)) => {}
        Some(_) => return Err(cx.manual(file, "`hooks` is not a table")),
    }
    let hooks = doc["hooks"].as_table_mut().expect("checked above");
    let mut originals: Vec<(String, Vec<Value>)> = Vec::new();
    for (event, _, _) in EVENTS {
        let Some(groups) = hooks.get_mut(event).and_then(Item::as_array_of_tables_mut) else {
            continue;
        };
        let mut found: Vec<Value> = Vec::new();
        let mut gi = 0;
        while gi < groups.len() {
            let group = groups.get_mut(gi).expect("in range");
            let removed = remove_axl_handlers(group, &mut found);
            let empty = group
                .get("hooks")
                .map(|h| match h {
                    Item::ArrayOfTables(a) => a.is_empty(),
                    Item::Value(v) => v.as_array().is_some_and(Array::is_empty),
                    _ => false,
                })
                .unwrap_or(true);
            if removed && empty {
                groups.remove(gi);
            } else {
                gi += 1;
            }
        }
        if !found.is_empty() {
            originals.push((event.to_string(), found));
            if groups.is_empty() {
                hooks.remove(event);
            }
        }
    }
    if hooks.is_empty() {
        doc.remove("hooks");
    }
    for (event, values) in originals {
        let key = format!("hooks.{event}");
        let what = format!(
            "{key} ({}) in {}",
            values
                .iter()
                .filter_map(|v| v.get("command").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", "),
            file.display()
        );
        cx.adopt(file, &key, what, Value::Array(values));
    }
    Ok(())
}

fn is_axl_hook(handler: &Table) -> bool {
    handler
        .get("command")
        .and_then(Item::as_str)
        .is_some_and(|c| is_axl_hook_command(c).is_some())
}

/// Removes the `axl hook` handlers of one group (an `[[hooks.E.hooks]]` array of tables, or
/// an inline `hooks = [{..}]` array), pushing each one's JSON onto `found`. True if any.
fn remove_axl_handlers(group: &mut Table, found: &mut Vec<Value>) -> bool {
    let before = found.len();
    match group.get_mut("hooks") {
        Some(Item::ArrayOfTables(handlers)) => {
            let mut i = 0;
            while i < handlers.len() {
                let h = handlers.get(i).expect("in range");
                if is_axl_hook(h) {
                    found.push(item_json(&Item::Table(h.clone())));
                    handlers.remove(i);
                } else {
                    i += 1;
                }
            }
        }
        Some(Item::Value(toml_edit::Value::Array(handlers))) => {
            let mut i = 0;
            while i < handlers.len() {
                let hit = handlers
                    .get(i)
                    .and_then(toml_edit::Value::as_inline_table)
                    .and_then(|t| t.get("command"))
                    .and_then(toml_edit::Value::as_str)
                    .is_some_and(|c| is_axl_hook_command(c).is_some());
                if hit {
                    let v = handlers.get(i).expect("in range").clone();
                    found.push(item_json(&Item::Value(v)));
                    handlers.remove(i);
                } else {
                    i += 1;
                }
            }
        }
        _ => {}
    }
    found.len() > before
}

fn plan_agents(cx: &mut Ctx) -> Result<(), EditError> {
    let file = cx.paths.agents.clone();
    let before = read(&file)?;
    let after = upsert_block(before.as_deref().unwrap_or(""), INSTRUCTIONS).map_err(|e| {
        EditError::NeedsManual {
            reason: e.describe(&file),
            snippet: INSTRUCTIONS.to_string(),
            file: file.clone(),
        }
    })?;
    if before.is_none() {
        cx.push(&file, EntryKind::CreatedFile, "file", None);
    }
    cx.push(&file, EntryKind::Block, "block", None);
    cx.plan.edits.push(FileEdit {
        path: file,
        before,
        after: Some(after),
    });
    Ok(())
}

/// Plans the removal of what install added: the `axle` MCP table, the hooks block and the
/// instructions block. Adopted hand-made entries are not restored (the backups keep them).
/// A file install created is deleted once nothing else is left in it.
pub fn plan_uninstall(home: &Path, manifest: &Manifest) -> Result<Plan, EditError> {
    let paths = Paths::new(home);
    let mine: Vec<&ManifestEntry> = manifest
        .entries
        .iter()
        .filter(|e| e.harness == Harness::Codex)
        .collect();
    let created = |file: &Path| {
        mine.iter()
            .any(|e| e.file == file && e.kind == EntryKind::CreatedFile)
    };
    let has = |file: &Path, key: &str| mine.iter().any(|e| e.file == file && e.key == key);
    let mut plan = Plan::default();

    let config = &paths.config;
    if (has(config, MCP_KEY) || has(config, "block"))
        && let Some(before) = read(config)?
    {
        let manual = |reason: String| EditError::NeedsManual {
            file: config.clone(),
            reason,
            snippet: "remove the `axle` MCP server and the `# >>> axle >>>` block by hand".into(),
        };
        let mut text = before.clone();
        if has(config, "block") {
            text = remove_block(&text).map_err(|e| manual(e.describe(config)))?;
        }
        if has(config, MCP_KEY) {
            let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
                manual(format!(
                    "it is not valid TOML ({e}); axl will not rewrite it"
                ))
            })?;
            remove_mcp(&mut doc);
            text = doc.to_string();
        }
        let gone = created(config) && text.trim().is_empty();
        plan.edits.push(FileEdit {
            path: config.clone(),
            before: Some(before),
            after: (!gone).then_some(text),
        });
    }

    let agents = &paths.agents;
    if has(agents, "block")
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

/// Removes `mcp_servers.axle`, and `mcp_servers` itself when that leaves it empty and install
/// is the one that wrote it (an implicit table, or an inline one).
fn remove_mcp(doc: &mut DocumentMut) {
    let empty = match doc.get_mut("mcp_servers") {
        Some(Item::Table(t)) => {
            t.remove("axle");
            t.is_empty() && t.is_implicit()
        }
        Some(Item::Value(toml_edit::Value::InlineTable(t))) => {
            t.remove("axle");
            false
        }
        _ => false,
    };
    if empty {
        doc.remove("mcp_servers");
    }
}
