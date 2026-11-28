# Task delivery status: GitHub PRs and CI (github) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** With `AXL_GITHUB_TOKEN` set, `axl serve` polls GitHub for tasks in flight: it tracks PR state and CI on the card, moves tasks to Merged when the PR merges into the default branch, links PRs that mention a task's short id, and shows source health on a banner, without ever making `task_complete` or the board wait on GitHub.

**Architecture:** Credentials are reached only through the `GithubCredentials` seam (`StaticToken` in v1). A `GithubClient` sends conditional GETs (stored ETags) to `AXL_GITHUB_API_URL` and classifies every failure into our own error words. A single `Poller` task plans each cycle (in-flight tasks, oldest waiting first, round-robin across repos, under an adaptive cap), turns results into `PrSnapshot` / CI facts and hands them to `DeliveryService`, which applies them with the core store primitives (`advance_stage`, `link_pr`). Health lives in `github_health` and drives a banner over SSE. Releases and deployments are the `-releases` plan.

**Tech Stack:** Rust 2024, axum 0.8, reqwest 0.13 (rustls), `secrecy` 0.10, sqlx 0.9, askama 0.16 + htmx, tokio.

**Spec:** `docs/superpowers/specs/2026-11-28-delivery-status-design.md` (binding), sections "GitHub access", "The poller", "The matching chain" (steps 1–2), "Linking" (fallback match), "Events", "UI", "Security", "Failure handling".

## Global Constraints

- **Base:** `docs/superpowers/plans/2026-11-28-delivery-status-core.md` merged (migration 0006 already holds `task_prs`, `github_cache`, `github_health`, `delivery_signals`). No new migration. Can run in parallel with `-signals`; rebase on whichever lands first (both touch `src/service/delivery.rs`, `src/events.rs`, `templates/projects.html`).
- **Wake-up:** the CI-failing note wakes the owner through wake-up's `WakeService::wake_owner` with a new trigger `WakeTrigger::CiFailing` (`ci_failing`), counted with `PeerPost` in the per-session peer budget (6 per session per hour). If wake-up has not merged, post the note only (Task 7 step 3 without the wake call) and tell the wake-up builder to add the trigger.
- Config, exact names and rules: `AXL_GITHUB_TOKEN` (optional); `AXL_GITHUB_POLL_SECS` default **90**, at least **30**; `AXL_GITHUB_MAX_REQUESTS_PER_CYCLE` 1–100, default **100** (lowers the ceiling only); `AXL_GITHUB_API_URL` default `https://api.github.com`, `https://` unless the host is loopback. Invalid values stop startup naming the variable.
- Cap per cycle: `min(ceiling, last X-RateLimit-Remaining / 10)`; with no reading yet, the ceiling.
- Backoff: `403`/`429` with `x-ratelimit-remaining: 0` → pause until `x-ratelimit-reset`; `retry-after` → that many seconds; secondary rate limit (403 without remaining 0) or `5xx` → 1, 2, 4, 8, 15, 15 … minutes, reset on success; `401` → health `rejected`, stop, retry every **15** minutes; `404` on a repo → that repo's projects `no_access`, skipped **15** minutes.
- Merged counts only when `merged_at` is set **and** `base.ref == base.repo.default_branch`.
- Request headers: `Accept: application/vnd.github+json`, `X-GitHub-Api-Version: 2022-11-28`, `User-Agent: axle`, `Authorization: Bearer <token>`, `If-None-Match: <etag>` when one is stored. The token goes only to `AXL_GITHUB_API_URL`, for repos derived from project keys (`links::repo_of_key`), never from reported URLs.
- The token is never logged, returned, rendered or put in an error: `SecretToken` has no `Display` and a redacted `Debug`; reqwest errors are mapped to our own words without their source.
- Third-party text (PR titles, check names) is stored capped (title 200 chars, at most 5 check names), rendered as text and framed as peer data over MCP (`mcp::defuse` + the existing frame).
- Tests never reach the real API: a stub axum server behind `AXL_GITHUB_API_URL` (Task 2).
- Conventional commits; no AI attribution. Docs in the same change: `configuration.md`, `security.md`, `web-ui.md`, `concepts.md`, `docs/delivery.md` (GitHub token section with the permission table), Helm values.

## Review Focus

