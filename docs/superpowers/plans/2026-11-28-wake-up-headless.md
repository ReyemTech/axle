# Waking idle agents: the headless runner (phase 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When routed work has waited 60 s with no free live session, and both the project and the runner machine opted in, `axl runner` does it in a bounded headless run (`claude -p`, `codex exec` or `opencode run`) inside a fresh worktree and delivers a draft PR the human merges.

**Architecture:** The server offers queued work (`RunOffered`). Runners compete for it through one guarded run transaction that checks the kill switches, the caps and that the task is still open and unreserved. The winner creates a worktree, spawns the harness from a fixed template with a scrubbed environment and `AXL_RUN_ID`, and supervises it (duration, budget, stop). After the model exits, the runner itself pushes and opens the PR. The child's bridge registers as `<harness>-headless`, a separate agent, and the server claims the task for that session when it registers. Runs are rows (`wake_attempts` with `kind = headless`, linked by `run_id`), so a restart loses nothing.

**Tech Stack:** Rust 2024, tokio (`process`, `nix`-style process groups via `std::os::unix::process::CommandExt::process_group`), reqwest 0.13, `git` and `gh` CLIs on the runner machine, launchd and systemd user units (`src/daemon/`).

**Spec:** `docs/superpowers/specs/2026-11-28-wake-up-design.md`, section "Phase 2: the headless runner" and "Open questions" 3 (the unanswered ask in `opencode run`) and 4. Depends on `2026-11-28-wake-up-core.md` being merged. It does not depend on the adapters plan.

## Global Constraints

- **Double opt-in:** the project's `headless_enabled` is on **and** the runner lists the project in `[runner] projects`. Either missing means no run.
- Defaults: `max_concurrent_runs` **1** per runner and **1** per project; `daily_run_cap` **5** runs per project per UTC day; `max_run_minutes` **30**; Claude Code `--max-budget-usd` from `max_budget_usd` (none by default). A run is not retried the same UTC day after `timeout` or `failed`.
- Offer only after the task has been queued, open and unreserved for **60 s**.
- Timeout: SIGTERM to the process group, SIGKILL **30 s** later, outcome `timeout`. Commits so far are still pushed as a draft PR, and the task is released with a note.
- Command lines are the spec's table, exactly. Claude Code: `claude -p --output-format stream-json --permission-mode default --permission-prompts none --max-turns <n> --allowedTools <allowlist> [--max-budget-usd <x>]`, prompt on stdin, cwd on the child. Codex: `codex exec --json -C <worktree> -s workspace-write -o <run>/last.txt -`, prompt on stdin. OpenCode: `opencode run --format json --title "axl <task-short>" <prompt>`, cwd on the child. Never `--dangerously-bypass-approvals-and-sandbox`, `--approve-for-me` or `--auto`.
- Default allowlist: reads and edits inside the worktree; `git status`, `git diff`, `git log`, `git add`, `git commit`; plus the project's `[runner.allow."<project>"] bash` entries. `git push` and `gh` are **never** on it.
- The child gets a scrubbed environment: no `AXL_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`, `SSH_AUTH_SOCK`, `CLAUDE_CODE_MESSAGING_SOCKET*` or other harness messaging tokens. It keeps only the variables its own CLI needs to reach its model provider (a per-harness pass list) plus `HOME`, `PATH`, `AXL_RUN_ID` and `AXL_SERVER_URL`. The bridge reads the Axle token from `~/.axl/token`, never from the environment. Nothing secret goes into a prompt or argv.
- The server sends a task id, project and harness name, never a command line. The runner fills a fixed template.
- Paths: worktrees `~/.axl/worktrees/<project>/<task-short>` on branch `axl/<task-short>` (`task-short` = first 8 hex of the task id); logs `~/.axl/logs/runner.log` and `~/.axl/logs/runs/<run id>.jsonl` (kept **14 days**); kept worktrees removed by `axl runner gc` after **14 days**.
- Units: launchd `com.reyemtech.axle.runner`, systemd `axle-runner.service`.
- Axle never merges, never pushes to a default branch, and never approves tool calls.
- Same-change docs: `docs/cli.md`, `docs/configuration.md`, `docs/web-ui.md`, `docs/http-api.md`, `docs/concepts.md`, `docs/security.md`, `docs/harnesses/*.md`.
- Conventional commits. No AI or tool attribution.

