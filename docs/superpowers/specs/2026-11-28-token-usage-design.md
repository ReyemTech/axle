# Token usage and cost — design

Status: draft for the owner's review. Owner decisions 1 to 5 (below) are binding.

## Intent

The owner runs many agents and has no idea what they use. Axle knows who works on what;
this adds how many tokens it took and roughly what that costs at API prices.

## Owner decisions (binding)

1. **Purpose:** analytics ("an idea of usage and cost") per task, harness, host, agent and
   project, plus totals over time. No budgets or enforcement in v1.
2. **Cost:** every session shows an **API-equivalent cost**, labelled an estimate on a
   subscription. A per-model price table ships with Axle and config can override it. An
   unknown model shows tokens and "no price".
3. **Attribution:** a session holding several tasks splits its usage evenly across them.
   Usage while holding no task is "no task" for that session's project, harness and host.
4. **Collection (approach A):** `axl hook` on Stop (and SessionEnd) reads what the harness
   appended to its own log since the last read and POSTs the delta to `/v1`. A cursor file
   and a server-side dedupe key prevent double counting. Fail-open, bounded, never blocks.
   Parsers are per harness, versioned, with fixtures.
5. **Approach B (OTLP receiver)** is documented, not built.

## What each harness gives us (research)

Verified on this machine (Claude Code 2.1.329, Codex CLI 0.172.0, OpenCode 2.3.2) by
reading field names only, and against the vendors' docs (appendix).

| | Claude Code | Codex | OpenCode |
|---|---|---|---|
| Log | transcript JSONL at the hook's `transcript_path` | rollout JSONL in `~/.codex/sessions/` (the hook's `transcript_path`) | SQLite `~/.local/share/opencode/opencode.db` |
| Usage unit | `type: "assistant"` line, `message.usage` | `token_usage_record` line, `payload.usage`; older: `event_msg` `token_count` | `message.data` (assistant) and `step-finish` parts |
| Dedupe key | `requestId` (1:1 with `message.id`) | `payload.response_id` | message `id` |
| Model | `message.model` | latest `turn_context.model` | `providerID` + `modelID` |
| Stop hook | yes | yes (`turn_id`, `model`) | no shell hooks |
| SessionEnd | yes, non-blocking, 1.5 s shared budget | yes, 1 s default, 3 s max | n/a |
| Billing signal | `oauthAccount` in `~/.claude.json`; API-key env vars | `auth_mode` in `~/.codex/auth.json` | per provider; unverified |

**Claude Code:** `input_tokens` (excludes cache), `output_tokens`,
`cache_creation_input_tokens` (split in `cache_creation.ephemeral_5m_input_tokens` and
`ephemeral_1h_input_tokens`), `cache_read_input_tokens`,
`output_tokens_details.thinking_tokens`, `speed`, `inference_geo`,
`server_tool_use.web_search_requests`. Each content block is its own line repeating
`message.id`, `requestId` and an identical `usage` (2,213 messages checked, none differ).
Subagents write their own files in `<transcript dir>/<session id>/subagents/*.jsonl`.

**Codex:** `token_usage_record.payload` has `response_id`, `turn_id`, `session_id` and a
per-response `usage` (plus turn and thread totals) with `input_tokens`,
`cached_input_tokens`, `cache_write_input_tokens`, `output_tokens`,
`reasoning_output_tokens`, `total_tokens`. Older rollouts only have `token_count`
(`info.last_token_usage`). Input includes cached input (`input >= cached` on all 4,071
local records) and output includes reasoning, as in the OpenAI API.

**OpenCode:** per assistant message `tokens.input`, `.output`, `.reasoning`,
`.cache.read`, `.cache.write` and its own `cost`. No shell hooks, and the plugin event API
differs between v1 (`session.idle`) and v2 (`context.data.on`), so it is deferred (PR 6).

## Billing kind

Axle cannot see the bill. The hook decides per record, client side, in this order:

