# Waking idle agents: live harness adapters (phase 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `WakeRequested` for an idle Claude Code, OpenCode or Codex session starts a new turn in the window the human already has open.

**Architecture:** Each harness gets one `WakeAdapter` (from `src/bridge/wake.rs`, core plan Task 9) in `src/bridge/adapters/`, chosen by `select_adapter`. The bridge reports the adapter's `WakeChannel` only after `check()` passes, so the router never reserves a session it cannot poke. Each adapter sits behind a spike that settles the harness's undocumented or experimental surface first. A failed spike sends that harness to phase 2 only, and no adapter ships for it. These ship as **three PRs in spec order**, each after its spike: Claude Code (spec PR 2), OpenCode (PR 3), Codex (PR 4).

**Tech Stack:** Rust 2024, tokio (`UnixStream`), reqwest 0.13, serde_json, the core plan's `WakeAdapter` / `PokeOutcome`.

**Spec:** `docs/superpowers/specs/2026-11-28-wake-up-design.md`, sections "Wake path, end to end (phase 1)", "Security" and "Open questions" 1–3. Depends on `2026-11-28-wake-up-core.md` being merged.

## Global Constraints

- **Spikes never modify the owner's real harness configs.** Allowed: `--help`, the docs in the spec's appendix, a throwaway session started in a scratch directory under `.superpowers/spikes/` with configuration passed only by flags or environment (`claude --settings <scratch.json>`, `CODEX_HOME=<scratch>`, `XDG_CONFIG_HOME` / `XDG_DATA_HOME=<scratch>`), and the harness's documented local sockets and APIs. Forbidden: editing `~/.claude*`, `~/.codex/`, `~/.config/opencode/`, running `axl install` against the real HOME, or changing a permission mode. When a step needs the owner's signed-in harness, the plan marks it **owner runs**. The implementer writes the script and the owner runs it.
- Each spike's findings are committed to `docs/superpowers/specs/2026-11-28-wake-up-spikes.md` (one section per spike: harness version, what was tried, the verdict, exact formats). `.superpowers/` stays uncommitted.
- Versions checked: Claude Code **2.1.224+** (socket), codex-cli **0.172.0**, OpenCode **2.3.2**. An adapter's `check()` fails closed on an older harness.
- The adapter pokes with the core's rendered wake text only: a pointer, framed peer title of at most 80 characters, no bodies, no secrets.
- Secrets copied for an adapter live only in `~/.axl/` files with mode **0600** (directory 0700), are never sent to the server, and never appear in argv, logs or prompts.
- Every poke finishes in **30 s** (`tokio::time::timeout`). Failure gives `PokeOutcome::Failed` and is reported once, never retried in a loop. Hooks and the bridge stay fail-open.
- Adapters never answer approval requests, never send `turn/steer`, and never change permission modes or inbound policies (`crossSessionInbound`, `bypassPermissions` holds).
- Same-change docs: `docs/harnesses/{claude-code,codex,opencode}.md`, `docs/cli.md`, `docs/troubleshooting.md`.
- Conventional commits. No AI or tool attribution.

## Review Focus

1. **Harness closed or restarted while its bridge lives.** The socket or server is gone: `check()` turns false, the next heartbeat reports `wake_channel = none`, and a poke in flight gives `Failed` within 30 s (Tasks 2, 5 and 7 `*_gone_reports_none`).
2. **A stale `<pid>.wake.json` left by a crashed session** whose pid was reused. The adapter must not poke another session's socket: the file records the session id and the adapter checks it (Task 2 `stale_wake_file_is_ignored`).
3. **The user's inbound policy holds or refuses the message** (`bypassPermissions`, `crossSessionInbound: refuse`). It is recorded as `held` / `refused`, never retried, and the policy is never changed (Task 2 `held_and_refused_are_reported_once`).
4. **Several OpenCode sessions in one project directory.** The wake goes to the session mapped to this bridge, or, under the spike's fallback, to the most recently idle one, never to all of them (Task 5 `wakes_exactly_one_opencode_session`).
5. **A Codex approval request arrives on the shared app-server while the bridge is attached.** The bridge does not answer it, and the TUI still gets it (Task 7 `approval_requests_are_left_alone`).

---

## File Structure

