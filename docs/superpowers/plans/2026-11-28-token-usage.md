# Token Usage and Cost Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every Claude Code and Codex session reports how many tokens it used to the Axle server, which stores them per request, splits them across the tasks the session held, prices them at API rates, and shows totals per project, task, agent, harness, host, model and day on a new Usage page and on each task page.

**Architecture:** `axl hook` (Stop and SessionEnd) reads what the harness appended to its own transcript since a per-transcript cursor, turns usage lines into a fixed numeric struct through a versioned parser, and POSTs the batch to `POST /v1/usage`. The server validates each record, prices it from an embedded TOML table (overridable by `AXL_PRICES_FILE`), and in one transaction upserts it by `(harness, dedupe_key)` (counts merge upward) and writes allocation rows for the tasks the session held at the record's time, derived from `task_events`. The UI and `axl usage reprice` read and rewrite those rows through new `Store` methods implemented in both backends.

**Tech Stack:** Rust 2024, axum 0.8, askama 0.16, htmx, sqlx 0.9 (SQLite + Postgres), tokio, reqwest 0.13, toml 0.9, sha2.

**Spec:** `docs/superpowers/specs/2026-11-28-token-usage-design.md` (owner-approved, binding). This plan covers the spec's PR split items 1 to 5 in one PR.

## Global Constraints

Controller rulings first; they override the spec text.

- Migration is **`0008_token_usage.sql`** in both `migrations/sqlite/` and `migrations/postgres/` (main has 0001-0005; 0006 and 0007 are reserved by unmerged branches). Never edit an existing migration.
- The allocation key uses COALESCE, not IFNULL: SQLite `CREATE UNIQUE INDEX usage_alloc_key ON usage_allocations (usage_id, COALESCE(task_id, ''))`; Postgres `... (usage_id, COALESCE(task_id::text, ''))`.
- Privacy: parsers copy only named numeric usage fields, model ids, opaque ids and timestamps into a fixed struct. Never store or send prompt or response text, tool input, paths, `cwd`, git data, email or organisation. Credential files are read for one key or mode field, never logged. Never print a token.
- Test fixtures are synthetic files under `tests/fixtures/usage/<harness>/<version>/` with every free-text string `"…"`, enforced by a test. Tests never read the real `~/.claude`, `~/.claude.json`, `~/.codex` or `~/.axl`: inject the home directory (temp dir) and the environment (`env_clear()` for the binary, a lookup closure for library calls).
- Tests and any local server run never use port 7420: bind `127.0.0.1:0`; a dead server is `http://127.0.0.1:9`.
- Postgres is tested with `AXL_TEST_PG_URL=postgres://postgres:x@localhost:55439/postgres cargo test --features pg-tests`.
- Every task ends green on `cargo fmt --check`, `cargo clippy --all-targets --features pg-tests -- -D warnings` and `cargo test`, and with a conventional commit with no AI attribution and no `Co-Authored-By` trailer. Never commit `.superpowers/`.

From the spec (values verbatim):

- Owner decision 2: "every session shows an **API-equivalent cost**, labelled an estimate on a subscription"; an unknown model shows tokens and "no price".
- Owner decision 3: "a session holding several tasks splits its usage evenly across them. Usage while holding no task is "no task" for that session's project, harness and host."
- `POST /v1/usage`: bearer token; unknown session 404, another token's session 403; answers `{"accepted": 1, "duplicate": 0, "rejected": []}` in one transaction; envelope errors (bad JSON, no session, over **500** records, body over **256 KB**) are 400 and store nothing; a record is rejected (`{key, reason}`) when the model is over **128** bytes or outside `[A-Za-z0-9._:/@-]`, the key is over **200** bytes, a count is below 0 or over **10⁹**, or `at` is more than **5 minutes** ahead or **30 days** old. One request against the token's rate limit; renews no lease, touches no heartbeat, publishes no event.
- Prices: USD per million tokens; `cache_write_1h` defaults to `cache_write_5m`; cache prices default to `input`; `long_context.above` compares input plus cache; match exact `id`, then the longest `match` glob, else NULL cost; `cost_micros = round(Σ tokens_k × price_k)`; `price_ref` like `builtin:2026-11-28#claude-opus-5-5`; built-in `version = "2026-11-28"`. A bad `AXL_PRICES_FILE` stops the server at start, naming file and line.
- Collector: cursor `~/.axl/sessions/usage/<sha256 of transcript path>.json`, mode 0600, written atomically, never holding the token; read to the last complete newline; advance only after a 2xx; keep on timeout, 429, 5xx or send failure; advance with one stderr line on 403, 404 or a 400 envelope error; a shrunk file or new inode restarts at 0; a new cursor drops records older than the session file's mtime; per run at most **8 MB** per file, **500** records, **300 ms** of parsing; cursors untouched for **30 days** are deleted on SessionEnd.
- Hook wiring: Claude Code `SessionEnd` handler `axl hook session-end` with `timeout: 3`; Codex `SessionEnd` with `timeout = 3`. Stop runs the collector concurrently with the inbox fetch inside the existing 2.5 s budget and 4 s ceiling; SessionEnd runs the collector only, **1 s**. Usage is collected before the `stop_hook_active` loop guard. Strictly fail-open: exit 0, at most one stderr line per run, mail first.
- Billing: `[usage.billing]` in `~/.axl/config.toml` always wins; then Claude Code `api` if the env has `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX` or `CLAUDE_CODE_USE_FOUNDRY`, or settings carry `apiKeyHelper`, else `subscription` if `~/.claude.json` has an `oauthAccount` object; Codex `auth_mode` in `~/.codex/auth.json` (`chatgpt` = subscription, `apikey` = api; env vars ignored); otherwise `unknown`, shown like a subscription ("est.").
- Off switch: `AXL_USAGE=off` or `[usage] enabled = false`.
- UI copy: tab **Usage**; cost header "API-equivalent cost"; `est.` after an amount for a row partly or wholly on subscription or unknown billing; the sentence "Estimated at API prices; subscription use is not billed per token."; "no task" and "no price" spelled out; task line `Usage 1.24M tokens · $3.40 API-equivalent est. · 3 sessions` linking to `/ui/usage?task=<id>&group=agent`, a parent adds `incl. subtasks`, no usage no line. Board cards and overview unchanged. No new MCP tool or field. No live updates on the Usage page.
- UI rules (DESIGN.md): share bars in ink beside their percentage, never a signal colour (The Signal Is State Rule); projects carry their line swatch (The Line Is Identity Rule); tabular numerals; no inline `style` attributes (CSP `style-src 'self'`); no chart library.
- Out of scope: OpenCode (spec PR 6), the OTLP receiver (approach B), screenshots, and the 90-day retention roll-up (`AXL_USAGE_RAW_DAYS`), which is a follow-up (see the end of this plan).

## Review Focus

1. **A Codex rollout that has both `token_usage_record` and `token_count` lines** must count each response once; the fallback keys (`<session_meta.id>:<ordinal>`) differ from `response_id`, so the server cannot dedupe them. Test in Task 7 (`codex_mixed_rollout_counts_once`).
2. **Claude Code writes `"model": "<synthetic>"` assistant lines with zero usage** (API errors, interrupted turns), and model ids may carry a `[1m]`-style suffix. A synthetic or all-zero line is skipped, and a bracketed suffix is stripped, so the server never rejects a whole session's records for a charset it cannot pass. Test in Task 6.
3. **A resumed, months-old transcript** sends records older than 30 days: the server rejects them per record, the collector still advances (a 2xx), and nothing is resent forever. Tests in Task 4 (`old_and_future_records_rejected`) and Task 9 (`rejected_records_still_advance`).
4. **A single transcript line longer than the 8 MB cap** (a tool result with a pasted image) must not stall the cursor forever. Test in Task 9 (`line_longer_than_cap_is_skipped`).
5. **Usage page query strings a person edits by hand**: a garbage `task=`, `agent=` or date, or `from` after `to`, gives a page with a message or an empty table, never a 500. Test in Task 13 (`bad_filters_never_500`).

---

## File Structure

| File | Responsibility |
|---|---|
| `migrations/{sqlite,postgres}/0008_token_usage.sql` | `usage_records`, `usage_allocations`, indexes |
| `src/domain.rs` | `UsageId`, `Billing` |
| `src/store/mod.rs` | usage types, `Pricer` trait, new `Store` methods |
| `src/store/sqlite.rs`, `src/store/postgres.rs` | the same in SQL (upsert, holdings, summary, reprice) |
| `src/usage/mod.rs` (new) | module root |
| `src/usage/prices.rs`, `src/usage/prices.toml` (new) | price table, override merge, `Pricer` impl |
| `src/usage/report.rs` (new) | wire types and limits shared by server and collector |
| `src/service/usage.rs` (new) | ingest validation, summary, reprice |
| `src/http/v1.rs`, `src/http/mod.rs` | `POST /v1/usage`, `AppState.usage` |
| `src/config.rs` | `ServerConfig.prices_file` (`AXL_PRICES_FILE`) |
| `src/main.rs` | price loading in `serve`, `axl usage reprice`, hook env, `session-end` |
| `src/usage/parsers/{mod,claude_code,codex}.rs` (new) | `claude-code/1`, `codex/1` |
| `src/usage/cursor.rs`, `src/usage/billing.rs` (new) | cursor and status files, billing detection |
| `src/usage/collect.rs` (new) | read, limit, POST, advance |
| `src/client/config.rs`, `src/cli/login.rs` | `[usage]` config table, kept across `axl login` |
| `src/hook/mod.rs` | `Event::SessionEnd`, Stop concurrency, env lookup |
| `src/install/claude_code.rs`, `src/install/codex.rs` | SessionEnd handlers |
| `src/cli/status.rs` | usage lines |
| `src/ui/usage.rs` (new), `templates/usage.html` (new), `templates/base.html`, `src/ui/mod.rs`, `static/app.css`, `static/app.js` | Usage page and tab |
| `src/ui/task_page.rs`, `templates/task.html` | task usage line |
| `tests/store_usage.rs`, `tests/usage_prices.rs`, `tests/http_usage.rs`, `tests/usage_reprice.rs`, `tests/usage_parsers.rs`, `tests/usage_local.rs`, `tests/usage_collect.rs`, `tests/ui_usage.rs` (new); `tests/hook.rs`, `tests/install_claude_code.rs`, `tests/install_codex.rs`, `tests/cli_onboarding.rs`, `tests/cli_login.rs` | tests |
| `tests/fixtures/usage/**` (new); `tests/fixtures/harness/claude-code/**/after*` | fixtures |
| `docs/http-api.md`, `docs/configuration.md`, `docs/security.md`, `docs/cli.md`, `docs/web-ui.md`, `docs/harnesses/claude-code.md`, `docs/harnesses/codex.md`, `docs/releasing.md`, `docs/architecture.md` | docs |

**Kinds.** Tasks marked **judgment** are parser- or attribution-heavy and deserve the most careful review: 1, 6, 7, 9, 10, 13. The rest are **mechanical**: 2, 3, 4, 5, 8, 11, 12, 14, 15.

---

### Task 1: Store — migration, upsert with merge-upward, allocation by holdings (judgment)