1. `[usage.billing]` in `~/.axl/config.toml`, per harness (`claude-code = "api"`). Always wins.
2. Detection, reading only key presence or a mode string, never a credential:
   - **Claude Code:** `api` if the hook's environment (inherited from the harness) has
     `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_USE_BEDROCK`,
     `CLAUDE_CODE_USE_VERTEX` or `CLAUDE_CODE_USE_FOUNDRY`, or settings carry
     `apiKeyHelper`. Else `subscription` if `~/.claude.json` has an `oauthAccount` object.
   - **Codex:** `auth_mode` in `~/.codex/auth.json`: `chatgpt` is `subscription`, `apikey`
     is `api`. Env vars are ignored: `OPENAI_API_KEY` is often set for other tools.
3. Otherwise `unknown`, shown like a subscription ("est.").

`claude auth status --json` knows too, but is too slow per Stop. The kind is cached in the
cursor file.

## Data model (migration 0007)

`0007_usage.sql` in both `migrations/sqlite/` and `migrations/postgres/`. 0004 to 0006 are
planned by other features; if they land under other numbers, this takes the next free one
at build time. Never edit or reuse a number. Postgres uses `BIGINT` and `COALESCE`.

```sql
CREATE TABLE usage_records (
    id TEXT PRIMARY KEY,
    harness TEXT NOT NULL,                -- from the session's agent, not the payload
    dedupe_key TEXT NOT NULL,             -- requestId / response_id / message id
    session_id TEXT NOT NULL REFERENCES sessions(id),
    agent_id TEXT NOT NULL REFERENCES agents(id),
    project_id TEXT REFERENCES projects(id),       -- the agent's home project
    token_id TEXT NOT NULL REFERENCES tokens(id),
    model TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,        -- uncached input only
    output_tokens INTEGER NOT NULL,       -- includes reasoning
    cache_write_5m_tokens INTEGER NOT NULL,
    cache_write_1h_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    reasoning_tokens INTEGER NOT NULL,    -- informational, never priced twice
    speed TEXT,                           -- 'fast' when the log says so
    billing TEXT NOT NULL CHECK (billing IN ('subscription', 'api', 'unknown')),
    cost_micros INTEGER,                  -- API-equivalent micro-USD; NULL = no price
    price_ref TEXT,                       -- 'builtin:2026-11-28#claude-opus-5-5'
    parser TEXT NOT NULL,                 -- 'claude-code/1'
    harness_session_id TEXT,              -- opaque; lets approach B map later
    occurred_at TEXT NOT NULL,            -- from the log
    received_at TEXT NOT NULL
);
CREATE UNIQUE INDEX usage_dedupe ON usage_records (harness, dedupe_key);
CREATE INDEX usage_time ON usage_records (occurred_at);
CREATE INDEX usage_session ON usage_records (session_id, occurred_at);
CREATE INDEX usage_project_time ON usage_records (project_id, occurred_at);

CREATE TABLE usage_allocations (
    usage_id TEXT NOT NULL REFERENCES usage_records(id),
    task_id TEXT REFERENCES tasks(id),    -- NULL = "no task"
    parts INTEGER NOT NULL                -- the record splits into this many shares
);
CREATE UNIQUE INDEX usage_alloc_key ON usage_allocations (usage_id, IFNULL(task_id, ''));
CREATE INDEX usage_alloc_task ON usage_allocations (task_id);
```

Host, harness and agent come from the session's agent row. Project is the agent's home
project on every row, task or not, so the project breakdown always adds up; task is a
separate dimension.

**Allocation rows, not a computed split:** at ingest the service works out which tasks the
session held at `occurred_at` and writes one row per task with `parts = n`, or one
`task_id NULL, parts = 1` row. Queries sum `tokens / parts`. Holdings at that time are
known at ingest; recomputing them per page load would replay task history for every
record, and the split stays stable when a task is later released or re-claimed.

"Held at `occurred_at`" comes from `task_events` (Controller's call). A task is held from
this session's `claimed` or `lease_expired_takeover` (a takeover is the new owner's claim)
until its `released`, `done`, `cancelled` or `force_released`, or another session's `claimed`
or `lease_expired_takeover`. Blocked, review and an expired
lease nobody re-claimed still count as held. No new event kind. The query: tasks whose
latest such event at or before `occurred_at` is this session's claim.