## Review Focus

1. **Two runners (or two offers) racing for one task, or a live session claiming it by pull mid-offer.** Exactly one run starts. A task claimed or reserved first gets no run (Task 2 `racing_runners_start_one_run`, `claimed_task_gets_no_run`).
2. **The model tries `git push`, `gh`, or reads a token from its environment.** The tool is denied, not asked, and the environment holds none of the scrubbed variables (Task 6 `allowlist_never_contains_push_or_gh`, `child_env_is_scrubbed`).
3. **A harness that ignores SIGTERM or forks children.** The whole process group is killed 30 s later, and the commits so far are still delivered as a draft (Task 7 `timeout_kills_group_and_delivers`).
4. **A re-run on a task whose branch exists only on `origin`, or whose PR is already open.** The runner continues that branch and updates the same PR, never opening a second one (Task 5 `rerun_continues_remote_branch`, Task 8 `rerun_reuses_open_pr`).
5. **Runner machine without push credentials or a `gh` login.** It reports "can't deliver PRs" in `axl status` and on its UI row, and declines offers (Task 8 `missing_credentials_decline_runs`).

---

## File Structure

| Path | Responsibility |
|---|---|
| `src/store/{mod,sqlite,postgres}.rs` | Runners, the guarded run transaction, run claims, run queries. |
| `src/service/runs.rs` | Offers, run start and finish, stop requests, history, notes. |
| `src/http/runner.rs` | `/v1/runner/*` routes. |
| `src/runner/mod.rs` | `axl runner` loop: register, heartbeat, events, accept. |
| `src/runner/config.rs` | `[runner]` config and checkouts. |
| `src/runner/worktree.rs` | Fetch, worktree, branch reuse, cleanup, gc. |
| `src/runner/harness.rs` | Command templates, allowlist rendering, environment scrubbing. |
| `src/runner/supervise.rs` | Spawn, process group, timeout, stop, logs. |
| `src/runner/deliver.rs` | Credential check, push, draft PR, thread note. |
| `src/daemon/{mod,launchd,systemd}.rs`, `src/cli/service.rs` | `--runner` unit. |
| `src/ui/…`, templates | Headless settings, Pause headless runs, Stop run, run badges, runner rows. |

---

### Task 1: SPIKE 4 (`codex exec` approvals) and SPIKE 3b (unanswered asks in `opencode run`)

**Files:**
- Create: `.superpowers/spikes/headless/` (scratch git repo, scratch `CODEX_HOME` with a copied `config.toml` only, scratch `XDG_CONFIG_HOME` for OpenCode, `probe.sh`)
- Modify: `docs/superpowers/specs/2026-11-28-wake-up-spikes.md` (sections "Spike 4" and "Spike 3b")

**Questions:** (4) codex-cli 0.172 has no `--ask-for-approval` on `exec`: what config key and default govern approvals there, and does the planned command line provably not skip approvals? (3b) What does an unanswered "ask" do in `opencode run` without `--auto`: deny, hang or fail? Also confirm locally that `claude --help` lists `--permission-prompts none` and `--max-budget-usd` as the spec's table assumes.

