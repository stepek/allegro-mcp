# Plan: GH-5 — Phase 5: Auth v2 — Device flow (primary user auth)

Status legend: each phase starts as **PENDING**. Implementer flips to **DONE**
(or **BLOCKED** with a note) as work lands.

---

## 0. Research findings (validated, not guesswork)

Confirmed against the live Allegro tutorial
<https://developer.allegro.pl/tutorials/uwierzytelnianie-i-autoryzacja-zlq9e75GdIR>
(fetched in full, 2026-09-10) — the "Device flow" section, plus the
"Przedłużenie ważności tokena" (token refresh) and "Kiedy token straci
ważność" (token invalidation) sections:

### 0.1 Device authorization endpoint

```
POST https://allegro.pl/auth/oauth/device            (sandbox: https://allegro.pl.allegrosandbox.pl)
Authorization: Basic base64(client_id:client_secret)
Content-Type: application/x-www-form-urlencoded
```

- The curl example puts `client_id` in the **query string**; the official
  PHP sample sends `client_id=CLIENT_ID` in the **form body**. Both work.
  **Decision: send it in the form body** (cleaner URL, matches the PHP
  sample, and Basic auth already carries the credentials).
- Optional `scope` form param (space-joined, URL-encoded) — same semantics
  as the other grants; `reqwest::RequestBuilder::form()` percent-encodes.
- Response fields (values in the docs' json5 sample are **quoted strings**
  — `"expires_in": "3600"`, `"interval": "5"` — the real API may return
  numbers; we must parse both, see §0.5):
  - `user_code` (e.g. `"cbt3zdu4g"`) — Allegro recommends displaying it to
    the user as `XXX XXX XXX` (groups of 3, space-separated);
  - `device_code` (e.g. `"645629715"`) — needed for polling; **single-use**;
  - `expires_in` — seconds both codes stay valid (sample: 3600);
  - `interval` — **required** minimum seconds between polls; polling faster
    yields HTTP 400 `slow_down`;
  - `verification_uri` (e.g. `https://allegro.pl/skojarz-aplikacje`);
  - `verification_uri_complete` (verification URL with the user code
    pre-filled — use this for a "clickable" banner).

### 0.2 Token polling

```
POST https://allegro.pl/auth/oauth/token
Authorization: Basic base64(client_id:client_secret)
Content-Type: application/x-www-form-urlencoded

grant_type=urn:ietf:params:oauth:grant-type:device_code&device_code={device_code}
```

