# Task delivery status: stages and linking (core) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A project can turn on delivery stages (In PR, Merged, Released, Deployed); a task with a linked PR goes from `task_complete` to a new `delivering` status and the human moves it through the stage columns to Done. No GitHub or generic source yet.

**Architecture:** Migration 0006 adds the `delivering` status, `tasks.delivery_stage` and every delivery table of the spec (later PRs add no schema). Pure rules (stage order, the `task_complete` table, the parent minimum, the board column, PR URL and short id parsing) live in a new `src/delivery/` module and are called from both stores, so the guarded writes decide the same way on SQLite and Postgres. A new `DeliveryService` (`src/service/delivery.rs`) owns linking, settings, Advance stage and Mark done; `TaskService::complete` calls the new guarded `Store::complete_task`. MCP, board, task page and projects page read through it. PR 2 (`-github`) and PR 4 (`-signals`) build on this; PR 3 (`-releases`) comes last.

**Tech Stack:** Rust 2024, axum 0.8, sqlx 0.9 (SQLite + Postgres), rmcp, askama 0.16 + htmx (vendored), tokio.

**Spec:** `docs/superpowers/specs/2026-11-28-delivery-status-design.md` (binding). Also read `AGENTS.md`, `DESIGN.md`, `PRODUCT.md`, and the done-ack spec (`2026-11-27-done-ack-design.md`) and wake-up spec (`2026-11-28-wake-up-design.md`) this builds on.

## Global Constraints