- [ ] **Step 1:** Run `codex exec --help`, `codex --help` and `codex features list` (if present), and read the Codex config reference for the approval-policy key. Run `opencode run --help` and read https://opencode.ai/docs/cli/. Run `claude --help`. Record the exact key names and defaults. Change no real config.
- [ ] **Step 2 (owner runs):** In the scratch repo, run the Codex template with `CODEX_HOME=<scratch>` (an auth copy only if the owner chooses) and a prompt that needs an out-of-sandbox write (`write /tmp/axl-spike-outside`). Record what happens.
- [ ] **Step 3 (owner runs):** Run the OpenCode template with a scratch permission config whose rule is `ask` for `bash`, and a prompt that runs `ls`. Record whether it denies, hangs (measure to 120 s) or exits.
- [ ] **Step 4: Verdicts.**
  - Codex **PASS**: the out-of-sandbox write is refused without a prompt, and the spike names the config key whose effective value in `exec` is a non-skipping policy. The runner then pins it explicitly with `-c <key>=<value>` so a user's config cannot loosen it. **FAIL** if the write succeeds, or if no key can be pinned.
  - OpenCode **PASS**: an `ask` with nobody to answer is denied or fails fast, or the run can be given permission rules that turn every unlisted tool into `deny` (no `ask` left). **FAIL** if it hangs and no deny-everything rule set is possible.
  - Claude Code **PASS**: both flags are present. **FAIL**: the template is adjusted to what `--help` shows, without widening permissions.
- [ ] **Step 5: Commit** `docs(spike): headless approval behaviour for codex exec and opencode run`.

**Fallbacks:** A harness that fails is not offered for headless runs. Its value is refused by `headless_harness` validation (Task 4) with the text `<harness> headless runs are not supported yet`, and `docs/harnesses/<harness>.md` says so. If all three fail, stop after Task 1 and report: phase 2 does not ship.

---

### Task 2: Runner and run store methods

**Files:**
- Modify: `src/domain.rs` (`WakeOutcome` gains `Offered`, `Started`, `Finished`, `Timeout`, `Cancelled`, `NoCommits`; `TaskEventKind` gains `RunStarted`, `RunFinished`, `PrOpened` (`run_started | run_finished | pr_opened`); `struct Runner { id: Uuid, host: String, harnesses: Vec<String>, projects: Vec<ProjectId>, paused: bool, last_seen_at: OffsetDateTime, can_deliver: bool }`), `src/store/{mod,sqlite,postgres}.rs`. No migration: 0006 already created `runners` and `server_settings`.
- Test: `tests/store_runs.rs`

**Interfaces:**
- Produces:
  - `async fn upsert_runner(&self, r: &Runner) -> StoreResult<()>`; `async fn runner(&self, id: Uuid) -> StoreResult<Option<Runner>>`; `async fn list_runners(&self) -> StoreResult<Vec<Runner>>`
  - `struct RunStart { run_id: Uuid, runner: Uuid, task: TaskId, harness: String, now: OffsetDateTime, day_start: OffsetDateTime }`
  - `enum RunDecision { Started(Task), Refused(&'static str) }`. The refusal texts are `headless runs are paused`, `headless runs are off for this project`, `runner does not allow this project`, `runner is busy`, `project is busy`, `daily run cap reached`, `not retried today after a failed run`, `task is no longer open`.
  - `async fn start_run(&self, s: &RunStart) -> StoreResult<RunDecision>`. One transaction, under the task's lock: `headless_paused` off, the project's `headless_enabled` on, the runner's projects contain it, the runner not paused, running runs per runner and per project below their caps (a run is running when its `started` row has no terminal row with the same `run_id`), `started` rows for the project since `day_start` below `daily_run_cap`, no `timeout` / `failed` row for the task since `day_start`, and the task `open` with no unexpired reservation. On success it appends the `started` row and a `run_started` event.
  - `async fn claim_for_run(&self, run_id: Uuid, session: SessionId, now: OffsetDateTime, ttl: Duration) -> StoreResult<Option<Task>>`: claims the run's task for that session in one transaction (same event as a normal claim) and sets `sessions.run_id`.
  - `async fn finish_run(&self, run_id: Uuid, outcome: WakeOutcome, detail: Option<&str>, pr_url: Option<&str>, now: OffsetDateTime) -> StoreResult<()>` (appends the terminal row and `run_finished`, plus `pr_opened` when `pr_url` is set)
  - `async fn running_runs(&self) -> StoreResult<Vec<RunView>>`, with `struct RunView { run_id, runner, task, project, harness, started_at }`