**Files:**
- Create: `migrations/sqlite/0008_token_usage.sql`, `migrations/postgres/0008_token_usage.sql`
- Modify: `src/domain.rs` (`id_type!` list ~line 56; new `Billing` after `EnrollmentStatus` ~240), `src/store/mod.rs` (new `// ---- usage ----` section in `trait Store`, types after `MessageReceipt`), `src/store/sqlite.rs`, `src/store/postgres.rs`
- Test: `tests/store_usage.rs` (new; `#[macro_use] mod common;`, an `Env` like `tests/store_tasks.rs` with `TaskService` + `IdentityService::resolve`)

**Interfaces:**
- Produces (domain): `UsageId` (via `id_type!`); `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)] #[serde(rename_all = "snake_case")] pub enum Billing { Subscription, Api, Unknown }` with `text_enum!` (`"subscription"`, `"api"`, `"unknown"`).
- Produces (store/mod.rs):

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageCounts { pub input: i64, pub output: i64, pub cache_write_5m: i64,
    pub cache_write_1h: i64, pub cache_read: i64, pub reasoning: i64 }
#[derive(Debug, Clone)]
pub struct NewUsage { pub key: String, pub model: String, pub counts: UsageCounts,
    pub speed: Option<String>, pub billing: Billing, pub occurred_at: OffsetDateTime }
#[derive(Debug, Clone)]
pub struct UsageBatch<'a> { pub session: SessionId, pub agent: &'a Agent, pub token: TokenId,
    pub parser: &'a str, pub harness_session_id: Option<&'a str>,
    pub records: &'a [NewUsage], pub now: OffsetDateTime }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Priced { pub cost_micros: i64, pub price_ref: String }
