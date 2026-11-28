# Task delivery status: generic signals (signals) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** CI jobs, Pulumi runs, ArgoCD hooks and people can tell Axle that a version was released or deployed, with `axl report` or a signed webhook, and the tasks it ships move to Released and Deployed; this works for any project key, with or without GitHub.

**Architecture:** One service path applies every released/deployed signal: `DeliveryService::apply_release` and `apply_deploy` validate the fields, log a `delivery_signals` row with its outcome, record versions in `delivery_versions`, match tasks by explicit id, exact merge SHA or stored version, and move them with the core `advance_stage`. Two entry points feed it: `POST /v1/delivery/report` (agent bearer token) and `POST /hooks/delivery/{hook_id}` (HMAC over a secret sealed with `AXL_SECRET_KEY`). `axl report` picks the webhook when `AXL_DELIVERY_URL`/`AXL_DELIVERY_SECRET` are set, else the agent token. The `-releases` plan later calls the same two service methods for GitHub releases and deployments.

**Tech Stack:** Rust 2024, axum 0.8, reqwest 0.13, sqlx 0.9, clap 4, `hmac` 0.13 + existing `sha2` 0.11, `chacha20poly1305` (XChaCha20-Poly1305, the release that pairs with `digest` 0.11), `base64` 0.22, askama 0.16 + htmx.

**Spec:** `docs/superpowers/specs/2026-11-28-delivery-status-design.md` (binding), sections "Sources", "Generic: CLI", "Generic: signed webhook", "Without a GitHub source", "Webhook secret at rest", "Security" (signal fields), "Failure handling" (bad signals), "MCP, HTTP and CLI surface".

## Global Constraints