- [ ] **Step 1: Write failing tests:** `racing_runners_start_one_run` (16 concurrent `start_run` for one task from 4 runners: exactly one `Started`); `claimed_task_gets_no_run`; `reserved_task_gets_no_run`; `caps_hold_inside_the_transaction` (runner busy, project busy, 6th run of the UTC day refused, the next UTC day allowed); `kill_switch_and_opt_in_refuse`; `not_retried_same_day_after_timeout`; `claim_for_run_claims_as_headless_session`; `finish_run_records_pr`.
- [ ] **Step 2: Run** `cargo test --test store_runs`. Expected: FAIL.
- [ ] **Step 3: Implement** both backends.
- [ ] **Step 4: Run** with and without `pg-tests`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(store): runners and the guarded headless run claim`

---

### Task 3: Run offers and the runner API

**Files:**
- Create: `src/service/runs.rs`, `src/http/runner.rs`
- Modify: `src/events.rs` (`RunOffered { task, project_id, harness }`, `RunStarted { run, task }`, `RunFinished { run, task, outcome }`), `src/service/wake/mod.rs` (`tick` calls `RunService::offer_due`), `src/http/v1.rs` (`register` accepts `run_id: Option<Uuid>`), `src/store/mod.rs` (`eligible`: harness `X` also admits `X-headless`), `docs/http-api.md`, `docs/concepts.md`
- Test: `tests/http_runner.rs`

**Interfaces:**
- Consumes: Task 2 store methods; core `WakeService`, `list_queued`.
- Produces:
  - `RunService::offer_due(&self) -> StoreResult<u32>`. For each queued task whose latest `queued` event is at least 60 s old, whose project has `headless_enabled`, and while `headless_paused` is off, it publishes `RunOffered` with the project's `headless_harness`. It appends an `offered` attempt at most once per task per 5 minutes.
  - `POST /v1/runner/register { runner_id, host, harnesses, projects, can_deliver }` gives `{ runner_id }`.
  - `POST /v1/runner/heartbeat { runner_id, running: [run_id] }` gives `{ paused: bool, stop: [run_id] }`. `stop` lists runs to end: Pause headless runs, Stop run, or a run no longer known. This is how a stop reaches a running run. The spec names no stop event.
  - `GET /v1/runner/events?runner=<id>` (SSE): `RunOffered` for the runner's projects only.
  - `POST /v1/runner/runs { runner_id, task_id, harness }` gives `200 { run_id, task: { id, title, framed_title, project } }` or `409 { error: <refusal text> }`. The response also carries the project's `max_run_minutes` and `max_budget_usd`. `RunService` lives in `AppState.runs`.
  - `POST /v1/runner/runs/{run_id}/finish { outcome, detail, pr_url }`.
  - `register` with a `run_id` whose run has started: the session is created for the `<harness>-headless` agent and `claim_for_run` claims the task at once.
  - `RunService::stop(run_id)` (Stop run) and `pause(bool)` (Pause headless runs).

- [ ] **Step 1: Write failing tests:** `offers_after_60s_only_when_opted_in`; `offer_not_sent_while_paused`; `runner_events_filtered_by_projects`; `start_returns_409_with_reason`; `register_with_run_id_claims_task_as_headless`; `headless_agent_is_separate_and_eligible` (eligibility `claude-code` admits `claude-code-headless`; the interactive agent's inbox and its stale-session takeover are untouched); `heartbeat_returns_stop_on_pause_and_stop_run`.
- [ ] **Step 2: Run** `cargo test --test http_runner`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --test http_runner --test wake_router --test http_v1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(http): offer queued work to headless runners`

---

### Task 4: Headless settings, Pause headless runs, Stop run and run badges in the UI

**Files:**
- Modify: `templates/projects.html` and `src/ui/admin.rs` (the Wake-ups panel gains **Headless runs when no agent is free**, off by default, plus harness, concurrency, runs per day, run length and, for Claude Code, budget), `src/ui/actions.rs` (`set_headless`, `set_headless_paused`, `stop_run`), `templates/base.html` (**Pause headless runs**, and `headless paused` while on), `templates/task.html` (**Stop run** while a run holds the task), `_card.html` / `src/ui/views.rs` (`starting headless run`, `headless run · 12m / 30m`, `draft PR`, and the Key), the overview (a runner row: host, projects, paused, runs today, `can't deliver PRs`), `docs/web-ui.md`
- Test: `tests/ui.rs`