/// Prices a record's counts; the store calls it inside the write so a merged row is priced
/// from its merged counts.
pub trait Pricer: Send + Sync {
    fn price(&self, model: &str, speed: Option<&str>, counts: &UsageCounts) -> Option<Priced>;
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageWritten { pub inserted: u32, pub duplicate: u32 }
#[derive(Debug, Clone, PartialEq)]
pub struct StoredUsage { pub id: UsageId, pub harness: String, pub session: SessionId,
    pub agent: AgentId, pub project: Option<ProjectId>, pub model: String,
    pub counts: UsageCounts, pub speed: Option<String>, pub billing: Billing,
    pub cost_micros: Option<i64>, pub price_ref: Option<String>, pub parser: String,
    pub harness_session_id: Option<String>, pub occurred_at: OffsetDateTime,
    pub received_at: OffsetDateTime,
    /// `(task, parts)`, `None` = "no task"; ordered by task id text, `None` first.
    pub allocations: Vec<(Option<TaskId>, i64)> }

// trait Store
/// In one transaction, for each record: upserts by `(agent.harness, key)`. A new row takes
/// the batch's session/agent/project/token, `pricer`'s price and one allocation row per task
/// `session` held at `occurred_at` (`parts` = how many), or one `(NULL, 1)` row. An existing
/// row keeps its session, model, billing, times and allocations, takes the larger value of
/// each count, and is re-priced from the merged counts.
async fn insert_usage(&self, batch: &UsageBatch<'_>, pricer: &dyn Pricer) -> StoreResult<UsageWritten>;
async fn get_usage(&self, harness: &str, key: &str) -> StoreResult<Option<StoredUsage>>;
```

- [ ] **Step 1: Write the failing tests** in `tests/store_usage.rs`

Test helpers: `struct NoPrice;` (returns `None`) and `struct PerToken;` (returns `Some(Priced { cost_micros: input+output+cache_write_5m+cache_write_1h+cache_read, price_ref: "test#per-token".into() })`), both `impl axle::store::Pricer`. `fn rec(key: &str, at: OffsetDateTime, input: i64) -> NewUsage` (model `"claude-opus-5-5"`, `output: 10`, other counts 0, `Billing::Subscription`). `async fn report(env, r: &Resolved, recs: &[NewUsage], pricer) -> UsageWritten` builds a `UsageBatch { session: r.session.id, agent: &r.agent, token: r.session.token_id, parser: "claude-code/1", harness_session_id: Some("hs-1"), records, now: env.clock.now() }`.

```rust
store_matrix!(usage_insert_and_read_back, |store: Arc<dyn Store>| async move {
    let env = Env::new(store);
    let a = env.session("mac", "claude-code").await;
    let w = report(&env, &a, &[rec("req_1", T0, 12)], &PerToken).await;
    assert_eq!(w, UsageWritten { inserted: 1, duplicate: 0 });
    let u = env.store.get_usage("claude-code", "req_1").await.unwrap().unwrap();
    assert_eq!((u.session, u.agent, u.project), (a.session.id, a.agent.id, a.agent.project_id));
    assert_eq!(u.counts.input, 12);
    assert_eq!(u.cost_micros, Some(22));
    assert_eq!(u.price_ref.as_deref(), Some("test#per-token"));
    assert_eq!(u.allocations, vec![(None, 1)]);
    assert!(env.store.get_usage("codex", "req_1").await.unwrap().is_none(), "keyed by harness");
});

store_matrix!(usage_duplicate_is_noop, |store| async move {
    // same record twice: second call -> UsageWritten { inserted: 0, duplicate: 1 };
    // get_usage unchanged (counts, cost, allocations, received_at)
});

store_matrix!(usage_merges_upward_and_reprices, |store| async move {
    // first: input 12, output 10. second (same key, from ANOTHER session of another
    // claude-code agent, so the same harness):
    // input 5, output 900, cache_read 7.
    // expect counts {input: 12, output: 900, cache_read: 7}, session/agent still the first's,
    // cost_micros == Some(12 + 900 + 7), allocations still the first's.
});

store_matrix!(usage_allocation_zero_one_three_tasks, |store| async move {
    // session holds 0 tasks at T0 -> [(None, 1)]
    // claims t1 at T0+1m, record at T0+2m -> [(Some(t1), 1)]
    // claims t2, t3 at T0+3m, record at T0+4m -> three rows, each parts == 3
});

store_matrix!(usage_holdings_at_occurred_at, |store| async move {
    // claim t1 at T0+1m; complete t1 at T0+5m; record with occurred_at T0+4m sent at T0+6m
    //   -> [(Some(t1), 1)]   (the turn that finishes a task)
    // record with occurred_at T0+6m -> [(None, 1)]
    // claim t2 at T0+7m, release at T0+8m: record at T0+7m30s -> t2; at T0+8m -> no task
    // claim t3 at T0+9m; mark it blocked at T0+10m (still held): record at T0+11m -> t3
});

store_matrix!(usage_holdings_end_on_takeover, |store| async move {
    // a (session S1) claims t at T0 with a 15 min lease; at T0+20m b (another agent) takes
    // it over (lease_expired_takeover). S1 record at T0+10m -> t; S1 record at T0+21m -> no task;
    // S1 record at T0+16m (lease expired, nobody re-claimed yet) -> t (expired still counts).
});
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test store_usage`
Expected: compile error (`UsageBatch`, `insert_usage` not found).

- [ ] **Step 3: Write the migrations**

SQLite `0008_token_usage.sql`: the spec's DDL verbatim (TEXT ids and timestamps, INTEGER counts, the `CHECK` on `billing`, the four indexes), except the allocation key uses `COALESCE(task_id, '')`. Postgres: `uuid` for every id column, `timestamptz` for `occurred_at` / `received_at`, `bigint` for counts, `parts` and `cost_micros`, and `COALESCE(task_id::text, '')`. Head each file with a comment naming the spec and "Kept in step with" the other flavour, like 0001.

- [ ] **Step 4: Add the domain types, the store types and the trait methods**

Implement in both backends, inside one write transaction (`BEGIN_WRITE` on SQLite, `pool.begin()` on Postgres), per record:

1. `let id = UsageId::new();` price the incoming counts with `pricer`.
2. Upsert, returning the row's id and counts:
   ```sql
   INSERT INTO usage_records (id, harness, dedupe_key, session_id, agent_id, project_id, token_id,
       model, input_tokens, output_tokens, cache_write_5m_tokens, cache_write_1h_tokens,
       cache_read_tokens, reasoning_tokens, speed, billing, cost_micros, price_ref, parser,
       harness_session_id, occurred_at, received_at)
   VALUES (...)
   ON CONFLICT (harness, dedupe_key) DO UPDATE SET
       input_tokens = MAX(usage_records.input_tokens, excluded.input_tokens), -- Postgres: GREATEST
       ... the same for the other five counts ...
   RETURNING id, model, speed, input_tokens, output_tokens, cache_write_5m_tokens,
             cache_write_1h_tokens, cache_read_tokens, reasoning_tokens
   ```
3. Returned id == `id`: inserted. Run the holdings query and insert the allocation rows.
   Otherwise: duplicate. Price the returned (merged) counts and `UPDATE usage_records SET cost_micros = ?, price_ref = ? WHERE id = ?`.

Holdings at `t` for session `s` (`SELECT DISTINCT`: a session that re-claimed a task it still held has two qualifying claims):

```sql
SELECT DISTINCT e.task_id FROM task_events e
WHERE e.kind = 'claimed' AND e.actor_session_id = :s AND e.created_at <= :t
  AND NOT EXISTS (SELECT 1 FROM task_events x
    WHERE x.task_id = e.task_id AND x.created_at > e.created_at AND x.created_at <= :t
      AND (x.kind IN ('released', 'done', 'cancelled', 'force_released')
           OR (x.kind IN ('claimed', 'lease_expired_takeover')
               AND x.actor_session_id IS NOT :s)))   -- Postgres: IS DISTINCT FROM $1
```

`get_usage` reads the row and its allocations (`ORDER BY COALESCE(task_id, '')`; Postgres `COALESCE(task_id::text, '')`).

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --test store_usage && AXL_TEST_PG_URL=postgres://postgres:x@localhost:55439/postgres cargo test --features pg-tests --test store_usage`
Expected: PASS, both backends.

- [ ] **Step 6: Run the full gate**

Run: `cargo fmt --check && cargo clippy --all-targets --features pg-tests -- -D warnings && cargo test`
Expected: clean, all tests pass.

- [ ] **Step 7: Commit**

```bash
git add migrations/sqlite/0008_token_usage.sql migrations/postgres/0008_token_usage.sql src/domain.rs src/store/mod.rs src/store/sqlite.rs src/store/postgres.rs tests/store_usage.rs
git commit -m "feat(store): record token usage and split it across held tasks"
```

---

### Task 2: Store — summary by dimension, reprice reads and writes (mechanical)

**Files:**
- Modify: `src/store/mod.rs`, `src/store/sqlite.rs`, `src/store/postgres.rs`
- Test: `tests/store_usage.rs`

**Interfaces:**
- Consumes: Task 1's types.
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskScope { Task { id: TaskId, subtasks: bool }, NoTask }
#[derive(Debug, Clone, Default)]
pub struct UsageFilter { /// `occurred_at >= from`
    pub from: Option<OffsetDateTime>, /// `occurred_at < to`
    pub to: Option<OffsetDateTime>, pub project: Option<ProjectId>, pub agent: Option<AgentId>,
    pub harness: Option<String>, pub host: Option<String>, pub task: Option<TaskScope>,
    pub model: Option<String>, pub billing: Option<Billing> }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageGroup { Total, Project, Task, Agent, Harness, Host, Model, Day, Hour }
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageRow {
    /// Project/agent/task id text, harness, host, model, `YYYY-MM-DD` (Day) or
    /// `YYYY-MM-DDTHH` (Hour), all UTC; `None`: no project / no task; always `None` for Total.
    pub key: Option<String>,
    /// Distinct records touching the group (not split by parts).
    pub requests: i64, pub sessions: i64,
    /// Token and cost sums divided by each allocation's `parts`.
    pub input: f64, pub output: f64, pub cache_write: f64, pub cache_read: f64,
    pub cost_micros: f64,
    /// Distinct records with a NULL cost; distinct records with billing other than `api`.
    pub unpriced: i64, pub estimated: i64,
    /// The cost of the non-`api` records.
    pub estimated_cost_micros: f64 }
#[derive(Debug, Clone, PartialEq)]
pub struct RepriceRow { pub id: UsageId, pub model: String, pub speed: Option<String>, pub counts: UsageCounts }

// trait Store
/// One row per group (one row for `Total`, even when nothing matches), unordered.
async fn usage_summary(&self, filter: &UsageFilter, group: UsageGroup) -> StoreResult<Vec<UsageRow>>;
/// Up to `limit` rows in id order after `after`, `occurred_at >= since` when set, only
/// NULL-cost rows when `unpriced_only`.
async fn usage_to_reprice(&self, since: Option<OffsetDateTime>, unpriced_only: bool,
    after: Option<UsageId>, limit: u32) -> StoreResult<Vec<RepriceRow>>;
/// Sets `cost_micros` / `price_ref` (`None`: both NULL) for each id, in one transaction.
async fn set_usage_costs(&self, costs: &[(UsageId, Option<Priced>)]) -> StoreResult<()>;
```

- [ ] **Step 1: Write the failing tests**

```rust
store_matrix!(usage_every_dimension_adds_up, |store| async move {
    // two agents (mac/claude-code in PROJECT, linux/codex in OTHER), one session holding
    // t1+t2, five records over two days, one priced NULL (NoPrice batch), one Billing::Api.
    let total = &env.store.usage_summary(&UsageFilter::default(), UsageGroup::Total).await.unwrap()[0];
    for g in [Project, Task, Agent, Harness, Host, Model, Day, Hour] {
        let rows = env.store.usage_summary(&UsageFilter::default(), g).await.unwrap();
        let sum = |f: fn(&UsageRow) -> f64| rows.iter().map(f).sum::<f64>();
        assert!((sum(|r| r.input) - total.input).abs() < 1e-6, "{g:?}");
        assert!((sum(|r| r.cost_micros) - total.cost_micros).abs() < 1e-6, "{g:?}");
    }
    assert_eq!(total.requests, 5);
    assert_eq!(total.unpriced, 1);
    assert_eq!(total.estimated, 4);
});

store_matrix!(usage_filters, |store| async move {
    // harness = "codex" only the codex rows; host; agent; project; model; billing = Api;
    // from/to bounds (from inclusive, to exclusive); task = NoTask -> only (None) allocations;
    // task = Task { id: parent, subtasks: true } includes a child task's usage, subtasks:false not.
    // sessions counts distinct sessions.
});

store_matrix!(usage_reprice_pages, |store| async move {
    // three rows, one NULL cost; usage_to_reprice(None, true, None, 10) -> just the NULL one;
    // (None, false, None, 2) then after = last id -> the third; since filter.
    // set_usage_costs([(id, Some(Priced{..})), (id2, None)]) then get_usage shows them.
});
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test store_usage usage_`
Expected: compile error (`usage_summary` not found).

- [ ] **Step 3: Implement**

One query shape for every group: `usage_records r JOIN usage_allocations a ON a.usage_id = r.id JOIN agents g ON g.id = r.agent_id`, each filter an optional `AND` bound with a `(? IS NULL OR col = ?)` pair as `list_tasks` does, sums as `SUM(r.input_tokens * 1.0 / a.parts)`, `COUNT(DISTINCT r.id)`, `COUNT(DISTINCT CASE WHEN r.cost_micros IS NULL THEN r.id END)`. Task scope with subtasks uses `WITH RECURSIVE tree(id) AS (SELECT :task UNION ALL SELECT t.id FROM tasks t JOIN tree ON t.parent_id = tree.id)` and `a.task_id IN (SELECT id FROM tree)`. Group key expressions: SQLite `substr(r.occurred_at, 1, 10)` / `substr(r.occurred_at, 1, 13)`; Postgres `to_char(r.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD')` / `'YYYY-MM-DD"T"HH24'`. Pick the group SQL from a `match` over `UsageGroup` returning `&'static str` (no string-built SQL from input).

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --test store_usage && AXL_TEST_PG_URL=postgres://postgres:x@localhost:55439/postgres cargo test --features pg-tests --test store_usage`
Expected: PASS.

- [ ] **Step 5: Full gate, then commit**

Run: `cargo fmt --check && cargo clippy --all-targets --features pg-tests -- -D warnings && cargo test`

```bash
git add src/store/mod.rs src/store/sqlite.rs src/store/postgres.rs tests/store_usage.rs
git commit -m "feat(store): sum token usage by project, task, agent, harness, host, model and day"
```

---

### Task 3: Price table with `AXL_PRICES_FILE` override (mechanical)

**Files:**
- Create: `src/usage/mod.rs` (`pub mod prices;`), `src/usage/prices.rs`, `src/usage/prices.toml`
- Modify: `src/lib.rs` (`pub mod usage;`)
- Test: `tests/usage_prices.rs` (new)

**Interfaces:**
- Consumes: `axle::store::{Pricer, Priced, UsageCounts}`.
- Produces:

```rust
pub const BUILTIN: &str = include_str!("prices.toml");
#[derive(Debug, thiserror::Error)]
pub enum PriceError {
    #[error("{file}: line {line}: {message}")] Bad { file: String, line: usize, message: String },
    #[error("cannot read {file}: {source}")] Io { file: String, source: std::io::Error },
}
#[derive(Debug, Clone)] pub struct PriceTable { /* built-in version, entries with their source */ }
impl PriceTable {
    pub fn builtin() -> PriceTable;                        // parses BUILTIN; a bad file is a bug (panics, tested)
    pub fn parse(text: &str, file: &str) -> Result<PriceTable, PriceError>;
    pub fn with_override(self, text: &str, file: &str) -> Result<PriceTable, PriceError>;
    /// builtin, then the file at `path` merged over it.
    pub fn load(path: Option<&std::path::Path>) -> Result<PriceTable, PriceError>;
    pub fn version(&self) -> &str;
    pub fn len(&self) -> usize;
}
impl axle::store::Pricer for PriceTable { /* see rules */ }
```

File format (the spec's): top-level `version`, `[[model]]` entries with `id`, optional `match` (globs, `*` only), `input`, `output`, optional `cache_write_5m`, `cache_write_1h`, `cache_read`, optional `fast = { … }` and `long_context = { above = N, … }` tiers that may set any of the five prices. Unknown keys are errors (`deny_unknown_fields`). Use `toml::Spanned<String>` for `id` so semantic errors (negative price, duplicate id, empty `match` glob) report the entry's line; syntax errors report the line from the `toml` error's span.

Pricing rules (planner decisions where the spec is silent):

- Lookup: exact `id`; else the entry whose matching glob is longest (most characters); ties go to the entry listed first; none: `None` (no price).
- Tier: start from the base prices; if `long_context` is set and `input + cache_write_5m + cache_write_1h + cache_read > above`, overlay the fields it sets; then if `speed == Some("fast")` and `fast` is set, overlay its fields (fast wins). Then apply defaults on the resolved tier: `cache_write_5m ← input`, `cache_write_1h ← cache_write_5m`, `cache_read ← input`.
- `cost_micros = (input×p.input + output×p.output + cw5m×p.cw5m + cw1h×p.cw1h + cache_read×p.cache_read).round() as i64`; `reasoning` is never priced (it is inside `output`).
- `price_ref`: `builtin:<builtin version>#<id>` for a built-in entry, `file:<override version>#<id>` for one from the override.
- Override merge: an override entry replaces the built-in entry with the same `id` entirely; new ids are appended.

- [ ] **Step 1: Write the failing tests** in `tests/usage_prices.rs`

```rust
fn c(input: i64, output: i64, cw5m: i64, cw1h: i64, cr: i64) -> UsageCounts { /* reasoning 0 */ }

#[test] fn builtin_parses_and_prices_the_spec_example() {
    let t = PriceTable::builtin();
    assert_eq!(t.version(), "2026-11-28");
    let p = t.price("claude-opus-5-5", None, &c(12, 845, 0, 3120, 48211)).unwrap();
    assert_eq!(p, Priced { cost_micros: 51550, price_ref: "builtin:2026-11-28#claude-opus-5-5".into() });
    // 12×4 + 845×20 + 3120×8 + 48211×0.2 = 51550.2
}
#[test] fn fast_tier() { /* same counts, speed Some("fast"): 12×8 + 845×40 + 24960 + 9642.2 → 68498 */ }
#[test] fn glob_match_and_precedence() {
    // the spec's glob: "claude-opus-5-5-20261128" -> claude-opus-5-5; "anthropic/claude-opus-5-5" too
    // override with id "x" match ["claude-*"] and id "y" match ["claude-opus-*"]:
    // "claude-opus-9" -> y (longer glob); exact id beats any glob
}
#[test] fn defaults_and_1h_fallback() {
    // override entry id "d": input 1, output 2, cache_write_5m 3 (no 1h, no cache_read):
    // c(1,1,1,1,1) -> 1 + 2 + 3 + 3 + 1 = 10
}
#[test] fn long_context_above_compares_whole_prompt() {
    // id "h": input 1, output 0, long_context = { above = 100000, input = 2 }
    // c(50_000, 0, 0, 0, 60_000) -> 220_000 (long: input 2, cache_read defaults to 2)
    // c(40_000, 0, 0, 0, 60_000) -> 100_000 (exactly `above`: base tier)
}
#[test] fn unknown_model_has_no_price() { assert!(PriceTable::builtin().price("nope-1", None, &c(1,1,0,0,0)).is_none()); }
#[test] fn override_replaces_by_id_and_adds() {
    // override version "2026-12-01" redefines claude-opus-5-5 input 1, output 1 and adds "new-1"
    // claude-opus-5-5 now priced from the override, price_ref "file:2026-12-01#claude-opus-5-5"
    // the other built-in entries unchanged (len == builtin len + 1)
}
#[test] fn bad_override_names_file_and_line() {
    // "version = \"v\"\n\n[[model]]\nid = \"a\"\ninput = \"x\"\noutput = 1\n" -> Err; to_string
    // contains "/etc/axl/prices.toml" and "line 5"; a duplicate id on line 9 -> "line 9";
    // an unknown key ("inptu") -> Err
}
#[test] fn serve_refuses_a_bad_prices_file() {
    // run env!("CARGO_BIN_EXE_axl") serve with env_clear(), HOME=<temp>, DATABASE_URL=sqlite://<temp>/a.db,
    // AXL_BIND=127.0.0.1:0, AXL_ADMIN_SECRET=..., AXL_PRICES_FILE=<temp>/bad.toml:
    // exits non-zero within 10 s; stderr names bad.toml and its line. (Added in Task 4, which wires serve.)
}
```

Leave `serve_refuses_a_bad_prices_file` out of this task (it lands with Task 4).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test usage_prices`
Expected: compile error (`axle::usage` not found).

- [ ] **Step 3: Write `src/usage/prices.toml`**

`version = "2026-11-28"`, a header comment listing the sources (the Anthropic pricing page `https://platform.claude.com/docs/en/about-claude/pricing` and the OpenAI API pricing page) and the date read. The `claude-opus-5-5` entry is the spec's block verbatim. Add the other current Anthropic and OpenAI models with prices copied from those pages on the day you build (use the `claude-api` skill or the page itself; never prices from memory), each with a `# source:` comment. OpenAI entries carry `input`, `cache_read` and `output` only. The model id used by the Codex fixtures (Task 7) must have an entry.

- [ ] **Step 4: Implement `src/usage/prices.rs`**

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --test usage_prices`
Expected: PASS.

- [ ] **Step 6: Full gate, then commit**

```bash
git add src/lib.rs src/usage/mod.rs src/usage/prices.rs src/usage/prices.toml tests/usage_prices.rs
git commit -m "feat(usage): built-in API price table with an override file"
```

---

### Task 4: `POST /v1/usage`, `AXL_PRICES_FILE` at start, docs (mechanical)

**Files:**
- Create: `src/usage/report.rs`, `src/service/usage.rs`
- Modify: `src/usage/mod.rs` (`pub mod report;`), `src/service/mod.rs` (`pub mod usage;`), `src/http/mod.rs` (`AppState.usage`, `AppState::with_prices`), `src/http/v1.rs` (route + handler), `src/config.rs` (`ServerConfig.prices_file`), `src/main.rs` (`serve` loads prices)
- Modify docs: `docs/http-api.md` (new `## POST /v1/usage` after `GET /v1/leases`), `docs/configuration.md` (`AXL_PRICES_FILE` in the server table and a "Prices" section with the file format)
- Test: `tests/http_usage.rs` (new; the `App` helper from `tests/http_v1.rs`), `tests/usage_prices.rs` (`serve_refuses_a_bad_prices_file`)

**Interfaces:**
- Consumes: `Store::insert_usage`, `PriceTable`, `IdentityService::load`, `v1::session_for`.
- Produces (`src/usage/report.rs`):

```rust
pub const MAX_RECORDS: usize = 500;
pub const MAX_BODY_BYTES: usize = 256 * 1024;
pub const MAX_KEY_BYTES: usize = 200;
pub const MAX_MODEL_BYTES: usize = 128;
pub const MAX_COUNT: i64 = 1_000_000_000;
pub const MAX_FUTURE: time::Duration = time::Duration::minutes(5);
pub const MAX_AGE: time::Duration = time::Duration::days(30);
#[derive(Debug, Serialize, Deserialize)]
pub struct UsageReport { pub session_id: SessionId, pub parser: String,
    #[serde(default)] pub harness_version: Option<String>,
    #[serde(default)] pub harness_session_id: Option<String>,
    /// Decoded one by one, so a malformed record is rejected alone.
    pub records: Vec<serde_json::Value> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireRecord { pub key: String, /** RFC 3339 */ pub at: String, pub model: String,
    pub input: i64, pub output: i64, pub cache_write_5m: i64, pub cache_write_1h: i64,
    pub cache_read: i64, pub reasoning: i64, #[serde(default)] pub speed: Option<String>,
    pub billing: Billing }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejected { pub key: String, pub reason: String }
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestOutcome { pub accepted: u32, pub duplicate: u32, pub rejected: Vec<Rejected> }
```

- Produces (`src/service/usage.rs`):

```rust
#[derive(Debug, thiserror::Error)]
pub enum UsageError { #[error("{0}")] Envelope(String), #[error(transparent)] Store(#[from] StoreError) }
pub struct UsageService { /* store, clock, prices: Arc<PriceTable> */ }
impl UsageService {
    pub fn new(store: Arc<dyn Store>, clock: Arc<dyn Clock>, prices: Arc<PriceTable>) -> Self;
    pub fn prices(&self) -> &PriceTable;
    pub async fn ingest(&self, r: &Resolved, report: &UsageReport) -> Result<IngestOutcome, UsageError>;
    pub async fn summary(&self, filter: &UsageFilter, group: UsageGroup) -> StoreResult<Vec<UsageRow>>;
}
// AppState: pub usage: Arc<UsageService>; AppState::new uses PriceTable::builtin();
// pub fn with_prices(self, prices: PriceTable) -> Self  (rebuilds `usage`)
// ServerConfig: pub prices_file: Option<PathBuf>   (AXL_PRICES_FILE; blank = None)
```

Validation (in `ingest`, against `clock.now()`), first failure wins, reasons exactly:
`"malformed record"` (does not decode as `WireRecord`; key taken from `value["key"]` when it is a string, else `""`), `"key empty or over 200 bytes"`, `"model empty, over 128 bytes or has characters outside A-Za-z0-9._:/@-"`, `"count below 0 or over 1000000000"`, `"timestamp not RFC 3339"`, `"timestamp more than 5 minutes ahead"`, `"timestamp more than 30 days old"`, `"speed over 16 bytes or not a-z"`. Envelope (400, `UsageError::Envelope`): `parser` empty, over 32 bytes or not `[a-z0-9-]+/[0-9]+`; `harness_version` over 64 bytes; `harness_session_id` over 200 bytes. Valid records go to `insert_usage` in one call with `self.prices` as the pricer; `accepted = inserted`, `duplicate = duplicate`.

Handler (planner decision: read the body by hand so an oversize body is the spec's 400, not axum's 413): take `axum::body::Body`; `axum::body::to_bytes(body, MAX_BODY_BYTES)` error → 400 `"body over 256 KB"`; `serde_json::from_slice::<UsageReport>` error → 400 `"bad request: <serde message>"`; `records.len() > MAX_RECORDS` → 400 `"more than 500 records"`; then `session_for` (404 / 403), then `ingest`. No `touch_heartbeat`, no lease renewal, no event.

- [ ] **Step 1: Write the failing tests** in `tests/http_usage.rs`

```rust
fn record(key: &str, at: &str) -> Value { json!({"key": key, "at": at, "model": "claude-opus-5-5",
    "input": 12, "output": 845, "cache_write_5m": 0, "cache_write_1h": 3120, "cache_read": 48211,
    "reasoning": 210, "speed": null, "billing": "subscription"}) }

#[tokio::test] async fn accepts_and_dedupes() {
    // register a session, POST one record at the clock's now -> 200 {"accepted":1,"duplicate":0,"rejected":[]}
    // POST it again -> {"accepted":0,"duplicate":1,"rejected":[]}
    // store.get_usage("claude-code", key): cost_micros Some(51550), billing Subscription
}
#[tokio::test] async fn auth_and_ownership() {
    // no bearer -> 401; unknown session -> 404; a session registered by another token -> 403
}
#[tokio::test] async fn envelope_errors_store_nothing() {
    // not JSON -> 400; no session_id -> 400; 501 records -> 400; a body of 256 KiB + 1 -> 400;
    // parser "Claude Code" -> 400; after each, get_usage(...) is None for every key sent
}
#[tokio::test] async fn partial_rejection() {
    // 4 records: good; model "bad model!"; input -1; key of 201 bytes
    // -> accepted 1, rejected == [{key:..., reason: "model empty, ..."}, ...] in request order
}
#[tokio::test] async fn old_and_future_records_rejected() {
    // at = now + 6 min -> "timestamp more than 5 minutes ahead"; now - 31 days -> "... 30 days old";
    // now + 4 min and now - 29 days accepted
}
#[tokio::test] async fn malformed_record_rejected_alone() { /* {"key":"k","input":"x"} + a good one -> accepted 1, rejected [{key:"k", reason:"malformed record"}] */ }
#[tokio::test] async fn not_a_heartbeat() {
    // advance clock 10 min, POST; get_session(..).last_heartbeat_at unchanged
}
#[tokio::test] async fn counts_against_the_rate_limit() {
    // app_with(|c| { c.rate_limit_burst = 1; c.rate_limit_per_sec = 1 }): 2nd POST -> 429
}
```

Add `serve_refuses_a_bad_prices_file` (Task 3 sketch) to `tests/usage_prices.rs`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test http_usage`
Expected: compile error / 404 for the route.

- [ ] **Step 3: Implement** report types, service, handler, `AppState.usage` / `with_prices`, `ServerConfig.prices_file`, and in `main.rs` `serve`: `let prices = PriceTable::load(config.prices_file.as_deref())?;` before `open_store`, then `AppState::new(...).with_prices(prices)`, and print `axl: prices: <version>, <n> models` (plus ` (+ <file>)` with an override).

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --test http_usage && cargo test --test usage_prices`
Expected: PASS.

- [ ] **Step 5: Write the docs.** `http-api.md`: request and answer shape (the spec's JSON), every limit and rejection reason above, 401/403/404/400/429, "not a heartbeat". `configuration.md`: `AXL_PRICES_FILE` (format, merge by `id`, a bad file stops the server, `price_ref`).

- [ ] **Step 6: Full gate, then commit**

```bash
git add src/usage src/service src/http src/config.rs src/main.rs tests/http_usage.rs tests/usage_prices.rs docs/http-api.md docs/configuration.md
git commit -m "feat(http): accept token usage reports at POST /v1/usage"
```

---

### Task 5: `axl usage reprice` (mechanical)

**Files:**
- Modify: `src/service/usage.rs` (`reprice`), `src/main.rs` (`Command::Usage { command: UsageCommand }`, `UsageCommand::Reprice { since: Option<String>, unpriced: bool }`)
- Modify docs: `docs/cli.md` ("Server and tokens" section), `docs/releasing.md` (a "Refresh the price table" step under "Cut a release": edit `src/usage/prices.toml` from the pricing pages, bump `version`, note that existing rows keep their cost until `axl usage reprice`)
- Test: `tests/usage_reprice.rs` (new)

**Interfaces:**
- Consumes: `usage_to_reprice`, `set_usage_costs`.
- Produces: `#[derive(Debug, Default, PartialEq, Eq)] pub struct Repriced { pub updated: u64, pub unpriced: u64 }`; `UsageService::reprice(&self, since: Option<OffsetDateTime>, unpriced_only: bool) -> StoreResult<Repriced>` (pages of 500 by id; `unpriced` = rows still without a price afterwards); `pub fn parse_since(raw: &str) -> Result<OffsetDateTime, String>` in `src/service/usage.rs` (`YYYY-MM-DD` = that UTC midnight, or RFC 3339).

CLI: needs `DATABASE_URL` and honours `AXL_PRICES_FILE` (loads the table the way `serve` does). Prints to stderr `axl: repriced <updated> records (<unpriced> without a price)`.

- [ ] **Step 1: Write the failing tests**

```rust
store_matrix!(reprice_all_and_unpriced_only, |store| async move {
    // insert 2 records with NoPrice and 1 with a pricer giving cost 1 / "old#x"
    // svc = UsageService::new(store, clock, Arc::new(PriceTable::builtin()))
    // svc.reprice(None, true) -> Repriced { updated: 2, unpriced: 0 }; the "old#x" row untouched
    // svc.reprice(None, false) -> updated 3; the old row now "builtin:2026-11-28#claude-opus-5-5"
    // since = after the first record -> only later rows updated
});
#[test] fn parse_since_forms() { /* "2026-11-01" -> 2026-11-01 0:00 UTC; RFC 3339 ok; "yesterday" -> Err */ }
#[test] fn cli_reprices_a_sqlite_file() {
    // seed sqlite://<temp>/a.db through SqliteStore::connect + insert_usage (NoPrice), drop it,
    // run axl usage reprice --unpriced with env_clear(), DATABASE_URL set -> exit 0,
    // stderr "axl: repriced 1 records (0 without a price)"
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test --test usage_reprice` → compile error.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass** — `cargo test --test usage_reprice` (and the pg-tests command) → PASS.
- [ ] **Step 5: Docs** (`cli.md`, `releasing.md`).
- [ ] **Step 6: Full gate, then commit**

```bash
git add src/service/usage.rs src/main.rs tests/usage_reprice.rs docs/cli.md docs/releasing.md
git commit -m "feat(cli): axl usage reprice recomputes stored costs"
```

---

### Task 6: Parser `claude-code/1` with fixtures and the privacy check (judgment)

**Files:**
- Create: `src/usage/parsers/mod.rs`, `src/usage/parsers/claude_code.rs`
- Create fixtures: `tests/fixtures/usage/claude-code/2.1.329/main.jsonl`, `main.expected.json`, `subagent.jsonl`, `subagent.expected.json`
- Modify: `src/usage/mod.rs` (`pub mod parsers;`), `src/hook/mod.rs` (`Harness::name`)
- Test: `tests/usage_parsers.rs` (new)

**Interfaces:**
- Consumes: `UsageCounts`, `WireRecord`, `Billing`, `crate::hook::Harness`.
- Produces:

```rust
// src/hook/mod.rs
impl Harness { pub fn name(self) -> &'static str }   // "claude-code" | "codex"

// src/usage/parsers/mod.rs
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRecord { pub key: String, pub at: OffsetDateTime, pub model: String,
    pub counts: UsageCounts, pub speed: Option<String>,
    /// From Codex's `token_count` fallback (dropped when the file has real records).
    pub fallback: bool }
impl ParsedRecord { pub fn to_wire(&self, billing: Billing) -> WireRecord; }
/// Survives between runs in the cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParserState { pub model: Option<String>, pub session_meta_id: Option<String>,
    pub ordinal: u64, pub saw_records: bool }
#[derive(Debug, Clone, PartialEq)]
pub enum Line { Record(ParsedRecord), /** looks like usage, not understood */ UsageShaped, Other }
pub trait Parser: Sync {
    fn id(&self) -> &'static str;
    /// The files one hook run reads, main transcript first.
    fn files(&self, transcript: &Path) -> Vec<PathBuf>;
    fn line(&self, line: &[u8], state: &mut ParserState) -> Line;
}
pub fn for_harness(h: Harness) -> &'static dyn Parser;
pub fn by_id(id: &str) -> Option<&'static dyn Parser>;
/// Parses a whole file (tests, fixtures): every `Line::Record`, in order.
pub fn parse_all(p: &dyn Parser, bytes: &[u8], state: &mut ParserState) -> Vec<ParsedRecord>;
```

`claude-code/1` rules:
- `files`: the transcript, then `<transcript dir>/<transcript file stem>/subagents/*.jsonl` sorted by name.
- A record is a line with `type == "assistant"` and an object `message.usage`. Key `requestId`, else `message.id`, else skip. `at` = the line's `timestamp` (unparseable: skip). Model `message.model` with a trailing `[...]` suffix removed; skip the line when the model is `<synthetic>` or empty, or when all five priced counts are 0.
- Counts: `input = usage.input_tokens`; `output = usage.output_tokens`; when `usage.cache_creation` is an object, `cache_write_5m = cache_creation.ephemeral_5m_input_tokens` and `cache_write_1h = cache_creation.ephemeral_1h_input_tokens`, else `cache_write_5m = usage.cache_creation_input_tokens`, `cache_write_1h = 0`; `cache_read = usage.cache_read_input_tokens`; `reasoning = usage.output_tokens_details.thinking_tokens`; missing numbers are 0; `speed = usage.speed` when it is a string. Deserialize into a private struct that names only these fields (`serde` ignores the rest, so text never enters memory as a field).
- `UsageShaped`: a line that is valid JSON with a `usage` key at `message.usage` but fails the record rules for a reason other than the deliberate skips (no key, bad timestamp, non-numeric counts). Lines that are not JSON or have no usage are `Other`.

Fixtures: real shapes from Claude Code 2.1.329, every free-text string `"…"`. `main.jsonl` has: a `user` line; two `assistant` lines with the same `requestId` and identical usage (a thinking block and a text block); one `assistant` with `cache_creation` split 5m/1h, `thinking_tokens` and `speed: "fast"`; one with only `cache_creation_input_tokens`; one `<synthetic>` zero-usage line; one with model `claude-opus-5-5[1m]`; a `summary` line. `subagent.jsonl`: two assistant lines with `isSidechain: true`. Expected files are JSON arrays of `WireRecord` with `"billing": "unknown"`.

- [ ] **Step 1: Write the failing tests** in `tests/usage_parsers.rs`

```rust
const ALLOWED_TEXT_KEYS: &[&str] = &["type", "timestamp", "requestId", "id", "uuid", "parentUuid",
    "sessionId", "session_id", "response_id", "turn_id", "agentId", "model", "role", "speed",
    "stop_reason", "service_tier", "inference_geo", "version", "cli_version", "userType",
    "auth_mode"];

#[test] fn fixtures_are_synthetic() {
    // walk tests/fixtures/usage recursively; for every *.jsonl line parse JSON and visit
    // every string value: under a key not in ALLOWED_TEXT_KEYS it must equal "…".
    // The whole file must not contain "/Users/", "/home/", "@" followed by a domain, or "sk-".
}
#[test] fn claude_main_fixture() {
    let p = parsers::by_id("claude-code/1").unwrap();
    let got = parse_all(p, &read("claude-code/2.1.329/main.jsonl"), &mut ParserState::default());
    assert_eq!(dedupe_by_key(got).iter().map(|r| r.to_wire(Billing::Unknown)).collect::<Vec<_>>(),
               expected("claude-code/2.1.329/main.expected.json"));
}
#[test] fn claude_skips_synthetic_and_strips_suffix() {
    // no record has model "<synthetic>"; the [1m] line's model is "claude-opus-5-5"
}
#[test] fn claude_files_include_subagents() {
    // temp dir: proj/abc.jsonl, proj/abc/subagents/agent-b.jsonl, agent-a.jsonl, other.txt
    // files(proj/abc.jsonl) == [abc.jsonl, abc/subagents/agent-a.jsonl, agent-b.jsonl]
}
#[test] fn claude_subagent_fixture() { /* like main */ }
#[test] fn claude_unrecognised_usage_line() {
    // {"type":"assistant","timestamp":"…","message":{"usage":{"input_tokens":"x"}}} -> Line::UsageShaped
    // {"type":"user"} -> Line::Other; "not json" -> Line::Other
}
```

`dedupe_by_key` (test helper, same rule as the collector in Task 9): keep the first record per key, each count the max.

- [ ] **Step 2: Run to verify they fail** — `cargo test --test usage_parsers` → compile error.
- [ ] **Step 3: Write the fixtures and expected files**, then implement the parser.
- [ ] **Step 4: Run to verify they pass** — `cargo test --test usage_parsers` → PASS.
- [ ] **Step 5: Full gate, then commit**

```bash
git add src/usage src/hook/mod.rs tests/usage_parsers.rs tests/fixtures/usage/claude-code
git commit -m "feat(usage): parse token usage from Claude Code transcripts"
```

---

### Task 7: Parser `codex/1` with the `token_count` fallback (judgment)

**Files:**
- Create: `src/usage/parsers/codex.rs`
- Create fixtures: `tests/fixtures/usage/codex/0.172.0/rollout.jsonl`, `rollout.expected.json`, `mixed.jsonl`, `mixed.expected.json`; `tests/fixtures/usage/codex/legacy-token-count/rollout.jsonl`, `rollout.expected.json`
- Modify: `src/usage/parsers/mod.rs` (register), `tests/usage_parsers.rs`

**Interfaces:**
- Consumes: Task 6's `Parser`, `ParserState`, `Line`, `ParsedRecord`.
- Produces: `codex/1` registered for `Harness::Codex`.

`codex/1` rules (each line is `{"timestamp": …, "type": …, "payload": {…}}`):
- `files`: the transcript only.
- `session_meta`: `state.session_meta_id = payload.id`. `turn_context`: `state.model = payload.model`.
- `token_usage_record`: key `payload.response_id`; usage `payload.usage`: `input = input_tokens − cached_input_tokens` (saturating at 0), `cache_read = cached_input_tokens`, `cache_write_5m = cache_write_input_tokens`, `cache_write_1h = 0`, `output = output_tokens`, `reasoning = reasoning_output_tokens`; model `state.model` (no model yet: `UsageShaped`); sets `state.saw_records = true`.
- `event_msg` with `payload.type == "token_count"` and a non-null `payload.info.last_token_usage`: the same mapping from `last_token_usage`; `state.ordinal += 1`; key `<session_meta_id>:<ordinal>` (no `session_meta_id`: `UsageShaped`); `fallback: true`. A null `info` is `Other`.
- The collector (Task 9) and `parse_all` drop `fallback` records from a file whose state has `saw_records`. Put this in one function, `pub fn drop_fallback(records: &mut Vec<ParsedRecord>, state: &ParserState)`, in `parsers/mod.rs`, and call it from `parse_all`.

Planner decision on the spec's settled question 3 (whether `cache_write_input_tokens` is inside `input_tokens`): keep the spec's mapping. Settling it needs real rollouts, which tests may not read. Leave a `// Open question (spec, settled question 3)` comment at the mapping and a line in `docs/harnesses/codex.md` (Task 10).

Fixtures: `0.172.0/rollout.jsonl` has `session_meta`, two `turn_context` lines (model changes mid-session), `response_item` lines with text `"…"`, three `token_usage_record` lines. `legacy-token-count/rollout.jsonl` has `session_meta`, `turn_context`, a `token_count` with `info: null`, two `token_count` with usage. `mixed.jsonl` has `token_count` lines before and after each `token_usage_record`. Use a model id the built-in price table prices (Task 3).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn codex_rollout_fixture() { /* parse_all == rollout.expected.json; second record carries the second turn_context model */ }
#[test] fn codex_legacy_token_count_fixture() { /* keys "<meta id>:1", "<meta id>:2"; info:null skipped */ }
#[test] fn codex_mixed_rollout_counts_once() { /* parse_all(mixed) == only the token_usage_record records */ }
#[test] fn codex_state_survives_a_split_read() {
    // parse the first half of rollout.jsonl, then the rest with the same ParserState:
    // same records as one pass (model and ordinal carried over)
}
#[test] fn codex_input_excludes_cached() { /* input_tokens 100, cached 80 -> input 20, cache_read 80; cached > input -> input 0 */ }
```

- [ ] **Step 2: Run to verify they fail** — `cargo test --test usage_parsers codex` → FAIL (no parser `codex/1`).
- [ ] **Step 3: Fixtures, then implement.**
- [ ] **Step 4: Run to verify they pass** — `cargo test --test usage_parsers` (includes `fixtures_are_synthetic` over the new files) → PASS.
- [ ] **Step 5: Full gate, then commit**

```bash
git add src/usage/parsers tests/usage_parsers.rs tests/fixtures/usage/codex
git commit -m "feat(usage): parse token usage from Codex rollouts"
```

---

### Task 8: Cursor file, status file, billing detection, `[usage]` config (mechanical)

**Files:**
- Create: `src/usage/cursor.rs`, `src/usage/billing.rs`
- Modify: `src/usage/mod.rs`, `src/client/config.rs` (`UsageConfig`, `ClientConfig.usage`, `usage_off`), `src/cli/login.rs` (~line 157: keep `old.usage`), every `ClientConfig { … }` literal (`tests/cli_login.rs:143`, `tests/cli_onboarding.rs` ×7): add `..ClientConfig::default()`
- Test: `tests/usage_local.rs` (new), `tests/cli_login.rs` (`login_keeps_usage_config`)

**Interfaces:**
- Produces (`src/client/config.rs`):

```rust
#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Clone)]
pub struct UsageConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub enabled: Option<bool>,
    /// Harness name -> billing kind; always wins over detection.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub billing: std::collections::BTreeMap<String, Billing> }
// ClientConfig gains:
#[serde(default, skip_serializing_if = "Option::is_none")] pub usage: Option<UsageConfig>,
/// Why usage collection is off: `Some("AXL_USAGE=off")`, `Some("[usage] enabled = false")`, or None (on).
/// `AXL_USAGE` is off for "off", "0", "false", "no" (ASCII case-insensitive).
pub fn usage_off(axl_usage: Option<&str>, cfg: &ClientConfig) -> Option<&'static str>;
```

- Produces (`src/usage/cursor.rs`):

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cursor { pub path: PathBuf, pub dev: u64, pub ino: u64, pub offset: u64,
    pub parser: String, pub state: ParserState, pub billing: Option<Billing>,
    pub session_id: Option<SessionId>, pub server_url: Option<String>,
    /// RFC 3339: records older than this are dropped (the session file's mtime at creation).
    pub floor: Option<String>,
    /// Discard bytes up to the next newline before parsing (a line over the size cap).
    #[serde(default)] pub skip_partial: bool,
    /// The "does not recognise this log" line was printed.
    #[serde(default)] pub warned: bool,
    pub updated_at: String }
pub fn dir(home: &Path) -> PathBuf;                         // <home>/.axl/sessions/usage
pub fn path_for(home: &Path, transcript: &Path) -> PathBuf; // dir/<sha256 hex of path bytes>.json
pub fn load(home: &Path, transcript: &Path) -> Option<Cursor>;  // missing or corrupt: None
pub fn save(home: &Path, c: &Cursor) -> std::io::Result<()>;    // dir 0700, file 0600, tmp + rename
/// Deletes cursor files whose mtime is older than `max_age`; returns how many.
pub fn prune(home: &Path, now: SystemTime, max_age: Duration) -> usize;
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status { pub last_success_at: Option<String>,
    #[serde(default)] pub billing: BTreeMap<String, Billing> }
pub fn status_path(home: &Path) -> PathBuf;                 // dir/status.json (never pruned)
pub fn load_status(home: &Path) -> Status;
pub fn save_status(home: &Path, s: &Status) -> std::io::Result<()>;
```

- Produces (`src/usage/billing.rs`):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source { Config, Detected }
/// `[usage.billing]` for this harness, else detection.
pub fn resolve(h: Harness, home: &Path, env: &dyn Fn(&str) -> Option<String>, cfg: &UsageConfig) -> (Billing, Source);
pub fn detect(h: Harness, home: &Path, env: &dyn Fn(&str) -> Option<String>) -> Billing;
```

Detection reads only: env var presence (non-empty); `<home>/.claude/settings.json` key `apiKeyHelper` present; `<home>/.claude.json` key `oauthAccount` is an object (deserialize into a struct with that one `Option<serde_json::Value>` field); `<home>/.codex/auth.json` key `auth_mode` (one `Option<String>` field). Any read or parse failure counts as "not present". Nothing is printed or logged.

- [ ] **Step 1: Write the failing tests** in `tests/usage_local.rs` (temp homes only; env from a `HashMap` closure)

```rust
#[test] fn claude_billing_order() {
    // empty home, empty env -> Unknown
    // .claude.json {"oauthAccount": {"x": "…"}} -> Subscription
    // + env ANTHROPIC_API_KEY=k (each of the five names, one at a time) -> Api
    // settings.json {"apiKeyHelper": "…"} with no env -> Api
    // .claude.json {"oauthAccount": null} -> Unknown
    // cfg billing {"claude-code": Subscription} with ANTHROPIC_API_KEY set -> (Subscription, Config)
}
#[test] fn codex_billing() {
    // auth.json {"auth_mode":"chatgpt"} -> Subscription; "apikey" -> Api; other / missing -> Unknown;
    // OPENAI_API_KEY in env is ignored
}
#[test] fn usage_off_switches() {
    // None + default -> None; Some("off"), Some("OFF"), Some("0") -> Some("AXL_USAGE=off");
    // enabled = false -> Some("[usage] enabled = false"); Some("on") + default -> None
}
#[test] fn config_round_trip() {
    // "[usage]\nenabled = false\n[usage.billing]\ncodex = \"api\"\n" parses; save_config writes it back
}
#[test] fn cursor_round_trip_private_and_atomic() {
    // save then load equal; file mode 0o600, dir 0o700; path_for differs per transcript path;
    // no "token" substring in the saved JSON; a corrupt file loads as None
}
#[test] fn prune_old_cursors_keeps_status() {
    // two cursor files, set one's mtime 31 days back (libc::utimes or File::set_modified);
    // prune(.., 30 days) == 1; status.json survives even when old
}
```

`tests/cli_login.rs`: `login_keeps_usage_config` — a config with `[usage] enabled = false` survives `axl login`.

- [ ] **Step 2: Run to verify they fail** — `cargo test --test usage_local` → compile error.
- [ ] **Step 3: Implement**, and fix the `ClientConfig` literals.
- [ ] **Step 4: Run to verify they pass** — `cargo test --test usage_local --test cli_login --test cli_onboarding` → PASS.
- [ ] **Step 5: Full gate, then commit**

```bash
git add src/usage src/client/config.rs src/cli/login.rs tests/usage_local.rs tests/cli_login.rs tests/cli_onboarding.rs
git commit -m "feat(usage): cursor files, billing detection and the [usage] config table"
```

---

### Task 9: Collector — read, cap, POST, advance (judgment)

**Files:**
- Create: `src/usage/collect.rs`
- Modify: `src/usage/mod.rs`
- Test: `tests/usage_collect.rs` (new; drives `collect` directly against stand-in axum servers on `127.0.0.1:0`, and against a live `AppState` for one end-to-end case)

**Interfaces:**
- Consumes: parsers, cursor, billing, `UsageConfig`, `report::{UsageReport, MAX_RECORDS}`, `SessionInfo`.
- Produces:

```rust
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const PARSE_BUDGET: Duration = Duration::from_millis(300);
pub const CURSOR_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
pub struct Collect<'a> {
    pub harness: Harness,
    pub home: &'a Path,
    /// The hook's stdin (JSON): `transcript_path`, `session_id`.
    pub stdin: &'a str,
    /// The bridge's session and its session file's mtime, when `find_session` found one.
    pub session: Option<(SessionInfo, SystemTime)>,
    pub env: &'a (dyn Fn(&str) -> Option<String> + Sync),
    /// SessionEnd: also prune old cursors.
    pub session_end: bool,
}
/// One run. `None`: nothing to say; `Some(line)`: the single stderr line.
pub async fn collect(c: Collect<'_>) -> Option<String>;
```

Algorithm (in order):
1. `usage_off(env("AXL_USAGE"), &load_config(..).unwrap_or_default())` → `None`. No `transcript_path` in stdin → `None`.
2. `parser = for_harness(harness)`; `files = parser.files(transcript)`. Load the main file's cursor. Session = `c.session`, else the cursor's cached `session_id` + `server_url`; neither → `None`. Token via `client::read_token(env("AXL_TOKEN"), dir)`; none → `None` (silent).
3. Billing = config override, else the main cursor's cached kind, else `billing::detect`; cache it in every cursor written.
4. In `spawn_blocking`, with an `Instant` deadline of `PARSE_BUDGET`, for each file: load or create its cursor (new: offset 0, `floor` = session file mtime, or the main cursor's floor); stat it; a different `dev`/`ino` or a size below `offset` → `offset = 0`, `state = default`, `skip_partial = false`; read at most `MAX_FILE_BYTES` from `offset`; with `skip_partial`, drop through the first `\n`; cut at the last `\n` (a chunk of `MAX_FILE_BYTES` with no `\n`: consume it all and set `skip_partial`); parse line by line, stopping at the deadline or at `MAX_RECORDS` records in total; drop records with `at < floor`; `drop_fallback`; note the new offset of the last line consumed. Unreadable main transcript → one line `cannot read the transcript for usage (<io error kind>)`, no path.
5. Merge records with the same key (first wins, each count the max).
6. No records: save cursors with their new offsets; if a file produced `UsageShaped` lines and no record at all and its cursor is not `warned`, set `warned` and return `usage parser <id> does not recognise this log`.
7. POST `<server_url>/v1/usage` with `UsageReport { session_id, parser: id, harness_version: None, harness_session_id: stdin.session_id, records }`, a 2 s timeout, bearer token. Errors never carry the URL or token (`without_url()`).
   - 2xx: save cursors at their new offsets; `save_status` with `last_success_at = now` and this harness's billing; `rejected` non-empty → `usage report: <n> of <m> records rejected (<first reason>)`.
   - 400, 403, 404: save cursors advanced; `usage report refused by the server (HTTP <code>); skipped`.
   - anything else (timeout, 429, 5xx, connect error): keep cursors; `usage report not sent (<reason>); will retry`.
8. `session_end`: `cursor::prune(home, now, CURSOR_MAX_AGE)`.

- [ ] **Step 1: Write the failing tests** in `tests/usage_collect.rs`

Helpers: a temp home with `.axl/token` = `"tok"`; a transcript built from the Claude fixture lines; `fake_usage(status: u16) -> (url, Arc<Mutex<Vec<Value>>>)` recording request bodies; `hanging() -> url` (accepts, never answers); `run(home, stdin, session, env) -> Option<String>`.

```rust
#[tokio::test] async fn sends_once_and_advances_on_2xx() {
    // 200 {"accepted":n,...}: one POST with n records, parser "claude-code/1", harness_session_id
    // from stdin; a second run sends nothing (cursor at EOF)
}
#[tokio::test] async fn keeps_cursor_on_5xx_and_timeout() {
    // 503: next run resends the same keys; hanging server under tokio::time::timeout(3 s):
    // collect returns within 2.5 s with "not sent", cursor offset unchanged
}
#[tokio::test] async fn advances_on_403_404_400() { /* each: one line "refused"; next run sends nothing */ }
#[tokio::test] async fn rejected_records_still_advance() { /* 200 with rejected:[..]: line "1 of 3 records rejected (timestamp more than 30 days old)"; next run sends nothing */ }
#[tokio::test] async fn partial_last_line_waits() {
    // transcript ends with half a line: not sent; append the rest + "\n": next run sends it
}
#[tokio::test] async fn shrunk_or_replaced_file_restarts() {
    // after a 2xx, truncate and rewrite the file (or rename a new file over it): the next run
    // reads from 0 and resends (server dedupes)
}
#[tokio::test] async fn new_cursor_drops_history_before_the_session_file() {
    // session mtime = between record 2 and 3 timestamps -> only records 3.. sent
}
#[tokio::test] async fn caps_continue_next_run() {
    // 600 assistant lines -> first POST 500 records, second 100; a file over 8 MB of
    // padding lines (each "{\"type\":\"user\",\"message\":\"…\"}") is read in two runs
}
#[tokio::test] async fn line_longer_than_cap_is_skipped() {
    // one 9 MB line without "\n", then "\n", then a real record line: within two runs the record
    // is sent and the long line never is
}
#[tokio::test] async fn subagent_files_are_read() { /* main + subagents/agent-a.jsonl records in one POST */ }
#[tokio::test] async fn off_sends_nothing() {
    // env AXL_USAGE=off -> no request; [usage] enabled=false -> no request
}
#[tokio::test] async fn no_session_or_token_is_silent() { /* None session and no cursor -> None, no request; no token -> None */ }
#[tokio::test] async fn falls_back_to_the_cursor_session() {
    // run once with a session (2xx), append a record, run with session None (bridge gone):
    // POST goes to the cached server with the cached session id
}
#[tokio::test] async fn unrecognised_log_warns_once() { /* UsageShaped-only file: line once, second run None */ }
#[tokio::test] async fn session_end_prunes() { /* an old unrelated cursor is deleted when session_end */ }
#[tokio::test] async fn privacy_nothing_but_numbers_and_ids() {
    // capture the POST body; every string in records is one of key/at/model/speed/billing;
    // the body never contains "…", the temp home path, or "tok"
}
#[tokio::test] async fn end_to_end_against_the_server() {
    // live AppState (sqlite temp file) + registered session; collect; store.get_usage(...)
    // has the fixture's counts and Billing from the env (ANTHROPIC_API_KEY set -> Api)
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test --test usage_collect` → compile error.
- [ ] **Step 3: Implement `collect.rs`.**
- [ ] **Step 4: Run to verify they pass** — `cargo test --test usage_collect` → PASS.
- [ ] **Step 5: Full gate, then commit**

```bash
git add src/usage tests/usage_collect.rs
git commit -m "feat(usage): collect usage deltas from harness logs and report them"
```

---

### Task 10: Hook wiring — Stop and SessionEnd (judgment)

**Files:**
- Modify: `src/hook/mod.rs` (`Event::SessionEnd`, `run` signature, Stop/SessionEnd flow), `src/main.rs` (`hook()` passes an env lookup; `Hook` help text lists `session-end`)
- Modify docs: `docs/harnesses/claude-code.md` (Hooks table: `session-end`; a "Token usage" section: what is read, when, cursor files, off switch, billing override), `docs/harnesses/codex.md` (same, plus the cache-write open question), `docs/security.md` (new `## Token usage` section: the spec's Privacy paragraph verbatim in substance, and that credential files are read for one key, never logged)
- Test: `tests/hook.rs`