- **Base:** `2026-11-28-delivery-status-core.md` merged. No new migration (0006 holds `delivery_signals`, `delivery_versions`, `delivery_hooks`). Runs in parallel with `-github`; rebase on whichever lands first. `-releases` depends on this plan's Task 3.
- Signal fields: version at most **128** chars of `[A-Za-z0-9._+/-]`; environment at most **64** of `[A-Za-z0-9._-]`; SHA **7 to 40** hex; URLs `https` only, at most **2 KB**. Rendered as text; links get `rel="noopener noreferrer"`.
- Webhook: `POST /hooks/delivery/{hook_id}`, JSON `{ "kind": "released" | "deployed", "version", "sha"?, "environment"?, "url"?, "tasks"? }`. Headers `X-Axl-Timestamp` (Unix seconds), `X-Axl-Signature: sha256=<hex>` = HMAC-SHA256 over `<timestamp>.<raw body>`, optional `X-Axl-Delivery`. Constant-time comparison (`Mac::verify_slice`). Timestamp within **300 s** either way. Body at most **16 KB**. **60** requests per minute per hook. Responses: `202` with `{ "outcome", "matched", "message" }`, `401` with no detail for any signature or time failure, `409` for a replayed delivery id, `413` over size, `429` over rate.
- Rulings: a disabled webhook (no `AXL_SECRET_KEY`) and an unknown `hook_id` both answer `404` with no body, so the URL leaks nothing. Delivery ids are deduplicated by the unique `(project_id, delivery_id)` index, which keeps them for good (the spec's 24 h is a floor). Generic signals are accepted unless that stage's source is `none` (with a GitHub source on too, the first signal wins). A deploy's `version` is tried first as a stored release version, then, when it is 7–40 hex, as a merge-SHA prefix; `sha`, when given, is also a merge-SHA prefix.
- `hook_id`: 16 random bytes, lowercase hex, unique; never the project key. Secret: 32 random bytes, shown once as `axd_` + base64url (no padding); stored sealed (XChaCha20-Poly1305, random 24-byte nonce prefixed) under `AXL_SECRET_KEY` (32 bytes, standard base64; anything else stops startup naming `AXL_SECRET_KEY`); fingerprint = first 8 hex of SHA-256 of the secret.
- CLI exactly: `axl report released <version> [--sha <commit>] [--url <page>] [--task <id>]... [--project <key>] [--soft]` and `axl report deployed <environment> <version-or-sha> [--url <page>] [--project <key>] [--soft]`. Output `released v1.4.2: 3 tasks moved to Released` / `deployed production v1.4.2: 2 tasks moved to Deployed` / `… 0 tasks moved (unmatched)`. `--soft` exits 0 on any Axle error with one stderr line. `AXL_DELIVERY_URL` is the full hook URL.
- History kind added here: `TaskEventKind::EnvironmentDeployed` => `environment_deployed` (payload `environment`, `version`, `url`), for deploys to an environment other than the required one; it never moves a stage.
- Agents cannot move stages through MCP; `axl report` with an agent token can (same trust as any agent); every signal records its source (`cli` or `webhook`).
- Conventional commits; no AI attribution. Docs: `cli.md`, `http-api.md`, `configuration.md` (`AXL_SECRET_KEY`, `AXL_DELIVERY_URL`, `AXL_DELIVERY_SECRET`), `security.md` (webhook), `web-ui.md`, `concepts.md`, `docs/delivery.md` (generic source with Pulumi, ArgoCD and Vercel examples), Helm `secretKeySecret`.

## Review Focus

1. **A signature computed over re-serialised JSON instead of the raw bytes**, or a body with a trailing newline. Verification uses the exact received bytes; a CLI-signed request with a trailing newline still verifies (Task 6 `signature_over_raw_body`).
2. **A mistyped environment** (`prod` for `production`). Matching ignores ASCII case and is exact otherwise; `prod` moves no stage, becomes an `environment_deployed` note on the matched tasks, and shows with its environment in the Delivery panel's signal list, so the typo is visible (Task 3 `other_environment_is_a_note`, Task 8 `signal_log_shows_outcome`).
3. **The same CI job retried** (same `X-Axl-Delivery`). The second is `409` and nothing moves twice (Task 6 `replay_is_409`).
4. **`axl report` in a script while Axle is down.** With `--soft` it exits 0 within its timeout and prints one stderr line; without, it exits non-zero (Task 7 `soft_is_fail_open`).
5. **A deploy naming a short SHA that prefixes two merge SHAs, or a version never released.** Every in-flight task whose merge SHA has that prefix moves (the reporter named it); an unknown version is `unmatched` and moves nothing (Task 3 `deploy_resolution`).

---

## File Structure

| Path | Responsibility |
|---|---|
| `src/delivery/signal.rs` | Field validators, `ReportBody`, `ReleaseSignal`, `DeploySignal`, result text. |
| `src/delivery/seal.rs` | `SecretKey`, seal/open, fingerprint, hook id and secret generation, HMAC sign/verify. |
| `src/service/delivery.rs` | `apply_release`, `apply_deploy`, hook create/rotate/delete. |
| `src/store/{mod,sqlite,postgres}.rs` | Versions, SHA lookup, signal list, hooks. |
| `src/http/delivery.rs` | `/v1/delivery/report` and `/hooks/delivery/{hook_id}`. |
| `src/cli/report.rs`, `src/main.rs`, `src/client/api.rs` | `axl report`. |
| `src/ui/*`, templates | Sources, environment, webhook panel, signal log, released/deployed chips and timeline links. |

---

### Task 1: Signal fields and request types

**Files:**
- Create: `src/delivery/signal.rs`
- Modify: `src/delivery/mod.rs`, `src/domain.rs` (`EnvironmentDeployed`)
- Test: unit tests in `signal.rs`

**Interfaces:**
- Produces:
  - `pub fn check_version(s: &str) -> Result<(), String>`, `check_environment`, `check_sha`, `check_url` (messages name the field and the rule, e.g. `version must be at most 128 characters of A-Z a-z 0-9 . _ + / -`)
  - `#[derive(Deserialize)] #[serde(deny_unknown_fields)] pub struct ReportBody { pub kind: ReportKind, pub version: String, pub sha: Option<String>, pub environment: Option<String>, pub url: Option<String>, pub tasks: Option<Vec<String>>, pub project: Option<String> }`, `pub enum ReportKind { Released, Deployed }` (`project` is required on `/v1`, refused on the webhook)
  - `pub struct ReleaseSignal { pub project: ProjectId, pub version: String, pub sha: Option<String>, pub url: Option<String>, pub tasks: Vec<TaskId>, pub merge_shas: Vec<String>, pub source: SignalSource, pub delivery_id: Option<String> }`
  - `pub struct DeploySignal { pub project: ProjectId, pub environment: String, pub version: String, pub sha: Option<String>, pub url: Option<String>, pub tasks: Vec<TaskId>, pub source: SignalSource, pub delivery_id: Option<String> }`
  - `pub struct SignalResult { pub outcome: SignalOutcome, pub matched: u32, pub message: String }`
  - `ReportBody::into_release(self, project, source, delivery_id) -> Result<ReleaseSignal, String>` and `into_deploy(…) -> Result<DeploySignal, String>` (deploy needs `environment`; every field validated; task ids parsed as full UUIDs or `axl-` short ids)
- [ ] **Step 1: Write failing tests:** `field_limits` (version of 128 ok / 129 refused, `v1.4.2+build/7` ok, `v1 4` refused; environment `prod-eu.1` ok, `prod/eu` refused; SHA 6 and 41 hex refused, 7 and 40 ok, `xyz1234` refused; URL `http://` refused, 2049 bytes refused), `body_unknown_field_refused`, `deploy_needs_environment`, `short_id_tasks_accepted`.
- [ ] **Step 2: Run** `cargo test --lib signal`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): generic signal fields and request types`.

### Task 2: Store — versions, SHA lookup, signal log

**Files:**
- Modify: `src/store/mod.rs`, `src/store/sqlite.rs`, `src/store/postgres.rs`, `src/domain.rs` (`DeliveryVersion`)
- Test: `tests/store_delivery_signals.rs`

**Interfaces:**
- Produces:
  - `struct DeliveryVersion { project_id: ProjectId, version: String, commit_sha: Option<String>, url: Option<String>, released_at: OffsetDateTime, task_ids: Vec<TaskId> }`
  - `async fn record_version(&self, v: &DeliveryVersion) -> StoreResult<bool /*new*/>` (insert; an existing `(project, version)` keeps its row and gains any new task ids)
  - `async fn get_version(&self, project: ProjectId, version: &str) -> StoreResult<Option<DeliveryVersion>>`
  - `async fn latest_versions(&self, project: ProjectId, limit: u32) -> StoreResult<Vec<DeliveryVersion>>` (newest `released_at` first; used by `-releases`)
  - `async fn tasks_by_merge_sha(&self, project: ProjectId, sha_prefix: &str) -> StoreResult<Vec<TaskId>>` (tasks of the project, not `done`/`cancelled`, whose PR `merge_commit_sha` starts with the lowercase prefix)
  - `async fn task_by_short_id(&self, project: ProjectId, short: &str) -> StoreResult<Option<TaskId>>`
  - `async fn recent_signals(&self, project: ProjectId, limit: u32) -> StoreResult<Vec<DeliverySignal>>` (newest first)
- [ ] **Step 1: Write failing tests** (`store_matrix!`): `version_record_and_merge_ids`, `latest_versions_order`, `merge_sha_prefix` (`3f2a9c1` finds the task with `3f2a9c1e…`; closed tasks excluded; another project's task excluded), `short_id_lookup`, `recent_signals_order`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement** in both stores.
- [ ] **Step 4: Run** both backends. Expected: PASS.
- [ ] **Step 5: Commit** `feat(store): delivery versions, merge-SHA lookup and signal log`.

### Task 3: Applying released and deployed signals

**Files:**
- Modify: `src/service/delivery.rs`
- Test: `tests/delivery_signals.rs`

**Interfaces:**
- Consumes: Task 1 types; Task 2 store; core `insert_signal`, `advance_stage`, `stages::implied_between`.
- Produces:
  - `pub async fn apply_release(&self, s: ReleaseSignal) -> Result<SignalResult, DeliveryError>` — refused (`outcome refused`, message `releases are turned off for this project`) when `release_source = none`; matches `s.tasks` ∪ tasks for each of `s.merge_shas` (and `s.sha` when given); records the version (`task_ids` = matched); moves each matched task with a stage below Released to `Released`, implied stages from its current stage, `detail { version, url }`; signal row with `matched` and `applied` (or `unmatched` when nothing matched, `duplicate` on a repeated delivery id, moving nothing)
  - `pub async fn apply_deploy(&self, s: DeploySignal) -> Result<SignalResult, DeliveryError>` — refused when `deploy_source = none`; resolves the version (stored version → its task ids; else, when hex 7–40, a merge-SHA prefix), plus `s.sha` and `s.tasks`; when `environment` equals `required_environment` ignoring ASCII case, moves matched tasks to `Deployed` (`detail { version, environment, url }`); otherwise appends `environment_deployed` on each matched task and moves nothing (outcome `applied`, message `deployed staging v1.4.2: 2 tasks noted`)
  - `SignalResult.message` exactly `released v1.4.2: 3 tasks moved to Released`, `deployed production v1.4.2: 2 tasks moved to Deployed`, `released v1.4.2: 0 tasks moved (unmatched)`
- [ ] **Step 1: Write failing tests** (`store_matrix!` + `TestClock`): `release_by_task_ids` (two delivering tasks at `Merged`, stages `[InPr, Merged, Released, Deployed]` → both `Released`, message exact), `release_by_merge_sha`, `release_implies_merged` (task at `InPr` → `Released`, `merged` implied), `release_at_last_stage_delivers` (stages end at Released → `done` with `done_at`), `deploy_by_version` (version recorded by an earlier release moves its tasks), `deploy_resolution` (unknown version → `unmatched`; `3f2a9c1` prefix moves the matching tasks), `other_environment_is_a_note` (`staging` → `environment_deployed`, stage unchanged; `PRODUCTION` counts as `production`), `source_none_refused`, `duplicate_delivery_id_moves_nothing`, `generic_project_without_github` (project key `gitlab.example.com/a/b`: release by `--task` works).
- [ ] **Step 2: Run** `cargo test --test delivery_signals`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): apply released and deployed signals`.