```sql
SELECT e.task_id FROM task_events e
WHERE e.kind IN ('claimed', 'lease_expired_takeover')
  AND e.actor_session_id = :s AND e.created_at <= :t
  AND NOT EXISTS (SELECT 1 FROM task_events x
    WHERE x.task_id = e.task_id AND x.created_at > e.created_at AND x.created_at <= :t
      AND (x.kind IN ('released', 'done', 'cancelled', 'force_released')
           OR (x.kind IN ('claimed', 'lease_expired_takeover')
               AND x.actor_session_id IS NOT :s)))
```

This matters: the turn that finishes a task ends after `task_complete`, so the holdings
at ingest time would put the whole turn under "no task".

**Duplicates merge upward:** an existing `(harness, dedupe_key)` keeps the larger value of
each count (`GREATEST` / `MAX` in the upsert) and its first session and allocation. A
resent record is a no-op; a line written early with partial usage is corrected later.

`cost_micros` is computed at ingest and changed only by `axl usage reprice`. It is derived
data, not history, so the append-only rule does not apply.

## Prices

A TOML table embedded in the binary (`src/usage/prices.toml`), written by hand from the
vendors' pricing pages at release, dated, with its sources listed in the file. USD per
million tokens:

```toml
version = "2026-11-28"

[[model]]
id = "claude-opus-5-5"
match = ["claude-opus-5-5-*", "anthropic/claude-opus-5-5*"]
input = 4.00
output = 20.00
cache_write_5m = 5.00
cache_write_1h = 8.00
cache_read = 0.20
fast = { input = 8.00, output = 40.00 }   # optional, used when speed = "fast"
# optional: long_context = { above = 100000, input = …, output = …, … }  (Haiku 5.5)
```

- `cache_write_1h` defaults to `cache_write_5m`; cache prices default to `input`. OpenAI
  models carry `input`, `cache_read` and `output` only.
- `long_context.above` compares the request's whole prompt (input plus cache), as billed.
- Match exact `id`, then the longest `match` glob; none: NULL cost, "no price".
- Tokens × $/MTok is micro-dollars: `cost_micros = round(Σ tokens_k × price_k)` per record.

**Override:** `AXL_PRICES_FILE=/etc/axl/prices.toml` on the server, same format. Entries
replace built-ins by `id`; new ids are added. A bad file stops the server at start, naming
file and line. **`axl usage reprice [--since <date>] [--unpriced]`** (server side)
recomputes `cost_micros`; `--unpriced` touches only NULL rows.

## `POST /v1/usage`

Bearer token like every `/v1` route. The session must belong to the token: unknown
session 404, another token's session 403, as for `/v1/heartbeat`.

```json
{ "session_id": "0199a000-…", "parser": "claude-code/1", "harness_version": "2.1.329",
  "harness_session_id": "8d3f…",
  "records": [{ "key": "req_011…", "at": "2026-11-28T10:04:05.123Z", "model": "claude-opus-5-5",
                "input": 12, "output": 845, "cache_write_5m": 0, "cache_write_1h": 3120,
                "cache_read": 48211, "reasoning": 210, "speed": null, "billing": "subscription" }] }
```

- Answers `{"accepted": 1, "duplicate": 0, "rejected": []}`, in one transaction. Envelope
  errors (bad JSON, no session, over 500 records, body over 256 KB) are 400, store nothing.
- A bad record goes in `rejected` as `{key, reason}` and the rest are stored: model over
  128 bytes or outside `[A-Za-z0-9._:/@-]`, key over 200 bytes, a count below 0 or over
  10⁹, `at` more than 5 minutes ahead or 30 days old.
- One request against the token's rate limit. Not a heartbeat, renews no lease (a report
  is not evidence the agent is alive now), publishes no event in v1.

## Collector (`axl hook`)

New module `src/usage/`: `collect.rs` (cursor, budget, POST) and `parsers/`, one per
harness. The hook calls it on Stop and SessionEnd.

**Wiring:**

- Claude Code: `axl install` adds `SessionEnd` (`axl hook session-end`, `timeout: 3`,
  which raises the 1.5 s shared budget). Stop also reads the subagent transcripts next to
  the main one, so SubagentStop is not needed.
