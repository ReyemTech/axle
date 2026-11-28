# Task delivery status: releases and deploys from GitHub (releases) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** For `github.com` projects, the poller moves merged tasks to Released when a GitHub release (or a matching tag) contains their merge commit, and to Deployed when a successful GitHub deployment to the required environment contains it.

**Architecture:** The poller gains release, tag and deployment work items for tasks waiting on those stages. Containment is computed with the compare API, first as one compare between the project's previous and new release (every in-flight merge SHA in the commit list is contained), falling back to one compare per task when the list is cut off or there is no previous release, within the cycle cap. Results are handed to the `-signals` service methods `apply_release` / `apply_deploy` with `source: github` and explicit task ids, so GitHub and generic signals share one apply path, one version table and one signal log.

**Tech Stack:** Rust 2024, reqwest 0.13, sqlx 0.9, askama 0.16, tokio.

**Spec:** `docs/superpowers/specs/2026-11-28-delivery-status-design.md` (binding), sections "The matching chain" (steps 3–4, required environment, "How contains is computed"), "Sources", "The poller", "UI" (projects page).

## Global Constraints

- **Base:** `-core`, `-github` and `-signals` merged. It uses `-github`'s `GithubClient`, stub, `budget::plan`, `WorkKind` and `Poller`, and `-signals`' `DeliveryService::{apply_release, apply_deploy}`, `record_version`, `latest_versions`. No new migration.
- Contains: `GET /repos/{o}/{r}/compare/{base}...{head}` contains `base` when `status` is `ahead` or `identical` (`behind` and `diverged` do not). The release-to-release list is cut off when `total_commits > commits.len()` or `commits.len() >= 250`.
- Releases: `GET /releases?per_page=20`; drafts and prereleases never count; with a tag prefix (`tag_pattern`, a prefix glob such as `v*` or `web-v*`; the `*` is only allowed last), only tags starting with the prefix. Tags source: `GET /tags?per_page=50`, same prefix rule. The tag's commit is resolved with `GET /commits/{tag}` (sha) when the listing does not carry it.
- Only releases published after the oldest waiting task's `merged_at` are considered, so a first poll does not replay a repo's history. Dedupe ids: `github:release:<id>`, `github:tag:<name>`, `github:deployment:<id>`.
- Deployments: `GET /deployments?per_page=30`, filtered in Rust by environment ignoring ASCII case (GitHub's own filter is exact); a deployment counts once its latest status (`GET /deployments/{id}/statuses?per_page=1`) is `success`; earlier states are re-read next cycle and write no signal. Deployments to other environments go to `apply_deploy` too, which records them as `environment_deployed` notes. The deploy URL is the status's `environment_url`, else `log_url`, else `target_url`.
- A deploy resolves its commit through a stored version whose name equals the deployment `ref`, else the deployment's `sha`; tasks are those whose merge SHA it contains (compare per task, within the cap), plus the stored version's task ids.
- All requests are conditional (ETags), counted against the cycle cap, and only for projects whose `release_source` / `deploy_source` is a GitHub one and that have tasks waiting for that stage.
- GitHub source options are hidden for non-`github.com` keys.
- Conventional commits; no AI attribution. Docs: `docs/delivery.md` (GitHub releases, tags, deployments, required environment, monorepo tag prefixes), `docs/web-ui.md`, `docs/concepts.md` (chain steps 3–4).

## Review Focus

1. **A monorepo whose projects tag `web-v*` and `api-v*` in one repo.** A `web-v2.0.0` release never moves an `api` task, and the previous-release compare uses the previous `web-v*` tag, not the previous tag in the repo (Task 3 `monorepo_prefix_scopes_releases`).
2. **A release with 400 commits since the previous one.** The cut-off list is not trusted; per-task compares decide, and tasks beyond the cap roll to the next cycle rather than being skipped (Task 2 `truncated_list_falls_back`).
3. **A deployment still `in_progress` when first seen, `success` a cycle later.** It moves tasks once, when it succeeds; a later `failure` status on an old deployment moves nothing back (Task 4 `deployment_counts_on_success_once`).
4. **A release created as a draft, then published.** The draft is ignored; the published release counts once (Task 3 `draft_then_published`).
5. **Merge SHA not contained (`behind` / `diverged`), e.g. a hotfix release cut from an old tag.** The task stays at Merged (Task 2 `compare_status_rules`).

---

## File Structure

| Path | Responsibility |
|---|---|
| `src/delivery/github/api.rs` | `Release`, `Tag`, `Compare`, `Commit`, `Deployment`, `DeploymentStatus`. |
| `src/delivery/github/contain.rs` | Containment: list check, per-task fallback, budget accounting. |
| `src/delivery/github/releases.rs` | Release and tag discovery per project → `apply_release`. |
| `src/delivery/github/deploys.rs` | Deployment discovery per project → `apply_deploy`. |
| `src/delivery/github/{budget,poller}.rs` | New work kinds and their place in the cycle. |
| `src/store/{mod,sqlite,postgres}.rs` | Waiting-for-stage query. |
| `src/ui/admin.rs`, `templates/projects.html` | GitHub source options and tag prefix. |

---

### Task 1: API types and stub routes

**Files:**
- Modify: `src/delivery/github/api.rs`, `tests/common/github_stub.rs` (fixture builders), `tests/github_client.rs`

**Interfaces:**
- Produces:
  - `Release { id: i64, tag_name: String, draft: bool, prerelease: bool, html_url: String, published_at: Option<OffsetDateTime> }`
  - `Tag { name: String, commit: TagCommit { sha: String } }`
  - `Compare { status: String, total_commits: u32, commits: Vec<CompareCommit { sha: String }> }`
  - `Commit { sha: String }`
  - `Deployment { id: i64, sha: String, ref_: String, environment: String, created_at: OffsetDateTime }` (`#[serde(rename = "ref")]`)
  - `DeploymentStatus { state: String, environment_url: Option<String>, log_url: Option<String>, target_url: Option<String> }`
  - stub helpers: `fixtures::release(id, tag, draft, prerelease)`, `fixtures::compare(status, shas, total)`, `fixtures::deployment(id, sha, ref, env)`, `fixtures::deployment_status(state)`
- [ ] **Step 1: Write failing tests:** `decode_release_tag_compare_deployment` (each fixture decodes through `GithubClient::get`, unknown fields ignored).
- [ ] **Step 2: Run** `cargo test --test github_client`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): GitHub release, tag, compare and deployment types`.

### Task 2: Containment

**Files:**
- Create: `src/delivery/github/contain.rs`
- Test: `tests/github_contain.rs`

**Interfaces:**
- Consumes: Task 1 types; `-github` `GithubClient`.
- Produces:
  - `pub fn contains(status: &str) -> bool` (`ahead` | `identical`)
  - `pub struct Waiting { pub task: TaskId, pub merge_sha: String, pub merged_at: OffsetDateTime }`
  - `pub struct Containment { pub contained: Vec<TaskId>, pub not_contained: Vec<TaskId>, pub deferred: Vec<TaskId>, pub requests: u32 }`
  - `pub async fn contained(client: &GithubClient, repo: &RepoRef, waiting: &[Waiting], previous: Option<(&str /*ref*/, OffsetDateTime)>, target: &str, budget: u32) -> Result<Containment, GhError>` — with `previous`, one compare `{previous}...{target}`: tasks whose merge SHA is in the list are contained; if the list is cut off, or there is no previous, or a task merged before `previous`'s time is absent from the list, that task gets its own compare `{merge_sha}...{target}`, while `budget` lasts; the rest are `deferred`. Compare ETags are never committed (a compare result is used once), so compares are never sent conditionally.
- [ ] **Step 1: Write failing tests** (stub): `compare_status_rules` (`ahead`, `identical` → contained; `behind`, `diverged` → not), `list_marks_contained` (prev...new lists two of three SHAs; the third merged after `previous` → not contained, one request in total), `truncated_list_falls_back` (`total_commits: 400`, 250 listed → one compare per task), `no_previous_compares_each`, `budget_defers` (budget 2, four tasks without previous → two compared, two deferred).
- [ ] **Step 2: Run** `cargo test --test github_contain`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): commit containment through the compare API`.