### Task 4: Secret key, sealing and webhook hooks

**Files:**
- Create: `src/delivery/seal.rs`
- Modify: `Cargo.toml` (`hmac`, `chacha20poly1305`, `base64`), `src/config.rs` (`ServerConfig.secret_key: Option<SecretKey>`), `src/store/{mod,sqlite,postgres}.rs` (hooks), `src/service/delivery.rs`
- Test: unit tests in `seal.rs`, `tests/store_delivery_signals.rs`, `tests/delivery_signals.rs`

**Interfaces:**
- Produces:
  - `pub struct SecretKey([u8; 32])` (redacted `Debug`) with `fn from_base64(s: &str) -> Result<Self, ConfigError>`
  - `pub fn seal(key: &SecretKey, plain: &[u8]) -> Vec<u8>`, `pub fn open(key: &SecretKey, sealed: &[u8]) -> Option<Vec<u8>>`
  - `pub fn fingerprint(secret: &str) -> String` (8 hex), `pub fn new_hook_id() -> String`, `pub fn new_secret() -> String` (`axd_…`)
  - `pub fn sign(secret: &[u8], timestamp: i64, body: &[u8]) -> String` (`sha256=<hex>`) and `pub fn verify(secret: &[u8], timestamp: i64, body: &[u8], header: &str) -> bool` (constant time)
  - store: `struct DeliveryHook { project_id, hook_id, secret_sealed: Vec<u8>, secret_fingerprint, created_at, rotated_at: Option<OffsetDateTime> }`; `async fn put_hook(&self, h: &DeliveryHook) -> StoreResult<()>` (insert or replace for the project, keeping `created_at`), `async fn delete_hook(&self, project) -> StoreResult<bool>`, `async fn hook_by_id(&self, hook_id: &str) -> StoreResult<Option<DeliveryHook>>`, `async fn hook_for_project(&self, project) -> StoreResult<Option<DeliveryHook>>`
  - service: `pub async fn create_hook(&self, project: &Project) -> Result<(String /*url*/, String /*secret, shown once*/), DeliveryError>`, `rotate_hook` (same `hook_id`, new secret), `delete_hook`; all refuse with `webhooks need AXL_SECRET_KEY` when the key is unset. URL = `AXL_PUBLIC_URL` + `/hooks/delivery/<hook_id>`.
