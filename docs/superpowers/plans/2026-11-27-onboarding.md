# Onboarding (install, login, enrollment, local service) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A beginner runs one command (`axl install`, a pasted `curl … | sh`, or `axl login <url>`) and their Claude Code, Codex and OpenCode agents are connected to Axle, with no token copying and no config editing.

**Architecture:** All logic lives in the `axl` binary. The server gains an `enrollments` table (both stores), an `EnrollService`, unauthenticated `/v1/enroll` + `/v1/login/{start,poll}` behind a per-IP limiter, an authenticated `/v1/whoami`, a rendered `/install.sh`, and UI for issuing codes and approving device logins. The client gains `~/.axl/config.toml`, `axl login|logout|install|uninstall|status|service`, and format-preserving editors for each harness's config, all tracked in `~/.axl/install-manifest.json`.

**Tech Stack:** Rust 2024, clap 4 (derive), axum 0.8, sqlx 0.9 (SQLite + Postgres), askama 0.16, reqwest 0.13 (rustls), `toml_edit` (new), `jsonc-parser` with the `cst` feature (new), `similar` (new, dry-run diffs), shellcheck in CI.

**Spec:** `docs/superpowers/specs/2026-11-27-onboarding-design.md`

## Global Constraints

- New migrations are `migrations/{sqlite,postgres}/0003_enrollment.sql`; `0001`/`0002` stay untouched.
- macOS and Linux only; Unix-only code stays behind `cfg(unix)`.
- No token or device code in terminal output, logs, URLs, process arguments, or harness config files. The one-time code in the pasted command is the only exception.
- Never need root or sudo. `install.sh` refuses root unless `AXL_ALLOW_ROOT=1`.
- `~/.axl/` is mode 0700; `~/.axl/token` and `~/.axl/config.toml` are mode 0600.
- Server URL precedence: `AXL_SERVER_URL`, then `config.toml` `server_url`, then `http://127.0.0.1:7420`. Token precedence: `AXL_TOKEN`, then `~/.axl/token`.
- One-time code: 10 Crockford base32 chars shaped `XXXXX-XXXXX`, valid **15 minutes**, single use. Device flow expires after **10 minutes**.
- Per-IP limit on `/v1/enroll` and `/v1/login/*`: **10 requests per minute per IP**, separate from the per-token limiter.
- Enrollment failure body is always `invalid or expired code`.
- Block markers are exactly the lines `# >>> axle >>>` and `# <<< axle <<<`, in config.toml and in every AGENTS.md / CLAUDE.md block alike.
- launchd label `com.reyemtech.axle` at `~/Library/LaunchAgents/com.reyemtech.axle.plist`; systemd user unit `~/.config/systemd/user/axle.service`.
- Service env: `DATABASE_URL=sqlite://<home>/.axl/axl.db`, `AXL_BIND=127.0.0.1:7420`, UI auth `auto`. Logs at `~/.axl/logs/axl.log`.
- Backups: `<file>.axl-backup-<UTC timestamp>` before the first change to an existing file.
- Harness commands use an absolute, non-canonicalised `axl` path.
- Installer URL: `https://github.com/ReyemTech/axle/releases/download/v<ver>/axle-mcp-installer.sh`, version from `env!("CARGO_PKG_VERSION")`.
- UI follows DESIGN.md: verb-labelled buttons, a text label beside every glyph, no shadows, no new external assets, strict CSP.
- Tests never touch the real home directory: every client-side function takes `home: &Path`, and every test uses a temp HOME.
- Commit messages carry no AI or tool attribution.

## Review Focus

1. **Racing redemptions.** Two concurrent `POST /v1/enroll` with the same code, or two polls after approval: exactly one gets a token (pinned in Task 2 `code_consumed_once_under_race` and `device_consumed_once_under_race`).
2. **A user's config that is not what the golden fixtures expect** — trailing commas or comments in `~/.claude.json`, an `opencode.jsonc` with comments, a Codex `config.toml` where `mcp_servers` is an inline table. Expected: either a byte-preserving edit or "needs manual step" with the snippet, never a reformat or crash (pinned in Tasks 9–11 `*_unusual_shape_*` tests).
3. **Spoofed client IP.** With `AXL_TRUSTED_PROXY_HEADER` unset, a client-sent `X-Forwarded-For` must not change the limiter key; with it set, the right-most entry is used (pinned in Task 4 `forwarded_for_ignored_unless_trusted`).
4. **A public URL that would break the shell script** (quote, space, `$`, or a path). Expected: config rejects it at startup with a message naming `AXL_PUBLIC_URL` (pinned in Task 4 `public_url_rejects_unsafe_values`).
5. **Re-running install after the binary moved or Homebrew upgraded.** Expected: paths rewritten, nothing duplicated, no new backups when nothing changed (pinned in Task 13 `rerun_is_noop` and `rerun_rewrites_moved_binary`).

---

## File Structure

| Path | Responsibility |
|---|---|
| `src/client/mod.rs` | Client-side module root (re-exports). |
| `src/client/config.rs` | `~/.axl/` layout, `config.toml`, token file, precedence. |
| `src/client/api.rs` | reqwest calls: enroll, login start/poll, whoami, readyz, revoke-self. |
| `src/domain.rs` | `EnrollmentId`, `EnrollmentKind`, `EnrollmentStatus`, `Enrollment`. |
| `src/store/{mod,sqlite,postgres}.rs` | Enrollment store methods. |
| `src/service/enroll.rs` | Code generation, hashing, TTLs, exchange/poll/approve/deny, events. |
| `src/http/enroll.rs` | Unauthenticated enroll/login routes, per-IP limiter, client IP. |
| `src/http/install_script.rs` + `templates/install.sh` | `GET /install.sh`. |
| `src/ui/connect.rs` + `templates/connect.html` | Tokens-page connect form, pending list, `/ui/connect`. |
| `src/install/mod.rs` | `Harness` enum, detection, `Change` plan, apply/dry-run. |
| `src/install/paths.rs` | Absolute binary path rules. |
| `src/install/manifest.rs` | `install-manifest.json`. |
| `src/install/fsutil.rs` | Backups, atomic writes, marker blocks. |
| `src/install/instructions.md` | The single instruction text (`include_str!`). |
| `src/install/{claude_code,codex,opencode}.rs` | One editor per harness. |
| `src/daemon/{mod,launchd,systemd}.rs` | `axl service` unit files and control. |
| `src/cli/{mod,login,install,uninstall,status,service}.rs` | Command handlers called from `main.rs`. |
| `tests/fixtures/harness/<harness>/<case>/{before,after}.*` | Golden files. |
| `tests/{client_config,store_enroll,http_enroll,ui_connect,install_*,daemon,cli_onboarding}.rs` | Tests. |

---

### Task 1: Client configuration and precedence

**Files:**
- Create: `src/client/mod.rs`, `src/client/config.rs`, `tests/client_config.rs`
- Modify: `src/lib.rs` (add `pub mod client;`), `src/bridge/mod.rs:72-112` (`Config::resolve` and `Config::token` use the client module), `src/hook/mod.rs:237-244` (`token` delegates)

**Interfaces:**
- Produces:
  - `pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:7420";` (moved here; `bridge::DEFAULT_SERVER_URL` re-exports it)
  - `pub struct AxlDir { pub root: PathBuf }` with `fn new(home: &Path) -> Self` (`<home>/.axl`), `fn token_path(&self) -> PathBuf`, `fn config_path(&self) -> PathBuf`, `fn manifest_path(&self) -> PathBuf`, `fn db_path(&self) -> PathBuf`, `fn logs_dir(&self) -> PathBuf`, `fn integrations_dir(&self) -> PathBuf`, `fn ensure(&self) -> io::Result<()>` (creates with mode 0700, tightens an existing looser dir)
  - `#[derive(Serialize, Deserialize, Default, Debug, PartialEq)] pub struct ClientConfig { pub server_url: Option<String>, pub port: Option<u16> }`
  - `pub fn load_config(dir: &AxlDir) -> Result<ClientConfig, ClientError>` (missing file = default)
  - `pub fn save_config(dir: &AxlDir, cfg: &ClientConfig) -> Result<(), ClientError>` (0600, atomic rename)
  - `pub fn resolve_server_url(env_url: Option<&str>, dir: &AxlDir) -> String` (no trailing `/`)
  - `pub fn read_token(env_token: Option<&str>, dir: &AxlDir) -> Result<String, String>` (same error strings as today: `no token (set AXL_TOKEN or write ~/.axl/token)`)
  - `pub fn write_token(dir: &AxlDir, token: &str) -> Result<(), ClientError>` (0600, atomic) and `pub fn remove_token(dir: &AxlDir) -> Result<bool, ClientError>`
  - `pub enum ClientError { Io(io::Error), Toml(String), Http(String), Server(String) }` (thiserror)