1. **The token in a log line after a 401 or a network error.** Captured poller output and every `Display`/`Debug` of the errors contain no part of the token (Task 6 `token_never_logged`).
2. **A PR merged into a release branch, or closed without merge.** The task stays in In PR with chip "merged into release/1.4" or "PR closed unmerged" and a Needs you card; it never reaches Merged (Task 7 `merge_into_other_branch_stalls`).
3. **A busy repo with 60 updated PRs and 30 tasks in flight on a low remaining budget.** The cycle stops at the cap, oldest waiting first, and the rest rolls over; a `304` costs nothing (Task 5 `cycle_respects_cap_and_rolls_over`).
4. **Two PRs mention the same short id.** Nothing links; the task page lists both with Link buttons; an existing reported link is never replaced (Task 8 `two_matches_link_nothing`).
5. **A project without stages in a repo shared with one that has stages.** No request is made on its behalf and its tasks are not fallback candidates (Task 5 `no_requests_without_stages`).

---

## File Structure

| Path | Responsibility |
|---|---|
| `src/delivery/github/mod.rs` | Module root; `GithubConfig`. |
| `src/delivery/github/credentials.rs` | `GithubCredentials`, `SecretToken`, `CredentialError`, `StaticToken`. |
| `src/delivery/github/client.rs` | Conditional GET, rate-limit reading, error classification. |
| `src/delivery/github/api.rs` | Serde types for pulls, check runs, combined status. |
| `src/delivery/github/budget.rs` | Pure cap, backoff and cycle planning. |
| `src/delivery/github/poller.rs` | The poll loop, `nudge`, fallback candidates, panic restart. |
| `src/service/delivery.rs` | `apply_pr`, `apply_ci`, fallback linking, health changes. |
| `src/store/{mod,sqlite,postgres}.rs` | ETag cache, health, PR snapshot, in-flight query. |
| `src/config.rs` | The four variables. |
| `src/events.rs` | `DeliveryHealthChanged`. |
| `src/ui/*`, templates | CI/merged/stalled chips, Needs you cards, banner, Recheck now, candidates, health line. |
| `tests/common/github_stub.rs` | Scripted stub GitHub. |
| `deploy/helm/axle/*`, `tests/helm.sh` | `github.tokenSecret`. |

---

### Task 1: Config and the credential seam

**Files:**
- Create: `src/delivery/github/mod.rs`, `src/delivery/github/credentials.rs`
- Modify: `Cargo.toml` (`secrecy = "0.10"`), `src/config.rs` (`ServerConfig.github`), `src/delivery/mod.rs`
- Test: unit tests in `src/config.rs` and `credentials.rs`

