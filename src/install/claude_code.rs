//! Claude Code: an Axle-owned plugin directory linked into `~/.claude/skills/axle`, or (the
//! fallback) user-scope entries in `~/.claude.json` and `~/.claude/settings.json`; in both
//! modes the rules block in `~/.claude/CLAUDE.md`. Existing hand-made `axle` entries and bare
//! `axl hook <event>` hooks are adopted: replaced by the managed form and recorded in the
//! manifest.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use jsonc_parser::cst::{CstInputValue, CstObject, CstObjectProp};
use serde_json::Value;

use super::jsonc::{self, number, object, string, strings};
use super::{
    EditError, EntryKind, FileEdit, Harness, INSTRUCTIONS, LinkEdit, Manifest, ManifestEntry, Plan,
    is_axl_hook_command, remove_block, upsert_block,
};
use crate::client::AxlDir;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeMode {
    /// Plugin directory under `~/.axl/integrations/claude-code/`, linked into
    /// `~/.claude/skills/axle`.
    Plugin,
    /// Entries written straight into the user-scope Claude Code files.
    UserScope,
}

/// How `axl install` configures Claude Code.
///
/// Decided from the Claude Code docs, 2026-11-27, not from a live session (the spike was
/// answered by documentation research): a directory under `~/.claude/skills/` that holds
/// `.claude-plugin/plugin.json` is a skills-directory plugin, loaded at every session start
/// with no flag and no install step, and at user level it loads its hooks, `.mcp.json`
/// servers and skills (https://code.claude.com/docs/en/plugins/create.md). A symlink to
/// `~/.axl/integrations/claude-code/` counts as in place. Caveats that make
/// `ClaudeMode::UserScope` stay implemented and tested: a same-named marketplace plugin
/// shadows it, and managed settings (`strictPluginOnlyCustomization`,
/// `strictKnownMarketplaces`) can silently skip it. Claude Code version: not run.
pub const MODE: ClaudeMode = ClaudeMode::Plugin;

/// Settings key and `axl hook` argument for each event the hooks cover.
const EVENTS: [(&str, &str); 4] = [
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("PostToolUse", "post-tool-use"),
    ("Stop", "stop"),
];

const PLUGIN_FILES: [&str; 4] = [
    ".claude-plugin/plugin.json",
    ".mcp.json",
    "hooks/hooks.json",
    "skills/axle/SKILL.md",
];

struct Paths {
    claude_json: PathBuf,
    settings: PathBuf,
    claude_md: PathBuf,
    plugin_dir: PathBuf,
    link: PathBuf,
    config_toml: PathBuf,
}

impl Paths {
    fn new(home: &Path) -> Self {
        Self {
            claude_json: home.join(".claude.json"),
            settings: home.join(".claude").join("settings.json"),
            claude_md: home.join(".claude").join("CLAUDE.md"),
            plugin_dir: AxlDir::new(home).integrations_dir().join("claude-code"),
            link: home.join(".claude").join("skills").join("axle"),
            config_toml: AxlDir::new(home).config_path(),
        }
    }
}

pub(super) fn read(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io::Error::new(
            e.kind(),
            format!("cannot read {}: {e}", path.display()),
        )),
    }
}