- [ ] **Step 1: Write the failing tests** in `tests/client_config.rs` (each uses a fresh temp HOME dir helper like `tests/cli.rs::TempDir`):

```rust
#[test] fn server_url_env_beats_config_beats_default() {
    let home = TempHome::new(); let dir = AxlDir::new(home.path());
    assert_eq!(resolve_server_url(None, &dir), "http://127.0.0.1:7420");
    save_config(&dir, &ClientConfig { server_url: Some("https://axle.example.com/".into()), port: None }).unwrap();
    assert_eq!(resolve_server_url(None, &dir), "https://axle.example.com");
    assert_eq!(resolve_server_url(Some("http://10.0.0.5:7420"), &dir), "http://10.0.0.5:7420");
    assert_eq!(resolve_server_url(Some("  "), &dir), "https://axle.example.com");
}
#[test] fn token_env_beats_file() { /* write_token "axl_file"; read_token(Some("axl_env")) == "axl_env"; read_token(None) == "axl_file" */ }
#[test] fn files_and_dir_have_private_modes() {
    // after write_token + save_config: mode(.axl) == 0o700, mode(token) == 0o600, mode(config.toml) == 0o600
}
#[test] fn ensure_tightens_a_loose_dir() { /* pre-create .axl 0o755 -> ensure -> 0o700 */ }
#[test] fn missing_config_is_default_and_bad_toml_is_an_error() {}
```

Plus in `src/bridge/mod.rs` tests: `resolve_reads_server_url_from_config_toml` (Env with `HOME` = temp dir holding `config.toml`, no `AXL_SERVER_URL` → `Config.server_url == "https://axle.example.com"`).

- [ ] **Step 2: Run** `cargo test --test client_config` and `cargo test --lib bridge` — Expected: FAIL (unresolved `axle::client`).
- [ ] **Step 3: Implement** `src/client/config.rs` per Interfaces; `Config::resolve` calls `resolve_server_url(env.get("AXL_SERVER_URL"), &AxlDir::new(home))` when `HOME` is set, and the bridge/hook token functions call `read_token`. Use `std::os::unix::fs::PermissionsExt` behind `cfg(unix)`; atomic write = write `<file>.tmp` with mode set via `OpenOptions::mode`, then `rename`.
- [ ] **Step 4: Run** `cargo test --test client_config --test bridge --test hook && cargo test --lib` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(client): ~/.axl/config.toml with server URL precedence"`

---

### Task 2: Enrollment store and migrations (both backends)

**Files:**
- Create: `migrations/sqlite/0003_enrollment.sql`, `migrations/postgres/0003_enrollment.sql`, `tests/store_enroll.rs`
- Modify: `src/domain.rs` (add `EnrollmentId` to `id_type!` at line 56; add types), `src/store/mod.rs` (trait methods + request types), `src/store/sqlite.rs`, `src/store/postgres.rs`, `src/service/auth.rs` (expose minting)

**Interfaces:**
- Produces (domain):
  - `text_enum!`-backed `pub enum EnrollmentKind { Code, Device }` (`code`/`device`) and `pub enum EnrollmentStatus { Pending, Approved, Consumed, Expired, Denied }`
  - `pub struct Enrollment { pub id: EnrollmentId, pub kind: EnrollmentKind, pub label: String, pub host: String, pub status: EnrollmentStatus, pub token_id: Option<TokenId>, pub created_at: OffsetDateTime, pub expires_at: OffsetDateTime, pub approved_at: Option<OffsetDateTime>, pub requester_ip: Option<String> }`