**Interfaces:**
- Consumes: core `links::RepoRef`.
- Produces:
  - `pub struct SecretToken(secrecy::SecretString)` with `fn new(String) -> Self`, `fn expose(&self) -> &str` (crate-private use only in `client.rs`), `Debug` printing `SecretToken(***)`, no `Display`
  - `#[derive(thiserror::Error)] pub enum CredentialError { #[error("GitHub credentials are unavailable")] Unavailable }`
  - `#[async_trait] pub trait GithubCredentials: Send + Sync { async fn token_for(&self, repo: &RepoRef) -> Result<Option<SecretToken>, CredentialError>; }` (exactly the spec's signature)
  - `pub struct StaticToken(Option<SecretToken>)` returning the same token for every repo
  - `pub struct GithubConfig { pub token: Option<SecretToken>, pub poll: Duration, pub max_requests: u32, pub api_url: String }` on `ServerConfig.github`
- [ ] **Step 1: Write failing tests:** `github_config_defaults` (90 s, 100, `https://api.github.com`, no token), `github_poll_floor` (`29` → error naming `AXL_GITHUB_POLL_SECS`), `github_max_requests_range` (`0` and `101` rejected), `github_api_url_rules` (`http://127.0.0.1:9000` ok, `http://example.com` rejected naming `AXL_GITHUB_API_URL`), `secret_token_debug_redacted` (`format!("{:?}", cfg)` has no token text), `static_token_any_repo`.
- [ ] **Step 2: Run** `cargo test --lib config credentials`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(config): GitHub token and poller settings behind a credential seam`.

### Task 2: Stub GitHub server and the client

**Files:**
- Create: `tests/common/github_stub.rs`, `src/delivery/github/client.rs`, `src/delivery/github/api.rs`, `tests/github_client.rs`
- Modify: `tests/common/mod.rs` (`pub mod github_stub;`), `src/store/{mod,sqlite,postgres}.rs` (ETag cache)

**Interfaces:**
- Produces (stub, test-only):
  - `GithubStub::start() -> GithubStub` with `url: String`; `fn on(&self, method: &str, path_and_query: &str, responses: Vec<StubResponse>)` (served in order, the last repeats); `fn requests(&self) -> Vec<RecordedRequest>`
  - `StubResponse::json(status, value)` with builders `.etag(&str)`, `.header(name, value)`; `StubResponse::not_modified()`, `::rate_limited(reset_unix)` (403, remaining 0), `::retry_after(secs)` (429), `::status(code)`
  - `RecordedRequest { method, path_and_query, if_none_match: Option<String>, authorization: Option<String> }`
- Produces (client):
  - store: `async fn get_etag(&self, key: &str) -> StoreResult<Option<String>>`, `async fn put_etag(&self, key: &str, etag: &str, now: OffsetDateTime) -> StoreResult<()>`
  - `pub struct GithubClient` with `new(api_url: String, creds: Arc<dyn GithubCredentials>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) -> Self`
  - `pub enum Fetched<T> { Fresh { body: T, etag: Option<String>, key: String }, NotModified }`
  - `pub async fn get<T: DeserializeOwned>(&self, repo: &RepoRef, path_and_query: &str) -> Result<Fetched<T>, GhError>` (path relative to `/repos/{owner}/{name}`; key = `GET ` + full path and query)
  - `pub async fn commit_etag(&self, key: &str, etag: &str) -> Result<(), GhError>` — called by the poller after the result is applied, so a crash between fetch and apply re-reads
  - `pub fn rate(&self) -> RateInfo` (`remaining: Option<u32>`, `reset: Option<OffsetDateTime>`, `token_expires: Option<OffsetDateTime>` from `github-authentication-token-expiration`)
  - `#[derive(thiserror::Error)] pub enum GhError { NoToken, Unauthorized, NotFound, RateLimited { until: OffsetDateTime }, Secondary, Server(u16), Unreachable, Decode }` with our own `Display` words ("GitHub token rejected", "GitHub rate limited until …", "GitHub unreachable", …)
  - `api.rs`: `PullRequest { number, state, title, html_url, merged_at, merge_commit_sha, head: Branch { ref_, sha }, base: Base { ref_, repo: BaseRepo { default_branch } } }`, `CheckRuns { check_runs: Vec<CheckRun { name, status, conclusion }> }`, `CombinedStatus { state, statuses: Vec<Status { context, state }> }`
- [ ] **Step 1: Write failing tests** (`tests/github_client.rs`, `store_matrix!` for the store part): `etag_round_trip`, `sends_if_none_match` (second GET carries the committed etag; a `304` gives `NotModified`), `uncommitted_etag_not_sent`, `rate_headers_read` (`x-ratelimit-remaining: 420` → `rate().remaining == Some(420)`), `errors_classified` (401 → `Unauthorized`; 403 remaining 0 → `RateLimited { until: reset }`; 429 `retry-after: 30` → `RateLimited { until: now + 30s }`; 403 without remaining 0 → `Secondary`; 502 → `Server(502)`; closed port → `Unreachable`), `token_only_to_stub` (every recorded request has `authorization == "Bearer t0k"`, and only the stub received any), `no_token_no_request` (`StaticToken(None)` → `NoToken`, stub saw nothing).
- [ ] **Step 2: Run** `cargo test --test github_client`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): conditional GitHub client with a scripted stub for tests`.

### Task 3: Budget, backoff and cycle planning (pure)

**Files:**
- Create: `src/delivery/github/budget.rs`
- Test: unit tests in the file, with `TestClock` times

**Interfaces:**
- Produces:
  - `pub fn cycle_cap(remaining: Option<u32>, ceiling: u32) -> u32`
  - `pub struct Backoff { step: u32, until: Option<OffsetDateTime> }` with `fn on_transient(&mut self, now) -> OffsetDateTime` (1, 2, 4, 8, 15 minutes, then 15), `fn on_until(&mut self, until)`, `fn on_success(&mut self)`, `fn ready(&self, now) -> bool`
  - `pub struct WorkItem { pub repo: RepoRef, pub task: Option<TaskId>, pub kind: WorkKind, pub waiting_since: OffsetDateTime }` and `pub enum WorkKind { PullList, Pull(i64), Checks(String /*head sha*/), Status(String) }` (the `-releases` plan adds kinds)
  - `pub fn plan(items: Vec<WorkItem>, cap: u32) -> (Vec<WorkItem>, u32 /*deferred*/)` — per repo, sort by `waiting_since`; then take one per repo in round-robin order (repos by their oldest item) until `cap`
- [ ] **Step 1: Write failing tests:** `cap_tenth_of_remaining` (`Some(420)`, 100 → 42; `Some(5000)`, 100 → 100; `None`, 60 → 60; `Some(3)` → 0), `backoff_sequence` (1, 2, 4, 8, 15, 15 minutes; `on_success` resets), `plan_round_robin_oldest_first` (repo A items at t1, t3, B at t2, cap 2 → A@t1, B@t2; deferred 1).
- [ ] **Step 2: Run** `cargo test --lib budget`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): poller budget, backoff and planning`.

