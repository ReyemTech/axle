# AGENTS.md

Guidance for coding agents (Claude Code, Codex, OpenCode) working in this repository.

Axle is a self-hosted Rust MCP server: coding agents on different machines and harnesses send
each other durable messages and claim tasks under renewable leases, with a live web UI for the
human owner. One crate (`axle-mcp`) builds one binary, `axl`, which is the server
(`axl serve`), the per-session stdio bridge (`axl bridge`), the harness hook (`axl hook`), and
the admin and onboarding CLI (`install`, `login`, `status`, `service`, `token`, `project`, …).

## Commands

```sh
cargo fmt --check
cargo clippy --all-targets --features pg-tests -- -D warnings   # CI runs this exact line
cargo test                                                      # SQLite backend only
AXL_TEST_PG_URL=postgres://postgres:x@localhost:5432/postgres cargo test --features pg-tests
bash tests/helm.sh     # Helm chart: lint, render and assert every mode (needs helm)
bash tests/smoke.sh    # Docker image end to end (needs docker; AXL_SMOKE_IMAGE reuses an image)
DATABASE_URL=sqlite://./axl.db cargo run -- serve   # local server on 127.0.0.1:7420, UI at /ui
```

Single test binary / single test:

```sh
cargo test --test store_tasks                 # one file under tests/
cargo test --test store_tasks lease_boundary              # filter by name
cargo test --test store_tasks lease_boundary::sqlite      # one backend
```

Store tests are written once with `store_matrix!(name, |store| async move { … })`
(`tests/common/mod.rs`), which expands to `name::sqlite` (always) and `name::postgres` (only
with `--features pg-tests`, a fresh schema per test). New store behaviour gets a
`store_matrix!` test, never a SQLite-only one.

## Architecture (the parts that span files)

`docs/architecture.md` maps the tree; the load-bearing ideas:

- **Layers:** `src/http/` (axum router: probes, `/v1`, `/mcp`, `/ui`, bearer auth, rate
  limits) → `src/mcp/` (the 18 MCP tools on rmcp) and `src/ui/` → `src/service/` (business
  rules shared by MCP, `/v1` and the UI) → `src/store/` (`Store` trait with `sqlite.rs` and
  `postgres.rs`). Rules live in services and in the store's guarded SQL, not in handlers.
- **Two stores, one behaviour.** Every store method exists in both backends with identical
  semantics. Guarded changes (claims, status transitions) are a single statement or
  transaction whose `WHERE` clause is the rule, so a race has exactly one winner. The current
  time is computed in Rust (`src/clock.rs`) and passed in, never `now()` in SQL.
- **Migrations** live in `migrations/sqlite/` and `migrations/postgres/`, numbered in step
  (`0001_…`, `0002_…`). They are embedded and run at every start. Add a new numbered file to
  both; never edit an existing migration.
- **History is append-only:** task events and messages are never rewritten; an edit records
  the previous value. "Archive"/"ack" hides, it does not delete.
- **Event bus** (`src/events.rs`): after each commit an outbox row is written, then broadcast
  in process. `/v1/events` and the UI's `/ui/live` SSE stream consume it. Events are hints;
  durable state is always re-read from inboxes and tasks. In-process state is why the server
  runs as a single replica.
- **Identity is never a tool argument.** The bridge adds the token and `X-Axl-Host/Harness/
  Project/Session` headers; the server resolves project → agent → session. Peer-written text
  is returned framed as information, not instructions.
- **Bridge and hook** (`src/bridge/`, `src/hook/`) run on agent machines. Hooks are strictly
  fail-open: an Axle outage must never block a harness.
- **Install** (`src/install/`, `src/daemon/`) edits Claude Code, Codex and OpenCode configs
  with timestamped backups and an install manifest, and writes launchd/systemd user units.
- **Web UI** is server-rendered askama templates (`templates/`) plus htmx and its SSE
  extension, vendored in `static/` and embedded in the binary. No JS framework, no build step,
  strict CSP (no external fonts, scripts or styles), CSRF on every POST.

## Rules

- UI work follows `DESIGN.md` (tokens, components, named rules) and `PRODUCT.md` (density is a
  feature; no glyph or colour carries meaning alone; light and dark themes).
- Conventional commits drive releases (release-plz, `release-plz.toml`): `feat`, `fix`,
  `perf`, `refactor`, `revert` or any `type!:` start a release; `docs`, `chore`, `ci`, `test`,
  `style` do not. The release PR writes `CHANGELOG.md`; don't hand-edit released sections.
- Update the user docs under `docs/` (`mcp-tools.md`, `configuration.md`, `cli.md`,
  `web-ui.md`, …) in the same change as anything a user would notice.
- Larger changes get a design doc in `docs/superpowers/specs/` and a plan in
  `docs/superpowers/plans/` before they are built. `.superpowers/` (no `docs/`) is git-ignored
  scratch; never commit it.
- Releasing (tags, dist, Homebrew, crates.io, GHCR) is documented in `docs/releasing.md`.