**Interfaces:**
- Consumes: `store.put_wake_settings`, `RunService::{stop, pause}`, `running_runs`, `list_runners`.
- Produces: validation errors `run length must be 1–240 minutes`, `runs per day must be 1–50`, `concurrency must be 1–4`, `budget must be a positive amount`, and the Task 1 fallback text for an unsupported harness. Every change is a CSRF POST with a system note.

- [ ] **Step 1: Write failing tests:** `headless_settings_persist_validate_and_note`; `pause_headless_shows_state_and_stops_runs`; `stop_run_button_only_while_running`; `run_badges_render_with_text` (a `started` row 12 minutes ago with a 30-minute limit shows `headless run · 12m / 30m`); `runner_row_shows_cant_deliver`; `headless_forms_need_csrf`.
- [ ] **Step 2: Run** `cargo test --test ui headless`. Expected: FAIL.
- [ ] **Step 3: Implement** with DESIGN.md components. Every state is text.
- [ ] **Step 4: Run** `cargo test --test ui`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): headless run settings, kill switch and run status`

---

### Task 5: Runner config, checkouts and worktrees

**Files:**
- Create: `src/runner/mod.rs` (module root), `src/runner/config.rs`, `src/runner/worktree.rs`, `tests/runner_worktree.rs`
- Modify: `src/client/config.rs` (parse the `[runner]` tables in `~/.axl/config.toml`), `src/bridge/mod.rs` (after registering, record `project key → git toplevel` with a timestamp in `~/.axl/checkouts.json`, 0600, newest wins), `docs/configuration.md`

**Interfaces:**
- Produces:
  - `struct RunnerConfig { max_concurrent: u32, projects: Vec<String>, allow: HashMap<String, Allow>, checkouts: HashMap<String, PathBuf> }` with `struct Allow { bash: Vec<String> }`, parsed exactly from the spec's TOML.
  - `fn source_checkout(cfg: &RunnerConfig, home: &Path, project_key: &str) -> Result<PathBuf, String>`: `[runner.checkouts]` first, else the newest entry in `checkouts.json`. The checkout's normalised `origin` must equal the project key (`bridge::project::normalize_origin`), else `checkout origin does not match <key>`.
  - `async fn prepare(checkout: &Path, home: &Path, project_key: &str, task_short: &str) -> Result<Worktree, String>`: `git fetch origin`, then reuse `axl/<task-short>` when it exists locally or on `origin`, else create it from the default branch (`origin/HEAD`). Always `git worktree add ~/.axl/worktrees/<project>/<task-short>`, never touching the checkout itself. `struct Worktree { path: PathBuf, branch: String, base: String /*commit at start*/ }`.
  - `async fn remove(wt: &Worktree)` (`git worktree remove`) and `async fn gc(home: &Path, older_than: Duration) -> Vec<PathBuf>`.

- [ ] **Step 1: Write failing tests** with a bare remote in a temp directory: `config_parses_spec_example`; `checkout_pin_wins_then_latest`; `origin_mismatch_refused`; `fresh_worktree_from_default_branch`; `rerun_continues_local_branch`; `rerun_continues_remote_branch`; `checkout_itself_is_untouched` (its HEAD, index and status are unchanged); `gc_removes_only_old_kept_worktrees`.
- [ ] **Step 2: Run** `cargo test --test runner_worktree`. Expected: FAIL.
- [ ] **Step 3: Implement.** All git commands use explicit `-C <dir>` and inherit the runner's own environment.
- [ ] **Step 4: Run** the same tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(runner): runner config, checkouts and per-task worktrees`

---

### Task 6: Harness templates, allowlist and environment scrubbing

**Files:**
- Create: `src/runner/harness.rs`, `tests/runner_harness.rs`