### Task 4: Store — in-flight query, PR snapshot, CI and health

**Files:**
- Modify: `src/store/mod.rs`, `src/store/sqlite.rs`, `src/store/postgres.rs`, `src/domain.rs`
- Test: `tests/store_delivery_github.rs`

**Interfaces:**
- Produces:
  - `struct InFlight { task: Task, pr: TaskPr, delivery: ProjectDelivery, waiting_since: OffsetDateTime }` (`waiting_since` = the newest `task_stages.reached_at`)
  - `async fn delivery_in_flight(&self) -> StoreResult<Vec<InFlight>>` — tasks with a linked GitHub PR (number not null), status not `done`/`cancelled`, in projects with stages, `github_enabled` and a repo, whose stage rank < the last enabled stage's
  - `async fn fallback_candidates(&self, repo: &str) -> StoreResult<Vec<(Task, String /*short id*/)>>` — tasks with no PR, not `done`/`cancelled`, in projects with stages and `github_enabled` mapped to `repo`
  - `struct PrSnapshot { repo: String, number: i64, state: PrState, title: String, head_ref: String, head_sha: String, base_ref: String, default_branch: String, merged_at: Option<OffsetDateTime>, merge_commit_sha: Option<String> }`
  - `async fn update_pr_snapshot(&self, s: &PrSnapshot, now: OffsetDateTime) -> StoreResult<Option<TaskPr /*previous*/>>` (title capped at 200 chars; `None` when no task holds that PR)
  - `async fn set_ci(&self, task: TaskId, head_sha: &str, state: CiState, failing: &[String], now: OffsetDateTime) -> StoreResult<Option<CiState /*previous*/>>` — writes only when the state word or the failing list changes; appends `ci_changed` (payload `state`, `head_sha`, `failing`) only when the word changes
  - `async fn ci_note_sent(&self, task: TaskId, head_sha: &str) -> StoreResult<bool>` (a `ci_changed` event with `state = failing` and that `head_sha` exists)
  - `enum HealthState { Ok, Missing, Rejected, RateLimited, NoAccess, Unreachable }` (`text_enum!`: `ok | missing | rejected | rate_limited | no_access | unreachable`), `struct GithubHealth { scope: String, state: HealthState, since: OffsetDateTime, until: Option<OffsetDateTime>, last_ok_at: Option<OffsetDateTime> }`
  - `async fn set_github_health(&self, scope: &str, state: HealthState, until: Option<OffsetDateTime>, now: OffsetDateTime) -> StoreResult<bool /*changed*/>` (keeps `since` while the state is unchanged; `Ok` sets `last_ok_at`)
  - `async fn github_health(&self) -> StoreResult<Vec<GithubHealth>>`