**Interfaces:**
- Consumes: `usage::collect::{collect, Collect}`.
- Produces:

```rust
pub enum Event { SessionStart, UserPromptSubmit, PostToolUse, Stop, SessionEnd }  // "session-end", name "SessionEnd"
pub const SESSION_END_BUDGET: Duration = Duration::from_secs(1);
/// `env` is the process environment (`AXL_TOKEN`, `AXL_USAGE`, the billing variables).
pub async fn run(event: Event, harness: Harness, stdin: &str, home: Option<&Path>,
    env: &(dyn Fn(&str) -> Option<String> + Sync)) -> Output;
```

Flow:
- SessionStart, UserPromptSubmit, PostToolUse: unchanged (token from `env("AXL_TOKEN")`).
- Stop: `tokio::join!(run_mail(..), usage_bounded(..))`, where `usage_bounded` is `tokio::time::timeout(NETWORK_BUDGET, collect(..))` (a timeout is `Some("usage report not sent (timed out); will retry")`). The loop guard stays inside `run_mail`, so usage is collected even when `stop_hook_active` is true. Stdout comes from mail only. Stderr: mail's line if any, else usage's.
- SessionEnd: `collect` only, `session_end: true`, under `SESSION_END_BUDGET`; stdout always empty.
- `find_session` gives `(pid, info)`; the session file mtime is `std::fs::metadata(session_file::path(home, pid))?.modified()`.