**Interfaces:**
- Produces:
  - `struct TaskBrief { id: TaskId, short: String /*first 8 hex*/, title: String, project_key: String, framed_title: String }`, built from the `POST /v1/runner/runs` response (Task 3)
  - `enum HeadlessHarness { ClaudeCode, Codex, Opencode }` (from `claude-code`, `codex`, `opencode`; unsupported per Task 1 → `Err`)
  - `struct Invocation { program: String, args: Vec<String>, cwd: PathBuf, stdin: Option<String>, env: Vec<(String, String)>, files: Vec<(PathBuf, String)> /*run-local config to write first*/ }`
  - `fn allowlist(allow: Option<&Allow>) -> Vec<String>`: the default entries plus the project's `bash` entries, minus any entry starting with `git push` or `gh` (a project entry asking for them is dropped and logged).
  - `fn invocation(h: HeadlessHarness, wt: &Worktree, run_dir: &Path, prompt: &str, settings: &ProjectWakeSettings, allow: &[String], max_turns: u32) -> Invocation`. Argv per Global Constraints. For Codex, `-c` pins the spike 4 approval key and sets sandbox writable roots to the worktree and its git directory (`git rev-parse --git-common-dir`). For OpenCode, a run-local config (spike 3b) is passed by environment and turns every unlisted tool to `deny`.
  - `fn child_env(parent: &[(String, String)], h: HeadlessHarness, run_id: Uuid, server_url: &str) -> Vec<(String, String)>`: an allow list, never a deny list, so anything new stays out.
  - `fn run_prompt(task: &TaskBrief) -> String`: the core's wake-text frame (task id, project, framed title of at most 80 characters, a pointer to `task_get`), plus "commit your work on this branch; do not push; Axle opens the PR".

- [ ] **Step 1: Write failing tests:** `claude_argv_matches_spec` (with and without a budget); `codex_argv_matches_spec_and_pins_approvals`; `opencode_argv_has_no_auto`; `no_template_bypasses_permissions` (for every harness, argv contains none of `--dangerously-bypass-approvals-and-sandbox`, `--approve-for-me`, `--auto`, `bypassPermissions`); `allowlist_never_contains_push_or_gh`; `child_env_is_scrubbed` (a parent env holding all the scrubbed names gives a child env with none of them, plus `AXL_RUN_ID`); `prompt_holds_no_secret_or_body`.
- [ ] **Step 2: Run** `cargo test --test runner_harness`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(runner): fixed harness templates with allowlists and a scrubbed environment`

---

### Task 7: Run supervision and headless bridge registration

**Files:**
- Create: `src/runner/supervise.rs`, `tests/runner_supervise.rs` (a fake harness script on `PATH` that commits, sleeps, ignores SIGTERM or forks, by argument)
- Modify: `src/bridge/mod.rs` (with `AXL_RUN_ID` set: the harness label becomes `<harness>-headless` and the register body carries `run_id`), `src/runner/mod.rs`

**Interfaces:**
- Produces:
  - `struct RunLimits { max_minutes: u32, kill_grace: Duration /*30 s*/ }`
  - `enum Exit { Finished(i32), Timeout, Stopped, Failed(String) }`
  - `async fn supervise(inv: &Invocation, log: &Path, limits: RunLimits, stop: watch::Receiver<bool>) -> Exit`: spawns in its own process group (`process_group(0)`) and copies stdout to `~/.axl/logs/runs/<run id>.jsonl`. At the limit, or when `stop` flips, it sends SIGTERM to the group and SIGKILL after `kill_grace`.
  - `fn last_message(log: &Path, max: usize /*2048*/) -> String` (the final assistant text, framed by the caller)
  - The runner's accept loop (`src/runner/mod.rs`): on `RunOffered` for an allowed project and harness while under `max_concurrent`, `POST /v1/runner/runs`; on `Started`, prepare the worktree, run, deliver (Task 8), then finish. The heartbeat every 10 s carries the running ids and applies `stop`.

- [ ] **Step 1: Write failing tests:** `normal_exit_is_finished`; `timeout_kills_group_and_delivers` (the fake ignores SIGTERM and forks a child: both are gone 30 s after the limit under paused time, the outcome is `timeout`, and the commits made so far reach delivery); `stop_flag_ends_run_as_stopped`; `log_written_and_last_message_capped`; `headless_bridge_registers_with_suffix_and_run_id` (a bridge with `AXL_RUN_ID` set registers `claude-code-headless` with the `run_id`, and the server claims the task for it).
- [ ] **Step 2: Run** `cargo test --test runner_supervise --test bridge headless`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(runner): supervise headless runs with limits and a stop switch`