- Produces (store, `src/store/mod.rs`):
  - `pub struct NewEnrollment<'a> { pub id: EnrollmentId, pub kind: EnrollmentKind, pub secret_hash: &'a str, pub user_code_hash: Option<&'a str>, pub label: &'a str, pub host: &'a str, pub requester_ip: Option<&'a str>, pub now: OffsetDateTime, pub expires_at: OffsetDateTime }`
  - `pub struct NewToken<'a> { pub id: TokenId, pub hash: &'a str }` — no label: the store derives it inside the consuming transaction from the enrollment row (code: `<row.label> (<host>)`; device: `<row.host> (device login)`), because the caller cannot know the row's label before consuming.
  - `pub enum PollOutcome { Pending, Denied, Expired, Consumed(Enrollment) }`
  - `async fn insert_enrollment(&self, e: &NewEnrollment<'_>) -> StoreResult<()>` — also opportunistic cleanup in the same transaction: `pending`/`approved` rows with `expires_at <= now` become `expired`; rows with `expires_at < now - 1 day` are deleted.
  - `async fn consume_code(&self, secret_hash: &str, host: &str, now: OffsetDateTime, token: &NewToken<'_>) -> StoreResult<Option<Enrollment>>` — one transaction: guarded `UPDATE enrollments SET status='consumed', token_id=?, host=? WHERE secret_hash=? AND kind='code' AND status='pending' AND expires_at > ? RETURNING …`; only on a returned row, `INSERT INTO tokens`.
  - `async fn device_by_user_code(&self, user_code_hash: &str, now: OffsetDateTime) -> StoreResult<Option<Enrollment>>` (pending and unexpired only)
  - `async fn decide_device(&self, id: EnrollmentId, approve: bool, now: OffsetDateTime) -> StoreResult<Option<Enrollment>>` — guarded `status='pending' AND expires_at > now` → `approved` (sets `approved_at`) or `denied`.
  - `async fn poll_device(&self, secret_hash: &str, now: OffsetDateTime, token: &NewToken<'_>) -> StoreResult<PollOutcome>` — guarded `approved → consumed` + token insert in one transaction; otherwise maps the row's state (missing, consumed, or past `expires_at` → `Expired`).
  - `async fn cancel_enrollment(&self, id: EnrollmentId, now: OffsetDateTime) -> StoreResult<bool>` — guarded `pending → denied` (the UI's Revoke).
  - `async fn list_pending_enrollments(&self, now: OffsetDateTime) -> StoreResult<Vec<Enrollment>>` (pending, unexpired, newest first)
- Produces (auth): `pub fn mint() -> Result<(String, String), AuthError>` returning `(plaintext, hash)`; `pub fn hash(plaintext: &str) -> String` made `pub`. `AuthService::create` reuses `mint`.

- [ ] **Step 1: Write the migrations.** Columns exactly per spec §2: `id, kind CHECK (kind IN ('code','device')), secret_hash UNIQUE NOT NULL, user_code_hash UNIQUE NULL, label, host, status CHECK (status IN ('pending','approved','consumed','expired','denied')), token_id REFERENCES tokens(id), created_at, expires_at, approved_at, requester_ip`; index on `(status, expires_at)`. SQLite: TEXT timestamps like `0001`; Postgres: `uuid`/`timestamptz`. No `-- no-transaction` needed.
- [ ] **Step 2: Write the failing store-matrix tests** in `tests/store_enroll.rs` using `store_matrix!` from `tests/common`:

```rust
store_matrix!(code_lifecycle, |store: Arc<dyn Store>| async move {
    // insert code enrollment at 9:00, expires 9:15
    // consume_code at 9:14 -> Some(e), e.status == Consumed, e.token_id == Some(tok.id), e.host == "laptop"
    // list_tokens contains the token labelled "work laptop (laptop)"
    // second consume_code -> None; no second token row
});
store_matrix!(code_expires_after_15_minutes, |store| async move { /* consume at 9:15:00 -> None, no token */ });
store_matrix!(code_consumed_once_under_race, |store| async move {
    // 16 concurrent consume_code with distinct NewToken ids -> exactly 1 Some; list_tokens().len() == 1
});
store_matrix!(device_approve_then_poll, |store| async move {
    // poll before decision -> Pending; decide_device(approve) -> Approved; poll -> Consumed(e); poll again -> Expired
});
store_matrix!(device_denied, |store| async move { /* decide(false) -> poll == Denied; no token */ });
store_matrix!(device_expires_after_10_minutes, |store| async move { /* approve at 9:05, poll at 9:10 -> Expired */ });
store_matrix!(device_consumed_once_under_race, |store| async move { /* 16 concurrent polls -> one Consumed */ });
store_matrix!(cancel_and_list_pending, |store| async move {
    // two pending, one cancelled -> list_pending has one; cancel twice -> second false
});
store_matrix!(cleanup_marks_and_deletes_old_rows, |store| async move {
    // row expiring 9:15; insert another at 9:20 -> first not in list_pending, consume -> None;
    // insert at day+2 -> old row gone (device_by_user_code / consume both None)
});
```

- [ ] **Step 3: Run** `cargo test --test store_enroll` — Expected: FAIL (methods undefined).
- [ ] **Step 4: Implement** the trait methods in `src/store/sqlite.rs` (using `ts::enc`, `uuid_of`, `BEGIN IMMEDIATE` semantics via the existing transaction helper) and `src/store/postgres.rs` (`FOR UPDATE` not needed: the guarded UPDATE is the lock). Follow `use_token`/`transition_task` style.
- [ ] **Step 5: Run** `cargo test --test store_enroll --test store_tokens` and, with Postgres available, `AXL_TEST_PG_URL=… cargo test --features pg-tests --test store_enroll` — Expected: PASS on both.
- [ ] **Step 6: Commit** `git commit -m "feat(store): enrollments table and guarded consume for both backends"`

---

### Task 3: Enrollment service and event

**Files:**
- Create: `src/service/enroll.rs`
- Modify: `src/service/mod.rs`, `src/events.rs` (new variant), `src/http/v1.rs:254-260` (`Watcher::wants` returns `false` for it), `src/ui/views.rs:911-928` (`task_frames` maps it to a `tokens` frame), `src/http/mod.rs` (`AppState.enroll: Arc<EnrollService>`)

**Interfaces:**
- Consumes: Task 2 store methods, `auth::mint`, `auth::hash`.
- Produces:
  - `pub const CODE_TTL: Duration = Duration::minutes(15); pub const DEVICE_TTL: Duration = Duration::minutes(10); pub const POLL_INTERVAL_SECS: u64 = 7; /* 7 s keeps polling under the 10/min per-IP bucket (refill 1 per 6 s) for the whole 10-minute flow */`
  - `pub const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";`
  - `pub fn normalize_code(input: &str) -> Option<String>` — uppercase, drop `-` and whitespace, `I`/`L`→`1`, `O`→`0`; `None` unless the result is all-alphabet and exactly 10 (one-time) or 8 (user code) chars.
  - `pub fn format_code(raw: &str) -> String` — `K7QM4ZP2RX` → `K7QM4-ZP2RX`; 8 chars → `WXYZ-1234` shape.
  - `pub struct IssuedCode { pub id: EnrollmentId, pub code: String /* formatted */, pub expires_at: OffsetDateTime }`
  - `pub struct DeviceStart { pub device_code: String /* "axld_" + 64 hex */, pub user_code: String, pub interval: u64, pub expires_in: u64 }`
  - `pub enum PollReply { Pending, SlowDown, Denied, Expired, Token { token: String, host: String } }`
  - `impl EnrollService { pub fn new(store, clock, bus: EventBus) -> Self; pub async fn issue_code(&self, label: &str) -> Result<IssuedCode, EnrollError>; pub async fn exchange(&self, code: &str, host: &str, ip: Option<&str>) -> Result<Option<String /* token */>, EnrollError>; pub async fn start_device(&self, host: &str, ip: Option<&str>) -> Result<DeviceStart, EnrollError>; pub async fn poll(&self, device_code: &str) -> Result<PollReply, EnrollError>; pub async fn lookup_user_code(&self, user_code: &str) -> Result<Option<Enrollment>, EnrollError>; pub async fn decide(&self, id: EnrollmentId, approve: bool) -> Result<bool, EnrollError>; pub async fn cancel(&self, id: EnrollmentId) -> Result<bool, EnrollError>; pub async fn pending(&self) -> Result<Vec<Enrollment>, EnrollError> }`
  - `Event::EnrollmentChanged { id: EnrollmentId, status: EnrollmentStatus }` (`name()` = `"EnrollmentChanged"`, `project_id()` = `None`)
- Rules: code token label `"<label> (<host>)"` (derived by the store, Task 2); device token label `"<host> (device login)"`; device enrollment `label` = host. `host` is passed through `bridge::sanitize_label`. `SlowDown` when the same device code polls again sooner than `POLL_INTERVAL_SECS` (in-memory `Mutex<HashMap<String /*hash*/, OffsetDateTime>>`, entries dropped on terminal replies). Every state change emits `EnrollmentChanged` via `bus.emit`.

- [ ] **Step 1: Write failing unit tests** in `src/service/enroll.rs`:

```rust
#[test] fn codes_are_crockford_and_shaped() {
    let raw = random_code(10); assert_eq!(raw.len(), 10); assert!(raw.bytes().all(|b| CROCKFORD.contains(&b)));
    assert_eq!(format_code("K7QM4ZP2RX"), "K7QM4-ZP2RX");
}
#[test] fn normalize_accepts_typing_variants() {
    assert_eq!(normalize_code("k7qm4-zp2rx").as_deref(), Some("K7QM4ZP2RX"));
    assert_eq!(normalize_code(" K7QM4 ZP2RX ").as_deref(), Some("K7QM4ZP2RX"));
    assert_eq!(normalize_code("IOL").is_none(), true);
    assert_eq!(normalize_code("10LO1-ABCDE").as_deref(), Some("10101ABCDE"));
    assert!(normalize_code("K7QM4-ZP2RU").is_none()); // U is not Crockford
}
#[tokio::test] async fn exchange_mints_labelled_token_and_emits() {
    // issue_code("work laptop") -> exchange(code, "laptop", None) == Some(t), t.starts_with("axl_")
    // auth.list() has "work laptop (laptop)"; bus subscriber received EnrollmentChanged { status: Consumed }
}
#[tokio::test] async fn poll_too_fast_is_slow_down() { /* start, poll -> Pending, poll same instant -> SlowDown, +7s -> Pending */ }
#[tokio::test] async fn malformed_code_is_none_without_store_hit() {}
```

Also extend `tests/http_v1.rs` with `enrollment_events_are_not_sent_to_agents` (subscribe to `/v1/events`, issue a code, assert no frame within 200 ms).

- [ ] **Step 2: Run** `cargo test --lib service::enroll` — Expected: FAIL.
- [ ] **Step 3: Implement** per Interfaces (`getrandom` rejection sampling `b & 31` for unbiased Crockford chars).
- [ ] **Step 4: Run** `cargo test --lib && cargo test --test http_v1` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(enroll): enrollment service with one-time codes and device flow"`

---

### Task 4: Enrollment HTTP endpoints, per-IP limiter, `AXL_PUBLIC_URL`, `/v1/whoami`

**Files:**
- Create: `src/http/enroll.rs`, `tests/http_enroll.rs`
- Modify: `src/config.rs` (fields + parsing + tests), `src/http/mod.rs:139-152` (router), `src/http/v1.rs` (whoami route), `src/ui/mod.rs:384-388` (`client_ip` moves to `src/http/enroll.rs` as `pub fn client_ip(headers, extensions, trusted: Option<&str>) -> String` and the UI login reuses it)

**Interfaces:**
- Produces (config, on `ServerConfig`):
  - `pub public_url: Option<String>` from `AXL_PUBLIC_URL` — trimmed, no trailing `/`, must parse as `http`/`https` with a host, no path other than `/`, and contain only `[A-Za-z0-9.:/\-\[\]]`; else `ConfigError::BadPublicUrl(String)`.
  - `pub trusted_proxy_header: Option<String>` from `AXL_TRUSTED_PROXY_HEADER` (validated with `trusted_header_name`).
  - `pub fn advertised_url(&self, host_header: Option<&str>) -> Result<String, MissingPublicUrl>` — `public_url` if set; else, on a loopback bind, `http://<Host>` when Host's name is `127.0.0.1`, `localhost` or `[::1]`, falling back to `http://<bind>`; else `Err`. `MissingPublicUrl`'s `Display` is `set AXL_PUBLIC_URL to this server's public address (for example https://axle.example.com)`.
- Produces (HTTP):
  - `pub const ENROLL_PER_MINUTE: u32 = 10;` and `AppState.enroll_limit: Arc<RateLimiter<String>>`
  - `pub fn routes() -> Router<AppState>` with `POST /enroll {code, host}` → 200 `{token, server_url}` / 400 `{"error":"invalid or expired code"}`; `POST /login/start {host}` → `{device_code, user_code, verify_url, interval, expires_in}` (`verify_url` = `<advertised>/ui/connect`); `POST /login/poll {device_code}` → 200 `{token, server_url}` or 400 `{"error": "authorization_pending"|"slow_down"|"denied"|"expired"}`. Over the limit: 429 `rate_limited` with `Retry-After`. `/enroll` and `/login/start` call `advertised_url` first and return 503 naming `AXL_PUBLIC_URL` before touching the store (a code is never consumed without being able to answer `server_url`).
  - Router: `.nest("/v1", v1::routes().route_layer(require_token).merge(enroll::routes()))` so enroll routes skip bearer auth.
  - `GET /v1/whoami` (authenticated) → `{"label": "<token label>", "version": env!("CARGO_PKG_VERSION")}`.
  - All responses `Cache-Control: no-store`.

- [ ] **Step 1: Write failing config tests** in `src/config.rs`: `public_url_parsed_and_trimmed` (`"https://axle.example.com/"` → `"https://axle.example.com"`), `public_url_rejects_unsafe_values` (each of `"https://a.example/'x"`, `"https://a b"`, `"ftp://a"`, `"https://a.example/sub"`, `"https://$(id)"` → `BadPublicUrl`), `advertised_url_rules` (loopback + Host `localhost:7420` → `http://localhost:7420`; loopback + Host `evil.example` → `http://127.0.0.1:7420`; `0.0.0.0` bind without public URL → `Err`).
- [ ] **Step 2: Write failing HTTP tests** in `tests/http_enroll.rs` (copy the `app()` helper shape from `tests/http_v1.rs`, with `TestClock`):

```rust
#[tokio::test] async fn enroll_with_code_returns_token_once() {
    // issue via state.enroll.issue_code("work laptop"); POST /v1/enroll -> 200, body.token starts "axl_",
    // body.server_url == "http://127.0.0.1:7420"; same request again -> 400 {"error":"invalid or expired code"}
}
#[tokio::test] async fn enroll_expired_and_unknown_look_identical() { /* advance 15 min; compare bodies byte-equal to unknown code */ }
#[tokio::test] async fn token_from_enroll_authenticates_whoami() { /* GET /v1/whoami -> label "work laptop (laptop)", version == CARGO_PKG_VERSION */ }
#[tokio::test] async fn device_flow_start_poll_approve() {
    // start -> verify_url ends "/ui/connect", interval 7, expires_in 600, user_code matches ^[0-9A-Z]{4}-[0-9A-Z]{4}$
    // poll -> 400 authorization_pending; state.enroll.decide(id, true); +7s poll -> 200 token
}
#[tokio::test] async fn device_flow_deny_and_expiry() { /* deny -> "denied"; fresh start, +10 min -> "expired" */ }
#[tokio::test] async fn per_ip_limit_is_10_per_minute() { /* 10 bad enrolls -> 400, 11th -> 429 with Retry-After; a valid bearer request still 200 */ }
#[tokio::test] async fn forwarded_for_ignored_unless_trusted() {
    // untrusted: 11 requests with varying X-Forwarded-For -> 11th 429
    // AXL_TRUSTED_PROXY_HEADER=X-Forwarded-For: "1.1.1.1, 10.0.0.9" keys by "10.0.0.9"; different right-most -> separate buckets
}
#[tokio::test] async fn non_loopback_without_public_url_start_is_503() { /* body names AXL_PUBLIC_URL */ }
#[tokio::test] async fn whoami_requires_token() { /* 401 */ }
```

- [ ] **Step 3: Run** `cargo test --test http_enroll && cargo test --lib config` — Expected: FAIL.
- [ ] **Step 4: Implement** per Interfaces. `client_ip`: with a trusted header, the right-most comma-separated non-empty entry; otherwise `ConnectInfo` IP; `"unknown"` without either.
- [ ] **Step 5: Run** `cargo test --test http_enroll --test http_v1 --test ui && cargo test --lib` — Expected: PASS.
- [ ] **Step 6: Commit** `git commit -m "feat(http): enroll and device-login endpoints with per-IP limit, whoami"`

---

### Task 5: UI — Connect a machine, pending list, `/ui/connect`, overview empty state

**Files:**
- Create: `src/ui/connect.rs`, `templates/connect.html`, `templates/_connect_result.html`, `tests/ui_connect.rs`
- Modify: `src/ui/mod.rs:151-190` (routes; `to_login` and `login` carry `next`), `src/ui/admin.rs` (`TokensPage` gains `pending: Vec<PendingRow>`, `connect_hint: Option<String>`), `templates/tokens.html`, `templates/_overview_panel.html:12-13`, `static/app.js` (copy buttons + countdown), `static/app.css`

**Interfaces:**
- Consumes: `EnrollService::{issue_code, pending, cancel, lookup_user_code, decide}`, `ServerConfig::advertised_url`.
- Produces routes: `POST /ui/tokens/connect` (form `label`) → renders the result panel inline (not a redirect: the code must not land in a URL); `POST /ui/enrollments/{id}/revoke`; `GET /ui/connect` (user-code form); `POST /ui/connect/lookup` (form `user_code`) → confirmation card; `POST /ui/connect/{id}/approve`, `POST /ui/connect/{id}/deny`.
- Login redirect: `to_login` adds `?next=<path>` only for `/ui/connect`; `login` follows `next` only if it equals `/ui/connect` (no open redirect).
- Copy text (exact): result panel commands `curl -fsSL <url>/install.sh | sh -s -- <CODE>` and `axl login <url> --code <CODE>`; confirmation heading `Connect this machine?` with buttons `Approve` and `Deny`; overview empty state `No agents yet. Connect a machine to get started.` linking `/ui/tokens#connect`.
- Live: the `tokens` SSE frame (Task 3) re-fetches the pending list fragment.

- [ ] **Step 1: Write failing tests** in `tests/ui_connect.rs` (reuse the login/csrf helpers pattern from `tests/ui.rs`: `cookie_pair`, `csrf_of`, `select_texts`):

```rust
#[tokio::test] async fn connect_form_shows_code_and_both_commands_once() {
    // POST /ui/tokens/connect label=work laptop -> page text contains a code matching ^[0-9A-Z]{5}-[0-9A-Z]{5}$,
    // "curl -fsSL http://127.0.0.1:7420/install.sh | sh -s -- <code>" and "axl login http://127.0.0.1:7420 --code <code>"
    // GET /ui/tokens afterwards does not contain the code; pending list shows "work laptop" with a Revoke button
}
#[tokio::test] async fn connect_without_csrf_is_rejected() {}
#[tokio::test] async fn connect_panel_hints_public_url_when_missing() { /* 0.0.0.0 bind + secret auth -> text "AXL_PUBLIC_URL" and no commands */ }
#[tokio::test] async fn revoke_pending_enrollment_invalidates_code() { /* then enroll -> invalid or expired code */ }
#[tokio::test] async fn device_approval_shows_host_ip_time_then_approves() {
    // start_device("build-02", Some("10.0.0.7")); POST lookup user_code -> texts include "build-02", "10.0.0.7",
    // "Connect this machine?"; POST approve -> poll returns Token
}
#[tokio::test] async fn device_deny() {}
#[tokio::test] async fn connect_page_redirects_to_login_with_next_and_back() { /* Location "/ui/login?next=/ui/connect"; after login -> "/ui/connect"; next=https://evil -> "/ui/" */ }
#[tokio::test] async fn unknown_user_code_says_so() { /* "No pending request with that code" */ }
#[tokio::test] async fn overview_empty_state_points_at_connect() {}
```

- [ ] **Step 2: Run** `cargo test --test ui_connect` — Expected: FAIL.
- [ ] **Step 3: Implement** handlers and templates following DESIGN.md (labelled buttons; `.aspect` glyph + word for pending state; countdown as text `expires in 14:59`; peer data — host, label, IP — rendered escaped via askama).
- [ ] **Step 4: Run** `cargo test --test ui_connect --test ui` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(ui): connect a machine, pending enrollments and device approval"`

---

### Task 6: `GET /install.sh` and Helm `publicUrl`

**Files:**
- Create: `templates/install.sh`, `src/http/install_script.rs`, `tests/install_script.rs`
- Modify: `src/http/mod.rs` (route `/install.sh`), `deploy/helm/axle/values.yaml`, `deploy/helm/axle/values.schema.json`, `deploy/helm/axle/templates/_helpers.tpl` (new `axle.publicUrl`), `deploy/helm/axle/templates/deployment.yaml:75-78` (env `AXL_PUBLIC_URL`), `tests/helm.sh`, `.github/workflows/ci.yml` (shellcheck step)

**Interfaces:**
- Consumes: `ServerConfig::advertised_url`.
- Produces: `#[derive(Template)] #[template(path = "install.sh", escape = "none")] pub struct InstallScript { pub server_url: String, pub version: &'static str }`; `GET /install.sh` → 200 `text/x-shellscript; charset=utf-8` or 503 text naming `AXL_PUBLIC_URL`.
- Script behaviour (spec §3, in order): refuse root unless `AXL_ALLOW_ROOT=1`; `uname -s`/`uname -m` accept `Darwin|Linux` × `x86_64|arm64|aarch64` only; reuse `axl` on PATH when `axl --version` ≥ `{{ version }}` (POSIX version compare function), else `curl -fsSL https://github.com/ReyemTech/axle/releases/download/v{{ version }}/axle-mcp-installer.sh | sh` and use `$HOME/.cargo/bin/axl`; plain `http://` non-loopback → warn and read `y` from `/dev/tty`; then `axl install --server '{{ server_url }}'` plus `--code "$1"` when `$1` is set; then `axl status`. `set -eu`. The code is only ever `"$1"`, never echoed.
- Helm: `publicUrl: ""` in values; `axle.publicUrl` = explicit value, else `https://<first ingress host>` when ingress enabled with TLS, `http://<host>` without TLS, else empty (env omitted).

- [ ] **Step 1: Write failing tests** in `tests/install_script.rs`:

```rust
#[tokio::test] async fn install_sh_bakes_url_and_version() {
    // loopback app GET /install.sh -> 200, content-type starts "text/x-shellscript",
    // body contains "axl install --server 'http://127.0.0.1:7420'" and "releases/download/v{CARGO_PKG_VERSION}/axle-mcp-installer.sh"
}
#[tokio::test] async fn install_sh_503_without_public_url_off_loopback() {}
#[test] fn rendered_script_passes_shellcheck() {
    // render InstallScript { "https://axle.example.com", "0.1.1" } to temp file; if `shellcheck` is on PATH run
    // `shellcheck -s sh` and assert success; else print "skipped: shellcheck not installed" (CI installs it)
}
#[test] fn script_refuses_root_and_unknown_os() {
    // run rendered script with `sh` and a PATH shim dir: `id -u` -> 0 => exit != 0, stderr mentions AXL_ALLOW_ROOT;
    // `uname -s` -> FreeBSD => exit != 0, stderr "unsupported"
}
```

In `tests/helm.sh`: assert `AXL_PUBLIC_URL` = `https://axle.example.com` with `--set ingress.enabled=true,ingress.hosts[0].host=axle.example.com,ingress.tls[0].hosts[0]=axle.example.com`; = `https://x.test` with `--set publicUrl=https://x.test`; absent by default.

- [ ] **Step 2: Run** `cargo test --test install_script && bash tests/helm.sh` — Expected: FAIL.
- [ ] **Step 3: Implement** template, handler, Helm helper, schema entry (`"publicUrl": {"type": "string"}`), CI `shellcheck` install (`apt-get install -y shellcheck`).
- [ ] **Step 4: Run** the same commands — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(server): serve install.sh pinned to the server version; helm publicUrl"`

---

### Task 7: `axl login` and `axl logout`

**Files:**
- Create: `src/client/api.rs`, `src/cli/mod.rs`, `src/cli/login.rs`, `tests/cli_login.rs`
- Modify: `src/main.rs` (new `Login`, `Logout` variants dispatching to `axle::cli`), `src/lib.rs` (`pub mod cli;`)

**Interfaces:**
- Consumes: Task 1 config functions, Task 4 endpoints.
- Produces (api): `pub struct Api { base: String, client: reqwest::Client }` with `fn new(base: &str) -> Self`, `async fn readyz(&self) -> Result<(), ClientError>`, `async fn enroll(&self, code: &str, host: &str) -> Result<String, ClientError>`, `async fn login_start(&self, host: &str) -> Result<DeviceStartReply, ClientError>`, `async fn login_poll(&self, device_code: &str) -> Result<PollStatus, ClientError>`, `async fn whoami(&self, token: &str) -> Result<WhoAmI, ClientError>` (`WhoAmI { label: String, version: String }`), `async fn revoke_self(&self, token: &str) -> Result<(), ClientError>` (needs a new authenticated `POST /v1/token/revoke` — add it in `src/http/v1.rs` revoking the caller's `AuthToken`, with an `http_v1` test `revoke_self_revokes_caller_only`).
- Produces (cli): `pub struct LoginArgs { pub url: Option<String>, pub code: Option<String>, pub no_browser: bool, pub force: bool }`; `pub async fn login(home: &Path, args: LoginArgs, out: &mut dyn Write) -> Result<LoginOutcome, ClientError>` with `LoginOutcome { pub server_url: String, pub host: String }`; `pub async fn logout(home: &Path, revoke: bool, out: &mut dyn Write) -> Result<(), ClientError>`.
- Rules: URL defaults to `resolve_server_url`; the written `server_url` is the URL the user gave (normalised), not the server's reply. A token for a different `server_url` → error `already connected to <old>; pass --force to switch` unless `--force`. Browser opener: `open` on macOS, `xdg-open` on Linux via `std::process::Command`, skipped with `--no-browser`; the terminal shows the verify URL and user code. Poll honours `interval`, adds 5 s on `slow_down`, and treats HTTP 429 like `slow_down` (waits `Retry-After`). Output `Connected to <url> as <host>`.

- [ ] **Step 1: Write failing tests** in `tests/cli_login.rs`, starting a real server on `127.0.0.1:0` in-process (`axle::http::serve` with a temp SQLite store) and calling `axle::cli::login` with a temp HOME:

```rust
#[tokio::test] async fn login_with_code_writes_token_and_config() {
    // out contains "Connected to http://127.0.0.1:<port> as <host>"; out does NOT contain the token;
    // token file mode 0600 and starts "axl_"; config.toml server_url == base
}
#[tokio::test] async fn login_refuses_switching_servers_without_force() { /* config has other URL + token -> Err mentions "--force"; files unchanged */ }
#[tokio::test] async fn device_login_completes_after_ui_approval() { /* spawn login(no_browser); approve via state.enroll; join -> Ok */ }
#[tokio::test] async fn bad_code_reports_server_message() { /* Err text "invalid or expired code" */ }
#[tokio::test] async fn logout_removes_token_and_revoke_revokes() { /* --revoke -> whoami with old token 401 */ }
```

- [ ] **Step 2: Run** `cargo test --test cli_login` — Expected: FAIL.
- [ ] **Step 3: Implement** per Interfaces; `main.rs` passes `HOME` and `std::io::stdout()`.
- [ ] **Step 4: Run** `cargo test --test cli_login --test http_v1` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(cli): axl login (code and device flow) and axl logout"`

---

### Task 8: Install core — binary path, backups, marker blocks, manifest, instructions

**Files:**
- Create: `src/install/mod.rs`, `src/install/paths.rs`, `src/install/fsutil.rs`, `src/install/manifest.rs`, `src/install/instructions.md`, `tests/install_core.rs`
- Modify: `src/lib.rs` (`pub mod install;`), `Cargo.toml` (`toml_edit`, `jsonc-parser = { features = ["cst"] }`, `similar`)

**Interfaces:**
- Produces:
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)] pub enum Harness { ClaudeCode, Codex, Opencode }` with `fn name(self) -> &'static str` (`claude-code`/`codex`/`opencode`), `fn detect(self, home: &Path, path_env: Option<&OsStr>) -> bool` (config dir `~/.claude/`, `~/.codex/`, `~/.config/opencode/`, or binary `claude`/`codex`/`opencode` on PATH).
  - `pub fn axl_path(current_exe: &Path, path_env: Option<&OsStr>) -> PathBuf` — the first `axl` on PATH with the same `(dev, ino)` as `current_exe`; else `current_exe` uncanonicalised; a path matching `*/Cellar/axle/<ver>/bin/axl` maps to `<prefix>/bin/axl` (prefix = everything before `/Cellar/`).
  - `pub struct FileEdit { pub path: PathBuf, pub before: Option<String>, pub after: Option<String> }` (`None` = absent / delete) and `pub struct LinkEdit { pub path: PathBuf, pub target: Option<PathBuf> /* None = remove the symlink */, pub before: Option<PathBuf> }` (`apply` creates/replaces/removes the symlink; `render_diff` prints `link <path> -> <target>`) and `pub struct Plan { pub edits: Vec<FileEdit>, pub links: Vec<LinkEdit>, pub notes: Vec<String> }`; `pub fn render_diff(plan: &Plan) -> String` (unified, via `similar`); `pub fn apply(plan: &Plan, now: OffsetDateTime) -> io::Result<()>` — skips edits where `before == after`; backs up an existing file once per run to `<file>.axl-backup-<YYYYMMDDTHHMMSSZ>`; atomic write preserving the original mode.
  - `pub fn upsert_block(text: &str, body: &str) -> String` and `pub fn remove_block(text: &str) -> String` for `# >>> axle >>>` … `# <<< axle <<<` (appended with one blank line separator when absent; replaced in place when present; `remove_block` restores the original text byte-for-byte if install added it).
  - `pub const INSTRUCTIONS: &str = include_str!("instructions.md");` — the text now in `docs/harnesses/claude-code.md` "Suggested instructions".
  - `#[derive(Serialize, Deserialize, Default)] pub struct Manifest { pub version: u32 /* 1 */, pub entries: Vec<ManifestEntry> }`, `pub struct ManifestEntry { pub harness: Harness, pub file: PathBuf, pub kind: EntryKind, pub key: String, pub original: Option<serde_json::Value /* or string snippet */> }`, `pub enum EntryKind { Added, Replaced, Adopted, Block, Link, CreatedFile }`; `fn load(dir: &AxlDir) -> Manifest`, `fn save(&self, dir: &AxlDir) -> io::Result<()>`, `fn record(&mut self, e: ManifestEntry)` (dedupe by `(harness, file, key)`).
  - `pub fn is_axl_hook_command(cmd: &str) -> Option<hook::Event>` — matches `axl hook <event>` and `<any path>/axl hook <event>`.

