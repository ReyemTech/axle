# Onboarding: install, login, enrollment and local service — design

Status: draft for owner review (2026-11-27). Wake adapters are a separate, later spec.

## Why

Today a machine joins Axle entirely by hand. Someone with database access on the server runs
`axl token create`, the token gets copied to `~/.axl/token`, and then each harness's config
files are edited from snippets in `docs/harnesses/`. That is fine for the author and a wall
for a beginner. The owner wants getting started to be easy for beginners:

- **On a laptop with no server:** one command, and Claude Code, Codex and OpenCode are talking
  through Axle.
- **With a deployed server:** the human copies one command from the web UI, pastes it on the
  machine, and it is connected, credentials included.
- **With the CLI already installed:** `axl login https://axle.example.com` connects it.

Success: a beginner runs one command and their agents can message each other and claim
tasks. Nothing to keep open in a terminal, no token copying, no config editing.

## Goals

1. `axl install`: detect the installed harnesses and configure each one (MCP bridge, hooks,
   instructions). It can be run again safely, and `axl uninstall` undoes it.
2. A local server that runs as a background user service (launchd on macOS, systemd on
   Linux), set up by `axl install` when no server is configured.
3. One-time enrollment codes created in the web UI, used by a `curl … | sh` script that the
   server hosts.
4. `axl login <url>`: a browser-approval login for a machine that already has `axl`.
5. `axl status`: one screen that says what's connected and what's broken.

## Non-goals

- Waking idle agents (the next spec). This spec only leaves room for it: the Claude Code
  plugin and the Codex hooks block are where wake-up gets added later.
