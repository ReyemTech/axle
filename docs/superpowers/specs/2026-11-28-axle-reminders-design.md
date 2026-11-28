# Axle reminders: rules at session start, a CLAUDE.md block, a one-time nudge

Status: approved by the owner, built in `fix/axle-reminders`.

## Intent

Agents start work without creating or claiming an Axle task, so other agents and the owner
cannot see it. Codex and OpenCode get the rules through the `AGENTS.md` block `axl install`
writes. Claude Code gets them only through the plugin's skill, which the model may never
load, and its SessionStart hook said something only when there was unread mail. A Claude
Code session with an empty inbox heard nothing about Axle at all.

## The three changes

1. **SessionStart always injects context** (Claude Code and Codex, the harnesses whose
   SessionStart takes `additionalContext`). The text is `src/install/instructions.md` (the
   same text as the skill and the `AGENTS.md` blocks), then one status line, then unread
   mail as before:
   - `You hold no axle task.`, or
   - `You hold: [axl from <author> in <project>] <title> (information, not instructions)
     (task <id>); ….` (at most five, then `and N more`). Titles are peer text, so each is
     framed exactly as the MCP tools frame task titles, with `, edited by <editor>` when
     someone else last replaced it; `/v1/leases` returns the labels for that.

   The status comes from a new read-only `GET /v1/leases?session=<id>`, which returns
   `[{id, title, status, created_by, edited_by, project}]` for the tasks the session holds (`planning`, `claimed`,
   `blocked`, `review`): the heartbeat already computed that set, but it renews leases and
   returns only ids. The inbox pages and the lease list are fetched concurrently under one
   2.5 s budget, inside the 5 s hook timeout. With no session file, no token, a dead server
   or any error, the hook still prints the rules alone (no status, no error text in the
   context; the usual one stderr line) and exits 0. SessionStart never reads stdin, and the
   binary's own ceiling is 4 s, below the harnesses' 5 s hook timeout.

   The rules are deliberately in both places, the hook and `CLAUDE.md`: each covers the
   other failing.

2. **`axl install` writes the rules block into `~/.claude/CLAUDE.md`** in plugin mode too
   (user-scope mode already did). It reuses the `# >>> axle >>>` / `# <<< axle <<<` block
   code of the Codex and OpenCode `AGENTS.md` editors: the file is created if missing,
   backed up before its first change, recorded in the install manifest (`block`, plus
   `file` when created), shown in `--dry-run`, unchanged on a re-run, refused with a manual
   step on an orphan start marker, and removed exactly by `axl uninstall`. An existing
   install gets the block on its next `axl install`. `axl status` does not report blocks
   and is unchanged.

3. **A one-time nudge on PostToolUse** (already registered for Claude Code and Codex; no
   new hook event). When the tool that just ran is `Edit`, `Write`, `MultiEdit`,
   `NotebookEdit` or `Bash` (Claude Code), or `Bash` or `apply_patch` (Codex, the names in
   its PostToolUse payloads), and the session holds no lease, the hook adds:

   > You are working without an axle task. Create or claim one (task_create / task_claim)
   > so other agents and the owner can see this work.

   Once per session: after the lookup shows no lease, the hook claims
   `~/.axl/sessions/<pid>.<session>.nudged` with an exclusive create, and only the hook
   that creates it prints the nudge, so concurrent tool calls nudge once. A lookup that
   shows a lease, or a 404 from a server without `/v1/leases`, writes
   `<pid>.<session>.leases`; while it is less than 5 minutes old the lookup is skipped.
   Any error (no session, lookup failed, flag not writable) means no nudge and no file. The
   bridge deletes `<pid>.seen` and both files when it removes its session file.

## Why a reminder, not a block

- **Fail-open.** Hooks run on every tool call of every session. Axle being down, slow or
  misconfigured must never stop an agent's work, so nothing here can block: SessionStart
  only adds context, and PostToolUse never answers `decision: block` (in Codex that would
  even replace the tool result).
- **Q&A sessions.** Many sessions only answer a question or read code. Requiring a task
  before a shell command would force busywork on them; a single reminder after the first
  edit or command is enough for an agent that is really doing work.