- [ ] **Step 1: Write failing tests** in `tests/install_core.rs`:

```rust
#[test] fn axl_path_prefers_path_entry_with_same_inode() { /* symlink dir/bin/axl -> real exe; PATH=dir/bin -> dir/bin/axl (not resolved) */ }
#[test] fn axl_path_falls_back_to_current_exe_uncanonicalised() {}
#[test] fn cellar_path_maps_to_prefix_bin() {
    assert_eq!(axl_path(Path::new("/opt/homebrew/Cellar/axle/0.1.1/bin/axl"), None), PathBuf::from("/opt/homebrew/bin/axl"));
}
#[test] fn block_upsert_is_idempotent_and_removable() {
    let orig = "# mine\nkeep = 1\n";
    let once = upsert_block(orig, "x"); assert!(once.contains("# >>> axle >>>\nx\n# <<< axle <<<\n"));
    assert_eq!(upsert_block(&once, "x"), once); assert_eq!(remove_block(&once), orig);
}
#[test] fn apply_backs_up_once_and_skips_unchanged() { /* two applies, second identical -> exactly one *.axl-backup-20261127T090000Z file */ }
#[test] fn dry_run_diff_writes_nothing() {}
#[test] fn hook_command_matching() {
    assert!(is_axl_hook_command("axl hook stop").is_some());
    assert!(is_axl_hook_command("/Users/x/.cargo/bin/axl hook post-tool-use").is_some());
    assert!(is_axl_hook_command("other hook stop").is_none());
}
#[test] fn detect_by_dir_or_binary() { /* temp HOME with .codex -> Codex true; PATH shim `opencode` -> Opencode true; nothing -> false */ }
```

