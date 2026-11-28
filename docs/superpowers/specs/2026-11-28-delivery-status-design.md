# Task delivery status — design

Status: owner decisions settled (2026-11-28); items marked *Controller's call* were settled
for the builder. Extends `2026-11-27-agent-coordination-design.md`.

**Build order (Controller's call):** done-ack (`0004`, branch `feat/done-ack`) → wake-up
(`0005`, adds `requested`, branch `docs/wake-up-spec`) → this (`0006`). Plan against both
merged.

## Why

A task is `done` at `task_complete`, but for code work the PR still has to pass CI, merge,
release and deploy. The owner tracks that by hand. Success: an agent opens a PR and completes
its task; the card moves through In PR, Merged, Released and Deployed on its own, shows CI
while in PR, and lands in Done at the last stage the project cares about.

## Goals and limits

- Optional stages after Review, per project, any subset; PR linking by report or short id;
  signals from GitHub (built in) or generic (CLI, signed webhook); the chain PR → merge
  commit → release → deploy by commit containment.
- Not in v1: writing to GitHub, cluster or cloud credentials, GitHub App/OAuth/Enterprise
  Server (seam only), GitHub webhooks (hook point only), several PRs per task.
- Failure even if it "works": a project with no stages behaves differently from today; a
  missing token slows or fails `task_complete`; the token appears in any log, error, page or
  snapshot; an agent moves a task to Deployed through MCP.

## Approaches

- **A. Token + polling (chosen).** One read-only token; a poller asks only about tasks in
  flight, with conditional requests. Works behind NAT and on a laptop, no public URL, no app
  registration. Latency of one interval is fine for a board read at a glance.
- **B. GitHub webhooks.** Instant, but needs a public HTTPS endpoint and replay of missed
  deliveries; laptop and VPN servers cannot receive them. Later optional speed-up that only
  nudges the poller (see "Hook point").
- **C. GitHub App.** Best for the hosted axle-saas, heavy for one self-hosted owner (app
  registration, private key, install flow). The `GithubCredentials` seam keeps it open.

## Stages and statuses

Stages, in order: `in_pr`, `merged`, `released`, `deployed`. Board labels: **In PR**,
**Merged**, **Released**, **Deployed**.

**One new status, `delivering`**, not four: finished by its agent, waiting on the world. Its
column comes from `delivery_stage`. One status keeps the CHECK, filters and guards small.

Rules for `delivering`:

- The owner is kept (as on `done`, the record of who did it). The lease does not run, as for
  `review` and `blocked`.
- Not claimable. Agents cannot move it with MCP; only signals and the human do.
- The human may **Mark done** (skip the remaining stages), **Advance stage** (one stage
  forward), **Force release** (back to `open`, the PR link kept) or **Cancel**.

**Parents follow their children.** The "last child closes → review" rule changes: a child
counts as closed when it is `done` or `cancelled`, not `delivering`. When every child is
`done`, `delivering` or `cancelled` and at least one is `delivering`, the parent becomes
`delivering` at the **minimum** stage across its delivering children, recomputed (forward
only) on every child stage move. It reaches `done` only when all non-cancelled children are
`done`. A parent with its own PR is not special: its stage is the minimum of its own and its
children's. The same guarded statement runs in both stores.

### How `task_complete` interacts

`task_complete` decides between `done` and `delivering` in the same guarded write:

| Project stages | Task has a linked PR | Result |
|---|---|---|
| none enabled | any | `done`, exactly as today |
| some enabled | no | `done` (the task skips the stages) |
| some enabled | yes | `delivering`, at the furthest stage already reached, at least `in_pr` |
| parent, any child `delivering` | any | `delivering` at the minimum child stage (rule above) |

- If `links` holds a PR URL of the project's repo and no PR is linked yet, the first such URL
  is linked before the decision (`linked_by = complete_links`).
- If the linked PR already reached the last enabled stage (merged before completion, and the
  project stops at Merged), the task goes straight to `done`.
- The result carries `next_step: "Delivery is tracked by Axle; this task moves to done when
  it reaches <last stage>. Nothing more to do."` so the agent does not poll.
- `task_complete` never calls GitHub. It reads only the stored PR state.

### Composing with done-ack and wake-up

- `done_at` is set when the task finally reaches `done` (last enabled stage, Mark done, or
  stages turned off), not at `task_complete`, so the unseen window starts when the change is
  live. Stage times live in `task_stages`. Done-ack's `task_list` default never hides
  `delivering` tasks.
- `requested` and `delivering` never meet: a requested task cannot be claimed or completed.
- Stage moves are not wake triggers. The one exception is **CI failing**: Axle posts a
  system note into the task's own thread, which wakes the owner agent under the wake-up
  thread trigger and counts toward the peer/thread wake budget.

### Skipping and stage changes

- A later signal can arrive first (a deploy names a SHA that is merged but never released).
  The task moves to the later stage; skipped stages are recorded as `implied` on the timeline.
- Moves are forward only: one guarded `UPDATE … WHERE status = 'delivering' AND stage_rank <
  :new`. Two signals racing for one task have one winner.
- Turning a stage off: each delivering task is recomputed. A task at or past the new last
  enabled stage goes to `done`, others stay at their furthest enabled stage. Turning all
  stages off moves every delivering task in the project to `done`, with a history note
  "delivery stages turned off".
- Turning a stage on applies to tasks completed from then on; done tasks are not reopened.

**Stages that start later than In PR (Controller's call).** `delivery_stage` always records
the furthest stage reached, enabled or not. It is null only while no PR is linked or before
`in_pr` is reached. Stages reached before `task_complete` (a PR merged during review) are
recorded in `task_stages` at the time they happen. A delivering task whose furthest stage
precedes the first enabled stage (the project starts at Released, the PR is only open) sits
in the first enabled column, labelled "waiting for Released".

## Linking a task to its PR

**Repo mapping (Controller's call).** A project's repo is the first two path segments after
the host: `github.com/acme/mono/web` → `acme/mono`. Several projects may share a repo
(monorepo). Polling is per repo, PR URL validation checks the repo (not the full key), and
the fallback match only considers tasks in projects mapped to that repo.

**Agent report.** `task_update` gains `pr` (URL). The owner or creator may set it in any
status except `done` and `cancelled`. The PR is validated (see Security) and stored. The
session-start Axle rules (`src/install/instructions.md`, and the MCP `INSTRUCTIONS`) gain:

> When you open a pull request for an Axle task, report it at once with `task_update { id,
> pr: "<url>" }`. Put the task's short id (`axl-xxxxxxxx`, given by `task_claim`) in the
> branch name. After `task_complete`, Axle follows the PR to merge, release and deploy; you
> are done with the task.

**Short id.** `axl-` plus the **last** 8 hex digits of the task UUID, lowercase
(`axl-4deae8e3`); UUIDv7's leading digits are a timestamp shared within a minute.
`task_claim` and `task_get` return `short_id`; card and task page show it in mono.

**Fallback match.** For a project with stages and a GitHub source, the poller reads the
repo's recently updated PRs. A PR matches a task when its head branch or title contains the
short id: case-insensitive `(^|[^a-z0-9])axl-([0-9a-f]{8})([^0-9a-f]|$)`, so
`feat/axl-4deae8e3-rate-limit` and `Rate limit (axl-4deae8e3)` both match. Only tasks that are
not `done` or `cancelled`, in projects mapped to the PR's repo, are candidates.

**Conflicts (v1: one PR per task, one task per PR).**

- A task has at most one PR; a PR belongs to at most one task (unique on repo + number).
- Reporting a second PR for a task **re-links**: the old link is kept in history and the
  stage state is recomputed from the new PR. Refused once the task reached `merged`:
  `this task was already merged via #12; open a new task for follow-up work`.
- Reporting a PR already linked to another task is refused, naming that task.
- The fallback links only when the task has no PR and exactly one PR matches. With several,
  nothing links and the task page says "2 PRs mention axl-4deae8e3 — link one", with a
  **Link** button on each. The fallback never replaces a reported or human link.
- Children carry their own PRs; a parent follows the parent rule above. The human can
  **Link PR** and **Unlink PR** on the task page (CSRF POST, history entry).

## The matching chain

1. **In PR.** A linked open PR. CI state comes from the PR's head SHA.
2. **Merged.** The PR's `merged_at` is set **and** its base is the repo's default branch
   (*Controller's call*). Store `merge_commit_sha` (for squash and rebase merges, the commit
   on the base branch). A PR closed without merge, or merged into another branch, stalls the
   task in In PR with chip "PR closed unmerged" or "merged into <branch>" and a Needs you
   card (actions: Link another PR, Mark done, Force release).
3. **Released.** A release whose tag commit contains the merge SHA. Prereleases and drafts
   never count in v1.
4. **Deployed.** A deploy signal for the project's required environment naming a version,
   tag or SHA whose commit contains the merge SHA. Every in-flight task contained moves.

**Required environment:** a project setting, default `production`, case-insensitive. Other
environments show on the timeline ("deployed to staging") but do not move the column.

**How "contains" is computed (GitHub source).** With the compare API:
`GET /repos/{o}/{r}/compare/{merge_sha}...{target}` contains when `status` is `ahead` or
`identical`. To keep calls down, a new release is first compared against the previous
release of the same project (`{prev_tag}...{new_tag}`; in a monorepo, the project's
optional release tag prefix such as `web-v*` picks its tags); every in-flight merge SHA in that
commit list is contained. When the list is cut off (over 250 commits) or there is no previous
release, fall back to one compare per in-flight task, within the cycle cap. A deploy naming a
version resolves it to a commit through the stored releases, or through
`GET /repos/{o}/{r}/commits/{ref}` for a tag or SHA, then uses the same test.

**Without a GitHub source** (a key not on `github.com`, or GitHub off) Axle computes no
containment, and v1 does no local git containment either. Generic signals name the exact
SHA or version: a release names its tasks (`--task`) or the merge SHAs it ships, a deploy
names a version (moving that release's tasks) or an exact merge SHA. Anything else is logged
as `unmatched`.

## Sources

Each project picks its sources in its delivery settings:

| Stage | Built-in source | Generic source |
|---|---|---|
| In PR, CI | GitHub pull requests, check runs, commit statuses | — |
| Merged | GitHub pull requests | — |
| Released | GitHub releases (default) or tags matching a pattern | `axl report released`, webhook |
| Deployed | GitHub deployments for the required environment | `axl report deployed`, webhook |

- Releases ignore drafts and prereleases; the optional tag prefix (`v*`, `web-v*`) scopes
  tags and releases. Deployments count at latest status `success`. With both a GitHub and a
  generic source on, the first signal wins. Non-`github.com` keys get only generic.

### Generic: CLI

```sh
axl report released v1.4.2 [--sha <commit>] [--url <release page>] [--task <id>]...
axl report deployed production v1.4.2 [--url <run or deploy page>]
axl report deployed production 3f2a9c1
```

- Project from the usual detection (`AXL_PROJECT`, `.axl`, git `origin`) or `--project`.
- Auth: if `AXL_DELIVERY_URL` and `AXL_DELIVERY_SECRET` are set (CI, a Pulumi run, an ArgoCD
  hook), it sends a signed webhook. Otherwise it uses the machine's agent token. Prints what
  matched: `released v1.4.2: 3 tasks moved to Released`.
- Fail-open like hooks for scripts: `--soft` exits 0 on any Axle error, with a stderr line.

### Generic: signed webhook

`POST /hooks/delivery/{hook_id}`, JSON:
`{ "kind": "released" | "deployed", "version", "sha"?, "environment"?, "url"?, "tasks"? }`.

- `hook_id` is a random id per project, not the project key, so the URL leaks nothing.
- Headers: `X-Axl-Timestamp` (Unix seconds), `X-Axl-Signature: sha256=<hex>` (HMAC-SHA256
  over `<timestamp>.<raw body>`), optional `X-Axl-Delivery` id.
- Refused unless the signature matches (constant-time), the timestamp is within 300 s, and
  the delivery id (when sent) is new for the project (kept 24 h). Body at most 16 KB, 60
  requests per minute per hook. `202` with the match count, `401` for any signature or time
  failure (no detail), `409` for a replay. Disabled without `AXL_SECRET_KEY`.

## GitHub access

`AXL_GITHUB_TOKEN`: a fine-grained personal access token, read-only, scoped to the repos.
Repository permissions, all **Read-only**:

| Permission | Used for |
|---|---|
| Metadata | Required by GitHub for every fine-grained token |
| Pull requests | PR state, head SHA, merge commit, branch name, title |
| Checks | Check runs on the head SHA |
| Commit statuses | Legacy commit statuses on the head SHA |
| Contents | Releases, tags, commits, compare |
| Deployments | Deployments and their statuses |

**Credential seam.** Code reaches GitHub credentials only through:

```rust
#[async_trait]
pub trait GithubCredentials: Send + Sync {
    /// The token to use for one repository, or None when this provider has none for it.
    async fn token_for(&self, repo: &RepoRef) -> Result<Option<SecretToken>, CredentialError>;
}
```

v1 ships `StaticToken` (the env var, every repo). `AppInstallation` and `UserOAuth` are
future axle-saas providers, not designed here; they need only the per-repo lookup.
`SecretToken` wraps `secrecy::SecretString` (no `Display`, redacted `Debug`).

### The poller

A single task in `axl serve`, started only when some project has stages and a GitHub source.

- **Interval:** `AXL_GITHUB_POLL_SECS`, default **90**, at least 30.
- **What is in flight:** tasks with a linked PR whose furthest stage is before the project's
  last enabled stage, in any status but `done` and `cancelled` (so CI shows during review).
  Plus, per project, the PR list for the fallback match.
- **Per repo per cycle:** `GET /pulls?state=all&sort=updated&direction=desc&per_page=50`
  (state, merge SHA, branch, title for the fallback); `GET /pulls/{n}` only for linked PRs
  not on that page; check runs and combined status for each in-PR head SHA; releases or tags
  when a merged task waits for Released; deployments for the required environment when a task
  waits for Deployed.
- **Conditional requests:** every GET sends `If-None-Match` with the stored ETag. A `304`
  changes nothing and does not count against GitHub's rate limit.
- **Cap:** adapts to GitHub's budget: at most a tenth of the last `X-RateLimit-Remaining`,
  never above 100 (`AXL_GITHUB_MAX_REQUESTS_PER_CYCLE` lowers the ceiling). Work is ordered
  oldest waiting first, round-robin across repos; the rest rolls to the next cycle.
- **Backoff:** on `403`/`429` with `x-ratelimit-remaining: 0` or `retry-after`, pause until
  the reset time. On secondary rate limits or `5xx`, back off 1, 2, 4 … 15 minutes. On `401`,
  stop polling and mark the token rejected; retry every 15 minutes. On `404` for one repo,
  mark that project "no access" and skip it for 15 minutes.
- **API base:** `AXL_GITHUB_API_URL`, default `https://api.github.com` (the tests' stub);
  `https://` unless loopback. Every result becomes a signal row applied by the same service
  code as generic signals.

**Hook point.** The poller exposes `nudge(repo)`, which polls one repo now. A future
`POST /hooks/github` (GitHub webhooks) only verifies the delivery and calls `nudge`; the
poller stays the one place that reads GitHub and applies state. Not built in v1.

## Data model

Migrations `0006_delivery.sql` (SQLite and Postgres), after 0004 and 0005. If the order
changes, this renumbers to the next free number in both folders. Never edit an earlier
migration.

- `tasks`: status CHECK gains `delivering`. The SQLite rebuild (as 0002 did) must keep every
  status and column from 0004 (`done_at`, `acked_at`) and 0005 (`requested`, the request
  columns) and their indexes. New `delivery_stage` (`in_pr | merged | released | deployed`,
  null until `in_pr` is reached). Index on `(project_id, status, delivery_stage)`.
- `project_delivery`: one row per project. `stages` (JSON array, a subset in stage order;
  empty means off), `repo` (derived `owner/name`, nullable), `github_enabled` (default true
  for `github.com` keys), `release_source` (`github_releases | github_tags | generic |
  none`), `tag_pattern` (nullable prefix glob),
  `deploy_source` (`github_deployments | generic | none`), `required_environment` (default
  `production`), `updated_at`. No row means no stages: today's behaviour.
- `task_prs`: `task_id` (PK, one PR per task), `project_id`, `repo` (`owner/name`), `number`,
  `url`, `linked_by` (`agent | human | branch_match | title_match | complete_links`),
  `head_ref`, `base_ref`, `head_sha`, `title` (third-party text, capped 200), `state` (`open
  | closed | merged`), `merged_at`, `merge_commit_sha`, `ci_state` (`none | pending | passing |
  failing`), `ci_failing` (JSON, up to 5 check names), `checked_at`. Unique on
  `(repo, number)`. Re-links and unlinks are task history events with the previous value.
- `task_stages`: `(task_id, stage)` PK, `reached_at`, `implied` (bool), `signal_id`,
  `detail` (version, environment). Append-only per stage; the furthest is on `tasks`.
- `delivery_signals`: append-only log. `id, project_id, kind (pr | ci | merged | released |
  deployed), source (github | cli | webhook | human), version, sha, environment, url,
  delivery_id, outcome (applied | unmatched | duplicate | refused), matched (count),
  received_at`. Unique on `(project_id, delivery_id)` where set.
- `delivery_versions`: `project_id, version, commit_sha, url, released_at`, with the task ids
  of a generic release (JSON). Resolves deploys that name a version.
- `delivery_hooks`: `project_id` (PK), `hook_id` (unique), `secret_sealed`,
  `secret_fingerprint` (first 8 hex of SHA-256, for display), `created_at`, `rotated_at`.
- `github_cache`: `key` (method + path + query) PK, `etag`, `fetched_at`.
- `github_health`: `scope` (`token` or a project id), `state` (`ok | missing | rejected |
  rate_limited | no_access | unreachable`), `since`, `until`, `last_ok_at`.

**Webhook secret at rest.** HMAC needs the key itself, so a hash cannot verify a signature.
Webhooks keep HMAC; the secret is sealed (XChaCha20-Poly1305) under the new `AXL_SECRET_KEY`
(32 bytes, base64; documented in `configuration.md`), and only its fingerprint is shown
again. Without `AXL_SECRET_KEY` webhooks are disabled and the panel says why; `axl report`
with an agent token still works.

Both stores implement every new method with the same semantics; current time comes from
`src/clock.rs`, never `now()` in SQL.

## Events

- `TaskChanged` with new history kinds: `pr_linked`, `pr_relinked`, `pr_unlinked`,
  `ci_changed`, `stage_reached` (with the stage), `delivered` (reached done through stages).
  `/v1/events` and `/ui/live` already route `TaskChanged`; no new routing.
- `DeliveryHealthChanged { scope, state }`: UI only (the banner), not sent to sessions.
- `ci_changed` fires only when the state word changes. On `failing`, one system note per
  head SHA goes to the task's own thread ("CI failing on PR #12: lint, test (sqlite)"),
  reaching owner and creator and waking the owner agent.

## UI

Follows DESIGN.md: labelled text on every chip, no glyph or colour alone, density kept.

**Board.** Enabled stages appear as columns after Review and before Done, in stage order.
Filtered to one project: its enabled stages. All projects: the union, and a project's tasks
only ever sit in its own enabled columns. Column head: `IN PR · 4`. Empty stage columns stay
quiet with one sentence ("Nothing waiting for merge"). A new aspect for delivery stages, a
solid glyph with an outer ring ("block cleared, train still moving"), is added to the Key and
to DESIGN.md's Shape Carries State Rule, always beside the stage word.

**Card chips** (one meta row, wraps):

- `PR #123` (mono link to the PR) and, while in PR or earlier, `CI passing` (ok),
  `CI failing` (danger), `CI pending` (warn) or `no CI`; each links to the PR's checks page.
- `merged 2h` (link to the commit), `v1.4.2` (link to the release),
  `deployed production 20m` (link to the deploy signal's URL), `deployed staging` as a quiet
  extra on a card still waiting for production.
- `stalled · GitHub token rejected` (warn) when the source is unhealthy.

**Needs you.** New cards, only for `delivering` tasks: **CI failing**, **PR closed
unmerged** and **Merged into <branch>**, with task, project, PR link and age.

**Task page.** A **Delivery** panel above the history: the short id, then a timeline, one
row per stage: label, state word, time, and the link (PR, checks, merge commit, release,
deploy run). Implied stages read "implied by deploy". Actions: **Link PR** (URL field),
**Unlink PR**, **Advance stage** (guarded forward-only write, history entry, source
`human`), **Recheck now** (calls `nudge`), **Mark done**. All CSRF POSTs.

**Projects page.** Each row gains a **Delivery** panel, expanded in place like Wake-ups:

- Stage checkboxes: In PR, Merged, Released, Deployed. Help text: "Tasks with a PR move
  through these after the agent completes them. Done means the last one ticked."
- Release source (GitHub releases, GitHub tags, Generic only, None), tag prefix, and deploy
  source (GitHub deployments, Generic only, None); GitHub options are hidden for
  non-`github.com` keys.
- Required environment, default `production`.
- Webhook (only with `AXL_SECRET_KEY`): **Create signing secret** shows URL and secret once;
  then only the fingerprint, **Rotate**, **Delete**, and the last signal with its outcome.
- A health line: "GitHub: ok, checked 40s ago" or the failure in words.

**Banner.** When a source used by an enabled project is unhealthy, a warn bar under the top
bar: "GitHub token rejected since 10:02 — delivery stages are paused." Or "missing", "rate
limited until 10:40", "no access to acme/web-app". It clears on the next good poll.

## MCP, HTTP and CLI surface

- **Task object** gains `short_id` and `delivery` (null when the project has no stages):
  `{ stage, stages_enabled, pr: { url, number, state, ci } | null }`. `status` may be
  `delivering`.
- **`task_update`** gains `pr` (string URL). Errors: `pr must be a pull request in
  github.com/acme/web-app`, `PR #12 is already linked to task <id>`, the merged re-link error.
- **`task_complete`** may return `status: "delivering"` with `next_step`.
- **`task_claim`** returns `short_id` and `branch_hint: "axl-4deae8e3"`.
- **`task_list`** accepts `status: delivering`. Tool count is unchanged (18 with wake-up).
- **HTTP:** `POST /v1/delivery/report` (agent bearer token, same body as the webhook plus
  `project`); `POST /hooks/delivery/{hook_id}` (HMAC); `POST /hooks/github` reserved.
- **CLI:** `axl report released|deployed` as above.
- **Config:** `AXL_GITHUB_TOKEN`, `AXL_GITHUB_POLL_SECS`, `AXL_GITHUB_MAX_REQUESTS_PER_CYCLE`,
  `AXL_GITHUB_API_URL`, `AXL_SECRET_KEY`; client side `AXL_DELIVERY_URL`,
  `AXL_DELIVERY_SECRET`. Invalid values stop startup naming the variable.
- **Helm:** `github.tokenSecret` and `secretKeySecret` values mount the two secrets as env.

Docs to update in the same changes: `concepts.md` (status diagram, stages, the chain),
`mcp-tools.md` (task object, `task_update.pr`, `task_complete`, `task_claim`),
`http-api.md` (report, webhook, events kinds), `cli.md` (`axl report`), `configuration.md`
(the variables, including `AXL_SECRET_KEY`), `web-ui.md` (columns, chips, panel, settings, banner), `security.md` (token,
webhook), a new `docs/delivery.md` (setup per source, Pulumi/ArgoCD/Vercel examples),
`DESIGN.md` (aspect, chips), `src/install/instructions.md` and the MCP `INSTRUCTIONS`.

## Security

- **Token:** read once into `SecretToken`; never logged (errors mapped to our own words),
  returned or rendered. The UI shows only state and the expiry GitHub reports, warning 7
  days before. Requests go only to `AXL_GITHUB_API_URL`, for repos derived from project
  keys, never from reported URLs.
- **Reported PR URLs:** must parse as `https://github.com/<owner>/<repo>/pull/<n>` (optional
  trailing `/files` etc. dropped), `<owner>/<repo>` equal to the project's mapped repo (not
  the full key), case-insensitive, `n` a positive integer below 2^31, no userinfo or port.
  For non-GitHub keys a PR URL must be https on the key's host, stored as a link only.
- **Signal fields:** version at most 128 chars of `[A-Za-z0-9._+/-]`, environment at most 64
  of `[A-Za-z0-9._-]`, SHA 7 to 40 hex, URLs `https` only and at most 2 KB. Rendered as text;
  links get `rel="noopener noreferrer"`.
- **Third-party text** (PR titles, check names) is rendered as text and framed as peer data
  over MCP, like agent text.
- Agents cannot move stages through MCP. `axl report` with an agent token can, which is the
  same trust as any agent on the server; signals record their source.

## Failure handling

- **Token missing:** the GitHub source is off; settings and the banner say so. Generic
  signals still work. Linking still works (it is offline); the fallback match does not.
- **Token rejected, rate limited, GitHub down, no repo access:** stages stall with the
  reason on the banner and cards. Nothing is lost: the next good poll reads current state.
  A new token needs a restart; the 15-minute retry covers a GitHub-side fix.
- **Nothing breaks:** `task_complete`, `task_update`, claims, the board and the hooks never
  wait on GitHub. A poller panic is caught, logged and restarted.
- **Bad signals:** unmatched or refused signals are logged with their outcome and shown in
  the project's Delivery panel, so a mistyped environment is visible.

## Testing

- **Stub GitHub:** an in-test axum server behind `AXL_GITHUB_API_URL` with scripted
  responses: ETag and `304`, pagination, rate limit `403` with reset, `429` with
  `retry-after`, `401`, `404`, compare `ahead` / `behind` / `diverged` / truncated lists,
  deployments and statuses. It asserts the requests made (cap, conditional headers, no
  request for projects without stages) and that the token is sent only to it.
- **Store, both backends (`store_matrix!`):** the `task_complete` table; parent at the
  minimum child stage and `done` only when all non-cancelled children are; forward-only moves
  and Advance stage with a racing pair; implied and pre-completion stages; `done_at` only at
  the final stage; link uniqueness and re-link; shared-repo fallback; default-branch merges;
  stages off and on; signal dedupe; the 0006 rebuild keeps 0004/0005 columns and statuses.
- **Poller with the fake clock:** interval, backoff, adaptive cap from
  `X-RateLimit-Remaining`, round-robin across repos, health transitions.
- **MCP:** `task_update.pr` validation cases, `task_complete` `next_step`, task object
  shape, `short_id` from the UUID tail.
- **HTTP:** webhook signature, stale/future timestamp, replay, size, rate limit, disabled
  without `AXL_SECRET_KEY`; report with an agent token.
- **UI:** columns (per project, union, "waiting for" label), chips, Needs you cards,
  settings CSRF, secret shown once, banner, Link/Unlink/Advance/Mark done/Recheck.
- **CLI:** `axl report` signs correctly, `--soft`.
- **Logging:** logs captured through a 401 cycle never contain the token. `tests/helm.sh`
  covers the new secret values.

## PR split

1. **Stages and linking, no sources.** Migration, `delivering`, `project_delivery`, settings
   panel (stages only), `task_update.pr` with URL validation, `short_id`, the `task_complete`
   table and parent rule, board columns, task page panel, Link/Unlink/Mark done and
   **Advance stage**, instructions text. Stages move only by the human; useful with no
   GitHub source.
2. **GitHub PRs and CI.** Credentials seam, `StaticToken`, poller, ETags, backoff, health and
   banner, PR state, CI chips, Merged, fallback match, Needs you cards, stub server.
3. **Releases and deploys from GitHub.** Releases/tags, deployments, compare containment,
   `delivery_versions`, Released and Deployed, required environment.
4. **Generic signals.** `/v1/delivery/report`, `axl report`, signed webhook, sealed secrets,
   `AXL_SECRET_KEY`, `docs/delivery.md` with tool examples.

Each PR updates its user docs. 2 and 4 can run in parallel after 1.

## Open questions

None. All six were settled by the owner and are written above as decided.