| Path | Responsibility |
|---|---|
| `src/bridge/adapters/mod.rs` | `select_adapter` branches per harness. |
| `src/bridge/adapters/claude.rs` | Inbox-socket adapter (or the spike's fallback). |
| `src/bridge/adapters/opencode.rs` | Shared-server API adapter and status stream. |
| `src/bridge/adapters/codex.rs` | App-server client adapter. |
| `src/hook/mod.rs` | `session-start` writes `<pid>.wake.json` for Claude Code. |
| `src/cli/codex.rs` | `axl codex [--experimental-wake] [args]` launcher. |
| `src/install/opencode.rs` | One-time pairing into `~/.axl/opencode.json`. |
| `tests/adapters_*.rs` | Adapters against fakes (recording socket, fake OpenCode, fake app-server). |

---

### Task 1: SPIKE 1, the Claude Code inbox socket line format

**Files:**
- Create: `.superpowers/spikes/claude-socket/` (scratch: a `settings.json` with one SessionStart hook that dumps `CLAUDE_CODE_MESSAGING_SOCKET` and `CLAUDE_CODE_MESSAGING_SOCKET_TOKEN` to a 0600 scratch file, and `poke.py` / `poke.sh` that connect and send)
- Modify: `docs/superpowers/specs/2026-11-28-wake-up-spikes.md` (section "Spike 1")

**Question:** What exact auth line and message line make an idle Claude Code session start a turn, and do the fallbacks work if the socket cannot be used?

- [ ] **Step 1:** Read https://code.claude.com/docs/en/cross-session-messaging, …/hooks and …/plugins-reference, and `claude --help` (confirm `--settings`). Write down the documented env names, the framing (newline-delimited JSON or text), the auth handshake and the 30 s limit.
- [ ] **Step 2 (owner runs):** `cd .superpowers/spikes/claude-socket && claude --settings ./settings.json`. Leave the session idle at its prompt.
- [ ] **Step 3:** From another terminal, run `poke` with the documented format (auth line, then one message line `[axl wake] spike test, reply with the word pong`).
- [ ] **Step 4: Verdict.** **PASS** when, within 5 s, the idle session starts a turn whose input is the message, three times out of three, and a wrong token is refused. Record the exact bytes sent and the response lines.
- [ ] **Step 5: Only on FAIL, try the fallbacks in order, with the same pass criterion.** (a) A plugin monitor: a scratch plugin passed with `--plugin-dir` whose monitor runs a script that prints the message. (b) An `asyncRewake` hook started at SessionStart that polls a scratch file and exits 2 with the message. Record which one passes. The verdict is one of `socket`, `monitor`, `async-rewake` or `none`.
- [ ] **Step 6: Commit** the spike section: `docs(spike): claude code inbox socket wake`.

**Fallback if every option fails:** Claude Code is phase 2 only. Skip Tasks 2 and 3. `select_adapter` keeps `NoneAdapter` for `claude-code`, and `docs/harnesses/claude-code.md` says live wake is not available and why.

---

### Task 2: Claude Code socket adapter (when spike 1 says `socket`)

**Files:**
- Create: `src/bridge/adapters/mod.rs`, `src/bridge/adapters/claude.rs`, `tests/adapters_claude.rs`
- Modify: `src/hook/mod.rs` (`session-start` writes the wake file), `src/bridge/session_file.rs` (`wake_path(home, pid)`), `src/bridge/wake.rs` (`select_adapter` branch), `docs/harnesses/claude-code.md`

**Interfaces:**
- Consumes: `WakeAdapter`, `PokeOutcome`, `WakeChannel::ClaudeSocket`.
- Produces:
  - `struct WakeFile { session_id: SessionId, socket: PathBuf, token: String }`, stored as `~/.axl/sessions/<pid>.wake.json` (0600). It is written by `axl hook session-start` from `CLAUDE_CODE_MESSAGING_SOCKET` / `_TOKEN` and the session file's `session_id`, and removed with the session file.
  - `pub struct ClaudeSocketAdapter { home: PathBuf, harness_pid: u32, session: SessionId }`. `check()` is true when the file exists, its `session_id` matches, and the socket accepts a connection. `poke(text)` connects, writes the auth line and then one message line in spike 1's exact format, reads the reply, and maps it to `Sent`, `Held(reason)` or `Refused(reason)`. Everything else is `Failed`.

- [ ] **Step 1: Write failing tests** against a recording `UnixListener` that replays spike 1's recorded replies: `pokes_with_auth_then_message_line` (the exact bytes), `held_and_refused_are_reported_once`, `stale_wake_file_is_ignored` (a different `session_id` makes `check` false and nothing is connected), `claude_socket_gone_reports_none`, `poke_times_out_at_30s` (paused tokio time), `wake_file_is_0600_and_never_sent` (a hook test: the file mode, and the token never appears in any request body the fake server records).
- [ ] **Step 2: Run** `cargo test --test adapters_claude` and `cargo test --test hook wake_file`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same commands plus `cargo test --test bridge`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(bridge): wake idle Claude Code sessions through the inbox socket`

---

### Task 3: Claude Code fallback adapter (only when spike 1 says `monitor` or `async-rewake`)

**Files:**
- Create: `src/bridge/adapters/claude.rs` (`ClaudeMonitorAdapter`), `src/cli/watch.rs` (`axl watch`, for `monitor`) or a hook event `async-rewake` in `src/hook/mod.rs`, `tests/adapters_claude.rs`
- Modify: `src/install/claude_code.rs` and the golden fixtures (the plugin's monitor entry or the SessionStart async hook), `docs/harnesses/claude-code.md`

**Interfaces:**
- Produces: `ClaudeMonitorAdapter` with channel `ClaudeMonitor`. `poke(text)` writes the text atomically to `~/.axl/sessions/<pid>.wake` (0600). The monitor or rewake process (`axl watch --pid <pid>` / `axl hook async-rewake`) waits for that file, consumes it, and emits it the way spike 1 recorded (a monitor line, or exit 2 with the text on stderr). Then it exits or keeps watching, as recorded. `check()` is true while the watcher's heartbeat file `<pid>.watch` is under 60 s old.

- [ ] **Step 1: Write failing tests:** `watch_emits_and_consumes_once`, `watch_ignores_other_pid_file`, `monitor_gone_reports_none`, `async_rewake_exits_2_only_with_a_message` (with no file it exits 0 after its poll window), plus the install golden fixtures.
- [ ] **Step 2: Run** `cargo test --test adapters_claude --test install_claude_code`. Expected: FAIL.
- [ ] **Step 3: Implement** only the branch the spike chose. Delete this task's unused branch from the plan when executing.
- [ ] **Step 4: Run** the same tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(bridge): wake idle Claude Code sessions through a monitor fallback`

---

### Task 4: SPIKE 3a, the OpenCode v2 shared server (port, auth, session mapping)

**Files:**
- Create: `.superpowers/spikes/opencode/` (scratch `XDG_CONFIG_HOME` / `XDG_DATA_HOME`, `probe.sh`)
- Modify: `docs/superpowers/specs/2026-11-28-wake-up-spikes.md` (section "Spike 3a")

**Question:** How does a bridge find the per-user OpenCode server, authenticate, map itself to its own session, read status, and start a turn? (What an unanswered "ask" does in `opencode run` is spike 3b, in the headless plan.)

- [ ] **Step 1:** Read https://opencode.ai/docs/server/ and https://opencode.ai/v1/docs/server/, and run `opencode --help` and `opencode serve --help`. Write down the default port or discovery file, the auth scheme, `GET /api/info`, `GET /api/event`, `POST /api/session/{id}/prompt` and `POST /api/pair`.
- [ ] **Step 2:** Start an isolated server with the scratch XDG dirs (`opencode serve` on a non-default port if supported) and open a TUI against it in a scratch project directory. If the shared server cannot be isolated, **owner runs** Steps 2–4 against their own server, and pairing happens once with the credential kept in the scratch directory.
- [ ] **Step 3:** Pair, list sessions, and find how a process that is a child of the TUI (the bridge's position: parent pid, cwd, env such as an OpenCode session variable) can identify its own session id.
- [ ] **Step 4: Verdict.** **PASS** when (1) the server is found without user input, (2) an authenticated `POST …/prompt` to the mapped session makes the idle TUI start a turn within 5 s, three times out of three, (3) `GET /api/event` shows busy and idle transitions for that session, and (4) an unauthenticated call is refused. A **mapping fallback** counts as a pass if exact mapping is impossible: the most recently idle session whose directory is the bridge's cwd (spec). Record which mapping applies.
- [ ] **Step 5: Commit** `docs(spike): opencode shared server wake`.

**Fallback if it fails** (no discovery, no auth, or no turn start): OpenCode is phase 2 only. Skip Task 5 and say so in `docs/harnesses/opencode.md`.

---

### Task 5: OpenCode API adapter and pairing

**Files:**
- Create: `src/bridge/adapters/opencode.rs`, `tests/adapters_opencode.rs`
- Modify: `src/install/opencode.rs` (pair once, write `~/.axl/opencode.json` 0600 and record it in the manifest; `axl uninstall` removes it), `src/bridge/wake.rs`, `src/bridge/mod.rs` (heartbeat `turn_state` from `adapter.turn_state()`), `docs/harnesses/opencode.md`, `docs/cli.md`

**Interfaces:**
- Produces: `pub struct OpencodeAdapter { base: Url, credential: String, cwd: PathBuf, mapping: Mapping }` with `enum Mapping { Exact(String), MostRecentIdleInDir }`, from spike 3a. A background task reads `GET /api/event` and keeps `turn_state` (`Busy` / `Idle`) for the mapped session. When it changes, it posts `/v1/turn` at once instead of waiting for the next heartbeat. `poke(text)` sends `POST /api/session/{id}/prompt` with the text as the user message and gives `Sent` on 2xx. A 401 gives `Refused("opencode pairing expired; run axl install")`. `check()` is `GET /api/info` 2xx with a version of 2.3.2 or later.

- [ ] **Step 1: Write failing tests** against an axum fake OpenCode server: `pairs_once_and_stores_0600`, `reads_status_stream_into_turn_state`, `pokes_mapped_session`, `wakes_exactly_one_opencode_session` (three sessions in the directory under the fallback mapping: only the most recently idle one is prompted), `opencode_gone_reports_none`, `expired_pairing_is_refused_not_retried`, `credential_never_sent_to_axle` (the fake Axle server's recorded bodies hold no credential).
- [ ] **Step 2: Run** `cargo test --test adapters_opencode --test install_opencode`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same tests plus `cargo test --test bridge`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(bridge): wake idle OpenCode sessions through the shared server`

---

### Task 6: SPIKE 2, Codex app-server thread sharing

**Files:**
- Create: `.superpowers/spikes/codex/` (a scratch `CODEX_HOME` copy containing only `config.toml`; **owner runs** the signed-in steps against their own `CODEX_HOME` read-only, with no config edits; `client.py`, a JSON-RPC client over the Unix socket)
- Modify: `docs/superpowers/specs/2026-11-28-wake-up-spikes.md` (section "Spike 2")

**Question:** Can two clients share one loaded thread on one `codex app-server`, and does a TUI started with `codex --remote unix://<socket>` render a turn another client started?

- [ ] **Step 1:** Read https://developers.openai.com/codex/app-server and run `codex app-server --help` and `codex --help` (confirm `--remote`). Write down the socket flag, `initialize`, `thread/loaded/list`, `turn/start`, the `turn/started` / `turn/completed` notifications and the approval request shape.
- [ ] **Step 2 (owner runs):** Start `codex app-server` on a scratch socket and `codex --remote unix://<socket>` in a scratch git repository. Leave it idle.
- [ ] **Step 3:** Connect `client.py` as a second client: `initialize`, `thread/loaded/list`, then `turn/start` on the TUI's thread with `spike test, reply pong`.
- [ ] **Step 4: Verdict.** **PASS** when (1) the second client sees the TUI's thread in `thread/loaded/list`, (2) its `turn/start` makes the TUI show and run the turn within 5 s, three times out of three, (3) both clients receive `turn/started` / `turn/completed`, and (4) an approval request raised in that turn appears in the TUI and can be answered there while the second client ignores it.
- [ ] **Step 5: Commit** `docs(spike): codex app-server shared thread wake`.

**Fallback if it fails:** Codex is phase 2 only. `axl codex` is not shipped (skip Task 7), a plain `codex` keeps `wake_channel = none`, and `docs/harnesses/codex.md` says so. If only (4) fails, Task 7 still ships behind `--experimental-wake`, and the docs warn that approvals in a woken turn may need the TUI focused.

---

### Task 7: `axl codex --experimental-wake` and the app-server adapter

**Files:**
- Create: `src/cli/codex.rs`, `src/bridge/adapters/codex.rs`, `tests/adapters_codex.rs`
- Modify: `src/main.rs` (`Codex { #[arg(long)] experimental_wake: bool, #[arg(trailing_var_arg)] args: Vec<String> }`), `src/bridge/wake.rs`, `docs/harnesses/codex.md`, `docs/cli.md`

**Interfaces:**
- Produces:
  - `axl codex [--experimental-wake] [args…]`. Without the flag it execs `codex args…` unchanged. With it, it reuses or starts the user's `codex app-server` on `~/.axl/codex/app-server.sock` (0600; reused when it answers `initialize`), then execs `codex --remote unix://<socket> args…` with `AXL_CODEX_SOCKET` set so the bridge (a child) finds it.
  - `pub struct CodexAppServerAdapter { socket: PathBuf, cwd: PathBuf }`. `check()` passes when `initialize` succeeds. Turn state comes from `turn/started` → `Busy` and `turn/completed` → `Idle`, posted to `/v1/turn` at once. `poke(text)` takes `thread/loaded/list`, picks the thread whose cwd is the bridge's cwd (the most recently active one if there are several), and sends `turn/start` with the text. It never sends `turn/steer` and never replies to server-initiated approval requests.

- [ ] **Step 1: Write failing tests** against a fake JSON-RPC app-server on a temp socket: `launcher_reuses_running_app_server`, `launcher_without_flag_is_plain_codex` (a fake `codex` on `PATH` records argv), `pokes_turn_start_on_matching_thread`, `never_sends_turn_steer`, `approval_requests_are_left_alone`, `turn_notifications_drive_turn_state`, `codex_app_server_gone_reports_none`.
- [ ] **Step 2: Run** `cargo test --test adapters_codex`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo fmt --check && cargo clippy --all-targets --features pg-tests -- -D warnings && cargo test`. Expected: clean and PASS.
- [ ] **Step 5: Commit** `feat(cli): axl codex --experimental-wake over the shared app-server`