- [ ] **Step 2: Run** `cargo test --test install_core` — Expected: FAIL.
- [ ] **Step 3: Implement** per Interfaces (`std::os::unix::fs::MetadataExt` for dev/ino).
- [ ] **Step 4: Run** `cargo test --test install_core` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(install): binary path rules, backups, marker blocks and manifest"`

---

### Task 9: Claude Code editor (plugin, with user-scope fallback)

**Files:**
- Create: `src/install/claude_code.rs`, `tests/install_claude_code.rs`, `tests/fixtures/harness/claude-code/{empty,unrelated,comments,handmade}/…`
- Modify: `src/install/mod.rs` (dispatch)

**Interfaces:**
- Consumes: Task 8 (`Plan`, `FileEdit`, `INSTRUCTIONS`, `is_axl_hook_command`, `Manifest`).
- Produces: `pub fn plan_install(home: &Path, axl: &Path, manifest: &Manifest, mode: ClaudeMode) -> Result<(Plan, Vec<ManifestEntry>, Vec<String> /* adopted descriptions */), EditError>`; `pub fn plan_uninstall(home: &Path, manifest: &Manifest) -> Result<Plan, EditError>`; `pub enum ClaudeMode { Plugin, UserScope }`; `pub const MODE: ClaudeMode` set by the spike result.
- Plugin layout under `~/.axl/integrations/claude-code/`: `.claude-plugin/plugin.json` (`{"name":"axle","version":<CARGO_PKG_VERSION>,"description":…}`), `.mcp.json` (`mcpServers.axle = {type:"stdio", command:<axl>, args:["bridge","--harness","claude-code"]}`, no env), `hooks/hooks.json` (SessionStart, UserPromptSubmit, PostToolUse, Stop → `<axl> hook <event>`, `timeout: 5`), `skills/axle/SKILL.md` (frontmatter `name: axle` + `INSTRUCTIONS`); symlink `~/.claude/skills/axle` → that directory.
- Adoption (both modes): user-scope `mcpServers.axle` in `~/.claude.json` and any `axl hook <event>` hook in `~/.claude/settings.json` are Axle's. In Plugin mode they are removed (recorded `Adopted` with the original snippet); an `env.AXL_SERVER_URL` moves to `config.toml` if it has no `server_url`. UserScope mode writes `mcpServers.axle` in `~/.claude.json`, the four hooks in `~/.claude/settings.json` (matched by command for uninstall), and a marker block in `~/.claude/CLAUDE.md`.
- JSON edits use `jsonc-parser` CST so unrelated keys, order, comments and whitespace survive.