- [ ] **Step 1: Write the failing tests** in `tests/hook.rs`

```rust
#[test] fn session_end_without_anything_is_silent() {
    // session-end, "{}", no session file -> code 0, stdout "", stderr ""
}
#[tokio::test] async fn stop_reports_usage_to_the_server() {
    // live server (existing `live`), transcript in the temp home, stdin {"transcript_path": ..., "session_id": "hs"};
    // stop -> code 0; store.get_usage("claude-code", key) exists with the session's id
}
#[tokio::test] async fn stop_reports_usage_even_when_stop_hook_active() { /* {"stop_hook_active":true,...} -> usage stored, stdout "" */ }
#[tokio::test] async fn urgent_block_prints_while_usage_post_hangs() {
    // axum app: /v1/inbox/summary answers one urgent item; /v1/usage never answers
    // stop -> stdout has "decision":"block"; finished in under 4 s (Instant)
}
#[tokio::test] async fn session_end_after_the_bridge_left_uses_the_cursor() {
    // stop once with a session file (2xx), remove the session file, append a record,
    // session-end -> the new record is stored under the same session
}
#[tokio::test] async fn usage_off_sends_nothing() { /* AXL_USAGE=off in the hook env: fake server sees no /v1/usage */ }
#[test] fn session_end_codex_flag() { /* "session-end --harness codex" with nothing set: silent, exit 0 */ }
```