- Windows (on the roadmap for after 0.1).
- OAuth for claude.ai connectors, multiple users, roles.
- Installing the harnesses themselves.
- Repo-local (committed or per-project) harness config. `axl install` is user-level only;
  the manual snippets in `docs/harnesses/` still cover repo-local setups. *(Builder-check
  call: a committed file can't hold an absolute binary path, and nobody asked for it.)*

## Anti-goals (failure even if it "works")

- A token or enrollment secret printed to the terminal, written to a log, or put in a URL
  query string. (The one-time code in the pasted command is the single exception. It is
  short-lived and single-use, so shell history doesn't leak a usable credential.)
- Damaging a user's existing harness config: other MCP servers, hooks, comments, formatting.
- Needing root or sudo.
- A script that silently does something different from `axl install`. All the logic lives
  in the binary.

## Approaches considered

1. **Everything in the `axl` binary (chosen).** New commands `install`, `uninstall`, `login`,
   `status` and `service` edit harness config files directly, keeping their formatting. The
   server adds enrollment endpoints and serves a tiny `install.sh` that only downloads `axl`
   and calls it. The logic is in one tested place, the script is short enough to audit, and
   it works the same with or without the script.
2. **A large `install.sh` with `sed`/`jq`.** Rejected: fragile against real-world configs,
   hard to test, needs `jq`, and duplicates `axl install` anyway.
3. **Shell out to harness CLIs (`claude mcp add`, `codex mcp add`).** Rejected as the primary
   path: those CLIs must be on PATH with stable flags, and hooks, plugins and instructions
   aren't covered, so file edits are needed anyway.

The owner chose 1, and chose that `axl install` also sets up a local server when none is
configured (instead of assuming one exists).

## Design

### 1. Client configuration

- `~/.axl/config.toml` (new): `server_url = "https://…"`. Written by `axl login` and by local
  installs.
- `~/.axl/token` (existing): the host token, mode 0600.
- Server URL precedence everywhere (bridge, hooks, status): `AXL_SERVER_URL`, then
  `config.toml`, then `http://127.0.0.1:7420`. The token keeps its current order:
  `AXL_TOKEN`, then `~/.axl/token`.
- Harness configs written by `axl install` don't contain the token or the URL. The bridge
  reads both from `~/.axl/`, so switching servers is just `axl login` again.
- `~/.axl/` is mode 0700.

### 2. Enrollment (server)

A new `enrollments` table, added by migration `0003_enrollment.sql` for both SQLite and
Postgres. Columns:
- `id`, `kind` (`code` | `device`)
- `secret_hash`: SHA-256 of the one-time code, or of the device code for `device`
- `user_code_hash`: device flow only
- `label`, `host`
- `status`: `pending` | `approved` | `consumed` | `expired` | `denied`
- `token_id`: set when consumed
- `created_at`, `expires_at`, `approved_at`, `requester_ip`

**One-time codes (for the pasted command).**
- **Creating one.** On the Tokens page, a **Connect a machine** form takes a label, for
  example "work laptop". It creates a `code` enrollment that is valid for **15 minutes** and
  can be used **once**.
- **What the page shows.** The code is shown once, together with two commands ready to copy:
  - `curl -fsSL https://<server>/install.sh | sh -s -- <CODE>`
  - `axl login https://<server> --code <CODE>`
- **Code format.** 10 characters of Crockford base32 split into two groups, for example
  `K7QM4-ZP2RX`. It is easy to type and has roughly 50 bits of entropy, enough given the
  single use, the expiry and the rate limit.
- **Exchanging it.** `POST /v1/enroll {code, host}` needs no auth.
  - **Success:** it atomically moves the enrollment from `pending` to `consumed`, creates a
    host token labelled `<label> (<host>)`, and returns `{token, server_url}`.
  - **Failure:** the response is always the same `invalid or expired code`.

**Device flow (for `axl login <url>` without a code).**
1. **Start.** `POST /v1/login/start {host}` returns
   `{device_code, user_code, verify_url, interval, expires_in}`.
   - The `device_code` is the secret `axl` polls with. It is never shown to the user.
   - The `user_code` is short (for example `WXYZ-1234`) and shown in both the terminal and
     the browser.
   - The `verify_url` is `https://<server>/ui/connect`. The user code is not put in the URL;
     the user types it or confirms it.
   - It expires after **10 minutes**.
2. **Open the browser.** `axl` opens the `verify_url` (or prints it with `--no-browser`).
3. **Sign in and enter the code.** The human signs in with the normal UI auth (any
   `AXL_UI_AUTH` mode) and enters the user code. The page shows the host name, the
   requester's IP and when the request was made, then asks **Connect this machine?** with
   Approve and Deny buttons. The form uses the existing CSRF protection.
4. **Poll.** `POST /v1/login/poll {device_code}` returns `authorization_pending`,
   `slow_down`, `denied`, `expired`, or `{token, server_url}` once approved. Getting the
   token consumes the enrollment.

**Common rules.**
- Only hashes are stored.
- Consuming is a guarded UPDATE, the same pattern as task transitions, so two racing
  requests can't both get a token.
- Expired rows are cleaned up opportunistically.
- **Rate limits:** the unauthenticated endpoints (`/v1/enroll`, `/v1/login/*`) get a new
  per-IP limiter, separate from the per-token one. Suggested: 10 requests per minute per IP.
  `AXL_TRUSTED_PROXY_HEADER` (for example `X-Forwarded-For`) is honoured when set.
- **Live updates:** both kinds emit an event so the Tokens page updates live. It goes on the
  existing event bus/outbox as a non-task event, because `task_events` requires a
  `task_id`.

### 3. `GET /install.sh` (server)

- **Content.** A POSIX `sh` script rendered from a template, with the server URL baked in.
- **Where the URL comes from** *(builder-check call)*:
  - **`AXL_PUBLIC_URL`**, for example `https://axle.example.com`. It is required for
    `install.sh` and the UI's connect commands whenever the server binds to a non-loopback
    address. If it is unset there, `/install.sh` returns 503 with a message naming the
    variable, and the Connect panel shows the same hint.
  - **The request's Host**, used only on loopback binds, where it is always `127.0.0.1` or
    `localhost`.
  - **Why not just use Host:** today Host is only validated in `none` auth mode, and behind a
    TLS proxy the scheme reads as `http`.
  - **Helm:** the chart sets `AXL_PUBLIC_URL` from the ingress host and TLS settings.
  - **Docker:** the docs show the variable.
- **What the script does:**
  1. Refuses to run as root unless `AXL_ALLOW_ROOT=1` is set.
  2. Detects the OS and architecture (macOS or Linux, x86_64 or arm64) and refuses anything
     else with a clear message.
  3. Gets a matching `axl`. If `axl` is already on PATH at the server's version or newer, it
     uses that. Otherwise it runs the dist shell installer for **the server's own version**,
     which installs into `~/.cargo/bin` (the existing dist config).
  4. Runs `axl install --server <server> --code "$1"`, which logs in first.
  5. Prints the `axl status` summary.
- **Release pinning.** The version comes from the server (`env!("CARGO_PKG_VERSION")`), so a
  machine gets a CLI that matches its server. The download URL is GitHub Releases,
  `https://github.com/ReyemTech/axle/releases/download/v<ver>/axle-mcp-installer.sh`.
- **Without a code** (`curl … | sh`): the script runs `axl install --server <server>`, and
  install's login step uses the device flow.
- **Transport check.** If the server URL is plain `http://` and not loopback, the script
  prints a warning and asks for confirmation. When stdin is not a terminal, it reads the
  answer from `/dev/tty`.

### 4. `axl login [URL] [--code CODE] [--no-browser] [--force]`

- **Choosing the flow.** With `--code` it exchanges the code. Without it, it runs the device
  flow.
- **What it writes.** `~/.axl/config.toml` (the `server_url`) and `~/.axl/token`, both mode
  0600.
- **Switching servers.** If a token for a *different* server is already configured, it
  refuses without `--force`. A beginner shouldn't silently disconnect from their team
  server.
- **Output.** It prints `Connected to <url> as <host>`. The token itself is never shown.
- `axl logout` deletes the local token (and, with `--revoke`, revokes it on the server
  first).

### 5. `axl install [--server URL] [--code CODE] [--harness NAME]... [--local] [--dry-run] [--yes]`

**Step 1: make sure there's a server.**
- If a server is configured, check it with `/readyz`.
  - **If the check fails** *(builder-check call)*: configure the harnesses anyway, since the
    bridge is fail-open and the config is valid regardless. Then show a prominent warning in
    the summary with the likely fixes: check the VPN or URL, run `axl status`, run
    `axl login` again.
  - **Never switch to local mode silently.** Only `--local` does that.
- **Login when needed** (owner's call). Login runs inside install, so a beginner never has to
  know about `axl login`:
  - **`--server URL`** (optionally with `--code`): if this machine has no token for that
    server, install runs the `axl login` flow first. That is the code exchange, or the
    device flow when there's no code.
  - **No server configured, interactive terminal:** install asks **"Run Axle on this
    machine, or connect to a server?"**
    - **This machine:** local mode, below.
    - **A server:** prompt for the URL, then run the device-flow login inline.
  - **No server configured, non-interactive** (`--yes` or no TTY): local mode, unless
    `--server` is given.
- In local mode (chosen above, or `--local`):
  1. Run `axl service install`.
  2. Wait for `/readyz`.
  3. Mint a host token straight from the local SQLite database (the same code path as
     `axl token create`, with `DATABASE_URL` pointing at `~/.axl/axl.db`).
  4. Write the token and `config.toml`.

**Step 2: detect harnesses.**
- A harness counts as present if its config directory exists *or* its binary is on PATH:
  - Claude Code: `~/.claude/` or `claude`
  - Codex: `~/.codex/` or `codex`
  - OpenCode: `~/.config/opencode/` or `opencode`
- `--harness` limits the run to the named ones.
- Missing harnesses are listed as "not found", not treated as errors.

**Step 3: configure each harness.**
- **Scope.** User level only (see Non-goals).
- **Binary path** *(builder-check call)*. Every command written uses an absolute path to
  `axl`, because harness processes may have a different PATH. The path must survive
  upgrades:
  - **Never resolve symlinks.** Homebrew's `/opt/homebrew/bin/axl` points into
    `Cellar/axle/<version>/`, which `brew upgrade` deletes.
  - **Preference:** first the `axl` found on PATH, if it is the same file as
    `current_exe()` (same device and inode); otherwise `current_exe()` as reported, not
    canonicalised.
  - **A versioned Cellar path:** if the only path found is one, map it to the Homebrew
    prefix `bin/axl`.
  - **Checks:** `axl status` and `axl install` report a configured path that no longer
    exists, and `install` rewrites it.
- **Backups.** Before the first change to any existing file, it is copied to
  `<file>.axl-backup-<UTC timestamp>`.
- **Existing hand-made setups** *(builder-check call)*. The docs so far told people to add an
  unmarked `axle` MCP entry, with `env.AXL_SERVER_URL`, and bare `axl hook …` hooks. Install
  **adopts and replaces** those:
  - Any MCP entry named `axle` and any hook whose command is `axl hook <event>` (at any path)
    counts as Axle's.
  - Each one is replaced by the managed form. With the Claude Code plugin, that means
    removing the user-scope `axle` MCP entry and the bare hooks, so they don't run twice.
  - If `~/.axl/config.toml` has no `server_url` yet, the `AXL_SERVER_URL` from the old entry
    moves there.
  - Everything install touched (added, replaced, or adopted) is recorded in
    `~/.axl/install-manifest.json`, with the original snippet for replaced entries.
    Uninstall uses this manifest: it removes what install added, and for adopted entries it
    removes the managed form without restoring the hand-made one. The backups keep the
    originals.
  - Install says which entries it adopted.
- **Claude Code**
  - **Plugin.** A local plugin directory owned by Axle, `~/.axl/integrations/claude-code/`:
    - `.claude-plugin/plugin.json`
    - `.mcp.json`, which runs `axl bridge --harness claude-code`
    - `hooks/hooks.json`, for SessionStart, UserPromptSubmit, PostToolUse and Stop, each
      `axl hook <event>` with a 5 s timeout
    - `skills/axle/SKILL.md`, the instructions now in `docs/harnesses/claude-code.md`
  - **Loading it.** The plugin is linked into `~/.claude/skills/axle`. That is how Concord
    loads its `concord-relay` plugin without a marketplace (prior art in the owner's own
    setup). **The builder must check this loading mechanism against the current Claude Code
    first** (see "Left to the builder"). If it doesn't load, fall back to user-scope entries:
    - `mcpServers.axle` in `~/.claude.json`
    - the four hooks in `~/.claude/settings.json`, each tagged with
      `"_axl": true`-style identification, or matched by command, so uninstall can find them
    - an Axle block in `~/.claude/CLAUDE.md`
  - **Why a plugin.** Wake-up will add a plugin monitor here later.
- **Codex**
  - In `~/.codex/config.toml`, a `[mcp_servers.axle]` table runs
    `axl bridge --harness codex`.
  - A `[[hooks.*]]` block sits between `# >>> axle >>>` and `# <<< axle <<<` markers. It
    covers SessionStart, PostToolUse and Stop, each `axl hook <event>`. `axl hook` must learn
    Codex's payloads, or the block stays MCP-only. That's the builder's call, and it needs a
    test.
  - An Axle block goes in `~/.codex/AGENTS.md`, between the same markers.
  - The user is told that Codex asks them to trust new hooks on the next start.
  - Edits use `toml_edit`, so comments and ordering survive.
- **OpenCode**
  - `mcp.axle` in `~/.config/opencode/opencode.json` (or `opencode.jsonc`) runs
    `axl bridge --harness opencode`.
  - An Axle block goes in `~/.config/opencode/AGENTS.md`.
  - JSONC must keep its comments. If a comment-preserving edit isn't possible, print the
    exact snippet and mark the harness "needs manual step" instead of rewriting the file.
- **Instruction text.** One source in the binary (`include_str!` of a template) feeds the
  SKILL.md and the AGENTS.md blocks, so they don't drift.

**Step 4: summary.** A table of each harness, what was done, and anything the user must do
next (restart the harness, trust the Codex hooks). Exit code 0 if at least one harness was
configured or confirmed, and 1 if none.

**Re-running.** `axl install` again changes nothing when everything is already correct. It
updates paths if the binary moved, for example after a reinstall.

`--dry-run` prints the planned changes as a diff and writes nothing.

### 6. `axl uninstall [--harness NAME]... [--service] [--revoke]`

- Removes what `install` recorded in `~/.axl/install-manifest.json`: the marked blocks, the
  `axle` entries and the plugin link. Anything unrelated the user wrote stays.
- `--service` also stops and removes the local service. It never deletes the local database
  unless `--purge` is given.
- `--revoke` revokes the token on the server.

### 7. `axl status`

Prints one screen:
- the server URL
- whether it's reachable (`/readyz`)
- whether the token is valid
- whether the local service is running (if installed)
- each harness: configured, not configured, or not found

Each problem comes with the one command that fixes it. It's read-only and needs a new
authenticated `GET /v1/whoami`, which returns the token label and the server version.

### 8. `axl service install|uninstall|start|stop|restart|status|logs`

- **macOS:** `~/Library/LaunchAgents/com.reyemtech.axle.plist`, with `RunAtLoad` and
  `KeepAlive`, loaded via `launchctl bootstrap gui/$UID`.
- **Linux:** `~/.config/systemd/user/axle.service`, enabled with
  `systemctl --user enable --now`. If systemd isn't available, for example in some
  containers or on WSL1, print `axl serve` instructions instead.
- **Environment:**
  - `DATABASE_URL=sqlite://<home>/.axl/axl.db`
  - `AXL_BIND=127.0.0.1:7420`
  - UI auth `auto`, which on loopback means no sign-in, as today
- **Logs:** `~/.axl/logs/axl.log`.
- **Port in use:** if port 7420 is taken, say so and suggest `--port`. The chosen port is
  saved in `config.toml`.
- **Upgrades:** `axl service install` rewrites the unit with the current binary path, so
  after `brew upgrade axle` it's "run `axl service restart`". `axl status` warns when the
  running server's version differs from the CLI's.

### 9. UI changes

- **Tokens page:** a **Connect a machine** form and the one-time result panel (the code, the
  two commands with copy buttons, and the expiry countdown), plus a list of pending
  enrollments with Revoke. It follows DESIGN.md: labelled buttons, and text with every glyph.
- **New `/ui/connect`:** the device-approval page (user-code entry, then Approve or Deny).
- **Overview:** an empty state for "no agents yet" that points at Connect a machine.

### 10. Docs

- `README.md` Quickstart and `docs/getting-started.md` become:
  - local: `brew install reyemtech/tap/axle && axl install`
  - team: paste the command from the UI
- `docs/harnesses/*.md` keep the manual snippets under "Manual setup".
- A new `docs/cli.md` covers `install`, `login`, `status` and `service`.
- `docs/security.md` adds enrollment codes, the device flow and the per-IP limits.
- `docs/configuration.md` adds `AXL_PUBLIC_URL`, `AXL_TRUSTED_PROXY_HEADER` and
  `~/.axl/config.toml`, and the Helm values gain `publicUrl`, defaulting from the ingress.

## Constraints

- Both stores. New migrations `0003_*` for SQLite and Postgres; `0001`/`0002` stay
  untouched.
- macOS and Linux only. Code that doesn't compile on Windows stays behind `cfg(unix)`, as
  today.
- No new runtime services. Dependencies stay modest, for example `toml_edit` and a JSONC
  editor, plus a small browser opener such as `open`/`xdg-open` via `std::process`.
- The same peer-data framing, CSRF and CSP rules as the rest of the UI.
- No token in logs, URLs, process arguments or terminal output.
- Tests:
  - store tests for enrollment transitions and races on both backends
  - HTTP tests for enroll, login start/poll/approve/deny, expiry and rate limits
  - golden-file tests for every config editor: empty file, an existing unrelated config,
    comments, idempotent re-run, and uninstall restoring the original
  - service-file generation tests, without loading launchd or systemd in CI
  - `install.sh` checked with `shellcheck` and rendered in a test
  - a CLI end-to-end test: local `axl serve` → `axl login --code` → `axl install --dry-run`
    against a temporary HOME

## Left to the builder

- **Verify Claude Code plugin loading** from `~/.claude/skills/<name>` (Concord's method)
  with a quick spike before building on it. Use the user-scope fallback above if it doesn't
  load.
- Whether `axl hook` handles Codex hook payloads now or Codex stays MCP-only until the
  wake-up spec. Pick the smaller one and document it.
- The JSONC editing library, or the manual-step fallback.
- Exact poll intervals, the code alphabet details and the per-IP limit numbers, within the
  bounds above.
- How the `axl status` output is laid out.