Exactly **five** kinds of responses (docs' enumeration):

| Response | Meaning | Action |
|---|---|---|
| `200` + token JSON (`access_token`, `token_type`, `refresh_token`, `expires_in`≈43199, `scope`, `jti`) | authorized | **device_code can return a token only once** — reusing it afterwards yields 400 |
| `400` `{"error":"authorization_pending"}` | user hasn't approved yet | keep polling at `interval` |
| `400` `{"error":"slow_down"}` | polling too fast | slow down to `interval` (Allegro's own PHP sample does `interval++` → **+1 s per slow_down**, matching the ticket) |
| `400` `{"error":"access_denied"}` | user denied | **terminal** — stop polling |
| `400` `{"error":"Invalid device code"}` | device_code invalid, consumed or **expired** | **terminal** — stop, generate a new code |

Note the non-standard `"Invalid device code"` literal (spaces, not
snake_case) — this is Allegro's shape for RFC 8628's `expired_token`. Any
**other** 400 error code per the docs means "codes expired or your request
is malformed" → also terminal. 5xx / network failures are transient (retry
with a bounded consecutive-failure cap).

### 0.3 Refresh grant (rotation, single-use)

```
POST /auth/oauth/token   (Basic auth)
grant_type=refresh_token&refresh_token={refresh_token}
```

- Access token lives **12 h**; every refresh returns a **new pair**:
  new `access_token` (12 h) + **new single-use `refresh_token` (3 months)**.
- The old refresh token stays usable for a **60 s grace window** after the
  first refresh (crash-recovery window only — useless across restarts).
- Allegro never returns the refresh token's own expiry → client-side age
  heuristic needed (store `updated_at_epoch`; treat refresh as dead after
  90 days).
- Tokens (both access and refresh) also die when the user logs out
  everywhere, changes password/e-mail, gets a sales block, unlinks the app
  (Powiązane aplikacje), or exceeds 20 active sessions. → the **401
  auto-refresh hook** must handle "refresh also fails" by pointing the user
  at `allegro-mcp auth device`.

### 0.4 Registration constraint (docs)

Device flow requires an app registered as *"Aplikacja będzie działać w
środowisku bez dostępu do przeglądarki…"* (device-type app). **You cannot
change an app's type after registration** — a `client_credentials`-style
app registration cannot run device flow. Out of our control; document in
README. Sandbox doesn't require 2FA for registration.

### 0.5 Numeric leniency

The docs' json5 samples show `expires_in` / `interval` as strings while the
auth-code section shows `"expires_in":43199` unquoted — the API has been
inconsistent across responses. **Decision: a lenient `u64` deserializer
accepting both JSON numbers and numeric strings** for `expires_in` and
`interval` on the device response (the token response's `expires_in` is
already handled as a plain number today and is consistent in practice; we
harden it too — see Phase 3).

### 0.6 RFC 8628 deltas vs the ticket

- RFC 8628 §3.5 recommends increasing the polling interval by **5 s** on
  `slow_down`; the ticket says **+1 s** and Allegro's own sample does `+1`.
  **Decision: follow the ticket (+1 s)**, hoisted into a named const so it's
  a one-line change if Allegro starts enforcing harder.
- RFC 8628 `expired_token` ≈ Allegro's `"Invalid device code"` (§0.2).
- Ticket calls the terminal state `expired`; we name the variant
  `ExpiredOrInvalid` to cover both shapes.

---

## 1. Context (current code, branch `5`)

- `src/auth/mod.rs` (~650 lines): `AllegroAuth` — thread-safe
  `client_credentials` manager; `RwLock<Option<CachedToken>>` cache with
  60 s refresh margin + double-checked write-lock pattern (single-flight);
  `token()`, `status() -> AuthStatus { auth_flow: &'static str, .. }`
  (hardcoded `"client_credentials"`); constructors `from_env` / `new` /
  `with_base_url` (test injection) / `with_http_client` (config wiring) +
  builder `with_scopes`. `TokenResponse` has **no `refresh_token` field**.
  `CachedToken { access_token, expires_at: Instant }` — `Instant` is not
  serializable; persistence needs wall-clock epoch seconds.
- `src/config.rs`: `AuthFlow` enum with only `ClientCredentials`
  (`#[serde(rename_all = "snake_case")]`); **`auth_flow = "device_code"` is
  currently rejected** — and the unit test
  `from_toml_str_unknown_auth_flow_errors_listing_supported_values`
  (config.rs:760) *uses* `"device_code"` as its unknown-value fixture, so it
  **must be updated** in this phase. `token_path: Option<PathBuf>` exists,
  documented "reserved for a future phase" — consumed here. Host helpers
  `auth_base_url(sandbox)` / `api_base_url(sandbox)`. `deny_unknown_fields`.
- `src/main.rs`: clap `Cli` with root flags `--sandbox` (root-level, **not**
  `global = true`), `--stdio`, `--port`, `--config`, …; subcommands
  `Schema`, `Tools`, `Healthcheck`; `build_allegro_server()` reads
  `ALLEGRO_CLIENT_ID/SECRET`, builds
  `AllegroAuth::with_http_client(..).with_scopes(..)`; both transports share
  it.
- `src/dispatcher.rs`: `dispatch_with_base()` fetches token → builds one
  request → maps any 4xx/5xx to `Err(String)`. **No 401 retry today.**
- `src/http_server.rs:102-123`: eager startup auth check + banner with the
  stale "no device-flow exists" discrepancy comment; hard-fails startup when
  `token()` errors.
- `tests/http_server_integration.rs:122,169`: asserts
  `body["auth_flow"] == "client_credentials"` on `/auth/status` — stays
  green as long as the label follows the active flow.
- Deps: **no new Cargo.toml dependencies needed.** Atomic write = `std::fs`
  (temp file + `rename`); 0600 = `std::os::unix::fs::PermissionsExt`;
  lenient JSON = `serde_json` (already a dep); `dirs` 6.0 already present;
  wiremock/tempfile/serial_test already dev-deps.

---

## 2. Design decisions

1. **Keep `AllegroAuth` as the single façade** — no `TokenProvider` trait.
   `&AllegroAuth` / `Arc<AllegroAuth>` is threaded through `dispatcher`,
   `AllegroServer`, `http_server::AppState` concretely; introducing a trait
   would touch every call site for zero behavioural gain. Instead
   `AllegroAuth` gains an internal `mode: FlowMode { ClientCredentials,
   Device }` plus `Option<TokenStore>`; `token()`'s resolution chain differs
   per mode (§4.2). All existing constructors keep client-credentials
   semantics (existing tests untouched); a `with_token_store(TokenStore)`
   builder switches to device mode.
2. **Three auth files**: `mod.rs` (façade + refresh grant),
   `device.rs` (device authorization + polling state machine, disk-free),
   `token_store.rs` (JSON persistence). Device logic stays independently
   testable; persistence is isolated for atomicity/permission testing; the
   façade composes both. Refresh lives in `mod.rs` because it is a token-
   endpoint grant used by the façade's resolution chain (client_credentials
   never has refresh tokens, so it is device-adjacent, not device-specific).
3. **The server never runs the interactive flow.** Interactive device flow
   happens only in the `allegro-mcp auth device` CLI (ticket: "no browser
   loop needed in-server"). Server mode with `auth_flow = "device_code"`
   restores tokens from disk (or refreshes); if only an **unexpired pending
   grant** exists, it prints the verification banner (stderr / docker logs)
   and spawns a background poll task; if nothing is stored it fails startup
   with "run `allegro-mcp auth device`" instructions.
4. **One versioned JSON file** `tokens.json` holds both *granted tokens*
   and the *in-flight device grant* (`pending_device_grant`). Persisting the
   pending grant is what makes "kill mid-poll → restart → resume" work
   (acceptance criterion 2): the restarted process polls the same
   `device_code` instead of demanding a fresh authorization.
5. **Wall-clock epoch seconds in the file**, `Instant` only in memory —
   tokens must survive process restarts and sleep/suspend.
6. **401 auto-refresh in the dispatcher**: on exactly one 401 per dispatch,
   invalidate the cache, re-resolve the token (device mode: refresh grant;
   client_credentials mode: plain refetch), retry once. One retry, bounded.
7. **Config `token_path` wins over the default path**
   (`dirs::config_dir()/allegro-mcp/tokens.json`); subcommand-local
   `--token-path` on `auth device` wins over both for the interactive flow.
   Add `ALLEGRO_MCP_TOKEN_PATH` env override (aligns with the table in
   `.plan/gh-7.md`, useful for Docker).
8. **No MCP-protocol changes**: the banner goes to stderr (tracing + direct
   `eprintln!` in CLI), satisfying the ticket's "stderr banner / MCP logging
   notification" alternative. rmcp's logging capability stays off (only
   tools are advertised today) — see Open Questions.