`hook_with` already uses `env_clear()`; add an `envs: &[(&str, &str)]` variant for `AXL_USAGE` and `ANTHROPIC_API_KEY`. Existing tests (`server_down_fails_open`, `stop_active_exits_first`, `no_session_file_silent`) must still pass unchanged: their stdin has no `transcript_path`, so the collector is silent.

- [ ] **Step 2: Run to verify they fail** — `cargo test --test hook` → FAIL (`session-end` ignored, no usage stored).
- [ ] **Step 3: Implement**, update `main.rs`.
- [ ] **Step 4: Run to verify they pass** — `cargo test --test hook` → PASS.
- [ ] **Step 5: Docs** (`claude-code.md`, `codex.md`, `security.md`).
- [ ] **Step 6: Full gate, then commit**

```bash
git add src/hook/mod.rs src/main.rs tests/hook.rs docs/harnesses/claude-code.md docs/harnesses/codex.md docs/security.md
git commit -m "feat(hook): report token usage on Stop and SessionEnd"
```

---

### Task 11: Install adds SessionEnd for Claude Code and Codex (mechanical)

**Files:**
- Modify: `src/install/claude_code.rs` (`EVENTS` ~line 44 becomes `[(&str, &str, i64); 5]` with a timeout per event; `handler(command, timeout)`), `src/install/codex.rs` (`EVENTS` ~line 29 becomes `[(&str, &str, i64); 4]`; `hooks_body` uses the per-event timeout; `HOOK_TIMEOUT_SECS` stays 10 for the others)
- Modify fixtures: every `tests/fixtures/harness/claude-code/*/after*/` `hooks.json` / `settings.json`, regenerated with `AXL_BLESS_FIXTURES=1`
- Modify docs: `docs/harnesses/claude-code.md` and `docs/harnesses/codex.md` (the hooks blocks they show)
- Test: `tests/install_claude_code.rs` (~line 197 loop), `tests/install_codex.rs` (~line 278 loop and the `len() == 3` assert)