- [ ] **Step 1: Spike (no code kept).** In a scratch HOME, create the plugin directory by hand, link it into `~/.claude/skills/axle`, run `claude --debug` (or `claude mcp list` and `/hooks`), and record whether the MCP server, hooks and skill load. Also inspect `~/.claude/skills/concord-relay` on the owner's machine if present. Write the result (Plugin or UserScope, plus the Claude Code version) as a doc comment on `MODE`. If the link does not load, set `MODE = ClaudeMode::UserScope`; every later step still applies, with the UserScope fixtures.
- [ ] **Step 2: Write golden fixtures** for each case: `empty` (no `~/.claude.json`, no settings), `unrelated` (other MCP servers and hooks), `comments` (settings.json with `//` comments and odd indentation), `handmade` (the docs' unmarked `axle` entry with `env.AXL_SERVER_URL` and bare `axl hook …` hooks). Each has `before/` and `after/` trees (axl path placeholder `{{AXL}}`).
- [ ] **Step 3: Write failing tests** in `tests/install_claude_code.rs` (temp HOME, copy `before/`, apply plan, compare to `after/` byte-for-byte):

```rust
#[test] fn empty_home_installs_plugin() {}
#[test] fn unrelated_config_untouched() { /* other servers/hooks byte-identical */ }
#[test] fn comments_preserved() {}
#[test] fn handmade_entries_adopted_and_url_moved() { /* config.toml server_url == the old AXL_SERVER_URL; adopted list names "mcpServers.axle" and 4 hooks */ }
#[test] fn rerun_produces_empty_plan() {}
#[test] fn uninstall_restores_original_for_added_files() { /* empty + unrelated cases: tree == before/ (minus backups) */ }
#[test] fn uninstall_after_adoption_removes_managed_form_only() { /* handmade entries not restored */ }
#[test] fn claude_unusual_shape_trailing_commas() { /* ~/.claude.json with trailing commas -> either edited preserving them or Err(NeedsManual) with snippet; never reformatted */ }
#[test] fn config_files_contain_no_token_or_url() { /* no "axl_" and no "AXL_SERVER_URL" in any written file */ }
```

- [ ] **Step 4: Run** `cargo test --test install_claude_code` — Expected: FAIL.
- [ ] **Step 5: Implement** per Interfaces.
- [ ] **Step 6: Run** `cargo test --test install_claude_code` — Expected: PASS.
- [ ] **Step 7: Commit** `git commit -m "feat(install): Claude Code plugin editor with adoption"`

---

### Task 10: Codex editor (`toml_edit`)

**Files:**
- Create: `src/install/codex.rs`, `tests/install_codex.rs`, `tests/fixtures/harness/codex/{empty,unrelated,comments,handmade,inline_table}/…`

**Interfaces:**
- Consumes: Task 8.
- Produces: `pub fn plan_install(home: &Path, axl: &Path, manifest: &Manifest) -> Result<(Plan, Vec<ManifestEntry>, Vec<String>), EditError>`, `pub fn plan_uninstall(home: &Path, manifest: &Manifest) -> Result<Plan, EditError>`.
- Writes `[mcp_servers.axle]` with `command = "<axl>"`, `args = ["bridge", "--harness", "codex"]` in `~/.codex/config.toml` via `toml_edit::DocumentMut` (an existing `axle` table, including one with `env.AXL_SERVER_URL`, is adopted and replaced); a marker block in `~/.codex/AGENTS.md` holding `INSTRUCTIONS`. Summary note: `Codex asks you to trust new hooks on its next start` only if hooks are written.
- **Decision (spec "left to the builder"): Codex stays MCP-only in this plan** — `axl hook` handles Claude Code payloads only, and Codex wake-up is the next spec. No `[[hooks.*]]` block is written; documented in `docs/harnesses/codex.md` (Task 16). A test pins it.

- [ ] **Step 1: Write fixtures and failing tests** in `tests/install_codex.rs`: `empty_creates_config_and_agents_md`, `unrelated_tables_and_comments_survive` (comments, key order and blank lines byte-identical outside `mcp_servers.axle`), `handmade_axle_table_adopted_and_url_moved`, `rerun_produces_empty_plan`, `uninstall_restores_original`, `no_hooks_block_written` (`!after.contains("[[hooks")` and no marker in config.toml), `codex_unusual_shape_inline_table` (`mcp_servers = { other = {…} }` → result keeps `other` unchanged and adds `axle`, or `NeedsManual`; never drops `other`).
- [ ] **Step 2: Run** `cargo test --test install_codex` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --test install_codex` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(install): Codex config editor"`

---

### Task 11: OpenCode editor (JSON/JSONC)

**Files:**
- Create: `src/install/opencode.rs`, `tests/install_opencode.rs`, `tests/fixtures/harness/opencode/{empty,unrelated,jsonc_comments,handmade,both_files}/…`

**Interfaces:**
- Consumes: Task 8.
- Produces: same `plan_install` / `plan_uninstall` signatures as Task 10; `EditError::NeedsManual { file: PathBuf, snippet: String }`.
- Target file: `~/.config/opencode/opencode.jsonc` if it exists, else `opencode.json` (created when neither exists, with `"$schema": "https://opencode.ai/config.json"`). Entry `mcp.axle = {"type":"local","command":["<axl>","bridge","--harness","opencode"],"enabled":true}`. Marker block in `~/.config/opencode/AGENTS.md`. On a CST parse or edit failure: `NeedsManual` with the exact snippet; the file is not written and the summary says "needs manual step".

- [ ] **Step 1: Write fixtures and failing tests**: `empty_creates_opencode_json`, `unrelated_mcp_servers_survive`, `jsonc_comments_preserved`, `handmade_axle_adopted`, `prefers_jsonc_when_both_exist`, `rerun_produces_empty_plan`, `uninstall_restores_original`, `opencode_unusual_shape_unparseable_jsonc_needs_manual` (file unchanged byte-for-byte; error snippet contains `"bridge", "--harness", "opencode"`).
- [ ] **Step 2: Run** `cargo test --test install_opencode` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --test install_opencode` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(install): OpenCode config editor with manual-step fallback"`

---

### Task 12: `axl service` (launchd / systemd)

**Files:**
- Create: `src/daemon/mod.rs`, `src/daemon/launchd.rs`, `src/daemon/systemd.rs`, `src/cli/service.rs`, `tests/daemon.rs`
- Modify: `src/main.rs` (`Service { command: ServiceCommand }` with `install [--port N] | uninstall [--purge] | start | stop | restart | status | logs`), `src/lib.rs`

**Interfaces:**
- Produces:
  - `pub struct UnitSpec { pub axl: PathBuf, pub home: PathBuf, pub port: u16 }` with `fn env(&self) -> Vec<(&'static str, String)>` = `[("DATABASE_URL", "sqlite://<home>/.axl/axl.db"), ("AXL_BIND", "127.0.0.1:<port>"), ("AXL_UI_AUTH", "auto")]`.
  - `pub fn launchd_plist(spec: &UnitSpec) -> String` (Label `com.reyemtech.axle`, `ProgramArguments` = `[<axl>, "serve"]`, `RunAtLoad` and `KeepAlive` true, `StandardOutPath`/`StandardErrorPath` = `<home>/.axl/logs/axl.log`, XML-escaped paths); `pub fn plist_path(home) -> PathBuf`.
  - `pub fn systemd_unit(spec: &UnitSpec) -> String` (`[Service] ExecStart=<axl> serve`, `Environment=` lines quoted, `Restart=on-failure`, `StandardOutput=append:<log>`, `[Install] WantedBy=default.target`); `pub fn unit_path(home) -> PathBuf`.
  - `pub trait ServiceControl { fn install(&self, spec: &UnitSpec) -> Result<(), DaemonError>; fn uninstall(&self) -> …; fn start(&self) …; fn stop(&self) …; fn restart(&self) …; fn status(&self) -> Result<ServiceState, DaemonError>; }` with `Launchd` (`launchctl bootstrap gui/<uid> <plist>`, `bootout`, `kickstart -k`, `print`) and `Systemd` (`systemctl --user daemon-reload`, `enable --now axle.service`, …) implementations taking a `Runner` (`fn run(&self, prog: &str, args: &[&str]) -> io::Result<Output>`) so tests record commands instead of running them.
  - `pub fn platform(home: &Path, runner: Box<dyn Runner>) -> Result<Box<dyn ServiceControl>, NoServiceManager>`; `NoServiceManager`'s message prints the foreground instructions (`DATABASE_URL=sqlite://… axl serve`).
  - `pub fn port_free(port: u16) -> bool` and `pub fn install_unit(ctl: &dyn ServiceControl, dir: &AxlDir, spec: &UnitSpec) -> Result<(), DaemonError>` (the one place that checks the port, skipped when `ctl.status()` is already running, then calls `ctl.install` and saves the port); `axl service install` and Task 13 both call it. Install refuses a taken port with `port <N> is in use; pick another with --port` and saves a non-default port to `config.toml` (`port` and `server_url = http://127.0.0.1:<port>`).
  - `uninstall --purge` alone deletes `~/.axl/axl.db*`.

- [ ] **Step 1: Write failing tests** in `tests/daemon.rs`: `plist_matches_golden` (against `tests/fixtures/daemon/com.reyemtech.axle.plist` with `{{HOME}}`/`{{AXL}}`), `unit_matches_golden` (`tests/fixtures/daemon/axle.service`), `paths_with_spaces_are_escaped`, `launchd_install_runs_bootstrap_for_uid` (recorded `["launchctl","bootstrap","gui/<uid>",<plist>]`), `systemd_install_enables_now` (`["systemctl","--user","enable","--now","axle.service"]`), `no_systemd_prints_serve_instructions`, `taken_port_is_refused_with_hint` (bind a listener, assert error text contains `--port`), `uninstall_keeps_db_unless_purge`.
- [ ] **Step 2: Run** `cargo test --test daemon` — Expected: FAIL.
- [ ] **Step 3: Implement.** `uid` via `libc::getuid()` (already a dependency).
- [ ] **Step 4: Run** `cargo test --test daemon` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(cli): axl service with launchd and systemd user units"`

---

### Task 13: `axl install` orchestration

**Files:**
- Create: `src/cli/install.rs`, `tests/cli_onboarding.rs`
- Modify: `src/main.rs` (`Install { server, code, harness: Vec<Harness>, local, dry_run, yes }`)

**Interfaces:**
- Consumes: Tasks 1, 7 (`login`), 8–11 (`plan_install`), 12 (`platform`, `UnitSpec`), `AuthService::create` (local mint).
- Produces: `pub struct InstallArgs { pub server: Option<String>, pub code: Option<String>, pub harness: Vec<Harness>, pub local: bool, pub dry_run: bool, pub yes: bool }`; `pub async fn install(env: &InstallEnv, args: InstallArgs) -> Result<InstallReport, ClientError>` where `InstallEnv { home, path_env, current_exe, interactive: bool, prompt: &dyn Prompt, out: &mut dyn Write, service: Option<Box<dyn ServiceControl>> }` and `InstallReport { rows: Vec<HarnessRow>, server_warning: Option<String> }`, `HarnessRow { harness, outcome: Outcome /* Configured | AlreadyConfigured | NeedsManual(String) | NotFound */, next_step: Option<String> }`; `fn exit_code(&self) -> ExitCode` (0 if any Configured/AlreadyConfigured, else 1).
- Server step, in order (spec §5 step 1): `--local` → local mode. `--server URL` with no token for that URL (config `server_url` equals it and a token exists) → `login` (code exchange or device flow); if a token for a different server exists, install returns login's `--force` error unchanged (install has no `--force`; the user runs `axl login <url> --force` first). Under `--dry-run` login is skipped and `would log in to <url>` is printed. Configured server → `readyz`; failure sets `server_warning` (`Could not reach <url>. Check the VPN or URL, run axl status, or run axl login again.`) and continues; never switches to local. Nothing configured: interactive and not `--yes` → prompt `Run Axle on this machine, or connect to a server?` (`This machine` / `A server` → ask URL → device login); otherwise local mode.
- Local mode: `daemon::install_unit(&*service, &dir, UnitSpec)` (port from `config.port` or 7420; Task 12) → wait up to 15 s for `/readyz` → open `~/.axl/axl.db` with `SqliteStore::connect` and `AuthService::create("<host> (local)")` → `write_token` + `save_config` (load, modify, save: keeps `port`). Skipped (not just printed) under `--dry-run`, which prints `would install the local service` instead.
- Harness step: detect, filter by `--harness`, `plan_install` each (Claude Code with `claude_code::MODE`), merge into one `Plan` in the order Claude Code, Codex, OpenCode; when several edits target the same path (adopted `AXL_SERVER_URL` moving into `~/.axl/config.toml`) the first changed edit wins and later ones for that path are dropped; `--dry-run` prints `render_diff` and writes nothing (manifest included); otherwise `apply` then `Manifest::save`. Adopted entries are printed as `adopted hand-made <key> in <file>`.
- Summary table columns `Harness | Result | Next step` (Next step: `restart Claude Code` etc.).

- [ ] **Step 1: Write failing tests** in `tests/cli_onboarding.rs` (temp HOME; fake `Prompt`; recording `ServiceControl`; in-process server on port 0):

```rust
#[tokio::test] async fn install_with_server_and_code_logs_in_then_configures() { /* token written; rows Configured for detected harnesses; exit 0 */ }
#[tokio::test] async fn unreachable_server_still_configures_with_warning() { /* server_warning names "axl status"; service.install never called */ }
#[tokio::test] async fn no_server_non_interactive_goes_local() { /* recorded service install; token minted labelled "<host> (local)" */ }
#[tokio::test] async fn interactive_prompt_server_branch_runs_device_login() {}
#[tokio::test] async fn dry_run_writes_nothing() { /* HOME tree identical before/after (walk + bytes); output contains "+++" diff */ }
#[tokio::test] async fn rerun_is_noop() { /* second run: all AlreadyConfigured, no new backup files */ }
#[tokio::test] async fn rerun_rewrites_moved_binary() { /* change current_exe -> commands updated, one backup per changed file */ }
#[tokio::test] async fn no_harness_found_exits_1() {}
#[tokio::test] async fn harness_filter_limits_run() {}
#[test] fn e2e_serve_login_install_dry_run() {
    // real binary: spawn `axl serve --bind 127.0.0.1:<free port>` with DATABASE_URL = temp SQLite file;
    // issue a code by opening the same file with SqliteStore + EnrollService::issue_code;
    // run `axl login http://127.0.0.1:<port> --code <code>`, then `axl install --dry-run --yes`, both with HOME=temp
    // and a `.claude/` dir present; assert both exit 0, neither stdout nor stderr contains "axl_",
    // HOME has token + config.toml, and ~/.claude/ is unchanged
}
```

- [ ] **Step 2: Run** `cargo test --test cli_onboarding` — Expected: FAIL.
- [ ] **Step 3: Implement** per Interfaces; `main.rs` builds `InstallEnv` from the process (`std::io::IsTerminal` on stdin for `interactive`).
- [ ] **Step 4: Run** `cargo test --test cli_onboarding` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(cli): axl install with login, local mode and dry run"`