---

## 3. Files to create / modify

| File | Action | Purpose |
|---|---|---|
| `src/auth/token_store.rs` | **Create** | versioned JSON persistence: atomic write, 0600, pending grant + tokens, env label |
| `src/auth/device.rs` | **Create** | `DeviceAuthorizationResponse`, error taxonomy, poll state machine, `request_device_code`, `poll_for_token`, banner |
| `src/auth/mod.rs` | **Modify** | `TokenResponse.refresh_token`, `AuthError` variants, `FlowMode`, device-mode `token()` chain, `invalidate()` / `install_tokens()` / `refresh_now()`, `flow_label()` |
| `src/config.rs` | **Modify** | `AuthFlow::DeviceCode`, `token_path` docs → consumed, `ALLEGRO_MCP_TOKEN_PATH` env, fix the unknown-flow test fixture |
| `src/main.rs` | **Modify** | `Commands::Auth(AuthAction::Device)`, `--sandbox` → `global = true`, `run_auth_device`, device branch in `build_allegro_server` |
| `src/dispatcher.rs` | **Modify** | single 401 retry with re-resolved token |
| `src/http_server.rs` | **Modify** | flow-aware startup banner; delete the stale "no device-flow" comment |
| `src/lib.rs` | — | no change (`auth` already `pub`; new submodules declared inside `src/auth/mod.rs`) |
| `tests/device_flow_integration.rs` | **Create** | wiremock: happy path, slow_down, pending, access_denied, expired, resume, refresh rotation, refresh failure, 401 retry |
| `tests/config_integration.rs` | **Modify** | device_code acceptance (if it asserts the rejection — grep first) |
| `README.md`, `SECURITY.md` | **Modify** | device flow usage; drop "token_path reserved" wording (SECURITY.md:26) |

---

## Phase 1: `src/config.rs` — DONE

- [x] 1.1 Extend the enum:
  ```rust
  #[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
  #[serde(rename_all = "snake_case")]
  pub enum AuthFlow {
      #[default]
      ClientCredentials,
      DeviceCode,
  }
  ```
  The serde error for unknown values automatically lists both supported
  variants (message still contains `client_credentials`).
- [x] 1.2 Update `token_path` doc comment: consumed by the device flow;
  default `dirs::config_dir()/allegro-mcp/tokens.json`; relative paths are
  CWD-relative (document).
- [x] 1.3 `apply_env`: read `ALLEGRO_MCP_TOKEN_PATH` → `cfg.token_path =
  Some(PathBuf::from(v))` (non-empty, trimmed; empty string is ignored —
  same hygiene as `ALLEGRO_MCP_CONFIG` in `discover_path`).
- [x] 1.4 **Fix the existing test** `from_toml_str_unknown_auth_flow_errors_
  listing_supported_values` (config.rs:760): change the unknown fixture from
  `"device_code"` to `"authorization_code"`; additionally assert the message
  contains both `client_credentials` and `device_code`.
- [x] 1.5 New tests: `from_toml_str_device_code_flow_parses`;
  `apply_env_token_path_override` (`#[serial]`, reuse the `with_env`
  helper); `apply_env_token_path_empty_ignored`.
- [x] 1.6 Commit: `feat(config): accept auth_flow = "device_code", consume token_path (#5)`

## Phase 2: `src/auth/token_store.rs` — DONE

Declare in `src/auth/mod.rs`: `pub mod device; pub mod token_store;`
(added in Phase 3; create the store file first so Phase 2 lands compiles as
part of the same commit — or add the `mod` lines immediately).

- [x] 2.1 Types (all `serde::Serialize + Deserialize`, `#[serde(default)]`
  on the envelope, **no** `deny_unknown_fields` — the file is
  machine-written and forward compatibility is handled by `version`):
  ```rust
  pub const STORE_FORMAT_VERSION: u64 = 1;

  pub struct StoredTokens {
      pub access_token: String,
      pub refresh_token: Option<String>,
      pub expires_at_epoch: u64,   // access-token expiry, UNIX epoch s
      pub scope: Option<String>,
      pub updated_at_epoch: u64,   // when this pair was written (refresh-age heuristic)
  }
  pub struct PendingDeviceGrant {
      pub device_code: String,
      pub user_code: String,
      pub verification_uri: String,
      pub verification_uri_complete: Option<String>,
      pub interval_secs: u64,
      pub expires_at_epoch: u64,
  }
  #[derive(Default)]
  pub struct StoredState { pub tokens: Option<StoredTokens>, pub pending: Option<PendingDeviceGrant> }
  ```
  Envelope on disk:
  ```json
  { "version": 1, "env": "production",
    "tokens": { ... }, "pending_device_grant": { ... } }
  ```
  `"env"` is `"production" | "sandbox"` — tokens are **not** interchangeable
  between environments (docs + config.rs module doc); mismatch ⇒ hard error.