/// An `axl` path as it appears in a shell command: bare when it is plain, else single-quoted.
pub(super) fn shell_word(path: &str) -> String {
    let plain = !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+@:".contains(c));
    if plain {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

fn hook_command(axl: &str, arg: &str) -> String {
    format!("{} hook {arg}", shell_word(axl))
}

fn mcp_entry(axl: &str) -> CstInputValue {
    object(vec![
        ("type", string("stdio")),
        ("command", string(axl)),
        ("args", strings(&["bridge", "--harness", "claude-code"])),
    ])
}

fn handler(command: &str) -> CstInputValue {
    object(vec![
        ("type", string("command")),
        ("command", string(command)),
        ("timeout", number(5)),
    ])
}

fn hook_group(command: &str) -> CstInputValue {
    object(vec![(
        "hooks",
        CstInputValue::Array(vec![handler(command)]),
    )])
}

fn hooks_snippet(axl: &str) -> String {
    jsonc::render(object(vec![(
        "hooks",
        object(
            EVENTS
                .iter()
                .map(|(e, a)| {
                    (
                        *e,
                        CstInputValue::Array(vec![hook_group(&hook_command(axl, a))]),
                    )
                })
                .collect(),
        ),
    )]))
}

fn mcp_snippet(axl: &str) -> String {
    jsonc::render(object(vec![(
        "mcpServers",
        object(vec![("axle", mcp_entry(axl))]),
    )]))
}

fn plugin_files(axl: &str) -> Vec<(&'static str, String)> {
    let plugin_json = jsonc::render(object(vec![
        ("name", string("axle")),
        ("version", string(env!("CARGO_PKG_VERSION"))),
        (
            "description",
            string("Axle: message other coding agents and claim tasks under leases"),
        ),
    ]));
    let mcp_json = jsonc::render(object(vec![(
        "mcpServers",
        object(vec![("axle", mcp_entry(axl))]),
    )]));
    let hooks_json = hooks_snippet(axl);
    let skill = format!(
        "---\nname: axle\ndescription: How to coordinate with other agents through the axle MCP tools\n---\n\n{INSTRUCTIONS}"
    );
    vec![
        (PLUGIN_FILES[0], plugin_json),
        (PLUGIN_FILES[1], mcp_json),
        (PLUGIN_FILES[2], hooks_json),
        (PLUGIN_FILES[3], skill),
    ]
}

struct Ctx<'a> {
    manifest: &'a Manifest,
    paths: Paths,
    axl: String,
    plan: Plan,
    entries: Vec<ManifestEntry>,
    adopted: Vec<String>,
    /// `AXL_SERVER_URL` values found in adopted entries.
    urls: Vec<String>,
}

impl Ctx<'_> {
    fn known(&self, file: &Path, key: &str) -> bool {
        self.manifest
            .entries
            .iter()
            .any(|e| e.harness == Harness::ClaudeCode && e.file == file && e.key == key)
    }

    fn push(&mut self, file: &Path, kind: EntryKind, key: &str, original: Option<Value>) {
        self.entries.push(ManifestEntry {
            harness: Harness::ClaudeCode,
            file: file.to_path_buf(),
            kind,
            key: key.to_string(),
            original,
        });
    }

    /// Records an adoption of `original` under `key` unless an earlier run already did.
    fn adopt(&mut self, file: &Path, key: &str, what: String, original: Value) {
        if !self.known(file, key) {
            self.adopted.push(what);
        }
        self.push(file, EntryKind::Adopted, key, Some(original));
    }
}

/// Plans an install. Returns the plan, the manifest entries to record, and a description of
/// each hand-made entry that was adopted.
pub fn plan_install(
    home: &Path,
    axl: &Path,
    manifest: &Manifest,
    mode: ClaudeMode,
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
    let claude_json = edit_claude_json(&mut cx, mode)?;
    let settings = edit_settings(&mut cx, mode)?;
    cx.plan.edits.extend(claude_json);
    cx.plan.edits.extend(settings);
    if mode == ClaudeMode::Plugin {
        plan_plugin(&mut cx)?;
    }
    // In both modes: the plugin's skill loads only when the model asks for it, while
    // `~/.claude/CLAUDE.md` is read at every session start.
    plan_claude_md(&mut cx)?;
    plan_config_toml(&mut cx)?;
    Ok((cx.plan, cx.entries, cx.adopted))
}

fn env_url(entry: &Value) -> Option<String> {
    entry
        .get("env")?
        .get("AXL_SERVER_URL")?
        .as_str()
        .map(str::to_string)
}