### Task 3: Releases and tags → Released

**Files:**
- Create: `src/delivery/github/releases.rs`
- Modify: `src/store/{mod,sqlite,postgres}.rs` (`waiting_for`), `src/delivery/github/budget.rs` (`WorkKind::Releases(ProjectId)`, `Tags(ProjectId)`), `src/delivery/github/poller.rs`
- Test: `tests/github_poller.rs`, `tests/store_delivery_github.rs`

**Interfaces:**
- Consumes: Task 2 `contained`; `-signals` `apply_release`, `latest_versions`.
- Produces:
  - store: `async fn waiting_for(&self, project: ProjectId, stage: DeliveryStage) -> StoreResult<Vec<(Task, TaskPr)>>` — tasks of the project, not `done`/`cancelled`, whose stage rank is ≥ `merged` and < `stage`, with a merge SHA, when `stage` is at or before the project's last enabled stage
  - `pub fn tag_matches(pattern: Option<&str>, tag: &str) -> bool`
  - `pub async fn poll_releases(ctx: &PollCtx, project: &ProjectDelivery, budget: &mut u32) -> Result<(), GhError>` — lists releases (or tags), keeps the new eligible ones (not stored yet in `delivery_versions`, not draft/prerelease, prefix match, published after the oldest waiting `merged_at`) oldest first; for each, resolves its commit, computes containment against the project's previous stored version, and calls `apply_release(ReleaseSignal { tasks: contained, sha: Some(commit), url: Some(html_url), source: Github, delivery_id: Some("github:release:<id>") })`; deferred tasks are retried next cycle against the same release
  - `PollCtx` = the poller's shared client, service and log handles (defined in this task, used by Task 4)