- [ ] **Step 1: Write failing tests** (`store_matrix!`): `in_flight_selection` (includes a `review` task with a PR; excludes done, cancelled, at-last-stage, no-stages and `github_enabled = false` projects), `fallback_candidates_shared_repo` (two projects mapped to `acme/mono`: only the one with stages contributes), `snapshot_updates_row`, `ci_event_only_on_word_change` (pending → pending with a new list writes no event; pending → failing writes one), `ci_note_sent_per_sha`, `health_since_kept` (rejected at 10:02, again at 10:05 → `since` 10:02, `changed` false).
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement** in both stores.
- [ ] **Step 4: Run** both backends. Expected: PASS.
- [ ] **Step 5: Commit** `feat(store): in-flight delivery query, PR snapshots, CI state and source health`.

### Task 5: The poller

**Files:**
- Create: `src/delivery/github/poller.rs`, `tests/github_poller.rs`
- Modify: `src/http/mod.rs` (`AppState.poller: Option<PollerHandle>`), `src/main.rs` (`serve` spawns it), `src/delivery/github/mod.rs`

**Interfaces:**
- Consumes: Tasks 2–4; Task 6–7 service methods (`apply_pr`, `apply_ci`, `link_fallback`, `set_health`).
- Produces:
  - `pub struct Poller` with `new(state: &AppState, client: GithubClient, log: LogSink) -> Self`, `pub async fn cycle(&self) -> CycleReport`, `pub async fn run(self, shutdown: Shutdown)`
  - `pub type LogSink = Arc<dyn Fn(String) + Send + Sync>` (default writes `axl: github: …` to stderr; tests capture)
  - `pub struct CycleReport { pub requests: u32, pub not_modified: u32, pub deferred: u32 }`
  - `pub struct PollerHandle` with `pub fn nudge(&self, repo: RepoRef)` (polls that repo on the next loop turn, at once) and `pub fn candidates(&self, task: TaskId) -> Vec<CandidatePr>`; `pub struct CandidatePr { pub number: i64, pub url: String, pub title: String }`
  - Per repo per cycle: `GET /pulls?state=all&sort=updated&direction=desc&per_page=50`; `GET /pulls/{n}` only for linked PRs not on that page; `GET /commits/{sha}/check-runs?per_page=100` and `GET /commits/{sha}/status` for each head SHA of a task at or before In PR. ETags committed after apply. Errors feed `Backoff` and health per the Global Constraints.
  - Started in `serve` when a token is set; a cycle with no qualifying project makes no request. A token-less server with a qualifying project sets health `missing` for scope `token` and starts no poller. Each cycle runs in its own `tokio::spawn`; a panic is logged ("GitHub poller cycle panicked; restarting") and the loop continues.
- [ ] **Step 1: Write failing tests** (stub + `TestClock`, cycles called directly): `no_requests_without_stages`, `cycle_respects_cap_and_rolls_over` (remaining 20 → cap 2; 3 items → 2 requests, deferred 1; next cycle takes the third), `pull_list_then_unlisted_pull` (a linked PR absent from the list page gets one `GET /pulls/{n}`), `not_modified_changes_nothing` (all `304` → task row unchanged), `rate_limit_pauses_until_reset` (403 remaining 0 → next cycle before reset makes no request; health `rate_limited` with `until`), `unauthorized_stops_and_retries` (401 → health `rejected`; no request for 14 minutes; one at 15), `not_found_marks_no_access` (one repo 404 → its project `no_access`; another repo still polled), `nudge_polls_now`, `panic_restarts` (a stub body that makes apply panic → next cycle still runs).
- [ ] **Step 2: Run** `cargo test --test github_poller`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): GitHub poller with conditional requests, cap and backoff`.

### Task 6: Health, the banner and token safety

**Files:**
- Modify: `src/service/delivery.rs` (`set_health`), `src/events.rs` (`DeliveryHealthChanged { scope: String, state: String }`, UI only, not routed to `/v1/events` sessions), `src/ui/mod.rs`, `src/ui/views.rs` (`live_frames`), `templates/base.html`, new `templates/_delivery_banner.html`, `src/ui/admin.rs` + `templates/projects.html` (health line)
- Test: `tests/ui.rs`, `tests/github_poller.rs`

**Interfaces:**
- Produces: `GET /ui/delivery/banner` (fragment, refreshed on `DeliveryHealthChanged` through `/ui/live`). Banner copy (warn bar under the top bar, only when a project with stages uses GitHub): "GitHub token rejected since 10:02 — delivery stages are paused.", "GitHub token missing — delivery stages are paused.", "GitHub rate limited until 10:40 — delivery stages are paused.", "GitHub: no access to acme/web-app — its delivery stages are paused.", "GitHub unreachable since 10:02 — delivery stages are paused." It clears on the next good poll. Projects page Delivery panel gains a health line: "GitHub: ok, checked 40s ago" or the failure in the same words, plus "token expires <date>" (warn within 7 days) from `rate().token_expires`.
- [ ] **Step 1: Write failing tests:** `banner_states` (each health state renders its sentence; `ok` renders nothing; no stages → nothing), `banner_clears_on_ok`, `health_event_not_sent_to_sessions` (`/v1/events` stream has no `DeliveryHealthChanged`), `token_never_logged` (a cycle through 401, 5xx and an unreachable port with token `ghp_SECRET123`: captured `LogSink` output, every `GhError` `Display`/`Debug`, the banner and projects page HTML contain neither `ghp_SECRET123` nor `SECRET`), `token_expiry_warning`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): GitHub source health banner and token expiry`.