- **Base:** plan against `main` with `feat/done-ack` (PR #11, migration `0004_done_ack.sql`) and the wake-up core (migration `0005_wake_up.sql`, status `requested`) merged. Do not start before done-ack has merged (`done_at` semantics depend on it). If wake-up has not merged when this starts: name the files `0005_delivery.sql`, leave `requested` and the request columns out of the SQLite rebuild, and tell the wake-up builder to renumber to `0006` and keep `delivering` / `delivery_stage` in its own rebuild. Never edit an existing migration.
- Migration files: `migrations/sqlite/0006_delivery.sql`, `migrations/postgres/0006_delivery.sql`. The SQLite `tasks` rebuild (as 0002 does: `-- no-transaction`, `PRAGMA foreign_keys = OFF`, `tasks_new`, copy, rename) keeps every column and status of 0004 (`done_at`, `acked_at`) and 0005 (`requested`, `sanctioned`, `request_depth`, `origin_task_id`, `request_chain`, `request_dedupe_key`), and recreates `tasks_project_status`, `tasks_parent`, `tasks_owner_session`, `tasks_done` (partial, `WHERE status = 'done'`), `tasks_request_dedupe`, plus the new `tasks_delivery (project_id, status, delivery_stage)`.
- Stage order and exact strings: `in_pr` < `merged` < `released` < `deployed`; board labels **In PR**, **Merged**, **Released**, **Deployed**. Status string `delivering`.
- Short id: `axl-` + the **last** 8 hex digits of the task UUID, lowercase (`axl-4deae8e3`).
- Exact error strings: `pr must be a pull request in <host>/<owner>/<repo>` (e.g. `pr must be a pull request in github.com/acme/web-app`), `PR #<n> is already linked to task <id>`, `this task was already merged via #<n>; open a new task for follow-up work`.
- `task_complete` `next_step` exactly: `Delivery is tracked by Axle; this task moves to done when it reaches <Last stage label>. Nothing more to do.`
- Instructions text (verbatim, in `src/install/instructions.md` and appended to `INSTRUCTIONS` in `src/mcp/mod.rs`): "When you open a pull request for an Axle task, report it at once with `task_update { id, pr: "<url>" }`. Put the task's short id (`axl-xxxxxxxx`, given by `task_claim`) in the branch name. After `task_complete`, Axle follows the PR to merge, release and deploy; you are done with the task."
- `task_complete` never calls GitHub. Agents cannot move stages through MCP. Tool count unchanged (18 with wake-up, 17 without).
- `done_at` is set only when a task reaches `done` (last enabled stage, Mark done, stages turned off), never at completion into `delivering`. Done-ack's `task_list` default never hides `delivering` tasks.
- Every new store method exists in both backends with identical semantics and a `store_matrix!` test. Guarded changes are one statement or one transaction whose `WHERE` is the rule. Time comes from `Clock`, never `now()` in SQL. History is append-only; re-link and unlink record the previous value in the event payload.
- UI follows `DESIGN.md` / `PRODUCT.md`: labelled text on every chip, no glyph or colour alone, light and dark themes, CSRF on every POST, strict CSP, no new external assets.
- Docs in the same change: `concepts.md`, `mcp-tools.md`, `web-ui.md`, `http-api.md` (event kinds), new `docs/delivery.md` (stages and linking section), `DESIGN.md`, `src/install/instructions.md`, MCP `INSTRUCTIONS`.
- Conventional commits (`feat(…)`, `docs(…)`). No AI or tool attribution in commits or PRs.

## Review Focus

1. **A project with no stages, or stages turned on later.** `task_complete` gives `done` exactly as today, with `done_at` set; a task completed before stages were enabled stays done (Task 4 `complete_without_stages_is_unchanged`, Task 3 `enabling_stages_reopens_nothing`).
2. **Two people (or a human and a later signal) advancing the same task at once.** One forward move applies; the other is a no-op, never a double stage or a backward move (Task 5 `advance_race_has_one_winner`).
3. **A reported PR URL that is almost right**: other repo with the same owner, `http://`, userinfo, a port, `/pull/0`, `/pull/2147483648`, trailing `/files`, mixed case, a monorepo key `github.com/acme/mono/web` (Task 2 `pr_url_cases`).
4. **Parent with a mix of done, delivering and cancelled children.** It is `delivering` at the minimum child stage and `done` only when every non-cancelled child is done; a cancelled child never holds it back (Task 4 `parent_follows_minimum_child`).
5. **Stages turned off while tasks are delivering.** Tasks at or past the new last stage go to `done` with `done_at`; turning everything off empties every stage column with the note "delivery stages turned off" (Task 3 `stages_off_recomputes_delivering`).

---

## File Structure

| Path | Responsibility |
|---|---|
| `migrations/{sqlite,postgres}/0006_delivery.sql` | Whole delivery schema (spec "Data model"). |
| `src/domain.rs` | `TaskStatus::Delivering`, `DeliveryStage`, PR/CI/source/signal enums, `ProjectDelivery`, `TaskPr`, `TaskStageRow`, `DeliverySignal`, `short_id`, new event kinds, `Task.delivery_stage`. |
| `src/delivery/mod.rs`, `src/delivery/stages.rs` | Pure stage rules: order, last enabled, completion target, parent stage, board column, next stage. |
| `src/delivery/links.rs` | `RepoRef`, repo mapping from project keys, PR URL validation, short id scan. |
| `src/store/{mod,sqlite,postgres}.rs` | Settings, link/unlink, guarded completion, stage moves, signals, parent recompute. |
| `src/service/delivery.rs` | `DeliveryService`: settings, link/unlink, Advance stage, Mark done, the delivery view. |
| `src/service/tasks.rs` | `complete` through `Store::complete_task`; force release and cancel accept `delivering`. |
| `src/mcp/{mod,tools}.rs` | Task object `short_id` / `delivery`, `task_update.pr`, `task_complete.next_step`, `task_claim` short id, `task_list` `delivering`, `INSTRUCTIONS`. |
| `src/ui/{views,task_page,admin,actions,mod}.rs`, templates, `static/app.css` | Stage columns, card chips, Delivery panel, project Delivery panel. |

---

### Task 1: Schema 0006 and domain types

**Files:**
- Create: `migrations/sqlite/0006_delivery.sql`, `migrations/postgres/0006_delivery.sql`, `tests/store_delivery.rs`
- Modify: `src/domain.rs`, `src/store/sqlite.rs` and `src/store/postgres.rs` (`task_from`, `insert_task`), `src/service/tasks.rs` (the `Task` literal), exhaustive `TaskStatus` / `TaskEventKind` matches in `src/ui/views.rs`, `src/mcp/tools.rs`, `src/ui/task_page.rs`

**Interfaces:**
- Produces (`text_enum!`, these exact strings):
  - `TaskStatus::Delivering => "delivering"`
  - `enum DeliveryStage { InPr, Merged, Released, Deployed }` => `in_pr | merged | released | deployed`; `const ALL: [DeliveryStage; 4]`; `fn rank(self) -> u8` (1..=4); `fn label(self) -> &'static str` ("In PR", "Merged", "Released", "Deployed")
  - `enum PrLinkedBy { Agent, Human, BranchMatch, TitleMatch, CompleteLinks }`, `enum PrState { Open, Closed, Merged }`, `enum CiState { None, Pending, Passing, Failing }`
  - `enum ReleaseSource { GithubReleases, GithubTags, Generic, None }`, `enum DeploySource { GithubDeployments, Generic, None }`
  - `enum SignalKind { Pr, Ci, Merged, Released, Deployed }`, `enum SignalSource { Github, Cli, Webhook, Human }`, `enum SignalOutcome { Applied, Unmatched, Duplicate, Refused }`
  - `TaskEventKind` gains `PrLinked, PrRelinked, PrUnlinked, CiChanged, StageReached, Delivered` => `pr_linked | pr_relinked | pr_unlinked | ci_changed | stage_reached | delivered`
  - `Task.delivery_stage: Option<DeliveryStage>`
  - `struct ProjectDelivery { project_id: ProjectId, stages: Vec<DeliveryStage>, repo: Option<String>, github_enabled: bool, release_source: ReleaseSource, tag_pattern: Option<String>, deploy_source: DeploySource, required_environment: String, updated_at: OffsetDateTime }` with `fn defaults(project: &Project, now) -> Self` (no stages; `repo` from `links::repo_of_key`; GitHub keys: `github_enabled = true`, `GithubReleases`, `GithubDeployments`; others: `false`, `Generic`, `Generic`; `required_environment = "production"`) and `fn last_stage(&self) -> Option<DeliveryStage>`
  - `struct TaskPr { task_id, project_id, repo: String, number: Option<i64>, url: String, linked_by: PrLinkedBy, head_ref: Option<String>, base_ref: Option<String>, head_sha: Option<String>, title: Option<String>, state: PrState, merged_at: Option<OffsetDateTime>, merge_commit_sha: Option<String>, ci_state: CiState, ci_failing: Vec<String>, checked_at: Option<OffsetDateTime> }`
  - `struct TaskStageRow { task_id: TaskId, stage: DeliveryStage, reached_at: OffsetDateTime, implied: bool, signal_id: Option<Uuid>, detail: Option<serde_json::Value> }` (`detail` holds `version`, `environment`, `url` when known)
  - `struct DeliverySignal { id: Uuid, project_id: ProjectId, kind: SignalKind, source: SignalSource, version: Option<String>, sha: Option<String>, environment: Option<String>, url: Option<String>, delivery_id: Option<String>, outcome: SignalOutcome, matched: u32, received_at: OffsetDateTime }`
  - `pub fn short_id(id: TaskId) -> String`

- [ ] **Step 1: Write the migrations.** Columns per spec "Data model". Rulings the spec leaves open: `task_prs.number` is nullable (a non-GitHub PR is stored as a link only; unique `(repo, number)` ignores nulls on both backends) and `task_prs.url` is unique too; `delivery_versions` is unique on `(project_id, version)` with `task_ids` JSON text (default `[]`); `delivery_signals` has a partial unique index on `(project_id, delivery_id) WHERE delivery_id IS NOT NULL` and an index on `(project_id, received_at)`; `task_stages` PK `(task_id, stage)`. CHECKs on every enum column with the strings above. Postgres: `DROP/ADD CONSTRAINT tasks_status_check` as 0002 does, `jsonb`, `boolean`, `timestamptz`, `uuid`.
- [ ] **Step 2: Write failing tests** in `tests/store_delivery.rs` (`store_matrix!`): `migration_keeps_earlier_columns` (as the 0002 rebuild test in `store_plan.rs` does: migrate a bare database to 0005, insert by raw SQL a done task with `done_at` / `acked_at` and a `requested` task with `request_depth = 1`, run 0006, read both back unchanged; on SQLite `pragma_index_list('tasks')` lists all six indexes), `delivering_round_trips` (a task inserted with `status = Delivering`, `delivery_stage = Some(Merged)` reads back equal), and a unit test `short_id_is_uuid_tail` (`short_id("0193b2c4-…-9f1a4deae8e3")` == `"axl-4deae8e3"`, uppercase input lowercased).
- [ ] **Step 3: Run** `cargo test --test store_delivery`. Expected: FAIL (no migration, no variant).
- [ ] **Step 4: Implement** the types in `src/domain.rs` and the `delivery_stage` column in both stores' task decode and insert.
- [ ] **Step 5: Run** `cargo test` and `AXL_TEST_PG_URL=… cargo test --features pg-tests --test store_delivery --test store_tasks --test store_done_ack`. Expected: PASS.
- [ ] **Step 6: Commit** `feat(store): delivery schema 0006 and domain types`.

### Task 2: Pure delivery rules and link parsing

**Files:**
- Create: `src/delivery/mod.rs`, `src/delivery/stages.rs`, `src/delivery/links.rs`
- Modify: `src/lib.rs` (`pub mod delivery;`)
- Test: unit tests in both new files

**Interfaces:**
- Produces (`src/delivery/stages.rs`):
  - `pub fn last_enabled(enabled: &[DeliveryStage]) -> Option<DeliveryStage>`
  - `pub enum CompletionTarget { Done, Delivering(DeliveryStage) }`
  - `pub struct ChildState { pub status: TaskStatus, pub stage: Option<DeliveryStage> }`
  - `pub fn completion_target(enabled: &[DeliveryStage], own: Option<DeliveryStage>, has_pr: bool, children: &[ChildState]) -> CompletionTarget` — the spec's `task_complete` table plus the parent rule: no stages enabled → `Done`; otherwise collect own stage (`max(own, InPr)` when `has_pr`, nothing without a PR) and the stage of every `Delivering` child; empty → `Done`; else `m` = the minimum, and the result is `Done` when `m.rank() >= last_enabled.rank()`, else `Delivering(m)`. (A child stage is never `None` while delivering.)
  - `pub fn parent_stage(own: Option<DeliveryStage>, children: &[ChildState]) -> Option<DeliveryStage>` (minimum over own and delivering children; `None` when nothing delivers)
  - `pub fn column(enabled: &[DeliveryStage], furthest: Option<DeliveryStage>) -> Option<(DeliveryStage, bool)>` — the greatest enabled stage whose rank ≤ furthest; when furthest precedes every enabled stage, the first enabled stage and `true` ("waiting for"); `None` when no stages.
  - `pub fn next_enabled_after(enabled: &[DeliveryStage], furthest: Option<DeliveryStage>) -> Option<DeliveryStage>` (Advance stage target)
  - `pub fn implied_between(from: Option<DeliveryStage>, to: DeliveryStage) -> Vec<DeliveryStage>` (strictly between, in order)
  - `pub const STAGE_RANK_SQL: &str` — `CASE delivery_stage WHEN 'in_pr' THEN 1 WHEN 'merged' THEN 2 WHEN 'released' THEN 3 WHEN 'deployed' THEN 4 ELSE 0 END`, shared by both stores' guards
- Produces (`src/delivery/links.rs`):
  - `pub struct RepoRef { pub owner: String, pub name: String }` with `Display` = `owner/name` and `fn parse(s: &str) -> Option<RepoRef>`
  - `pub fn repo_of_key(key: &str) -> Option<RepoRef>` (first two path segments after the host, lowercased) and `pub fn is_github_key(key: &str) -> bool` (host exactly `github.com`)
  - `pub enum PrUrl { Github { repo: RepoRef, number: u32, url: String }, Link { url: String } }` (`url` canonical: `https://github.com/<owner>/<repo>/pull/<n>`)
  - `pub fn parse_pr_url(raw: &str, project_key: &str) -> Result<PrUrl, String>` (the `Err` is the exact `pr must be a pull request in …` message)
  - `pub fn find_short_ids(text: &str) -> Vec<String>` — case-insensitive `(^|[^a-z0-9])axl-([0-9a-f]{8})([^0-9a-f]|$)` by hand (no regex crate), returns lowercase `axl-xxxxxxxx`, deduplicated

- [ ] **Step 1: Write failing tests.** `stages.rs`: `completion_table` (one assertion per spec table row: `[]` + PR → `Done`; `[Merged]` + no PR → `Done`; `[InPr, Merged]` + PR at `None` → `Delivering(InPr)`; `[InPr, Merged]` + own `Merged` → `Done`; `[Released]` + own `InPr` → `Delivering(InPr)`), `parent_minimum` (children `Delivering(Released)`, `Delivering(Merged)`, `Done`, `Cancelled` → `Delivering(Merged)`; all `Done` + `Cancelled` → `Done`), `column_cases` (`[Released, Deployed]` + `Merged` → `(Released, true)`; `[InPr, Deployed]` + `Released` → `(InPr, false)`), `next_and_implied` (`[InPr, Deployed]` from `InPr` → `Deployed`, implied `[Merged, Released]`). `links.rs`: `repo_mapping` (`github.com/acme/mono/web` → `acme/mono`; `gitlab.example.com/a/b` maps but `is_github_key` false), `pr_url_cases` (accept `https://github.com/Acme/Web-App/pull/12/files` for key `github.com/acme/web-app` → canonical `/pull/12`; reject `http://`, `https://u@github.com/…`, `https://github.com:443/…`, other repo, `/pull/0`, `/pull/2147483648`, `/issues/12`; non-GitHub key accepts `https://<key host>/…` as `Link`, rejects another host), `short_id_scan` (`feat/axl-4deae8e3-rate-limit`, `Rate limit (AXL-4DEAE8E3)` match; `xaxl-4deae8e3`, `axl-4deae8e3f`, `axl-4deae8e` do not).
- [ ] **Step 2: Run** `cargo test --lib delivery`. Expected: FAIL.
- [ ] **Step 3: Implement** the functions above.
- [ ] **Step 4: Run** `cargo test --lib delivery`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): stage rules, repo mapping and PR URL validation`.

### Task 3: Store — project delivery settings and recompute

**Files:**
- Modify: `src/store/mod.rs`, `src/store/sqlite.rs`, `src/store/postgres.rs`
- Test: `tests/store_delivery.rs`

**Interfaces:**
- Consumes: Task 2 `completion_target`, `last_enabled`.
- Produces:
  - `async fn get_project_delivery(&self, project: ProjectId) -> StoreResult<Option<ProjectDelivery>>`
  - `async fn list_project_delivery(&self) -> StoreResult<Vec<ProjectDelivery>>`
  - `async fn set_project_delivery(&self, d: &ProjectDelivery, token: Option<TokenId>) -> StoreResult<Vec<Task>>` — upserts the row; in the same transaction recomputes every `delivering` task of the project with `completion_target(new stages, delivery_stage, has_pr, children)`; tasks whose target is `Done` move to `done` with `done_at = d.updated_at` and a `delivered` event (note `delivery stages turned off` when `stages` is empty, else `<Label> is now the last delivery stage`); returns the tasks moved.

- [ ] **Step 1: Write failing tests** (`store_matrix!`): `delivery_settings_default_absent` (no row → `None`), `delivery_settings_round_trip`, `stages_off_recomputes_delivering` (tasks delivering at `InPr` and `Merged` with stages `[InPr, Merged, Released]`; set `[InPr, Merged]` → the `Merged` one is `done` with `done_at` set and a `delivered` event; the `InPr` one stays; set `[]` → both done, note `delivery stages turned off`), `enabling_stages_reopens_nothing` (a done task stays done with the same `done_at`).
- [ ] **Step 2: Run** `cargo test --test store_delivery`. Expected: FAIL.
- [ ] **Step 3: Implement** in both stores.
- [ ] **Step 4: Run** both backends. Expected: PASS.
- [ ] **Step 5: Commit** `feat(store): project delivery settings with stage recompute`.

### Task 4: Store — linking, guarded completion and the parent rule

**Files:**
- Modify: `src/store/mod.rs` (`closes_child`, `child_event`), `src/store/sqlite.rs`, `src/store/postgres.rs` (`record_event`, new methods)
- Test: `tests/store_delivery.rs`

**Interfaces:**
- Consumes: Task 2 `completion_target`, `parent_stage`, `last_enabled`.
- Produces:
  - `struct NewTaskPr { repo: String, number: Option<i64>, url: String, linked_by: PrLinkedBy }`
  - `struct LinkPrRequest { task: TaskId, pr: NewTaskPr, actor: Actor, token: Option<TokenId>, now: OffsetDateTime, replace: bool }` (`replace = false` for the fallback match: links only when the task has no PR)
  - `enum LinkOutcome { Linked(Task), Relinked { task: Task, previous: TaskPr }, Unchanged(Task), TakenBy(TaskId), AlreadyMerged { number: Option<i64> }, HasLink, Closed(TaskStatus), NotFound }`
  - `async fn link_pr(&self, req: &LinkPrRequest) -> StoreResult<LinkOutcome>` — one transaction; refused for `done` / `cancelled`; `TakenBy` when `(repo, number)` or `url` belongs to another task; `AlreadyMerged` when the task's `delivery_stage` rank ≥ `merged`; on link sets `delivery_stage = max(current, in_pr)`, inserts `task_stages(in_pr)` if absent, appends `pr_linked` (or `pr_relinked` with payload `previous_url`), bumps `version`.
  - `async fn unlink_pr(&self, task: TaskId, actor: Actor, token: Option<TokenId>, now: OffsetDateTime) -> StoreResult<Option<TaskPr>>` — guard: not `done`/`cancelled`/`delivering`, stage rank < `merged`; deletes the row, sets `delivery_stage = NULL`, appends `pr_unlinked` with payload `previous_url`.
  - `async fn task_pr(&self, task: TaskId) -> StoreResult<Option<TaskPr>>`, `async fn task_prs(&self, tasks: &[TaskId]) -> StoreResult<Vec<TaskPr>>`, `async fn task_stages(&self, task: TaskId) -> StoreResult<Vec<TaskStageRow>>`
  - `struct CompleteRequest { transition: TransitionRequest, link: Option<NewTaskPr> }` and `struct Completed { transitioned: Transitioned, target: CompletionTarget }`
  - `async fn complete_task(&self, req: &CompleteRequest) -> StoreResult<Option<Completed>>` — the existing guarded transition (`claimed | review` by owner), and in the same transaction: `link` (`linked_by = complete_links`) only if the task has no PR and the PR is not taken (silently skipped otherwise); reads the project's stages, the PR and the children; sets `done` (with `done_at`) or `delivering` (no `done_at`, `delivery_stage` = target stage). The `done` event is appended either way (it carries the summary); for `delivering` its payload gains `"delivering": "<stage>"`.
  - Parent rule in `record_event` (both stores, one shared helper `recompute_parent`): `closes_child` also matches `Delivered`; children counted open are any status but `done`, `cancelled` (so `delivering` keeps `children_complete` from firing, as today). After any child change, when the parent is `delivering` its stage moves forward to `parent_stage(...)` (guard `STAGE_RANK_SQL < :new`), and it moves to `done` (`done_at`, `delivered` event) when every non-cancelled child is `done` and its own stage (if it has a PR) is at or past the last enabled stage.

- [ ] **Step 1: Write failing tests** (`store_matrix!`): `link_and_relink` (link #12 → stage `InPr`, `pr_linked`; link #13 → `pr_relinked` with `previous_url`; one `task_stages` row), `link_uniqueness` (#12 on task A, then on B → `TakenBy(A)`), `relink_refused_after_merge` (stage `Merged` → `AlreadyMerged { number: Some(12) }`), `fallback_never_replaces` (`replace: false` on a linked task → `HasLink`), `unlink_rules` (refused while `delivering`; allowed in `review`, clears `delivery_stage`), `complete_without_stages_is_unchanged` (`done`, `done_at = now`, no PR needed), `complete_table` (stages `[InPr, Merged]`: no PR → `done`; PR → `delivering` at `InPr`, `done_at` null; PR already `Merged` → `done`), `complete_links_first_pr` (no PR, `link` given → linked with `complete_links`; a taken URL is skipped and the result is `done`), `parent_follows_minimum_child` (parent completed with children delivering at `Released` and `Merged` plus one cancelled → parent `delivering` at `Merged`; children reach `done` one by one → parent `done` only after the last, with `done_at`), `delivering_not_hidden_by_task_list` (done-ack default filter still returns `delivering` rows).
- [ ] **Step 2: Run** `cargo test --test store_delivery`. Expected: FAIL.
- [ ] **Step 3: Implement** in both stores.
- [ ] **Step 4: Run** both backends plus `--test store_tasks --test store_plan`. Expected: PASS (existing parent/children tests unchanged).
- [ ] **Step 5: Commit** `feat(store): PR links, delivering completion and the parent minimum`.

### Task 5: Store — forward-only stage moves, signals and Mark done

**Files:**
- Modify: `src/store/mod.rs`, `src/store/sqlite.rs`, `src/store/postgres.rs`
- Test: `tests/store_delivery.rs`

**Interfaces:**
- Consumes: Task 4 `recompute_parent`; Task 2 `STAGE_RANK_SQL`, `last_enabled`.
- Produces:
  - `enum SignalInsert { Inserted, Duplicate }` and `async fn insert_signal(&self, s: &DeliverySignal) -> StoreResult<SignalInsert>` (`Duplicate` on the `(project_id, delivery_id)` index)
  - `struct StageMove { task: TaskId, to: DeliveryStage, implied: Vec<DeliveryStage>, signal_id: Option<Uuid>, detail: Option<serde_json::Value>, actor: Actor, token: Option<TokenId>, now: OffsetDateTime }`
  - `struct StageMoved { task: Task, delivered: bool }`
  - `async fn advance_stage(&self, m: &StageMove) -> StoreResult<Option<StageMoved>>` — one guarded `UPDATE tasks SET delivery_stage = :to … WHERE id = :id AND status NOT IN ('done','cancelled') AND <STAGE_RANK_SQL> < :rank`; on match inserts `task_stages` rows (`implied` for `m.implied`, `INSERT … ON CONFLICT DO NOTHING`), appends `stage_reached` (payload `stage`, `implied`); when the task is `delivering` and `:to` reaches the project's last enabled stage, moves it to `done` (`done_at`, `delivered` event); a task in any other status only records the stage (pre-completion stages). Then `recompute_parent`. `None` when the guard missed.
  - `async fn mark_delivered(&self, task: TaskId, actor: Actor, token: Option<TokenId>, now: OffsetDateTime) -> StoreResult<Option<Task>>` — guarded `delivering → done`, `done_at`, `delivered` event with note `marked done by the human`, then `recompute_parent`.

- [ ] **Step 1: Write failing tests** (`store_matrix!`): `advance_forward_only` (`Merged` then `InPr` → second is `None`), `advance_race_has_one_winner` (two concurrent `advance_stage(to: Merged)` via `tokio::join!` → exactly one `Some`, one `stage_reached` event), `implied_stages_recorded` (`InPr` → `Released` with implied `[Merged]` → two rows, `merged.implied = true`), `pre_completion_stage_recorded` (task in `review` with a PR reaches `Merged`; status stays `review`; later `complete_task` with stages `[InPr, Merged]` → `done`), `last_stage_delivers` (delivering at `InPr`, stages `[InPr, Merged]`, advance to `Merged` → `done`, `done_at = now`, `delivered: true`), `done_at_only_at_final_stage` (completion into `delivering` leaves `done_at` null until the final move), `mark_delivered_guarded` (only from `delivering`), `signal_dedupe` (same `delivery_id` twice → `Duplicate`; `None` delivery ids never collide).
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement** in both stores.
- [ ] **Step 4: Run** both backends. Expected: PASS.
- [ ] **Step 5: Commit** `feat(store): forward-only stage moves, signal log and mark done`.

### Task 6: DeliveryService and task completion

**Files:**
- Create: `src/service/delivery.rs`
- Modify: `src/service/mod.rs`, `src/service/tasks.rs` (`complete`, `force_release`, `cancel`), `src/http/mod.rs` (`AppState.delivery`)
- Test: `tests/store_delivery.rs` (service section, `store_matrix!` with a `TestClock`)

**Interfaces:**
- Consumes: Tasks 2–5.
- Produces:
  - `pub struct DeliveryService { store, clock, bus }` with `new(store, clock)` and `with_events(bus)`
  - `pub enum DeliveryError` (thiserror; `Display` is the exact user message): `BadPrUrl(String)`, `TakenBy { number: Option<i64>, task: TaskId }` (`PR #<n>`, or `this PR` for a link-only URL), `AlreadyMerged { number: Option<i64> }`, `NotAllowed` ("only the task's owner or creator can report its PR"), `Delivering` ("this task is delivering; use Mark done or Force release"), `NoNextStage` ("this task is already at its last delivery stage"), `NotDelivering` ("this task is not delivering"), `Task(TaskError)`, `Store(StoreError)`
  - `pub async fn settings(&self, project: &Project) -> Result<ProjectDelivery, DeliveryError>` (row or `ProjectDelivery::defaults`)
  - `pub async fn set_stages(&self, project: &Project, stages: Vec<DeliveryStage>) -> Result<Vec<Task>, DeliveryError>` (sorted, deduplicated; publishes `TaskChanged { kind: Delivered }` per moved task)
  - `pub async fn link_pr(&self, by: Caller<'_>, task: TaskId, raw_url: &str) -> Result<Task, DeliveryError>` (agents: owner or creator; human: any; `linked_by` `Agent` / `Human`)
  - `pub async fn unlink_pr(&self, task: TaskId) -> Result<Task, DeliveryError>` (human)
  - `pub async fn advance(&self, task: TaskId) -> Result<Task, DeliveryError>` (human: `next_enabled_after`, implied from `implied_between`, a `DeliverySignal { kind: <stage's kind>, source: Human, outcome: Applied, matched: 1 }`)
  - `pub async fn mark_done(&self, task: TaskId) -> Result<Task, DeliveryError>`
  - `pub struct DeliveryView { stage: Option<DeliveryStage>, stages_enabled: Vec<DeliveryStage>, column: Option<(DeliveryStage, bool)>, pr: Option<TaskPr>, timeline: Vec<TaskStageRow> }` and `pub async fn view(&self, task: &Task) -> Result<Option<DeliveryView>, DeliveryError>` (`None` when the project has no stages and the task has no PR)
  - `TaskService::complete(&self, caller, id, done: Completion) -> Result<Task, TaskError>` unchanged signature; it now builds a `CompleteRequest` whose `link` is the first `links` entry that `parse_pr_url` accepts as `PrUrl::Github` for the task's project, and calls `complete_task`
  - `TaskService::force_release` and `cancel` accept `Delivering` in their `from` lists; force release keeps the PR link and `delivery_stage`