- [ ] **Step 1: Write failing tests:** `waiting_for_selection` (`store_matrix!`), `release_moves_contained_tasks` (two merged tasks, one contained → that one `Released`, version recorded with its id, signal source `github`), `prerelease_and_draft_ignored`, `draft_then_published` (counts once, dedupe id), `monorepo_prefix_scopes_releases` (projects `web` with `web-v*` and `api` with `api-v*` in `acme/mono`: `web-v2.0.0` compares against the previous `web-v*` version and moves only `web` tasks), `tags_source` (`github_tags` with `v*`: tag `v1.2.0` counts), `old_releases_not_replayed`, `no_release_requests_when_nothing_waits`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): move tasks to Released from GitHub releases and tags`.

### Task 4: Deployments → Deployed

**Files:**
- Create: `src/delivery/github/deploys.rs`
- Modify: `src/delivery/github/budget.rs` (`WorkKind::Deployments(ProjectId)`), `src/delivery/github/poller.rs`
- Test: `tests/github_poller.rs`

**Interfaces:**
- Consumes: Task 2 `contained`; Task 3 `waiting_for`, `PollCtx`; `-signals` `apply_deploy`, `get_version`.
- Produces: `pub async fn poll_deployments(ctx: &PollCtx, project: &ProjectDelivery, budget: &mut u32) -> Result<(), GhError>` — for deployments newer than the oldest waiting `merged_at` and not yet logged under `github:deployment:<id>`: read the latest status; on `success`, resolve the commit (stored version named by `ref`, else `sha`), compute the contained waiting tasks (merge SHA `...` deploy SHA, plus the version's task ids), and call `apply_deploy(DeploySignal { environment: deployment.environment, version: ref, sha: Some(sha), tasks, url, source: Github, delivery_id })`.
- [ ] **Step 1: Write failing tests:** `deployment_to_required_env_moves` (`production`, success → `Deployed`; stages ending at Deployed → `done` with `done_at`), `environment_case_insensitive` (`Production` counts), `other_environment_noted` (`staging` → `environment_deployed`, stage unchanged), `deployment_counts_on_success_once` (`in_progress` cycle 1 → nothing; `success` cycle 2 → moved; cycle 3 with `failure` → nothing), `deploy_by_version_ref` (`ref: v1.4.2` with a stored version moves its tasks without a compare), `deploy_implies_release` (merged task deployed by SHA → `Deployed`, `released` implied).
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): move tasks to Deployed from GitHub deployments`.

### Task 5: Poller budget across stages

**Files:**
- Modify: `src/delivery/github/poller.rs`, `src/delivery/github/budget.rs`
- Test: `tests/github_poller.rs`

**Interfaces:**
- Consumes: Tasks 3–4.
- Produces: the cycle plan includes `Releases` / `Tags` / `Deployments` items (one per project with waiting tasks, `waiting_since` = its oldest waiting task's stage time) in the same round-robin, oldest-first `plan`; containment compares draw from the same cycle budget; a `404` or rate limit during these behaves as in `-github`.
- [ ] **Step 1: Write failing tests:** `cycle_cap_covers_release_and_deploy_work` (cap 3 across a PR list, a release check and compares → exactly 3 requests; the rest next cycle), `release_work_round_robins_with_prs`, `conditional_release_listing` (second cycle sends `If-None-Match`; `304` → no compare request).
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(delivery): release and deploy work under the poller cap`.

### Task 6: Projects page GitHub options and docs

**Files:**
- Modify: `src/ui/admin.rs`, `src/ui/actions.rs` (validate `tag_pattern`), `templates/projects.html`, `docs/delivery.md`, `docs/web-ui.md`, `docs/concepts.md`
- Test: `tests/ui.rs`

**Interfaces:**
- Produces: on `github.com` projects the Release source select gains **GitHub releases** and **GitHub tags**, a **Tag prefix** field (`v*`, `web-v*`; at most 64 chars of `[A-Za-z0-9._/-]` with an optional trailing `*`), and Deploy source gains **GitHub deployments**; non-GitHub keys never show them and a forged form value is refused (400).
- [ ] **Step 1: Write failing tests:** `github_sources_shown_for_github_keys`, `github_sources_hidden_and_refused_elsewhere`, `tag_prefix_validated`.
- [ ] **Step 2: Run** `cargo test --test ui`. Expected: FAIL.
- [ ] **Step 3: Implement;** write the docs (releases vs tags, prefixes for monorepos, required environment, what "contains" means, rate-limit cost).
- [ ] **Step 4: Run** `cargo fmt --check && cargo clippy --all-targets --features pg-tests -- -D warnings && cargo test`. Expected: clean.
- [ ] **Step 5: Commit** `feat(ui): GitHub release, tag and deployment sources`.