### Task 7: Applying PR state and CI

**Files:**
- Modify: `src/service/delivery.rs`, `src/domain.rs` / wake-up's `WakeTrigger` (`CiFailing`), wake-up's peer-budget trigger list
- Test: `tests/github_poller.rs`

**Interfaces:**
- Consumes: core `Store::advance_stage`, `insert_signal`; Task 4 store methods.
- Produces:
  - `pub async fn apply_pr(&self, s: PrSnapshot) -> Result<(), DeliveryError>` — updates the row; when merged with `base_ref == default_branch` → `advance_stage(Merged)` with a signal `{ kind: Merged, source: Github }` and `detail { "sha": merge_commit_sha }`; otherwise no stage move
  - `pub fn ci_state(runs: &CheckRuns, status: &CombinedStatus) -> (CiState, Vec<String>)` — `failing` if any run concluded `failure | timed_out | cancelled | action_required` or the combined state is `failure | error`; else `pending` if any run is not `completed` or the combined state is `pending` with statuses; else `passing` if there is at least one run or status; else `none`. Failing names: runs then contexts, at most 5.
  - `pub async fn apply_ci(&self, task: TaskId, head_sha: &str, state: CiState, failing: Vec<String>) -> Result<(), DeliveryError>` — `set_ci`; on a change to `failing` with no note for this head SHA, posts a system message into the task's thread "CI failing on PR #12: lint, test (sqlite)" (reaches owner and creator) and calls `wake_owner(task, WakeTrigger::CiFailing, None)`
  - `pub fn stall(pr: &TaskPr, stage: Option<DeliveryStage>) -> Option<Stall>`; `pub enum Stall { ClosedUnmerged, MergedInto(String) }` (merged into a non-default branch is read from `state = merged` while stage rank < merged)
- [ ] **Step 1: Write failing tests:** `merge_into_default_moves_to_merged` (stages `[InPr, Merged, Released]`, delivering → Merged, signal row `github`), `merge_completes_at_last_stage` (stages `[InPr, Merged]` → `done`, `done_at` set), `merge_during_review_recorded` (task in `review`: stage `merged` recorded, status unchanged), `merge_into_other_branch_stalls` (`Stall::MergedInto("release/1.4")`, stage stays `InPr`), `closed_unmerged_stalls`, `ci_state_rules` (table of runs/status → word), `ci_failing_note_once_per_sha` (failing → passing → failing on the same SHA posts one note; a new SHA posts another; note text exact), `ci_failing_wakes_owner` (a `WakeRequested` for the owner session with trigger `ci_failing`).
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): PR merge and CI from GitHub, with a CI-failing note`.

### Task 8: Fallback match by short id

**Files:**
- Modify: `src/service/delivery.rs`, `src/delivery/github/poller.rs`
- Test: `tests/github_poller.rs`

**Interfaces:**
- Consumes: core `links::find_short_ids`, `Store::link_pr` with `replace: false`; Task 4 `fallback_candidates`.
- Produces: `pub async fn link_fallback(&self, repo: &RepoRef, pulls: &[PullRequest]) -> Result<Vec<(TaskId, Vec<CandidatePr>)>, DeliveryError>` — for each candidate task, the PRs whose head ref or title contains its short id; exactly one → `link_pr` with `BranchMatch` (head ref matched) or `TitleMatch`; several → returned for `PollerHandle::candidates` (in memory, refreshed each cycle; lost on restart until the next cycle).
- [ ] **Step 1: Write failing tests:** `branch_match_links` (`feat/axl-4deae8e3-rate-limit` → linked, `branch_match`, stage `InPr`), `title_match_links` (`Rate limit (axl-4deae8e3)` → `title_match`), `two_matches_link_nothing` (both PRs returned as candidates; no row), `fallback_never_replaces_reported_link`, `fallback_skips_pr_linked_elsewhere`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): link PRs that mention a task's short id`.