---

### Task 14: `axl uninstall`

**Files:**
- Create: `src/cli/uninstall.rs`
- Modify: `src/main.rs` (`Uninstall { harness: Vec<Harness>, service: bool, purge: bool, revoke: bool }`), `tests/cli_onboarding.rs`

**Interfaces:**
- Consumes: `plan_uninstall` (Tasks 9–11), `Manifest`, `ServiceControl::uninstall`, `logout(revoke)` (Task 7).
- Produces: `pub async fn uninstall(env: &InstallEnv, args: UninstallArgs) -> Result<(), ClientError>`. Removes only manifest-recorded items for the selected harnesses (all when none given), deletes those entries from the manifest (and the file once empty), removes `~/.axl/integrations/claude-code/` and the skills link. `--service` stops and removes the unit; `--purge` (only with `--service`) deletes the database; `--revoke` revokes the token on the server and removes `~/.axl/token`.

- [ ] **Step 1: Write failing tests**: `install_then_uninstall_restores_tree` (HOME tree equals the pre-install tree except `.axl-backup-*` files and `~/.axl/` itself), `uninstall_one_harness_keeps_others`, `uninstall_ignores_unrecorded_user_edits` (a user-added key next to `axle` survives), `uninstall_service_keeps_db_without_purge`, `uninstall_revoke_invalidates_token`.
- [ ] **Step 2: Run** `cargo test --test cli_onboarding uninstall` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --test cli_onboarding` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(cli): axl uninstall from the install manifest"`