fn mcp_servers(
    file: &Path,
    obj: &CstObject,
    snippet: &str,
) -> Result<Option<CstObject>, EditError> {
    match obj.get("mcpServers") {
        None => Ok(None),
        Some(p) => match p.value().and_then(|v| v.as_object()) {
            Some(o) => Ok(Some(o)),
            None => Err(EditError::NeedsManual {
                file: file.to_path_buf(),
                reason: "`mcpServers` is not an object".into(),
                snippet: snippet.into(),
            }),
        },
    }
}

fn prop_value(p: &CstObjectProp) -> Value {
    p.value()
        .and_then(|v| v.to_serde_value())
        .unwrap_or(Value::Null)
}

fn edit_claude_json(cx: &mut Ctx, mode: ClaudeMode) -> Result<Option<FileEdit>, EditError> {
    let file = cx.paths.claude_json.clone();
    let before = read(&file)?;
    if before.is_none() && mode == ClaudeMode::Plugin {
        return Ok(None);
    }
    let snippet = mcp_snippet(&cx.axl);
    let (root, obj) = jsonc::parse_object(&file, before.as_deref().unwrap_or(""), &snippet)?;
    let servers = mcp_servers(&file, &obj, &snippet)?;
    let existing = servers.as_ref().and_then(|s| s.get("axle"));
    let key = "mcpServers.axle";
    match mode {
        ClaudeMode::Plugin => {
            let (Some(servers), Some(prop)) = (servers, existing) else {
                return Ok(None);
            };
            let value = prop_value(&prop);
            if let Some(u) = env_url(&value) {
                cx.urls.push(u);
            }
            note_dropped_env(cx, &value);
            prop.remove();
            if servers.properties().is_empty()
                && let Some(p) = obj.get("mcpServers")
            {
                p.remove();
            }
            cx.adopt(&file, key, format!("{key} in {}", file.display()), value);
        }
        ClaudeMode::UserScope => {
            let desired = mcp_entry(&cx.axl);
            match existing {
                Some(prop) => {
                    let current = prop_value(&prop);
                    if current == jsonc::to_value(&desired) {
                        cx.push(&file, EntryKind::Added, key, None);
                    } else {
                        if let Some(u) = env_url(&current) {
                            cx.urls.push(u);
                        }
                        note_dropped_env(cx, &current);
                        prop.set_value(desired);
                        cx.adopt(&file, key, format!("{key} in {}", file.display()), current);
                    }
                }
                None => {
                    obj.object_value_or_create("mcpServers")
                        .ok_or_else(|| EditError::NeedsManual {
                            file: file.clone(),
                            reason: "`mcpServers` is not an object".into(),
                            snippet: snippet.clone(),
                        })?
                        .append("axle", desired);
                    cx.push(&file, EntryKind::Added, key, None);
                }
            }
        }
    }
    if before.is_none() {
        cx.push(&file, EntryKind::CreatedFile, "file", None);
    }
    Ok(Some(FileEdit {
        path: file,
        before,
        after: Some(root.to_string()),
    }))
}