- [ ] **Step 1: Write failing tests:** `secret_key_config` (unset → `None`; 32-byte base64 ok; 31 bytes and non-base64 → error naming `AXL_SECRET_KEY`), `seal_round_trip` (and a flipped byte → `None`; another key → `None`), `sign_known_vector` (fixed secret, timestamp, body → fixed hex), `verify_rejects_wrong`, `hook_store_round_trip` (`store_matrix!`), `rotate_keeps_hook_id`, `hooks_refused_without_key`, `secret_not_stored_plain` (the raw `delivery_hooks` row contains no secret bytes).
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): sealed webhook secrets under AXL_SECRET_KEY`.

### Task 5: `POST /v1/delivery/report`

**Files:**
- Create: `src/http/delivery.rs`
- Modify: `src/http/v1.rs` (route), `src/http/mod.rs`
- Test: `tests/http_delivery.rs`

**Interfaces:**
- Consumes: Task 1 `ReportBody`; Task 3 service.
- Produces: `POST /v1/delivery/report` behind the existing agent bearer auth and per-token rate limit; body `ReportBody` with `project` (key) required; source `cli`; `delivery_id` from `X-Axl-Delivery` when sent. `202 { outcome, matched, message }`; `400 { error }` for a bad field or unknown project; `401` without a valid token.
- [ ] **Step 1: Write failing tests:** `report_with_agent_token` (release moves a task; response message exact), `report_needs_token` (401), `report_bad_field_400` (message names the field), `report_unknown_project_400`.
- [ ] **Step 2: Run** `cargo test --test http_delivery`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(http): delivery report endpoint for agent tokens`.