- [ ] **Step 1: Write failing tests:** `agent_link_permissions` (owner and creator may link; another agent gets `NotAllowed`; `done` task gets the `Closed` error), `link_error_messages` (exact strings from Global Constraints), `advance_moves_one_column` (stages `[InPr, Deployed]` at `InPr` → `Deployed` reached, `done`; signal row `source = human`), `advance_at_waiting_goes_to_first` (stages `[Released]`, PR open → advance reaches `Released` with `Merged` implied → `done`), `force_release_keeps_pr` (delivering → `open`, `task_pr` still `Some`, stage kept), `complete_reads_links` (`links: ["https://github.com/acme/web/pull/7"]` on a stages project → `delivering`, `linked_by = complete_links`).
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(service): delivery service and delivering completion`.

### Task 7: MCP surface and agent instructions

**Files:**
- Modify: `src/mcp/tools.rs` (`TaskUpdateParams.pr`, `StatusParam::Delivering`, `task_claim_impl`, `task_complete_impl`), `src/mcp/mod.rs` (task summary, `INSTRUCTIONS`), `src/install/instructions.md`, the harness fixtures under `tests/fixtures/harness/**` that embed the instructions
- Test: `tests/mcp_tools.rs`, `tests/install_claude_code.rs` (fixtures)

**Interfaces:**
- Consumes: Task 6 `DeliveryService::{link_pr, view}`, `short_id`.
- Produces (JSON shapes, `docs/mcp-tools.md`):
  - every task object gains `"short_id": "axl-…"` and `"delivery": null | { "stage": "in_pr" | … | null, "stages_enabled": [...], "pr": { "url", "number", "state", "ci" } | null }`
  - `task_update { id, pr }`: `pr` alone is a valid update; errors are `DeliveryError` messages
  - `task_complete` result: `status` may be `"delivering"`; then `"next_step"` (exact string, last enabled stage label)
  - `task_claim` result gains `"short_id"` and `"branch_hint"` (same value)
  - `task_list { status: "delivering" }`
- [ ] **Step 1: Write failing tests:** `task_update_pr_validation` (good URL → task `delivery.pr.number == 12`; wrong repo → `pr must be a pull request in github.com/acme/web-app`), `task_complete_next_step` (stages `[InPr, Merged]`, PR linked → `status == "delivering"`, `next_step == "Delivery is tracked by Axle; this task moves to done when it reaches Merged. Nothing more to do."`), `task_object_shape` (`short_id` matches the id tail; `delivery` null without stages), `task_claim_short_id` (`branch_hint == short_id`), `task_list_delivering`, `tool_count_unchanged`, `instructions_mention_pr_report` (`INSTRUCTIONS` and the installed skill contain `task_update { id, pr:`).
- [ ] **Step 2: Run** `cargo test --test mcp_tools`. Expected: FAIL.
- [ ] **Step 3: Implement;** regenerate the expected fixture files by hand from the new `instructions.md`.
- [ ] **Step 4: Run** `cargo test`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(mcp): task PR reporting, short ids and delivering status`.

### Task 8: Board — stage columns and card chips

**Files:**
- Modify: `src/ui/views.rs` (`columns`, `ColumnView`, `CardView`, `state_mark`), `templates/_columns.html`, `templates/_card.html`, `templates/board.html` (Key), `static/app.css`, `DESIGN.md` (Shape Carries State Rule, chips)
- Test: `tests/ui.rs`

**Interfaces:**
- Consumes: Task 2 `column`; Task 4 `task_prs`; Task 3 `list_project_delivery`.
- Produces: `ColumnView` gains `stage: Option<&'static str>` and `empty_text: &'static str`; `CardView` gains `short_id: String`, `pr: Option<PrChip { number: i64, url: String }>`, `waiting_for: Option<&'static str>`.
- Rulings: stage columns sit after Review and before Done, in stage order; filtered to one project → its enabled stages; all projects → the union, each task only in its own project's column (`column(...)`). They hold every `delivering` task whose parent is not also `delivering` (a parent that follows its children shows once). Column head `IN PR · 4`. Empty text: In PR "Nothing waiting for merge", Merged "Nothing waiting for release", Released and Deployed "Nothing waiting for deploy". The delivering aspect is a solid glyph with an outer ring, always beside the stage word; Key entry "In delivery: block cleared, train still moving". Card shows `axl-…` in mono and `PR #123` (mono link, `rel="noopener noreferrer"`); a waiting card shows "waiting for Released".
- [ ] **Step 1: Write failing tests:** `stage_columns_per_project` (project A `[InPr, Merged]`, B `[Released]`: filtered to A → columns In PR, Merged only; unfiltered → In PR, Merged, Released), `task_sits_in_own_column` (B's task at `InPr` sits in Released with "waiting for Released", never in In PR), `empty_stage_column_sentence`, `card_pr_chip_and_short_id` (`scraper`: `a[href="https://github.com/acme/web/pull/12"]` text `PR #12`; mono `axl-` text), `no_stages_board_unchanged` (column list equals today's).
- [ ] **Step 2: Run** `cargo test --test ui`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --test ui`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): delivery stage columns and PR chips`.

### Task 9: Task page Delivery panel and actions

**Files:**
- Modify: `src/ui/task_page.rs`, `src/ui/actions.rs`, `src/ui/mod.rs` (routes), `templates/task.html`
- Test: `tests/ui.rs`

**Interfaces:**
- Consumes: Task 6 `DeliveryService::{view, link_pr, unlink_pr, advance, mark_done}`.
- Produces routes (CSRF POST, redirect back without JS): `POST /ui/tasks/{id}/pr` (form `url`), `POST /ui/tasks/{id}/pr/unlink`, `POST /ui/tasks/{id}/advance`, `POST /ui/tasks/{id}/mark-done`. Force release and Cancel reuse the existing routes.
- Panel above the history: short id (mono), then one row per enabled stage: label, state word (`reached`, `implied`, `waiting`), time, link (PR for In PR). Implied rows read "implied by <later stage label>". Buttons, labelled: **Link PR** (URL field), **Unlink PR**, **Advance stage**, **Mark done** (the last two only while `delivering`). Errors render the `DeliveryError` message in the panel.
- [ ] **Step 1: Write failing tests:** `delivery_panel_timeline` (rows and state words for a task at `Released` with `Merged` implied), `link_pr_form` (POST with CSRF links; without CSRF → 403; bad URL shows the exact message), `advance_and_mark_done_buttons` (Advance moves one column; Mark done → `done`; both absent when not delivering), `unlink_hidden_after_merge`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --test ui`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): task delivery panel with link, advance and mark done`.

### Task 10: Projects page — Delivery stages panel

**Files:**
- Modify: `src/ui/admin.rs` (`ProjectRow`), `src/ui/actions.rs`, `src/ui/mod.rs`, `templates/projects.html`
- Test: `tests/ui.rs`

**Interfaces:**
- Consumes: Task 6 `DeliveryService::{settings, set_stages}`.
- Produces: `POST /ui/projects/{id}/delivery` (form: repeated `stage=in_pr` etc.; CSRF). The panel is expanded in place, like Wake-ups: four checkboxes **In PR**, **Merged**, **Released**, **Deployed**, help text exactly "Tasks with a PR move through these after the agent completes them. Done means the last one ticked.", a **Save stages** button. (Sources, environment, webhook and health come in later PRs.)
- [ ] **Step 1: Write failing tests:** `delivery_settings_save` (ticking In PR and Merged stores `[InPr, Merged]`), `delivery_settings_csrf` (403 without token), `unticking_all_moves_delivering_to_done` (through the form; the task page shows note `delivery stages turned off`), `unknown_stage_rejected` (400).
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --test ui`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): per-project delivery stages`.

### Task 11: Docs

**Files:**
- Create: `docs/delivery.md` (what stages are, how to turn them on, linking by report / short id / Link PR, Advance stage, Mark done; sources "coming" sections are left out until their PRs)
- Modify: `docs/concepts.md` (status diagram with `delivering`, stages, parent rule), `docs/mcp-tools.md` (task object, `task_update.pr`, `task_complete`, `task_claim`, `task_list`), `docs/web-ui.md` (columns, chips, Delivery panel, project settings), `docs/http-api.md` (new history kinds on `TaskChanged`), `docs/README.md` (link)

- [ ] **Step 1: Write the docs** from the spec sections they mirror, with the exact strings from Global Constraints.
- [ ] **Step 2: Run** `cargo fmt --check && cargo clippy --all-targets --features pg-tests -- -D warnings && cargo test`. Expected: clean.
- [ ] **Step 3: Commit** `docs: delivery stages and PR linking`.