- [x] 2.2 `TokenStore`:
  ```rust
  pub struct TokenStore { path: PathBuf, sandbox: bool }
  impl TokenStore {
      pub fn default_path() -> Result<PathBuf, AuthError>; // dirs::config_dir()/allegro-mcp/tokens.json; AuthError::Store on dirs::config_dir()==None
      pub fn new(path: PathBuf, sandbox: bool) -> Self;
      pub fn path(&self) -> &Path;
      pub fn load(&self) -> Result<StoredState, AuthError>; // missing file → Ok::default()
      pub fn save_tokens(&self, pair: &crate::auth::TokenResponse, effective_expires_in: u64) -> Result<(), AuthError>;
      pub fn save_pending(&self, resp: &super::device::DeviceAuthorizationResponse) -> Result<(), AuthError>;
      pub fn clear_pending(&self) -> Result<(), AuthError>; // rewrite envelope minus pending
      pub fn clear(&self) -> Result<(), AuthError>;         // remove file (logout)
  }
  ```
  `save_tokens` writes **only** `tokens` (dropping any stale `pending`) in
  one atomic write — grant completion and pending-clearance are the same
  operation, no torn state. `clear_pending` preserves `tokens`.
- [x] 2.3 Atomic write helper `fn write_atomic(path: &Path, bytes: &[u8]) ->
  io::Result<()>`:
  1. `create_dir_all(parent)`; on unix best-effort `set_permissions(0o700)`
     on the created allegro-mcp dir only when we created it (track via
     `metadata` before/after; never chmod a pre-existing dir);
  2. temp file **in the same directory**: `.<file_name>.tmp-{pid}` via
     `OpenOptions::new().write(true).create_new(true)` + `.mode(0o600)`
     (`#[cfg(unix)]` — avoids a world-readable window);
  3. `write_all` + `sync_all`;
  4. `fs::rename(tmp, path)` (atomic on the same filesystem);
  5. best-effort parent-dir `sync_all` (ignore errors).
  On `#[cfg(not(unix))]` skip `.mode()` (Windows ACLs apply; noted in Risks).
  Clean up the temp file on any error path (best-effort `remove_file`).