### Task 6: `POST /hooks/delivery/{hook_id}`

**Files:**
- Modify: `src/http/delivery.rs`, `src/http/mod.rs` (route outside `/v1` and `/ui`, body limit 16 KB, per-hook `RateLimiter<String>` at 60/min)
- Test: `tests/http_delivery.rs`

**Interfaces:**
- Consumes: Task 4 `hook_by_id`, `open`, `verify`; Task 3 service.
- Produces: the handler, in this order: unknown hook or no key → `404`; over 16 KB → `413`; over rate → `429`; missing/garbled headers, timestamp outside ±300 s, or bad signature → `401` empty body; parse `ReportBody` (a `project` field → `400`); `X-Axl-Delivery` already seen → `409`; else `202` with the service result, source `webhook`.
- [ ] **Step 1: Write failing tests:** `signed_release_accepted` (202, task moved), `signature_over_raw_body` (body with a trailing newline, signed as sent → 202), `bad_signature_401_no_detail` (body empty), `stale_and_future_timestamp_401` (−301 s and +301 s; ±300 s accepted), `replay_is_409`, `oversize_413`, `rate_limit_429` (61st request in a minute), `disabled_without_secret_key_404`, `unknown_hook_404`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(http): signed delivery webhook`.

### Task 7: `axl report`

**Files:**
- Create: `src/cli/report.rs`
- Modify: `src/main.rs` (`Command::Report { #[command(subcommand)] … }`), `src/cli/mod.rs`, `src/client/api.rs` (`report(token, body) -> Result<ReportReply, ClientError>` and `report_signed(url, secret, body)`)
- Test: `tests/cli_report.rs`

**Interfaces:**
- Consumes: Task 4 `sign`; `bridge::project::resolve_project` for the project (`AXL_PROJECT`, `.axl`, git `origin`) unless `--project`; client config for server URL and token.
- Produces: `pub async fn report(args: ReportArgs, env: &Env, out: &mut impl Write, err: &mut impl Write) -> ExitCode`. Signed path when both `AXL_DELIVERY_URL` and `AXL_DELIVERY_SECRET` are set (one without the other is an error naming the missing one): timestamp from the clock, a fresh UUIDv7 `X-Axl-Delivery`, no `project` field. Otherwise `POST /v1/delivery/report` with the agent token and `project`. `deployed <env> <arg>` sends `version = <arg>`. Prints the server's `message`. Timeout 10 s. `--soft`: any error → stderr `axl: report failed (ignored with --soft): <reason>`, exit 0.
- [ ] **Step 1: Write failing tests** (against an in-process server): `report_released_with_token` (stdout `released v1.4.2: 1 tasks moved to Released`), `report_signs_correctly` (with `AXL_DELIVERY_URL`/`SECRET` → 202 from the real webhook handler), `half_signed_env_is_error`, `soft_is_fail_open` (server down: `--soft` exit 0 with one stderr line; without `--soft` exit 1), `project_from_flag_and_origin`.
- [ ] **Step 2: Run** `cargo test --test cli_report`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(cli): axl report released and deployed`.

### Task 8: UI — sources, environment, webhook panel, signal log, chips

**Files:**
- Modify: `src/ui/admin.rs`, `src/ui/actions.rs`, `src/ui/mod.rs`, `src/ui/views.rs` (card chips), `src/ui/task_page.rs` (timeline links), `templates/projects.html`, `templates/_card.html`, `templates/task.html`, `static/app.css`
- Test: `tests/ui.rs`

**Interfaces:**
- Consumes: Task 2 `recent_signals`, `get_version`; Task 4 hook service; core settings.
- Produces:
  - Project Delivery panel gains: **Release source** (`Generic only`, `None`), **Deploy source** (`Generic only`, `None`) — the `-releases` plan adds the GitHub options and the tag prefix; **Required environment** (default `production`, validated as an environment); saved by `POST /ui/projects/{id}/delivery` (same form as core). Webhook block, only with `AXL_SECRET_KEY` (else the sentence "Webhooks need AXL_SECRET_KEY on the server."): **Create signing secret** shows URL and secret once (with "Copy it now; it is not shown again."); afterwards only the fingerprint, **Rotate**, **Delete**; routes `POST /ui/projects/{id}/hook`, `/hook/rotate`, `/hook/delete` (CSRF). A "Recent signals" list: kind, version, environment, source, outcome word, matched count, age (last 10).
  - Card chips: `v1.4.2` linking to the release URL, `deployed production 20m` linking to the deploy URL, `deployed staging` as a quiet extra on a card still waiting for production (from `environment_deployed`). Timeline rows for Released and Deployed show version, environment and link; implied rows read "implied by deploy".
- [ ] **Step 1: Write failing tests:** `sources_and_environment_save`, `environment_validated`, `secret_shown_once` (create page contains the secret; a reload shows only the fingerprint), `webhook_block_hidden_without_key`, `rotate_and_delete_csrf`, `signal_log_shows_outcome` (`unmatched` row with its words), `release_and_deploy_chips` (text and `href`, `rel="noopener noreferrer"`), `version_rendered_as_text`.
- [ ] **Step 2: Run** `cargo test --test ui`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): generic delivery sources, webhook secret and signal log`.

### Task 9: Helm and docs

**Files:**
- Modify: `deploy/helm/axle/values.yaml` (`secretKeySecret: { name: "", key: "secret-key" }`), `deploy/helm/axle/templates/deployment.yaml` (env `AXL_SECRET_KEY`), `tests/helm.sh`, `docs/delivery.md` (generic source: CLI, webhook, signing recipe in shell, examples for a Pulumi run, an ArgoCD PostSync hook job and a Vercel deploy hook relay), `docs/cli.md`, `docs/http-api.md` (report, webhook, status codes, `environment_deployed`), `docs/configuration.md`, `docs/security.md` (HMAC, sealing, replay, 404 when disabled), `docs/web-ui.md`, `docs/concepts.md` (generic chain)

- [ ] **Step 1: Write the Helm change and its `tests/helm.sh` assertions.**
- [ ] **Step 2: Run** `bash tests/helm.sh`. Expected: PASS.
- [ ] **Step 3: Write the docs;** check the shell signing recipe against `sign_known_vector`.
- [ ] **Step 4: Run** `cargo fmt --check && cargo clippy --all-targets --features pg-tests -- -D warnings && cargo test`. Expected: clean.
- [ ] **Step 5: Commit** `docs: generic delivery signals, axl report and the signed webhook`.