- Codex: `axl install --harness codex` adds `SessionEnd` with `timeout = 3`, the maximum.
  Codex asks the user to trust the new hook, as today.
- On Stop it runs concurrently with the inbox fetch, in the same 2.5 s budget and 4 s
  ceiling; the urgent-mail block never waits for it. SessionEnd: collector only, 1 s.
- The hook reads `transcript_path` from stdin. The session comes from `find_session`, or
  from the cursor once the bridge has gone (below).
- Usage is collected **before** the `stop_hook_active` loop guard, so continuation turns
  are counted; the guard still prevents blocking (Controller's call).

**Cursor file:** `~/.axl/sessions/usage/<sha256 of transcript path>.json`, mode 0600,
written atomically. It holds the path, `dev`, `ino`, byte `offset`, parser id, parser
state (Codex's current model), the billing kind and `updated_at`. It also caches the last
Axle session id and server URL, never the token, so Stop and SessionEnd can report after
the bridge removed its session file. The server accepts usage for an ended session (the
row persists) as long as it belongs to the token (Controller's call).

- Keyed by transcript, so the bridge's cleanup cannot delete it before SessionEnd, and
  `--resume` continues it. New records go to the session `find_session` returns now.
- Read from `offset` to the last complete newline; a partial line waits.
- Advance only after a 2xx. On timeout, 429, 5xx or any send failure keep it; the next
  Stop or SessionEnd resends and the server dedupes. No separate rate bucket in v1; each
  send is capped at 500 records and the rest goes next time (Controller's call). On 403, 404 or a 400 envelope error advance anyway, with one stderr
  line, so a bad batch cannot loop forever.
- A file that shrank or has a new inode restarts at 0; dedupe makes that safe.
- A new cursor starts at 0 but drops records older than the session file's mtime, so
  installing Axle does not import a transcript's earlier history.
- Per run: at most 8 MB per file, 500 records, 300 ms of parsing; the rest waits. Cursors
  untouched for 30 days are deleted on SessionEnd.

**Parsers** each have an id, the files to read for a hook input, and a line parser.

- `claude-code/1`: `type == "assistant"` lines with `message.usage`. Key `requestId`, else
  `message.id`; repeated lines dedupe in the run. `at` is the line's `timestamp`.
- `codex/1`: `token_usage_record`, key `response_id`; `input = input_tokens −
  cached_input_tokens`, `cache_read = cached_input_tokens`, `cache_write_5m =
  cache_write_input_tokens`. Fallback without `token_usage_record`: `token_count`'s
  `last_token_usage`, key `<session_meta.id>:<ordinal>`, only when `total_token_usage`
  has advanced past the last one counted (a repeated snapshot counts nothing).
- Unknown lines are skipped. A file with usage-shaped lines but no record prints once per
  session `axl: usage parser claude-code/1 does not recognise this log`.
- A changed format gets a new id (`claude-code/2`); both stay while old logs exist.

**Off switch:** `AXL_USAGE=off` or `[usage] enabled = false`. `axl status` shows it, the
billing kind per harness and the last successful report.

## Privacy

Only counts, model names, opaque ids (request, response, harness session) and timestamps
leave the machine. Never prompt or response text, tool input, file paths, `cwd`, git data,
email or organisation. Parsers copy named numeric fields into a fixed struct; nothing is
forwarded wholesale. Credential files are read for one mode field, never logged.
`docs/security.md` says exactly this.

## Web UI

**Usage page** (`/ui/usage`, new top-bar tab **Usage**). One GET form, all state in the
query string, so a view can be bookmarked:

- **Range** (Today, 7 days by default, 30 days, From/To); **filters** for project, agent,
  harness, host, task, model and billing; **group by** project (default), task, agent,
  harness, host, model or day.

Top to bottom:

1. **Totals**, one ruled counter line like the overview: input, output, cache write and
   cache read tokens, **API-equivalent cost**, and "of which subscription (est.)". Compact
   tabular numbers (`1.24M`), exact values in `title`.
2. **Breakdown table:** name, requests, the four token kinds, cost, share. The share bar
   sits beside its percentage, in ink, not a signal colour (The Signal Is State Rule).
   Projects carry their line swatch. "no task" and "no price" are spelled out. Sorted by
   cost, unpriced last.
3. **Over time:** one row per day (per hour for Today) with tokens, cost and a bar plus
   the same text. No chart library.

The cost column header says "API-equivalent cost". A row partly or wholly on a
subscription or unknown billing adds `est.` after the amount. The page carries one
sentence: "Estimated at API prices; subscription use is not billed per token."

**Task page:** one line in the lease panel's meta: `Usage 1.24M tokens · $3.40
API-equivalent est. · 3 sessions`, linking to `/ui/usage?task=<id>&group=agent`. A parent
adds `incl. subtasks` and sums its descendants. No usage, no line.

**Board cards and overview:** unchanged in v1. A per-card total adds a row to every card
for a number the owner does not act on at a glance, and it would change every turn.

## MCP

No new tool or field. Agents do not act on cost without budgets, and every field costs
context on every call. With budgets, `task_get` can gain a `usage` summary.

## Failure handling

| Failure | Behaviour |
|---|---|
| No session file, no token, usage off | Nothing; silent. |
| Transcript unreadable; parser recognises nothing | One stderr line (once per session). |
| Server down, timeout, 5xx; budget runs out | Cursor stays; the next Stop resends. |
| 403, 404, 400 envelope; some records rejected | Cursor advances; one stderr line. |
| Unknown model | Stored with NULL cost, "no price". |

The hook stays strictly fail-open: exit 0, at most one stderr line per run (mail first).

## Testing

- **Store** (`store_matrix!`, both backends): insert; duplicate no-op; merge upward;
  allocation with 0, 1 and 3 tasks; holdings at `occurred_at` across claim, complete and
  release; each dimension's totals equal the grand total; another token's session refused.
- **Prices:** each kind, `fast`, `long_context`, 1h fallback, glob precedence, unknown
  model, override merging, bad override file.
- **HTTP:** auth, 403/404, both caps, partial rejection, rate limit.
- **Parsers:** fixtures in `tests/fixtures/usage/<harness>/<version>/` (Claude Code
  2.1.329 main and subagent, Codex 0.172.0, an older `token_count`-only rollout) with
  expected records. Synthetic: real shapes, every text field `"…"`, enforced by a test.
- **Collector:** advance only on 2xx; partial line kept; shrunk file restarts; old records
  dropped; 8 MB cap continues next run; budget held against a server that never answers;
  nothing sent when off. **Hook:** the urgent-mail block still prints when the POST hangs.
- **UI:** filters, group-by, "no task", "no price", `est.`, the task line.

## Approaches

**A. The hook reads the harness's own log (chosen).** Hooks fire at turn boundaries, the
natural flush points, and Claude Code and Codex hand them the log path. No harness config
beyond hooks Axle already installs; works offline (the cursor waits). The cost is tracking
log formats per version, hence versioned parsers and fixtures.

**Why not tail from the bridge.** No harness tells an MCP server where its transcript is,
so the bridge would guess. It has no turn boundary to flush on, so it would poll. And it
would add file watching to the process that must always answer tool calls.

**B. OTLP receiver (later, not built).**

- **Claude Code** (`CLAUDE_CODE_ENABLE_TELEMETRY=1`, `OTEL_LOGS_EXPORTER=otlp`): the
  `claude_code.api_request` event has `model`, `input_tokens`, `output_tokens`,
  `cache_read_tokens`, `cache_creation_tokens`, `cache_creation_5m_tokens`,
  `cache_creation_1h_tokens`, `cost_usd`, `request_id`, `speed` and `session.id`.
  `request_id` is the transcript's `requestId`, which is why A keys on it.
- **Codex** (`[otel]` in `config.toml`, `exporter = "otlp-http"`): `codex.sse_event` on
  `response.completed` carries token counts, `turn.token_usage` is a histogram by
  `token_type`, metrics are tagged `auth_mode` and `model`. Exact count field names are
  not documented; unverified.
- **OpenCode:** community plugins or an experimental flag; unverified.

What B would take:

- `POST /v1/otlp/v1/logs` (OTLP/HTTP, protobuf and JSON): `opentelemetry-proto` and
  `prost`, a heavier binary, a second body format to cap.
- Auth: `OTEL_EXPORTER_OTLP_HEADERS` puts a token in the harness's environment, which the
  docs advise against; it needs an ingest-only token kind.
- Session mapping: OTel carries the harness's session id. A stores `harness_session_id`
  from day one, so B can map it to the Axle session. B alone, with no A record, cannot
  find the session, so it cannot split per task.
- Dedupe: one `api_request` maps to one row with `dedupe_key = request_id`, so A and B
  merge upward on the same row. For Codex, confirm the OTel and rollout ids match first.

B is worth it if a harness stops logging usage locally, or has telemetry but no hooks.

## PR split

1. **Store and API:** migration 0007 (both backends), store methods, prices with
   `AXL_PRICES_FILE`, `POST /v1/usage`, tests. Docs: `http-api.md`, `configuration.md`.
2. **Claude Code collector:** `src/usage/`, `claude-code/1`, cursor, billing detection,
   Stop and SessionEnd wiring, install adds SessionEnd, fixtures. Docs:
   `harnesses/claude-code.md`, `security.md`, `cli.md`.
3. **Codex collector:** `codex/1` with the `token_count` fallback, SessionEnd in the
   Codex block, fixtures. Docs: `harnesses/codex.md`.
4. **UI:** Usage page, task line, top-bar tab. Docs: `web-ui.md`, screenshots.
5. **`axl usage reprice`**, and refreshing the price table in `releasing.md`.
6. **OpenCode (later):** an installed plugin that runs `axl hook stop --harness opencode`
   when a turn finishes, and `opencode/1` reading `opencode.db` read-only. Waits for a
   stable plugin event API.

## Settled questions (Controller's call)

1. **Subscription value:** no plan price shown in v1.
2. **Retention:** per-request rows are kept 90 days (`AXL_USAGE_RAW_DAYS`), then rolled up
   to daily rows per session and model; allocations roll up with them.
3. **Codex cache writes:** whether `cache_write_input_tokens` is inside `input_tokens` is
   a builder spike with fixtures in PR 3.
4. **Surcharges:** no `inference_geo` or web-search pricing in v1; a future addition.
5. **OpenCode cost:** use its own per-message `cost` when present and the harness is
   API-billed; otherwise our table.
6. **Live updates:** none on the Usage page in v1.

## Appendix: sources

- Claude Code hooks (common input `session_id`, `transcript_path`; Stop
  `stop_hook_active`; SubagentStop `agent_transcript_path`; SessionEnd `reason` and its
  1.5 s budget, up to 60 s): https://code.claude.com/docs/en/hooks
- Claude Code monitoring (`claude_code.api_request` fields, `request_id`,
  `claude_code.token.usage`, `claude_code.cost.usage`, auth attributes):
  https://code.claude.com/docs/en/monitoring-usage
- Claude pricing (input, 5m and 1h cache writes, cache reads, output; fast mode; Haiku 5.5
  long-prompt tier; data residency): https://platform.claude.com/docs/en/about-claude/pricing
- Codex hooks (Stop input `turn_id`, `model`, `stop_hook_active`; SessionEnd 1 s default,
  3 s max): https://learn.chatgpt.com/docs/hooks.md
- Codex `[otel]` (exporters, `codex.sse_event`, `turn.token_usage`, `auth_mode`):
  https://learn.chatgpt.com/docs/config-file/config-advanced
- OpenCode plugins: https://opencode.ai/docs/plugins/ and the v2 plugin API
  (`context.data.on`, `session.execution.succeeded`): https://opencode.ai/v2/docs/build/plugins/cli
- Local inspection, field names only: Claude Code transcripts, `claude auth status --json`
  keys, `~/.claude.json` key `oauthAccount`; Codex rollouts, `~/.codex/auth.json` key
  `auth_mode`; the OpenCode `opencode.db` schema and `data` JSON keys.