---

### Task 8: Delivery as a draft PR

**Files:**
- Create: `src/runner/deliver.rs`, `tests/runner_deliver.rs` (a bare remote and a fake `gh` on `PATH` that records argv and answers `pr list` / `pr create` / `auth status`)

**Interfaces:**
- Produces:
  - `async fn credentials_ok(checkout: &Path) -> Result<(), String>`: `git ls-remote origin` and `gh auth status`, both run in the runner's own environment. On failure the runner registers `can_deliver = false` and declines offers.
  - `enum Delivery { Pr { url: String, reused: bool }, NoCommits, Failed(String) }`
  - `async fn deliver(wt: &Worktree, task: &TaskBrief) -> Delivery`. With no new commits since `wt.base`: `NoCommits`, no push. Otherwise `git push -u origin axl/<task-short>`, then `gh pr list --head axl/<task-short> --state open --json url`. When that is empty, `gh pr create --draft --head axl/<task-short> --title <task title> --body <summary>`.
  - After delivery the runner posts one framed system note in the thread (outcome, exit code, duration, PR link, last 2 KB) and, with a PR, moves the task to `review` as the headless session unless the agent already did. On `NoCommits`, `Timeout` or `Failed` it releases the task to `open` with the note. After a successful push it removes the worktree. On a failed push or PR it keeps the worktree and says so in the note.

- [ ] **Step 1: Write failing tests:** `delivers_draft_pr_and_moves_to_review`; `rerun_reuses_open_pr` (an existing PR: push only, no `pr create`, the same URL); `no_commits_no_push_no_pr`; `push_failure_keeps_worktree_and_notes`; `missing_credentials_decline_runs`; `gh_and_push_run_in_runner_env_only` (the fake harness sees no `GH_TOKEN`; the fake `gh` is only ever invoked by the runner process).
- [ ] **Step 2: Run** `cargo test --test runner_deliver`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(runner): deliver headless work as a draft pull request`

---

### Task 9: `axl runner` CLI, the service unit and status

**Files:**
- Modify: `src/main.rs` (`Runner { #[command(subcommand)] cmd: Option<RunnerCmd> }` with `RunnerCmd::{Pause, Resume, Gc}`; bare `axl runner` runs the loop), `src/daemon/{mod,launchd,systemd}.rs` and `src/cli/service.rs` (`axl service install --runner` writes `com.reyemtech.axle.runner` / `axle-runner.service`, running `axl runner`, logging to `~/.axl/logs/runner.log`), `src/cli/status.rs` (runner state, caps used today, running runs, `can deliver PRs: yes/no`), `tests/fixtures/daemon/` (new golden units), `docs/cli.md`, `docs/security.md`
- Test: `tests/daemon.rs`, `tests/cli_onboarding.rs`

**Interfaces:**
- Consumes: Tasks 3 and 5–8.
- Produces: `axl runner pause` and `resume` set the runner's local `paused` flag in `~/.axl/runner-state.json` (0600). The next heartbeat reports it, and a paused runner declines offers and stops nothing that is already running. `axl runner gc` removes kept worktrees and run logs older than 14 days and prints what it removed.

- [ ] **Step 1: Write failing tests:** `runner_unit_golden_launchd` and `runner_unit_golden_systemd`; `service_install_runner_beside_server`; `runner_pause_declines_offers`; `status_shows_runner_caps_and_delivery`; `gc_prints_removed`.
- [ ] **Step 2: Run** `cargo test --test daemon --test cli_onboarding runner`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo fmt --check && cargo clippy --all-targets --features pg-tests -- -D warnings && cargo test`, then `bash tests/smoke.sh` if docker is available. Expected: clean and PASS.
- [ ] **Step 5: Commit** `feat(cli): axl runner with a user service unit and status`