---

### Task 15: `axl status`

**Files:**
- Create: `src/cli/status.rs`
- Modify: `src/main.rs` (`Status`), `tests/cli_onboarding.rs`

**Interfaces:**
- Consumes: `resolve_server_url`, `read_token`, `Api::{readyz, whoami}`, `platform().status()`, `Harness::detect`, each editor's `plan_install` (empty plan = configured).
- Produces: `pub async fn status(env: &InstallEnv) -> StatusReport` with `StatusReport { lines: Vec<StatusLine>, ok: bool }`, `StatusLine { label: String, value: String, fix: Option<String> }`; rendered as aligned `label  value` lines with `  fix: <command>` under each problem. Read-only. Problems and their fixes: unreachable → `axl login <url>` / check VPN; no token or 401 → `axl login <url>`; service installed but stopped → `axl service start`; version mismatch (`whoami.version != CARGO_PKG_VERSION`) → `axl service restart` for local, else upgrade note; harness not configured → `axl install --harness <name>`; configured path missing → `axl install`. Exit 0 when `ok`, else 1. Never prints the token.

- [ ] **Step 1: Write failing tests**: `status_all_good` (lines include server URL, `reachable`, token label from whoami, each harness `configured` / `not found`; exit 0), `status_reports_missing_token_with_fix` (`fix: axl login http://127.0.0.1:<port>`), `status_reports_stale_binary_path` (configured command points at a deleted path → `fix: axl install`), `status_flags_version_mismatch` (stub server returning `version: "0.0.1"`), `status_is_read_only` (HOME tree unchanged).
- [ ] **Step 2: Run** `cargo test --test cli_onboarding status` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --test cli_onboarding` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(cli): axl status with one fix per problem"`

---

### Task 16: Docs

**Files:**
- Create: `docs/cli.md`
- Modify: `README.md:32-50` (Quickstart), `docs/getting-started.md`, `docs/harnesses/{claude-code,codex,opencode}.md` (existing snippets move under a `## Manual setup` heading; Codex notes MCP-only), `docs/security.md` (enrollment codes, device flow, per-IP limit 10/min, `AXL_TRUSTED_PROXY_HEADER`), `docs/configuration.md` (`AXL_PUBLIC_URL`, `AXL_TRUSTED_PROXY_HEADER`, `~/.axl/config.toml`), `docs/http-api.md` (`/v1/enroll`, `/v1/login/start`, `/v1/login/poll`, `/v1/whoami`, `/v1/token/revoke`, `/install.sh`), `docs/deploy/docker.md` (`-e AXL_PUBLIC_URL=…`), `deploy/helm/axle/README.md` (`publicUrl`), `docs/web-ui.md` (Connect a machine, `/ui/connect`)

- [ ] **Step 1: Write** the Quickstart exactly as: local `brew install reyemtech/tap/axle && axl install`; team "paste the command from the web UI (Tokens → Connect a machine)"; existing CLI `axl login https://axle.example.com`.
- [ ] **Step 2: Verify** every command in the docs exists: `cargo run -- --help`, `cargo run -- install --help`, `login`, `status`, `service --help` — Expected: each documented flag appears.
- [ ] **Step 3: Commit** `git commit -m "docs: one-command onboarding, CLI reference, enrollment security"`

---

## Self-review notes

- Spec coverage: §1 → T1; §2 → T2–T4; §3 → T6; §4 → T7; §5 → T8–T11, T13; §6 → T14; §7 → T15 (+ whoami in T4); §8 → T12; §9 → T5; §10 → T16; Constraints tests → T2 (both backends), T4 (HTTP), T9–T11 (golden), T12 (service files), T6 (shellcheck), T13 (CLI e2e).
- Decisions taken where the spec leaves a choice: Codex MCP-only; JSONC via `jsonc-parser` CST with manual-step fallback; poll interval 7 s (5 s would exhaust the per-IP bucket after ~4.5 min); user code 8 Crockford chars `XXXX-XXXX`; per-IP 10/min; right-most `X-Forwarded-For` entry; device token label `<host> (device login)`; UI Revoke of a pending enrollment sets `denied`; `slow_down` tracked in memory (single replica); `/v1/token/revoke` added for `logout --revoke`; the client stores the URL the user typed, not the server's reply.