/// Anything in the old entry's `env` besides the server URL is lost with the entry; say so.
fn note_dropped_env(cx: &mut Ctx, entry: &Value) {
    if let Some(env) = entry.get("env").and_then(Value::as_object) {
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

struct Found {
    event: String,
    command: String,
    value: Value,
}

/// Visits every `axl hook <event>` handler under `hooks`; `visit` returns true to remove it.
/// Emptied groups and event arrays are removed with it. Returns true if anything was removed.
fn walk_handlers(hooks: &CstObject, visit: &mut dyn FnMut(Found) -> bool) -> bool {
    let mut any = false;
    for ev in hooks.properties() {
        let (Some(event), Some(arr)) = (ev.decoded_name(), ev.array_value()) else {
            continue;
        };
        let mut removed_here = false;
        for group in arr.elements() {
            let Some(handlers) = group.as_object().and_then(|g| g.array_value("hooks")) else {
                continue;
            };
            let mut removed_group = false;
            for h in handlers.elements() {
                let Some(ho) = h.as_object() else { continue };
                let Some(command) = ho
                    .get("command")
                    .and_then(|p| p.value())
                    .and_then(|n| n.as_string_lit())
                    .and_then(|s| s.decoded_value().ok())
                else {
                    continue;
                };
                if is_axl_hook_command(&command).is_none() {
                    continue;
                }
                let value = h.to_serde_value().unwrap_or(Value::Null);
                if visit(Found {
                    event: event.clone(),
                    command,
                    value,
                }) {
                    h.remove();
                    removed_group = true;
                }
            }
            if removed_group {
                removed_here = true;
                if handlers.elements().is_empty() {
                    group.remove();
                }
            }
        }
        if removed_here {
            any = true;
            if arr.elements().is_empty() {
                ev.remove();
            }
        }
    }
    any
}

fn edit_settings(cx: &mut Ctx, mode: ClaudeMode) -> Result<Option<FileEdit>, EditError> {
    let file = cx.paths.settings.clone();
    let before = read(&file)?;
    if before.is_none() && mode == ClaudeMode::Plugin {
        return Ok(None);
    }
    let snippet = hooks_snippet(&cx.axl);
    let manual = |reason: &str| EditError::NeedsManual {
        file: file.clone(),
        reason: reason.into(),
        snippet: snippet.clone(),
    };
    let (root, obj) = jsonc::parse_object(&file, before.as_deref().unwrap_or(""), &snippet)?;
    if let Some(p) = obj.get("hooks")
        && p.value().and_then(|v| v.as_object()).is_none()
    {
        return Err(manual("`hooks` is not an object"));
    }
    let axl = cx.axl.clone();
    let desired = |event: &str| -> Option<(String, Value)> {
        let (_, arg) = EVENTS.iter().find(|(e, _)| *e == event)?;
        let cmd = hook_command(&axl, arg);
        let value = jsonc::to_value(&handler(&cmd));
        Some((cmd, value))
    };

    // Which events already hold exactly the managed handler and nothing else of ours.
    let mut seen: Vec<Found> = Vec::new();
    if let Some(hooks) = obj.object_value("hooks") {
        walk_handlers(&hooks, &mut |f| {
            seen.push(f);
            false
        });
    }
    let kept: HashSet<&str> = if mode == ClaudeMode::UserScope {
        EVENTS
            .iter()
            .map(|(e, _)| *e)
            .filter(|e| {
                let mine: Vec<&Found> = seen.iter().filter(|f| f.event == *e).collect();
                let (cmd, value) = desired(e).expect("known event");
                mine.len() == 1 && mine[0].command == cmd && mine[0].value == value
            })
            .collect()
    } else {
        HashSet::new()
    };

    let mut originals: Vec<(String, Vec<Value>)> = Vec::new();
    let mut removed_any = false;
    if let Some(hooks) = obj.object_value("hooks") {
        removed_any = walk_handlers(&hooks, &mut |f| {
            if kept.contains(f.event.as_str()) {
                return false;
            }
            match originals.iter_mut().find(|(e, _)| *e == f.event) {
                Some((_, v)) => v.push(f.value),
                None => originals.push((f.event, vec![f.value])),
            }
            true
        });
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
        cx.adopt(&file, &key, what, Value::Array(values));
    }

    if mode == ClaudeMode::Plugin {
        if removed_any
            && let Some(p) = obj.get("hooks")
            && p.value()
                .and_then(|v| v.as_object())
                .is_some_and(|h| h.properties().is_empty())
        {
            p.remove();
        }
    } else {
        for (event, arg) in EVENTS {
            let key = format!("hooks.{event}");
            if !kept.contains(event) {
                let hooks = obj
                    .object_value_or_create("hooks")
                    .ok_or_else(|| manual("`hooks` is not an object"))?;
                let arr = hooks
                    .array_value_or_create(event)
                    .ok_or_else(|| manual(&format!("`hooks.{event}` is not an array")))?;
                arr.append(hook_group(&hook_command(&cx.axl, arg)));
            }
            // An adoption above already recorded this key; the dedup keeps its original.
            cx.push(&file, EntryKind::Added, &key, None);
        }
    }
    if before.is_none() {
        cx.push(&file, EntryKind::CreatedFile, "file", None);
    }
    Ok(Some(FileEdit {
        path: file,
        before,
        after: Some(root.to_string()),
    }))
}

fn plan_plugin(cx: &mut Ctx) -> Result<(), EditError> {
    let dir = cx.paths.plugin_dir.clone();
    for (rel, content) in plugin_files(&cx.axl) {
        let path = dir.join(rel);
        let before = read(&path)?;
        cx.plan.edits.push(FileEdit {
            path: path.clone(),
            before,
            after: Some(content),
        });
        cx.push(&path, EntryKind::CreatedFile, "file", None);
    }
    let link = cx.paths.link.clone();
    let before = std::fs::read_link(&link).ok();
    if let Ok(meta) = link.symlink_metadata()
        && meta.is_dir()
    {
        return Err(EditError::NeedsManual {
            file: link,
            reason: "a real directory is where axl links the plugin".into(),
            snippet: format!(
                "move it aside, or remove it, then re-run; the link should point at {}",
                dir.display()
            ),
        });
    }
    let original = before
        .as_ref()
        .filter(|t| **t != dir)
        .map(|t| Value::String(t.to_string_lossy().into_owned()));
    cx.plan.links.push(LinkEdit {
        path: link.clone(),
        target: Some(dir),
        before,
    });
    cx.push(&link, EntryKind::Link, "skills/axle", original);
    Ok(())
}

fn plan_claude_md(cx: &mut Ctx) -> Result<(), EditError> {
    let file = cx.paths.claude_md.clone();
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

/// Moves an adopted `AXL_SERVER_URL` into `config.toml` when it has no `server_url`.
fn plan_config_toml(cx: &mut Ctx) -> Result<(), EditError> {
    let Some(url) = cx.urls.first().cloned() else {
        return Ok(());
    };
    let file = cx.paths.config_toml.clone();
    if plan_server_url(&file, &url, &mut cx.plan)? {
        cx.push(&file, EntryKind::Added, "server_url", None);
    }
    Ok(())
}

/// Plans writing `server_url = <url>` into the axl `config.toml` at `file` unless it already
/// has one (then a note says which URL was kept). Returns true when the key is added.
/// Shared by every harness editor that adopts an `AXL_SERVER_URL` entry.
pub(super) fn plan_server_url(file: &Path, url: &str, plan: &mut Plan) -> Result<bool, EditError> {
    let before = read(file)?;
    let snippet = format!("server_url = \"{url}\"\n");
    let mut doc: toml_edit::DocumentMut =
        before
            .as_deref()
            .unwrap_or("")
            .parse()
            .map_err(|e: toml_edit::TomlError| EditError::NeedsManual {
                file: file.to_path_buf(),
                reason: format!("it is not valid TOML ({e}); axl will not rewrite it"),
                snippet: snippet.clone(),
            })?;
    match doc.get("server_url").and_then(|v| v.as_str()) {
        Some(existing) if existing != url => {
            plan.notes.push(format!(
                "the old axle entry used AXL_SERVER_URL={url}; kept server_url = \"{existing}\" from {}",
                file.display()
            ));
            Ok(false)
        }
        Some(_) => Ok(false),
        None => {
            doc["server_url"] = toml_edit::value(url);
            plan.edits.push(FileEdit {
                path: file.to_path_buf(),
                before,
                after: Some(doc.to_string()),
            });
            Ok(true)
        }
    }
}

/// Plans the removal of what install added: the plugin files and link, or the user-scope
/// entries, matched by key and command. Adopted hand-made entries are not restored (the
/// backups keep them).
pub fn plan_uninstall(home: &Path, manifest: &Manifest) -> Result<Plan, EditError> {
    let paths = Paths::new(home);
    let mine: Vec<&ManifestEntry> = manifest
        .entries
        .iter()
        .filter(|e| e.harness == Harness::ClaudeCode)
        .collect();
    let created = |file: &Path| {
        mine.iter()
            .any(|e| e.file == file && e.kind == EntryKind::CreatedFile)
    };
    let has = |file: &Path, key: &str| mine.iter().any(|e| e.file == file && e.key == key);
    let mut plan = Plan::default();

    // ~/.claude.json: the user-scope axle server.
    if has(&paths.claude_json, "mcpServers.axle")
        && let Some(before) = read(&paths.claude_json)?
    {
        let (root, obj) = jsonc::parse_object(&paths.claude_json, &before, "")?;
        if let Some(servers) = mcp_servers(&paths.claude_json, &obj, "")?
            && let Some(p) = servers.get("axle")
        {
            p.remove();
            if servers.properties().is_empty()
                && let Some(p) = obj.get("mcpServers")
            {
                p.remove();
            }
        }
        plan.edits.push(finish_json(
            &paths.claude_json,
            before,
            &root,
            &obj,
            created(&paths.claude_json),
        ));
    }

    // ~/.claude/settings.json: every `axl hook <event>` handler.
    let event_keys: Vec<&str> = EVENTS
        .iter()
        .map(|(e, _)| *e)
        .filter(|e| has(&paths.settings, &format!("hooks.{e}")))
        .collect();
    if !event_keys.is_empty()
        && let Some(before) = read(&paths.settings)?
    {
        let (root, obj) = jsonc::parse_object(&paths.settings, &before, "")?;
        if let Some(hooks) = obj.object_value("hooks") {
            let removed = walk_handlers(&hooks, &mut |f| event_keys.contains(&f.event.as_str()));
            if removed
                && hooks.properties().is_empty()
                && let Some(p) = obj.get("hooks")
            {
                p.remove();
            }
        }
        plan.edits.push(finish_json(
            &paths.settings,
            before,
            &root,
            &obj,
            created(&paths.settings),
        ));
    }

    // ~/.claude/CLAUDE.md: the instruction block.
    if has(&paths.claude_md, "block")
        && let Some(before) = read(&paths.claude_md)?
    {
        let after = remove_block(&before).map_err(|e| EditError::NeedsManual {
            reason: e.describe(&paths.claude_md),
            snippet: "remove the `# >>> axle >>>` block by hand".into(),
            file: paths.claude_md.clone(),
        })?;
        let gone = created(&paths.claude_md) && after.trim().is_empty();
        plan.edits.push(FileEdit {
            path: paths.claude_md.clone(),
            before: Some(before),
            after: (!gone).then_some(after),
        });
    }

    // The plugin files and the link.
    for e in mine
        .iter()
        .filter(|e| e.kind == EntryKind::CreatedFile && e.file.starts_with(&paths.plugin_dir))
    {
        if let Some(before) = read(&e.file)? {
            plan.edits.push(FileEdit {
                path: e.file.clone(),
                before: Some(before),
                after: None,
            });
        }
    }
    for e in mine.iter().filter(|e| e.kind == EntryKind::Link) {
        let current = std::fs::read_link(&e.file).ok();
        if current.as_deref() == Some(paths.plugin_dir.as_path()) {
            let restore = e
                .original
                .as_ref()
                .and_then(Value::as_str)
                .map(PathBuf::from);
            plan.links.push(LinkEdit {
                path: e.file.clone(),
                target: restore,
                before: current,
            });
        }
    }
    Ok(plan)
}

/// The edit for a JSON file after removals: deleted when install created it and nothing is
/// left in it.
fn finish_json(
    path: &Path,
    before: String,
    root: &jsonc_parser::cst::CstRootNode,
    obj: &CstObject,
    created: bool,
) -> FileEdit {
    let empty = created && obj.properties().is_empty();
    FileEdit {
        path: path.to_path_buf(),
        before: Some(before),
        after: (!empty).then(|| root.to_string()),
    }
}