Values: Claude Code `("SessionEnd", "session-end", 3)`, the other four keep 5. Codex `("SessionEnd", "session-end", 3)`, the other three keep 10. Adoption and uninstall already iterate `EVENTS`, so an existing hand-made `axl hook session-end` handler is adopted and removed like the others.

- [ ] **Step 1: Update the tests first**: add `("SessionEnd", "session-end")` to both loops, assert `timeout == 3` for it (`5` / `10` for the rest), Codex `v["hooks"].as_table().unwrap().len() == 4`, and in both files a test that uninstall removes the SessionEnd handler (extend the existing uninstall round-trip test's event list).
- [ ] **Step 2: Run to verify they fail** — `cargo test --test install_claude_code --test install_codex` → FAIL.
- [ ] **Step 3: Implement**, then `AXL_BLESS_FIXTURES=1 cargo test --test install_claude_code` and review `git diff tests/fixtures`: only `SessionEnd` blocks added.
- [ ] **Step 4: Run to verify they pass** — `cargo test --test install_claude_code --test install_codex --test install_core --test cli_onboarding` → PASS.
- [ ] **Step 5: Docs.**
- [ ] **Step 6: Full gate, then commit**

```bash
git add src/install tests/install_claude_code.rs tests/install_codex.rs tests/fixtures/harness docs/harnesses
git commit -m "feat(install): register the SessionEnd hook for Claude Code and Codex"
```

---

### Task 12: `axl status` usage lines (mechanical)

**Files:**
- Modify: `src/cli/status.rs` (`status_with(env, env_url, env_token, env_usage: Option<&str>)`; `run` passes `std::env::var("AXL_USAGE").ok()`, `status` passes `None`; new `fn usage_lines(lines: &mut Vec<StatusLine>, home: &Path, cfg: &ClientConfig, env_usage: Option<&str>)` called after `harness_lines`)
- Modify docs: `docs/cli.md` (`axl status` section)
- Test: `tests/cli_onboarding.rs` (status section)

Lines, none with a `fix` (they are information, so `ok` is unaffected):
- `usage` — `on` | `off (AXL_USAGE=off)` | `off ([usage] enabled = false)`
- `usage billing` — per harness in `status.json` or the config: `claude-code subscription, codex api (from config)`; nothing known: `not reported yet`
- `usage report` — `last sent <RFC 3339>` | `none sent yet`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn status_shows_usage_lines() {
    // full_home + install; no status.json -> "usage" == "on", "usage report" == "none sent yet",
    // "usage billing" == "not reported yet"; report.ok still true
    // write status.json {last_success_at: "2026-11-28T10:00:00Z", billing: {"claude-code": "subscription"}}
    //   and config [usage.billing] codex = "api" -> "usage billing" ==
    //   "claude-code subscription, codex api (from config)", "usage report" == "last sent 2026-11-28T10:00:00Z"
    // config [usage] enabled = false -> "usage" == "off ([usage] enabled = false)"
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test --test cli_onboarding status_shows_usage_lines` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify it passes** — `cargo test --test cli_onboarding` → PASS.
- [ ] **Step 5: Docs, full gate, commit**

```bash
git add src/cli/status.rs tests/cli_onboarding.rs docs/cli.md
git commit -m "feat(cli): axl status shows usage collection, billing and the last report"
```

---

### Task 13: Usage page and top-bar tab (judgment)

**Files:**
- Create: `src/ui/usage.rs`, `templates/usage.html`
- Modify: `src/ui/mod.rs` (`pub mod usage;`, `.route("/ui/usage", get(usage::usage_page))` inside the session-protected router), `templates/base.html` (tab after History), `static/app.css` (`.usage-*` rules, tokens only, light and dark), `static/app.js` (set `--share` from `data-share`, like `tickLeases` sets `--lease`)
- Modify docs: `docs/web-ui.md` (new `## Usage` section after History; the top-bar list)
- Test: `tests/ui_usage.rs` (new; the `App` helper from `tests/ui.rs`, seeding through `state.usage.ingest`)

**Interfaces:**
- Consumes: `UsageService::summary`, `UsageFilter`, `UsageGroup`, `TaskScope`, `Labels`, `pulse_counts`, `html`, `board_filter`, `project_options`.
- Produces:

```rust
#[derive(Deserialize, Default)]
pub struct UsageQuery { range: Option<String>, from: Option<String>, to: Option<String>,
    project: Option<String>, agent: Option<String>, harness: Option<String>, host: Option<String>,
    task: Option<String>, model: Option<String>, billing: Option<String>, group: Option<String> }
pub async fn usage_page(State, Extension<SessionCtx>, Query<UsageQuery>) -> UiResult<Response>;
/// "845", "12.3k", "1.24M", "3.10B".
pub fn compact(n: f64) -> String;
/// "$0.00", "<$0.01", "$3.40", "$1,234.56".
pub fn usd(micros: f64) -> String;
```

Behaviour (planner decisions where the spec is silent):
- `range`: `today` (UTC midnight to now, over-time per hour), `7d` (default; now − 7 days to now), `30d`, `all` (no bounds; used by the task-page link), `custom` (`from`/`to` as `YYYY-MM-DD`, both inclusive UTC days; `from` after `to` is swapped). Days are UTC and the page says so.
- Filters: `project` (id or key, via `board_filter`), `agent` (id), `harness`, `host`, `billing` are selects; `model` and `task` are text fields (`task` takes a task id or `none` for "no task"; a task with children includes them). A value that does not parse or matches nothing gives an empty table and the note "No usage matches."; never a 404 or 500 (only a malformed custom date is a 422 "Dates are YYYY-MM-DD.").
- `group`: `project` (default), `task`, `agent`, `harness`, `host`, `model`, `day`.
- Totals: one `<dl class="counters usage-totals">` line: Input, Output, Cache write, Cache read, API-equivalent cost, Of which subscription (est.). Each value is `compact()`/`usd()` with the exact number in `title` (tokens with thousands separators, cost with 6 decimals).
- Breakdown `<table class="usage-table">`: Name, Requests, Input, Output, Cache write, Cache read, API-equivalent cost, Share. Names: project label with its line swatch, task title linking to the task (unknown id: the id), `harness@host` for agents, "no task", "no project". Cost cell: `usd()` plus ` est.` when `estimated > 0`; wholly unpriced rows say "no price"; partly unpriced add " + N no price". Sorted by cost descending, rows with no priced cost last, then by total tokens. Share = the row's share of total cost (header "Share of cost"); when the total cost is 0, share of total tokens (header "Share of tokens"). The bar is `<span class="share-bar" data-share="0.4210" aria-hidden="true"></span>` beside the visible text `42%`, ink fill.
- Over time: one `<li>` per day (per hour for Today), gaps filled with zero rows, each with the date, `compact(tokens)`, `usd(cost)` (+ ` est.`), and a bar sized against the busiest day.
- The page carries "Estimated at API prices; subscription use is not billed per token."
- `nav: "usage"`; no SSE refresh.

- [ ] **Step 1: Write the failing tests** in `tests/ui_usage.rs`

```rust
#[tokio::test] async fn tab_and_empty_page() {
    // GET /ui/ has a[href="/ui/usage"] with text "Usage"; GET /ui/usage -> 200, that link has
    // aria-current="page", text has "Estimated at API prices; subscription use is not billed per token."
}
#[tokio::test] async fn totals_and_default_project_breakdown() {
    // two projects; ingest 3 records; totals show compact numbers with exact `title`;
    // table rows per project carry span.project[data-line]; header "API-equivalent cost"
}
#[tokio::test] async fn group_by_task_shows_no_task() { /* one record before any claim -> a row "no task" */ }
#[tokio::test] async fn unknown_model_shows_no_price_last() { /* model "mystery-1": its row says "no price" and is last */ }
#[tokio::test] async fn subscription_shows_est() { /* billing subscription -> cost cell ends with "est."; billing api -> no "est." */ }
#[tokio::test] async fn filters_narrow_and_keep_state() {
    // ?harness=codex shows only the codex agent's row; the select keeps "codex" selected;
    // ?group=day&range=7d -> 7 or 8 day rows in the over-time list
}
#[tokio::test] async fn bad_filters_never_500() {
    // ?task=garbage, ?agent=garbage, ?project=nope, ?range=custom&from=2026-11-20&to=2026-11-10
    // -> 200; ?range=custom&from=bad -> 422 "Dates are YYYY-MM-DD."
}
#[test] fn compact_and_usd() {
    // compact(845.0) "845"; compact(12_345.0) "12.3k"; compact(1_240_000.0) "1.24M";
    // usd(0.0) "$0.00"; usd(5_000.0) "<$0.01"; usd(3_400_000.0) "$3.40"; usd(1_234_560_000.0) "$1,234.56"
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test --test ui_usage` → FAIL (404).
- [ ] **Step 3: Implement** handler, template, CSS, JS.
- [ ] **Step 4: Run to verify they pass** — `cargo test --test ui_usage --test ui` → PASS (the tab must not break existing top-bar assertions).
- [ ] **Step 5: Docs** (`web-ui.md`).
- [ ] **Step 6: Full gate, then commit**

```bash
git add src/ui/usage.rs src/ui/mod.rs templates/usage.html templates/base.html static/app.css static/app.js tests/ui_usage.rs docs/web-ui.md
git commit -m "feat(ui): Usage page with totals, breakdown and usage over time"
```

---

### Task 14: Task-page usage line (mechanical)

**Files:**
- Modify: `src/ui/task_page.rs` (`TaskView.usage: Option<UsageLine>`), `templates/task.html` (lease panel, after the `task.lease` paragraph)
- Modify docs: `docs/web-ui.md` (Task page section)
- Test: `tests/ui_usage.rs`

**Interfaces:**
- Consumes: `UsageService::summary(&UsageFilter { task: Some(TaskScope::Task { id, subtasks }), ..Default::default() }, UsageGroup::Total)`, `compact`, `usd`.
- Produces: `pub struct UsageLine { pub text: String, pub href: String }`; `subtasks` = the task has children (`count_tasks(&TaskFilter { parent: Some(id), .. }) > 0`).

Text: `Usage <compact tokens> tokens · <usd> API-equivalent[ est.] · <n> session(s)[ · incl. subtasks]`, singular "1 session"; href `/ui/usage?task=<id>&group=agent&range=all`. No requests: no line. Markup: `<p class="usage-line"><a href="{{ u.href }}">{{ u.text }}</a></p>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn task_line_shows_and_links() {
    // claim a task, ingest one subscription record at a later time -> task page has
    // "Usage 52.2k tokens · $0.05 API-equivalent est. · 1 session" (the spec example's counts:
    // 12 + 845 + 0 + 3120 + 48211 = 52,188 tokens, 51,550 micro-USD)
    // and a[href="/ui/usage?task=<id>&group=agent&range=all"]
}
#[tokio::test] async fn parent_line_includes_subtasks() { /* usage on a child only -> parent line ends "incl. subtasks" */ }
#[tokio::test] async fn no_usage_no_line() { /* no p.usage-line */ }
```

- [ ] **Step 2: Run to verify they fail** — `cargo test --test ui_usage task_line` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass** — `cargo test --test ui_usage --test ui` → PASS.
- [ ] **Step 5: Docs, full gate, commit**

```bash
git add src/ui/task_page.rs templates/task.html tests/ui_usage.rs docs/web-ui.md
git commit -m "feat(ui): show a task's token usage on its page"
```

---

### Task 15: Architecture map and final verification (mechanical)

**Files:**
- Modify: `docs/architecture.md` (`src/usage/` and `src/service/usage.rs` in the tree; one paragraph: the hook reads harness logs, `/v1/usage` stores records and allocations, costs are derived and rewritten only by `axl usage reprice`)

- [ ] **Step 1: Update `docs/architecture.md`.**
- [ ] **Step 2: Full verification**

Run:
```bash
cargo fmt --check
cargo clippy --all-targets --features pg-tests -- -D warnings
cargo test
AXL_TEST_PG_URL=postgres://postgres:x@localhost:55439/postgres cargo test --features pg-tests
```
Expected: all clean and passing on both backends.

- [ ] **Step 3: Privacy and port sweep**

Run: `git grep -n "7420" -- tests src/usage` → no new hits in test or usage code. `git grep -nE "\.claude|\.codex" -- tests/usage_*.rs tests/ui_usage.rs` → only paths built under a temp home. `ls .superpowers 2>/dev/null; git status --short` → nothing under `.superpowers/` staged.

- [ ] **Step 4: Commit**

```bash
git add docs/architecture.md
git commit -m "docs: map the token usage modules"
```

---

## Follow-ups (not in this PR)

- **Retention roll-up** (spec, settled question 2): keep per-request rows 90 days (`AXL_USAGE_RAW_DAYS`), then roll up to daily rows per session and model, allocations with them. Needs its own migration and a design for how a rolled-up row keeps the dedupe key out of reach of late resends.
- **Codex cache writes** (settled question 3): confirm against real rollouts whether `cache_write_input_tokens` is inside `input_tokens`; if it is, subtract it in `codex/2`.
- **OpenCode** (spec PR 6) and **approach B** (OTLP), as the spec describes.

## Planner decisions (where this plan departs from or fills a gap in the spec)

1. Migration `0008_token_usage.sql`, not 0007 (ruling). Allocation key with `COALESCE` (ruling); Postgres casts `task_id::text`. Postgres columns use `uuid`, `timestamptz` and `bigint`, matching 0001.
2. `insert_usage` takes a `&dyn Pricer`, so a merged duplicate is re-priced from its merged counts inside the same transaction. `get_usage` is added as a read for tests and debugging.
3. The holdings query uses `SELECT DISTINCT` (a session that re-claims a task it still holds has two qualifying claims) and `IS DISTINCT FROM` on Postgres.
4. `requests` per group counts distinct records and is not split by parts. Tokens and cost are split, so only they add up across tasks.
5. Records are decoded one by one: a malformed record is rejected as `"malformed record"`, not a 400. Envelope errors also cover a bad `parser` id and over-long `harness_version` / `harness_session_id`. The rejection reason strings are fixed in Task 4.
6. The body is read by hand with a 256 KiB cap, so an oversize body is the spec's 400 and not axum's 413.
7. Price tiers: `long_context` is applied first, then `fast` (fast wins), then the defaults. Override rows get `price_ref = file:<version>#<id>`. The glob is `*` only. Built-in entries other than the spec's Opus example are copied from the vendors' pages at build time.
8. Prices load in `serve` (`AppState::with_prices`); `AppState::new` uses the built-in table, so tests are unchanged.
9. `claude-code/1` skips `<synthetic>` and all-zero lines, strips a trailing `[…]` from model ids, and falls back to `cache_creation_input_tokens` as 5m when the split object is absent.
10. `codex/1` drops `token_count` fallback records from any file that has `token_usage_record` lines (no double counting). It keeps the spec's cache-write mapping; the spike is deferred because tests cannot read real logs. The older-rollout fixture lives in `codex/legacy-token-count/`, since its version is unknown.
11. The cursor also stores `floor` (session file mtime), `skip_partial` (a line over 8 MB is skipped, not retried forever) and `warned`. `~/.axl/sessions/usage/status.json` holds the last successful report and the billing per harness for `axl status`.
12. `ClientConfig` gains `usage`; `axl login` keeps it (it used to rebuild the config from scratch).
13. `hook::run` takes an env lookup instead of `env_token`. Stderr lines carry the binary's existing `axl hook: ` prefix, not `axl: `.
14. Stop joins mail and usage under the same 2.5 s budget, and the mail output is not delayed past it. A transient send failure prints its line only when mail printed none.
15. UI: `range=all` was added so the task link covers all time. Days are UTC. Share is share of cost (share of tokens when there is no cost). Bars use `data-share` plus app.js because the CSP blocks inline styles. `model` and `task` are text filters, and `task=none` selects "no task".
16. `axl status` usage lines never carry a fix, so they never fail the status check.