### Task 9: UI — CI and merge chips, Needs you cards, Recheck, candidates

**Files:**
- Modify: `src/ui/views.rs` (`CardView` chips), `src/ui/needs.rs` (`NeedKind`), `src/ui/task_page.rs`, `src/ui/actions.rs`, `src/ui/mod.rs`, `templates/_card.html`, `templates/_needs_you.html`, `templates/task.html`, `static/app.css`, `DESIGN.md` (chip list)
- Test: `tests/ui.rs`

**Interfaces:**
- Consumes: Task 7 `stall`; Task 5 `PollerHandle::{nudge, candidates}`; Task 4 health.
- Produces:
  - Card chips (one meta row, wraps), each labelled text: `CI passing` (ok), `CI failing` (danger), `CI pending` (warn), `no CI`, each linking to `<pr url>/checks`, shown while the stage is In PR or earlier; `merged 2h` linking to `https://github.com/<repo>/commit/<merge sha>`; `PR closed unmerged` / `merged into <branch>` (warn); `stalled · GitHub token rejected` (warn) when the project's source is unhealthy (health word from Task 6).
  - `NeedKind::{CiFailing, PrClosedUnmerged, MergedElsewhere}`, only for `delivering` tasks, with task, project, PR link and age; labels "CI failing", "PR closed unmerged", "Merged into <branch>"; actions for the last two: **Link another PR** (task page anchor), **Mark done**, **Force release**.
  - `POST /ui/tasks/{id}/recheck` (CSRF) → `nudge(repo)`, redirect back with "Recheck requested".
  - Task page: "2 PRs mention axl-4deae8e3 — link one" with a **Link** button per candidate posting to the core `POST /ui/tasks/{id}/pr`; Delivery timeline links In PR to the checks page and Merged to the merge commit.
- [ ] **Step 1: Write failing tests:** `ci_chip_words_and_links` (each state's text and `href` ending `/checks`), `ci_chip_hidden_after_merge`, `merged_chip_links_commit`, `stalled_chip_on_rejected_token`, `needs_you_delivery_cards` (three kinds; not shown for a `review` task with failing CI), `recheck_requires_csrf`, `candidate_link_buttons`, `pr_title_escaped` (title `<script>` renders as text).
- [ ] **Step 2: Run** `cargo test --test ui`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): CI and merge chips, delivery Needs you cards, Recheck now`.

### Task 10: Helm and docs

**Files:**
- Modify: `deploy/helm/axle/values.yaml` (`github.tokenSecret: { name: "", key: "token" }`), `deploy/helm/axle/templates/deployment.yaml` (env `AXL_GITHUB_TOKEN` from the secret when `name` is set), `tests/helm.sh` (render with and without; assert the `secretKeyRef`), `docs/configuration.md` (four variables), `docs/security.md` (token handling, permission table, requests only to `AXL_GITHUB_API_URL`), `docs/delivery.md` (GitHub section: fine-grained token, the six read-only permissions, poll interval, health and banner), `docs/web-ui.md` (chips, cards, banner, Recheck), `docs/concepts.md` (chain steps 1–2, fallback match), `docs/mcp-tools.md` (`delivery.pr.ci`)

- [ ] **Step 1: Write the Helm change and its `tests/helm.sh` assertions.**
- [ ] **Step 2: Run** `bash tests/helm.sh`. Expected: PASS.
- [ ] **Step 3: Write the docs.**
- [ ] **Step 4: Run** `cargo fmt --check && cargo clippy --all-targets --features pg-tests -- -D warnings && cargo test`. Expected: clean.
- [ ] **Step 5: Commit** `docs: GitHub delivery source, token and health`.