- [x] 2.4 Load semantics: missing file → `Ok(StoredState::default())`;
  `version != 1` → `AuthError::StoreVersion { found }` ("written by a newer
  allegro-mcp — delete the file or upgrade"); `env` mismatch →
  `AuthError::EnvMismatch { stored, current }`; unparsable JSON →
  `AuthError::StoreIo` carrying the path. **Never auto-delete** a corrupt
  file — surface it (`auth device` overwrites on success anyway).
- [x] 2.5 Unit tests (`#[cfg(test)]`, tempdir-based):
  round-trip save/load; missing file → default; corrupt JSON → error naming
  path; `version: 2` → `StoreVersion`; sandbox/prod mismatch →
  `EnvMismatch`; no `.tmp-*` leftovers after save; **perms test**
  (`#[cfg(unix)]`): file mode `& 0o777 == 0o600`, parent dir `0o700` when
  created; `save_tokens` drops pending; `clear_pending` keeps tokens;
  `clear` removes the file.
- [x] 2.6 Commit: `feat(auth): versioned on-disk token store with atomic 0600 writes (#5)`

## Phase 3: `src/auth/device.rs` — DONE

- [x] 3.1 Response type with lenient numerics (§0.5):
  ```rust
  #[derive(Debug, Clone, serde::Deserialize)]
  pub struct DeviceAuthorizationResponse {
      pub user_code: String,
      pub device_code: String,
      #[serde(deserialize_with = "u64_lenient")] pub expires_in: u64,
      #[serde(deserialize_with = "u64_lenient")] pub interval: u64,
      pub verification_uri: String,
      pub verification_uri_complete: Option<String>,
  }
  ```
  `u64_lenient` deserializes via `serde_json::Value` accepting
  `Number(as_u64)` and numeric `String::parse`. Harden the existing token
  response's `expires_in` the same way (in `mod.rs`) — one helper, used
  twice.
- [x] 3.2 Poll outcome + terminal taxonomy (ticket + §0.2):
  ```rust
  pub enum PollOutcome {
      Token(TokenResponse),     // 200
      AuthorizationPending,     // 400 authorization_pending → continue
      SlowDown,                 // 400 slow_down → interval += SLOW_DOWN_INCREMENT_SECS, continue
      AccessDenied,             // 400 access_denied → terminal
      ExpiredOrInvalid,         // 400 expired_token | invalid_grant | "Invalid device code"
                                //   | any other 4xx error code → terminal (§0.2: other codes = expired or bad request)
      Transient(String),        // 5xx / transport error / 200 body unparsable → retry, capped
  }
  pub enum DeviceFlowError {
      AccessDenied,
      Expired,                  // ExpiredOrInvalid OR wall-clock deadline passed
      Network(String),          // MAX_TRANSIENT_ERRORS consecutive transient failures
  }
  pub const SLOW_DOWN_INCREMENT_SECS: u64 = 1;  // ticket + Allegro PHP sample; RFC 8628 says 5 — see Risks
  pub const MAX_TRANSIENT_ERRORS: u32 = 5;
  ```
  `fn classify(status: u16, body: &str) -> PollOutcome` — pure, unit-testable:
  parse `{"error": ...}` with `serde_json`; `"Invalid device code"` matched
  case-insensitively as substring (docs show it verbatim); unknown 4xx
  error strings → `ExpiredOrInvalid` with the raw code surfaced in logs.
- [x] 3.3 Poll state machine (pure):
  ```rust
  pub struct PollState { interval: Duration, deadline: Instant, transient_errors: u32 }
  impl PollState {
      pub fn new(interval_secs: u64, expires_in_secs: u64) -> Self; // deadline = now + expires_in
      pub fn interval(&self) -> Duration;
      pub fn deadline_exceeded(&self) -> bool;
      /// ControlFlow::Break(reason) on terminal, Continue to poll again.
      pub fn advance(&mut self, outcome: &PollOutcome) -> ControlFlow<DeviceFlowError>;
  }
  ```
  Unit tests: pending keeps interval; slow_down adds exactly +1 s (twice in
  a row → +2 s); access_denied/expired break immediately; transient × 5
  breaks; deadline check boundary.
- [x] 3.4 `request_device_code(deps: &DeviceFlowDeps) -> Result<Device
  AuthorizationResponse, AuthError>`:
  `POST {auth_base_url}/auth/oauth/device`, `.basic_auth(id, Some(secret))`,
  `.form(&[("client_id", id)] (+ ("scope", space-joined) when scopes
  non-empty))`. `DeviceFlowDeps { http: reqwest::Client, auth_base_url:
  String, client_id: String, client_secret: String, scopes: Vec<String>,
  policy: PollingPolicy }` with `pub struct PollingPolicy { pub
  interval_override: Option<Duration> }` and
  `PollingPolicy::production()` (`None`) / `PollingPolicy::test_instant()`
  (`Some(Duration::ZERO)`) so wiremock tests don't real-sleep.
- [x] 3.5 `poll_for_token(deps, device_code: &str, state: PollState) ->
  Result<TokenResponse, DeviceFlowError>`:
  loop `{ if deadline_exceeded → Err(Expired); tokio::time::sleep(interval);
  POST /auth/oauth/token form [("grant_type",
  "urn:ietf:params:oauth:grant-type:device_code"), ("device_code", …)]
  Basic auth; classify; advance; Token → return }`. Uses
  `tokio::time::sleep` (pausable in tests); never `error_for_status` before
  body read (400 bodies carry the taxonomy).
- [x] 3.6 User-facing banner (pure fn, reused by CLI and server):
  ```rust
  pub fn format_user_code(code: &str) -> String;   // "cbt3zdu4g" → "cbt 3zd u4g" (char-based groups of 3)
  pub fn banner_text(resp: &DeviceAuthorizationResponse, sandbox: bool, resumed: bool) -> String;
  ```
  Banner names `verification_uri_complete` first, then
  `verification_uri` + grouped `user_code` as the fallback, the expiry, and
  "waiting/polling every {interval} s"; `resumed=true` adds "resuming your
  earlier device authorization". Never logs `device_code`.
- [x] 3.7 Unit tests: parse numeric AND string `expires_in`/`interval`;
  missing `verification_uri_complete` tolerated; classify matrix (§0.2
  table — all five responses + unknown 4xx + 500 + garbage body);
  `format_user_code` grouping incl. non-3-multiple length; banner contains
  the complete URI and never the device_code.
- [x] 3.8 Commit: `feat(auth): device authorization request + polling state machine (#5)`

## Phase 4: `src/auth/mod.rs` — device mode + refresh grant — DONE

- [x] 4.1 `TokenResponse` += `pub token_type: Option<String>` and
  `pub refresh_token: Option<String>` (existing test JSON has neither →
  still parses; `expires_in` gets the `u64_lenient` treatment from 3.1).
- [x] 4.2 `AuthError` additions:
  ```rust
  #[error("re-authorization required: {reason} — run `allegro-mcp auth device`")]
  ReauthRequired { reason: String },
  #[error("token store error: {0}")]      StoreIo(String),        // io/serde on tokens.json
  #[error("token store version {found} is not supported (expected 1) — delete the file or upgrade")]
  StoreVersion { found: u64 },
  #[error("tokens.json was created for the {stored} environment but allegro-mcp is running against {current}")]
  EnvMismatch { stored: &'static str, current: &'static str },
  ```
  `token_store.rs`/`device.rs` reuse these (no separate error enums beyond
  `DeviceFlowError`, which converts into `AuthError::DeviceFlow`-ish via
  `ReauthRequired`/display — keep one public error surface; map
  `DeviceFlowError` → `anyhow` at the CLI boundary).
- [x] 4.3 Flow wiring:
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq)]
  enum FlowMode { ClientCredentials, Device }
  // AllegroAuth fields += mode: FlowMode, store: Option<TokenStore>
  pub fn with_token_store(mut self, store: TokenStore) -> Self; // sets FlowMode::Device
  pub fn flow_label(&self) -> &'static str;                     // "client_credentials" | "device_code"
  ```
  Existing constructors default to `ClientCredentials`/`None` — every
  existing test call site is untouched.
- [x] 4.4 Device-mode `token()` resolution chain (inside the existing
  double-checked write-lock slow path, preserving single-flight):
  1. in-memory cache valid → return (unchanged);
  2. `store.load()`: `tokens` present && access token unexpired
     (epoch-now < `expires_at_epoch` − 60) → seed cache → return;
  3. `refresh_token` present && `updated_at_epoch` within
     `REFRESH_TOKEN_MAX_AGE` (const `90 * 24 * 3600 − 86_400` = 89 days,
     §0.3 heuristic) → **refresh grant** (4.5) → `store.save_tokens` →
     seed cache → return. **Stale-store guard**: if the refresh returns
     definitive rejection (`invalid_grant`/"Invalid"), reload the store
     once — a concurrent `auth device` run may have rotated the file; retry
     only if the stored `refresh_token` differs from the one we tried;
  4. else → `Err(ReauthRequired { reason: "no stored device authorization" })`.
  Cache entries for device mode use the stored/refreshed `expires_in` with
  the existing 120 s clamp.
- [x] 4.5 Refresh grant:
  `POST /auth/oauth/token` form `[("grant_type", "refresh_token"),
  ("refresh_token", rt)]` Basic auth (no `scope` param — docs don't send
  one; response carries the scope). **Persist the rotated pair BEFORE
  updating the in-memory cache** (§ Risks: rotation crash window).
  `400 invalid_grant` / `"Invalid"`-style rejection after the stale-store
  guard → `store.clear()` + `Err(ReauthRequired { reason: "refresh token
  rejected — authorization was revoked or expired" })`.
- [x] 4.6 New public methods:
  - `pub async fn invalidate(&self)` — drop the in-memory cache (both modes);
  - `pub async fn install_tokens(&self, resp: &TokenResponse) -> Result<(),
    AuthError>` — device mode only: `store.save_tokens(resp, …)` then seed
    the cache (used by the CLI and the background resume task);
  - `pub async fn refresh_now(&self) -> Result<(), AuthError>` — clear
    cache then run the device-mode refresh path (or plain refetch in
    client_credentials mode); used by the dispatcher's 401 retry.
- [x] 4.7 `status()`: `auth_flow` becomes `self.flow_label()`; add
  `pub persisted: bool` (device mode: store reported `tokens`; client_
  credentials: always `false`) — additive field, serde adds the key.
- [x] 4.8 Unit tests: refresh triggered only when access expired (seed
  store with live access → no network; with expired access + refresh →
  refresh called); rotation persisted (store updated with the NEW refresh
  token, old one never reused); refresh rejection → `ReauthRequired` +
  store cleared; stale-store guard (file rotated underneath → second
  attempt succeeds); `install_tokens` seeds cache; `invalidate()` forces
  re-resolution; `flow_label()` per mode; status `persisted` flag.
  Network-dependent ones go to the integration file (Phase 7); pure
  precedence logic is testable by injecting a store seeded in a tempdir and
  pointing `with_base_url` at a wiremock (integration) — keep unit tests to
  label/mode/cache-seeding.
- [x] 4.9 Commit: `feat(auth): device-mode token resolution + single-use refresh rotation (#5)`

## Phase 5: `src/dispatcher.rs` — 401 auto-refresh retry — DONE

- [x] 5.1 Restructure `dispatch_with_base`: hoist method/url/params/accept
  computation above a local closure `let build = |token: &str| builder…`;
  send once; if `status == StatusCode::UNAUTHORIZED`:
  `auth.invalidate().await` → `auth.token().await` → rebuild → retry
  **once**. If the retry also fails, return the retry's error (the original
  401 body is less interesting than the post-refresh one). Non-401 errors
  behave exactly as today.
- [x] 5.2 Rationale comment: device tokens are user-scoped and die
  out-of-band (password change, app unlink, 20-session cap — §0.3); the
  60 s pre-expiry refresh cannot see those, so the 401 hook is the only
  recovery path short of full re-auth.
- [x] 5.3 Unit tests (wiremock, mirroring the existing `test_dispatch_*`
  style): API mock returns 401 then 200 → `Ok`, and the mock saw 2 API
  hits; API always 401 → `Err` containing the second body; auth failure on
  the re-resolve → `Err` with `auth error` prefix.
- [x] 5.4 Commit: `feat(dispatcher): single 401 retry with token re-resolution (#5)`

## Phase 6: `src/main.rs` + `src/http_server.rs` — CLI + server wiring — DONE

- [x] 6.1 CLI:
  ```rust
  enum Commands { Schema { .. }, Tools { .. }, Auth { action: AuthAction }, Healthcheck }
  enum AuthAction {
      /// Authorize this machine with Allegro via the OAuth2 device flow.
      Device {
          /// Override where tokens are persisted (config file / default otherwise).
          #[arg(long)] token_path: Option<std::path::PathBuf>,
      },
  }
  ```
  Add `global = true` to the root `--sandbox` flag so
  `allegro-mcp auth device --sandbox` parses (clap: root-level `--sandbox`
  currently only accepts args *before* the subcommand). Verify the three
  existing parse tests still pass and add
  `auth_device_subcommand_parses` / `auth_device_token_path_flag_parses`.
- [x] 6.2 `async fn run_auth_device(cfg: config::Config, api_client:
  reqwest::Client, token_path_override: Option<PathBuf>) -> Result<()>`:
  1. read `ALLEGRO_CLIENT_ID` / `ALLEGRO_CLIENT_SECRET` (same mapping as
     `build_allegro_server`);
  2. path precedence: subcommand flag > `cfg.token_path` >
     `TokenStore::default_path()`;
  3. `store.load()` — **resume**: `pending` present && unexpired → banner
     (`resumed = true`, printed to **stderr**) → `poll_for_token` with the
     stored `device_code`/`interval`;
  4. fresh start otherwise: `request_device_code` → `store.save_pending` →
     banner to stderr → `poll_for_token`;
  5. success: `store.save_tokens(..)` (atomically clears pending — 2.3) →
     stdout summary: saved path, `expires_in` (human), scope;
  6. error mapping: `AccessDenied` → "authorization denied by user";
     `Expired` → "device/user code expired — rerun `allegro-mcp auth
     device`"; `Network` → transient failures; all exit non-zero via
     `anyhow::bail!`.
  No Ctrl-C handler — SIGINT kill is fine, persistence covers resume (that
  is acceptance criterion 2).
- [x] 6.3 Dispatch in `main()`: `Some(Commands::Auth { action:
  AuthAction::Device { token_path } }) => run_auth_device(cfg,
  api_client, token_path).await` (after config load, before any schema
  work — `auth device` must not require the OpenAPI schema).
- [x] 6.4 Device branch in `build_allegro_server()`:
  ```rust
  let auth = match cfg.auth_flow {
      config::AuthFlow::ClientCredentials => { /* current code */ }
      config::AuthFlow::DeviceCode => {
          let store = auth::token_store::TokenStore::new(effective_token_path, cfg.sandbox);
          auth::AllegroAuth::with_http_client(..).with_scopes(..).with_token_store(store.clone())
      }
  };
  ```
  Then device-mode startup policy:
  - `auth.token().await` succeeds → info "restored persisted authorization
    (expires in N s)" — the http_server eager check then passes unchanged;
  - `Err(ReauthRequired)` **and** `store.load()?.pending` unexpired → print
    `banner_text(resumed = true)` to stderr and
    `tokio::spawn` a resume task (`poll_for_token` with
    `PollingPolicy::production()`) that on success calls
    `auth.install_tokens(..)` and logs "device authorization completed";
    on terminal failure logs an error with the `auth device` hint. Server
    starts anyway; user-scoped tools return per-call `auth error` until the
    grant completes;
  - `Err(ReauthRequired)` with nothing usable → `anyhow::bail!("no Allegro
    authorization found — run `allegro-mcp auth device` first")` (startup
    fails loudly; docker logs carry the instruction).
- [x] 6.5 `src/http_server.rs`: replace the eager-check banner/discrepancy
  block (lines ~102-123): keep the hard-fail for client_credentials; for
  device mode (`auth.flow_label() == "device_code"`) a pending grant must
  not abort startup — use `auth.status().await` (token_cached/token_valid)
  instead of a bare `token().await` for the decision, and adjust the banner
  text per flow ("client_credentials check OK" vs "device authorization
  active/restored"). Delete the "no device-flow exists" comment.
- [x] 6.6 Commit: `feat(cli): allegro-mcp auth device + server-side device-mode wiring (#5)`

## Phase 7: integration tests + docs — DONE

- [x] 7.1 `tests/device_flow_integration.rs` (wiremock; tempdir token path;
  `PollingPolicy::test_instant()`; construct `AllegroAuth::with_base_url(..)
  .with_token_store(TokenStore::new(tmp, false))`):
  - `device_flow_happy_path_persists_tokens` — device endpoint 200 →
    token endpoint `authorization_pending` ×2 → 200; assert: request body
    carried `grant_type=urn:ietf:params:oauth:grant-type:device_code` +
    `device_code`, Basic auth header present, tokens.json contains access +
    refresh + epoch expiry, pending cleared, `auth.token()` returns the
    token without hitting the network again;
  - `device_flow_slow_down_backs_off` — `slow_down` ×2 then 200 (assert
    success; the +1 s state machine is unit-covered in 3.3);
  - `device_flow_access_denied_is_terminal` — 400 access_denied →
    `DeviceFlowError::AccessDenied`, file has pending only (no tokens);
  - `device_flow_expired_is_terminal` — 400 `{"error":"Invalid device
    code"}` → `Expired`; also `{"error":"expired_token"}`;
  - `resume_uses_persisted_pending_grant` — pre-seed store with pending →
    poll → 200; assert the `/auth/oauth/device` mock was **never** hit;
  - `refresh_rotates_and_persists` — seed store with expired access +
    refresh token → `auth.token()` → refresh grant hit, NEW refresh token
    persisted, cache seeded;
  - `refresh_rejection_forces_reauth` — seed expired pair, mock 400
    invalid_grant → `ReauthRequired`, store cleared;
  - `dispatcher_retries_once_on_401` — token endpoint + API 401-once →
    dispatch succeeds, API hit twice.
- [x] 7.2 Check `tests/config_integration.rs` (and grep the whole `tests/`
  tree) for a `device_code`-rejection assertion and update it to the
  acceptance case.
- [x] 7.3 Docs: README — short "headless login: `allegro-mcp auth device`"
  section incl. the device-type app registration requirement (§0.4);
  SECURITY.md:26 — replace "token_path reserved" with the actual
  0600/atomic/rotation story; docs/open-webui.md `/auth/status` sample may
  gain a device-mode example (optional).
- [x] 7.4 Commit: `test(auth): wiremock device-flow integration suite; docs (#5)`

---

## Tests to write (summary)

**Unit** — `device.rs`: lenient numeric parsing; classify matrix (pending /
slow_down / access_denied / "Invalid device code" / expired_token /
unknown-4xx / 5xx / garbage); `PollState` (+1 s on slow_down, transient cap,
deadline); `format_user_code` grouping; banner content (complete URI in,
device_code never out).
**Unit** — `token_store.rs`: round-trip; missing/corrupt/version/env-mismatch;
atomic write (no temp leftovers, content intact); `#[cfg(unix)]` 0600 file +
0700 created dir; save_tokens drops pending; clear_pending keeps tokens.
**Unit** — `mod.rs`: `flow_label()`; `invalidate()`; cache seeding via
`install_tokens`; status `persisted` flag; config: device_code parses,
unknown flow still lists both values, `ALLEGRO_MCP_TOKEN_PATH`.
**Integration** — `tests/device_flow_integration.rs`: the 9 scenarios in 7.1
(request shapes, polling sequences, resume, rotation, 401 retry).
**CLI** — clap parse tests for `auth device` (+ `--token-path`), global
`--sandbox` after subcommand.

## Acceptance criteria mapping (ticket → plan)

| Ticket acceptance | Covered by |
|---|---|
| Fresh machine → `auth device` → paste code at verification URL → token persisted | Phase 3 happy path + Phase 6 CLI + `device_flow_happy_path_persists_tokens` (7.1); manual sandbox smoke run |
| Kill process mid-poll → restart → resumes from persisted state | pending-grant persistence (2.1-2.3) + CLI resume (6.2.3) + `resume_uses_persisted_pending_grant` |
| Refresh works after expiry | Phase 4.4-4.5 + `refresh_rotates_and_persists`; `refresh_rejection_forces_reauth` covers the "lost refresh token = full re-auth" clause |
| Auto-refresh on 401 | Phase 5 + `dispatcher_retries_once_on_401` |
| Polling honors `interval` / `slow_down` (+1 s) / pending / denied / expired | §0.2 mapping, `PollState` unit tests + wiremock sequences |
| `verification_uri_complete` surfaced (stderr banner / MCP logging) | `banner_text` on stderr in CLI (6.2) and server resume path (6.4) |
| Token file atomic write + 0600 at `~/.config/allegro-mcp/tokens.json`, `token_path` override | Phase 2 (atomic rename, `PermissionsExt`, `dirs::config_dir()`) + config Phase 1 |
| `allegro-mcp auth device` subcommand | Phase 6.1-6.3 |

---

## Risks and open questions

1. **Quoted numbers in docs** (§0.5): `expires_in`/`interval` may arrive as
   strings — mitigated by the lenient deserializer; if Allegro also varies
   the token response's `expires_in`, the same helper covers it.
2. **`"Invalid device code"`** is a non-standard error literal (not
   snake_case); we match it case-insensitively **and** treat any unknown
   4xx as terminal — worst case we stop early and tell the user to rerun.
3. **slow_down increment**: ticket + Allegro sample say +1 s; RFC 8628 §3.5
   says +5 s. Const `SLOW_DOWN_INCREMENT_SECS` — one-line change if Allegro
   starts rate-limiting harder. Watch for 429s on the token endpoint in
   smoke tests.
4. **Rotation crash window**: between receiving the rotated pair and the
   atomic persist, a kill loses the new refresh token; the old one has only
   a 60 s grace (§0.3) → full re-auth. Mitigation: persist-first ordering
   (4.5) and a loud warn if persist fails while the cache holds the fresh
   pair (session survives until process exit).
5. **Refresh-token TTL is undocumented in-band** (3 months per docs): the
   89-day heuristic may force an early re-auth; a dead refresh token is
   handled cleanly (`ReauthRequired`).
6. **CLI + server share the file**: if `auth device` rotates tokens while a
   server is mid-flight, the server's cached access token stays valid but
   its next refresh could use a rotated-away token — covered by the
   stale-store re-check (4.4 step 3); worst case the server logs
   `ReauthRequired` until restarted. Documented; no file locking this phase.
7. **Windows**: 0600/0700 are unix-only (`#[cfg(unix)]`); Windows ACLs
   apply. CI matrix per gh-7 runs windows-latest — keep perms tests gated.
8. **`--sandbox` becomes `global = true`** — small CLI surface change;
   existing parse tests cover the before-subcommand position, add one for
   the after-subcommand position.
9. **Device-type app registration** is a prerequisite users must do in the
   portal (cannot convert an existing client-credentials app) — README must
   call this out; otherwise `POST /auth/oauth/device` fails with an opaque
   error.
10. **Open question — rmcp logging notifications**: ticket allows "MCP
    logging notification" as the banner channel; we chose stderr (this
    crate advertises only tools today). Enabling rmcp's logging capability
    to push `notifications/message` to clients is deliberately deferred.
11. **Open question — `scope` on device authorization**: docs support it;
    we send `cfg.scopes` when non-empty (single source of truth with
    client_credentials). If Allegro's device endpoint turns out to reject
    scope for some app types, the error surfaces at `request_device_code`
    — retry without scope is a possible follow-up, not built now.

## Cross-phase verification checklist (run after all phases land)

1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo build --locked && cargo test --locked` — all existing suites
   (`http_server_integration`, `config_integration`, `mcp_server_integration`,
   `schema_integration`, `tool_registry_integration`) stay green; the
   `/auth/status` label assertions still pass (flow-driven label).
4. Manual sandbox smoke (device-type sandbox app):
   `ALLEGRO_CLIENT_ID=.. ALLEGRO_CLIENT_SECRET=.. allegro-mcp --sandbox auth device`
   → banner shows `verification_uri_complete` → approve in browser →
   "Tokens saved to ~/.config/allegro-mcp/tokens.json"; `ls -l` the file
   (0600); rerun → resume/no-op path prints restored-authorization info.
5. Kill-mid-poll drill: start `auth device`, Ctrl-C after the banner,
   re-run → banner says "resuming", same user_code, completes.
6. `allegro-mcp --sandbox --stdio` (or `--port ..`) with
   `auth_flow = "device_code"` in allegro-mcp.toml → server starts from the
   persisted tokens; `GET /sale/categories` tool call succeeds; delete the
   file → server refuses to start with the `auth device` instruction.
