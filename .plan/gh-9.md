# Plan: GH-9 — Phase 9: Resilience — 429 backoff, error mapping, Trace-Id

Status legend: each phase starts as **PENDING**. Implementer flips to **DONE**
(or **BLOCKED** with a note) as work lands.

---

## 0. Research findings (validated, not guesswork)

### 0.1 Allegro error body — official shape

Confirmed against Allegro's public REST API design guidelines
(<https://allegro.github.io/restapi-guideline/Error/>, fetched 2026-09-10):

```json
HTTP/1.1 422 Unprocessable Entity

{
    "errors": [
        {
            "message":     "Delivery point data not passed",
            "code":        "MissingDeliveryPointException",
            "details":     "Exception was thrown from ... (null in production)",
            "path":        "Endpoint.getDeliveries.arg1",
            "userMessage": "Nie wybrano punktu dla odbioru osobistego."
        }
    ]
}
```

Field semantics per the guideline:

- `message` — internal dev-facing message;
- `code` — string error code / exception name;
- `details` — extra dev detail, **null in production** (parse but never rely on);
- `path` — which parameter failed validation, can be null;
- `userMessage` — localized via `Accept-Language` (we send `pl-PL` by
  default), *mandatory* per the guideline → this is the "user message" half
  of the ticket's dev+user message pairs.

The guideline's camelCase (`userMessage`) is authoritative — the ticket's
`{code,message,userMessage,path}` matches it minus `details`, which we parse
opportunistically (`Option`).

### 0.2 Trace-Id — official shape

Same guideline ("Provide Trace-Ids for Introspection"): **every** API
response carries a `Trace-Id` response header with a UUID value; it is the
correlation key Allegro support asks for. Consequences:

- capture it from **every** response we consider erroneous (429, 4xx, 5xx —
  and the token endpoint's errors too, when present);
- reqwest's `HeaderMap` lookup is case-insensitive and HTTP/2 lowercases
  names on the wire, so `headers().get("Trace-Id")` is correct regardless of
  wire casing.

### 0.3 Retry-After — RFC 7231 §7.1.3

`Retry-After` is legal as either delta-**seconds** (`Retry-After: 120`) or an
HTTP-date (`Retry-After: Wed, 21 Oct 2026 07:28:00 GMT`). Allegro sends
delta-seconds in practice, but we must not crash or mis-parse on the
HTTP-date form. `httpdate = "1.0"` (already a transitive dep in `Cargo.lock`
via hyper) parses IMF-fixdate; delta handling needs `SystemTime::now()`.

### 0.4 Rate-limit budget

Ticket: "soft cap under 9000 req/min per client_id, well under in
practice". The 9000/min figure is the Allegro per-`client_id` quota this
project has documented; "soft" = we deliberately cap *below* it (default
**8000**, ≈ 11 % headroom) so transient bursts never hit the real limit.
Enforcement is client-side only (Allegro does not expose quota headers on
the public API in a documented way — nothing to discover dynamically).

### 0.5 Test-infrastructure feasibility (verified in-repo)

- **wiremock 0.6.5** (`Cargo.lock`): `ResponseTemplate::new(429)
  .insert_header("Retry-After", "1").insert_header("Trace-Id", "…")` —
  `insert_header<K, V>` exists on `ResponseTemplate` (verified in
  `~/.cargo/registry/…/wiremock-0.6.5/src/response_template.rs:102`). Mock
  ordering convention (limited `up_to_n_times` mock mounted **first**,
  fallback last) is already documented in `tests/device_flow_integration.rs`.
- **No real sleeps in tests**: the repo already has the pattern —
  `auth::device::PollingPolicy { interval_override: Option<Duration> }` with
  `test_instant()` returning `Some(Duration::ZERO)`
  (`src/auth/device.rs:271-313`). Phase 9 replicates it as
  `BackoffPolicy { sleep_override: Option<Duration> }`. Rejected
  alternative: `tokio` `test-util` / `start_paused` — it would need a new
  dev-feature (`full` does not include `test-util`) and buys nothing over
  the established override pattern; auto-advance also interacts awkwardly
  with wiremock's real I/O.
- **No RNG dep needed for jitter**: xorshift64* seeded from wall-clock nanos
  is plenty (jitter is herd-decorrelation, not cryptography), keeps the
  dependency tree untouched, and is seedable for deterministic unit tests.
- **`httpdate`** is promoted from transitive to direct dependency (tiny,
  zero transitive deps of its own).

---

## 1. Current-state analysis (file:line refs, branch `9`)

### 1.1 Where requests happen

| Concern | Location | Today |
|---|---|---|
| reqwest client factory (UA/Accept-Language) | `src/http.rs:54-98` | Pooled `LazyLock` clients; no retry, no error mapping |
| **API dispatch** (all tool calls) | `src/dispatcher.rs:131-248` `dispatch_with_base` | one send; single 401→`refresh_now`→re-send; every other 4xx/5xx → `Err(format!("HTTP {status}: {body}"))` |
| Token mint (`client_credentials`) | `src/auth/mod.rs:380-409` `fetch_token` | `.send().await?.error_for_status()?` — **discards 429/4xx bodies and headers**; a 429 surfaces as generic `AuthError::Http` |
| Refresh grant (device mode) | `src/auth/mod.rs:580-607` `refresh_grant` | same `error_for_status()` pattern |
| Device-flow poll | `src/auth/device.rs:360-409` | own 5-outcome taxonomy + slow_down backoff; **a poll 429 lands in `classify_error_body` → `ExpiredOrInvalid` (terminal!)** — see §2.10 |
| Schema download | `src/schema/fetch.rs` via `http::build_schema_client` | 10 s timeout, no retry — out of scope |

### 1.2 Trace of today's failure paths (what must change)

- **429 from the API**: `dispatcher.rs:234-240` → `Err("HTTP 429 Too Many Requests: {raw body}")`. No backoff, no retry, no Retry-After respect, no Trace-Id.
- **5xx from the API**: same line — immediate error, no retry.
- **Network error**: `dispatcher.rs:204-207/226-228` → `Err("HTTP error: {reqwest display}")` — reqwest's message ("error sending request") says nothing about reachability to an end user.
- **401**: `dispatcher.rs:220-231` — exactly one forced refresh (`auth.refresh_now()`) + one re-send; the retry's response wins. This logic is **kept** (acceptance criteria explicitly re-test it) but moves inside the new attempt loop (§2.6).
- **Token-mint 429**: `fetch_token`'s `error_for_status()` → `AuthError::Http` → dispatcher prefixes `"auth error: "`. Worse: in device mode `resolve_device_token` (`auth/mod.rs:490-531`) treats **any 4xx** `AuthError::Http` via `is_definitive_rejection` (`auth/mod.rs:729-731`) as a dead refresh token → **wipes the token store on a transient 429**. Must be fixed (§2.7).
- **OAuth error body** `{"error","error_description"}`: parsed nowhere on the mint paths (`error_for_status` discards it). Only the device *poll* classifies it (`device.rs:161-189`).
- **Error → MCP tool error**: `src/server.rs:152-159` — `Err(msg: String)` → `CallToolResult::error(vec![ContentBlock::text(msg)])`. The string is the whole contract; Phase 9's job is to make that string *structured text* (status, Trace-Id, Dev line, User line).
- **Config**: `src/config.rs:106-156` — flat struct + `tools: Option<ToolFilters>`; no resilience knobs. Env layer `apply_env` (`config.rs:394-436`), validation `validate` (`config.rs:465-486`).
- **Server wiring**: `AllegroServer` (`src/server.rs:13-78`) holds `Arc<AllegroAuth>` + `reqwest::Client` + optional `api_base_url` override; `main.rs:376-449` `build_allegro_server` constructs it for both transports (stdio + HTTP share the handler, so one wiring point).

### 1.3 What already exists vs missing

Exists: 401-refresh-retry (dispatcher), device-poll slow_down backoff,
`PollingPolicy` sleep-injection pattern, `truncate_body`, wiremock test
conventions, config pipeline, pooled clients.

Missing: 429/5xx retry loop, Retry-After parsing, jitter, Allegro/OAuth
error-body parsing, Trace-Id capture anywhere, "unreachable" mapping,
client-side rate budget, `[resilience]` config, auth-mint 429 semantics
(and the store-wipe bug above).

---

## 2. Design decisions

1. **New module `src/resilience.rs`** (single file, not a directory, not a
   middleware crate). It owns: error-body parsers, Trace-Id/Retry-After
   extraction, the `DispatchError` taxonomy + report renderer, the
   `BackoffPolicy` (math + injectable sleep), the `RateBudget` sliding
   window, and the `Resilience` bundle passed to the dispatcher. Placement
   rationale: mirrors the repo's layering (`http.rs` = client factory,
   `dispatcher.rs` = request shaping, `auth/` = tokens); a
   `reqwest-middleware` layer was already rejected in Phase 7 (gh-7 plan,
   Option B) in favor of functional composition — Phase 9 follows the same
   call. `dispatcher.rs` stays the *only* place that talks to the Allegro
   API; `resilience.rs` is pure policy/parse/format plus the budget
   primitive, so it is unit-testable without network.
2. **Errors stay `Result<String, String>` at the public dispatcher surface**
   (backward compat for `server.rs` + every existing test). Internally the
   new `dispatch_with_resilience` returns `Result<String, DispatchError>`;
   legacy wrappers map through `DispatchError::report()`. The report is the
   ticket's "structured text": header line, `Trace-Id:` line (when present),
   `Retry-After:` line (when known), `Dev:` line, `User:` line.
3. **Retry policy**: 429 → up to **3 retries** with exponential backoff +
   jitter (attempts 1..=4 total); 5xx → **single retry** with backoff, only
   for idempotent methods (§2.5); `Retry-After` (429 only) **replaces** the
   computed delay (clamped to a 30 s ceiling — an MCP tool call must not
   hang minutes on a server-advised wait). Separate counters for 429 and
   5xx; worst case 5 sends per dispatch.
4. **Token-mint 429s are never retried and never wipe state**: new
   `AuthError::TokenMintRateLimited` surfaces immediately with the
   "token churn too high" actionable copy; `is_definitive_rejection` no
   longer matches it (it is not `TokenEndpoint{4xx}`), so the stale-store
   guard leaves the store intact.
5. **5xx retries are method-gated, 429 retries are not.** A 429 means the
   request was rejected *before* processing (rate limiter front-of-house) —
   replaying a POST is safe. A 5xx means the request *may have been
   processed* — replaying a POST risks duplicate offers/orders. Idempotent
   set per RFC 9110 §9.2.2: `GET`, `HEAD`, `PUT`, `DELETE`. The registry's
   OpenAPI surface is GET-dominated (search/list); POST creation tools are
   the exact ones where a blind retry is dangerous. (Refinement via
   per-operation safety annotations is out of scope — §Out-of-scope.)
6. **401-refresh interacts with the loop as the innermost concern**: each
   attempt is `send → (once per dispatch) 401? → refresh_now → re-send →
   classify`. A 429 never triggers refresh; a 401 arriving *after* the
   refresh was already spent falls through to the `Client` error mapping
   (exactly today's "persistent 401 surfaces the retry body" semantics,
   preserved for `test_dispatch_with_base_persistent_401_returns_second_body`).
7. **Rate budget = sliding 60 × 1 s window**, fixed `[u32; 60]` ring of
   per-second counters under a `std::sync::Mutex` — O(1) memory, accurate to
   the "per minute" shape (a token bucket would permit a full-cap burst in
   one second). When the next send would exceed the cap, `acquire()` waits
   up to `acquire_wait` (production: 1.1 s — one second-boundary rotation
   frees `cap/60` slots) and then fails with `BudgetExceeded`. It is
   acquired **per API send** (retries count — they are real requests). The
   budget lives in `Arc<RateBudget>` owned by `AllegroServer`, created once
   in `build_allegro_server` from config → shared across all concurrent
   tool calls, per process (= per `client_id` in every supported
   deployment; multi-process sharing is §Out-of-scope).
8. **Jitter**: full jitter is overkill for ≤ 3 retries; use `delay =
   min(base · 2^n, cap) + U[0, 250 ms)` from an internal seedable
   xorshift64*. When the server sent `Retry-After`, no jitter is added (the
   server was explicit). Seeded construction makes the bounds unit-testable.
9. **Trace-Id capture**: `extract_trace_id(&HeaderMap) -> Option<String>`
   (`to_str().ok()`, first value) called on every response that enters an
   error path (API 4xx/5xx, token-endpoint non-2xx). It is attached to the
   relevant `DispatchError`/`AuthError` variant and rendered as a dedicated
   `Trace-Id:` line. Success responses ignore it (per ticket: "every error
   report string").
10. **Device-poll 429 fix** (small, in scope): `device::classify` currently
    routes 429 into the terminal `ExpiredOrInvalid` branch. Add
    `if status == 429 { return PollOutcome::Transient(…) }` before the 4xx
    range check so an interactive `auth device` run survives a transient
    mint-side rate limit (bounded by the existing 5-transient cap). No
    Retry-After honoring in the poll loop — its interval contract belongs
    to the device-grant `interval`/`slow_down` protocol.
11. **Config surface stays minimal**: one knob,
    `[resilience] rate_limit_per_minute` (default 8000, `0` disables the
    guard, validated `< 9000`, env `ALLEGRO_MCP_RATE_LIMIT`). Backoff
    constants are named consts in `resilience.rs` — retry tuning is a
    behavior change, not an operator preference, and over-configuring
    invites foot-guns.

---

## 3. Files to create / modify

| File | Action | Purpose |
|---|---|---|
| `src/resilience.rs` | **Create** | Parsers (Allegro/OAuth bodies, Retry-After, Trace-Id), `DispatchError` + `report()`, `BackoffPolicy`, `RateBudget`, `Resilience` bundle, consts, `render_auth_error` |
| `src/dispatcher.rs` | **Modify** | Attempt loop (budget acquire → send → 401 refresh → classify 429/5xx/success), `dispatch_with_resilience`, legacy wrappers preserved |
| `src/auth/mod.rs` | **Modify** | `AuthError::TokenEndpoint` + `AuthError::TokenMintRateLimited`; `fetch_token`/`refresh_grant` read status/headers/body; `is_definitive_rejection` re-typed; `resolve_device_token` match arms updated |
| `src/auth/device.rs` | **Modify** | `classify`: 429 → `Transient` (one-line fix + unit test) |
| `src/config.rs` | **Modify** | `ResilienceConfig` (`rate_limit_per_minute`), `ConfigError::InvalidRateLimit`, env `ALLEGRO_MCP_RATE_LIMIT`, validation `< 9000`, tests |
| `src/server.rs` | **Modify** | `AllegroServer { resilience: Resilience }`, `with_resilience` test builder, `call_tool` → `dispatch_with_resilience` |
| `src/main.rs` | **Modify** | `build_allegro_server` wires `Resilience::production(cfg.resilience.rate_limit_per_minute)`; startup log gains `rate_limit` |
| `src/lib.rs` | **Modify** | `pub mod resilience;` |
| `Cargo.toml` | **Modify** | add `httpdate = "1.0"` (promote existing transitive dep) |
| `tests/resilience_integration.rs` | **Create** | wiremock suite per acceptance criteria (§7) |
| `tests/config_integration.rs` | **Modify** | `[resilience]` parsing/default/validation tests + add `ALLEGRO_MCP_RATE_LIMIT` to the pinned `LOAD_ENV_VARS` set; new env tests routed through `with_env`/`#[serial]` (see Phase 4) |
| `README.md`, `CHANGELOG.md` | **Modify** | document `[resilience]` knob + error report format |

No changes: `src/http_server.rs` (reuses the `AllegroServer` handler, gets
resilience for free), `src/auth/token_store.rs`, `src/tool_registry/*`,
existing tests (verified §8).

---

## Phase 1: DONE — `src/resilience.rs` (create)

### 1.1 Module doc + constants

```rust
//! Resilience layer — retry policy, rate budget, and error reporting for
//! Allegro API calls. Pure policy/parse/format (+ the budget primitive):
//! the dispatcher performs the I/O, this module decides and renders.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

pub const MAX_429_RETRIES: u32 = 3;
pub const MAX_5XX_RETRIES: u32 = 1;
pub const BACKOFF_BASE_MS: u64 = 500;
pub const BACKOFF_MAX_MS: u64 = 30_000;
pub const JITTER_MAX_MS: u64 = 250;
/// Ceiling applied to a server-advised Retry-After (429): an MCP tool call
/// must not hang for minutes; past the ceiling we fail cleanly instead.
pub const RETRY_AFTER_CAP_SECS: u64 = 30;
pub const TRACE_ID_HEADER: &str = "Trace-Id";
pub const RETRY_AFTER_HEADER: &str = "Retry-After";
/// Allegro's documented per-client_id quota (ticket).
pub const ALLEGRO_HARD_LIMIT_RPM: u32 = 9000;
/// Soft default: ~11 % headroom under the hard limit.
pub const DEFAULT_RATE_LIMIT_RPM: u32 = 8000;
const WINDOW_BUCKETS: usize = 60;
```

### 1.2 Error-body parsers

```rust
/// One entry of Allegro's `{"errors":[...]}` body (guideline §0.1).
/// Every field optional — the guideline marks `path`/`details` nullable
/// and malformed bodies must never break error reporting.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllegroErrorItem {
    pub code: Option<String>,
    pub message: Option<String>,
    pub user_message: Option<String>,
    pub path: Option<String>,
    pub details: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AllegroApiErrorBody {
    #[serde(default)]
    pub errors: Vec<AllegroErrorItem>,
}

/// Parses an Allegro API error body; `None` when the body is not the
/// documented shape (HTML error pages, proxies, empty bodies).
pub fn parse_allegro_errors(body: &str) -> Option<AllegroApiErrorBody> { … }

/// RFC 6749 §5.2 token-endpoint error body.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OAuthErrorBody {
    pub error: String,
    pub error_description: Option<String>,
}
pub fn parse_oauth_error(body: &str) -> Option<OAuthErrorBody> { … }
```

Both use `serde_json::from_str(..).ok()` (lenient: unknown fields ignored).

### 1.3 Header extraction

```rust
/// `Trace-Id` from any response (case-insensitive; HTTP/2-safe).
pub fn extract_trace_id(headers: &reqwest::header::HeaderMap) -> Option<String>;

/// `Retry-After` as delta-seconds OR HTTP-date (via `httpdate`), relative
/// to `now`; past dates clamp to `Some(Duration::ZERO)`; unparsable → None.
/// Result is clamped to `[0, RETRY_AFTER_CAP_SECS]`.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration>;
```

### 1.4 `DispatchError` taxonomy + report renderer

```rust
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// 429 persisted past MAX_429_RETRIES. `attempts` = **total API sends
    /// made for this dispatch** (initial + 429 retries + 5xx retries + any
    /// 401-refresh re-send) — deliberately the wire-observable count so
    /// test assertions on wiremock `received_requests()` can never
    /// disagree with the report. `retry_after` is from the LAST 429.
    #[error("HTTP 429 rate-limited after {attempts} attempts")]
    RateLimited {
        attempts: u32,
        retry_after: Option<Duration>,
        trace_id: Option<String>,
        body: String,
    },
    /// 5xx persisted past MAX_5XX_RETRIES (or method not retryable).
    #[error("HTTP {status} server error after {retries} 5xx-retries")]
    Server {
        status: u16,
        retries: u32,
        trace_id: Option<String>,
        body: String,
    },
    /// Any other 4xx (incl. 401 after the spent refresh) — Allegro body
    /// mapped when parseable.
    #[error("HTTP {status}")]
    Client {
        status: u16,
        trace_id: Option<String>,
        allegro: Option<AllegroApiErrorBody>,
        body: String,
    },
    /// Transport-level failure (DNS, refused, TLS, timeout) — no response,
    /// hence no trace-id.
    #[error("Allegro unreachable: {source}")]
    Unreachable { source: String },
    /// Client-side rate budget exhausted — request never sent.
    #[error("client-side rate budget exceeded ({cap} req/min per client_id)")]
    BudgetExceeded { cap: u32 },
    /// Pre-formatted auth error (dispatcher's "auth error: …" path),
    /// Trace-Id already embedded by `render_auth_error`.
    #[error("{0}")]
    Auth(String),
    /// Pre-flight request-shaping failure (e.g. the unsupported-HTTP-method
    /// check, dispatcher.rs:143-144). Rendered **verbatim** — the historical
    /// `String` error, byte-identical — so `test_dispatch_unsupported_method_
    /// returns_error` (asserts `contains("unsupported HTTP method")`) stays
    /// green when the legacy wrappers become one-liners over
    /// `dispatch_with_resilience` (§2.3).
    #[error("{0}")]
    Bad(String),
}
```

`Bad` is also the documented home for any future pre-flight shaping error
(path/argument problems surfacing before a send): no Trace-Id (no response
happened), no User line — the shaping messages are already end-user-safe.

`impl DispatchError { pub fn report(&self) -> String }` renders:

```
Allegro API error: HTTP 429 Too Many Requests — rate limited after 4 attempts
Trace-Id: 1311db4f-fe65-4cb2-b514-1bb47f781aa7      ← only when present
Retry-After: 30s                                     ← only when known
Dev: GET /sale/offers → 429; retries exhausted (backoff 500ms/1s/2s); body: {"errors":[…]}   ← raw body verbatim (truncated to 4 KB)
User: Allegro is limiting how often this app can call the API. Wait about 30 s, then try again with fewer requests.
```

Per-variant `User:` copy (actionable, no jargon):

- `RateLimited` — "Allegro is limiting how often this app can call the API. Wait a moment and retry; if it persists, reduce how many requests you make at once."
- `Server` — "Allegro had a temporary problem handling the request. Try again in a few seconds; if it keeps happening, wait before retrying."
- `Client` — prefers the first `errors[].userMessage` when present (Allegro localizes it for end users), else "Allegro rejected the request (HTTP {status}). Check the Dev details — often a missing or invalid parameter." `Dev:` line lists `code: message (path)` per item ahead of the raw body.
- `Unreachable` — "Allegro could not be reached — the network or the service may be down. Check connectivity and try again."
- `BudgetExceeded` — "This app made more than {cap} requests in the last minute and paused itself to protect your Allegro quota. Wait a few seconds and send fewer requests."
- `Auth` — string as-is.
- `Bad` — string as-is, verbatim (no header/Dev/User lines — see above).

Body truncation for the `Dev:` line reuses a local 4 KB char-boundary
truncation helper (same technique as `dispatcher::truncate_body`).

```rust
/// Renders an `AuthError` for the dispatcher's error path: keeps the
/// historical "auth error: " prefix (tests assert it) and appends the
/// Trace-Id line when the error carries one.
pub fn render_auth_error(e: &crate::auth::AuthError) -> String { … }
```

### 1.5 Backoff policy

```rust
/// Seeded xorshift64* — jitter only (not crypto); seedable for tests.
#[derive(Debug, Clone)]
struct JitterRng { state: u64 }   // impl: seeded(), next_u64(), jitter_ms(max)

#[derive(Debug, Clone)]
pub struct BackoffPolicy {
    /// `Some(d)` overrides every sleep with `d` (tests: `Some(ZERO)`),
    /// matching `auth::device::PollingPolicy`.
    sleep_override: Option<Duration>,
    jitter: Mutex<JitterRng>,
}
```

Derives (pinned — `Resilience` must be `Clone` because `AllegroServer` is
`#[derive(Clone)]` with a clone-identity test, `server.rs:12` /
`test_allegro_server_is_clone`):

- `JitterRng: Clone` (plain `u64` state) ⇒ `Mutex<JitterRng>: Clone`
  (std impls `Clone` for `Mutex<T: Clone>`) ⇒ `BackoffPolicy: Clone`.
  Clone semantics: **snapshot** — a cloned policy continues the RNG stream
  from the copied state; clones do not share entropy. That is acceptable
  here: server clones share the *budget* (the thing that must be shared)
  via `Arc`, while independent jitter streams still decorrelate concurrent
  dispatches (each send draws from one of a handful of snapshot lineages).
- `Debug` on both — derived, no secrets (the seed is not sensitive).

impl BackoffPolicy {
    pub fn production() -> Self;            // seed from SystemTime nanos
    pub fn test_instant() -> Self;          // sleep_override = Some(ZERO), fixed seed
    pub fn with_seed(seed: u64) -> Self;    // deterministic jitter bounds tests

    /// Delay before retry #`retry_no` (1-based).
    /// `retry_after = Some(d)` (429 only) replaces the computed delay,
    /// clamped to RETRY_AFTER_CAP_SECS, **no jitter** (server-explicit).
    /// Otherwise: `min(BACKOFF_BASE_MS << (retry_no-1), BACKOFF_MAX_MS) + U[0, JITTER_MAX_MS)`.
    pub fn delay_for(&self, retry_no: u32, retry_after: Option<Duration>) -> Duration;

    /// The sleep the dispatcher awaits (override-aware).
    pub async fn sleep(&self, d: Duration);
}
```

### 1.6 Rate budget (client-side guard)

```rust
/// Sliding 60 × 1 s window over API sends, per client_id (= per process).
#[derive(Debug)]
pub struct RateBudget {
    cap: u32,
    /// Ring of per-epoch-second counters; bucket i ≙ epoch_second % 60.
    state: Mutex<BudgetState>,
    /// How long `acquire` may wait for a second-boundary rotation before
    /// failing (production 1.1 s; tests may zero it).
    acquire_wait: Duration,
}

#[derive(Debug)]
struct BudgetState { buckets: [u32; WINDOW_BUCKETS], last_second: u64 }
```

Derives (pinned): `RateBudget` is **not** `Clone` by design — it exists to
be shared, so `Resilience` holds `Option<Arc<RateBudget>>` and server
clones share the single instance. It **is** `#[derive(Debug)]` (requires
`BudgetState: Debug`; `Mutex<T: Debug>: Debug` then follows) because
`Arc<RateBudget>: Debug` is needed for `Resilience`'s derived `Debug`.

impl RateBudget {
    pub fn new(cap: u32) -> Self;                       // acquire_wait = 1.1 s
    #[doc(hidden)]
    pub fn new_for_tests(cap: u32, acquire_wait: Duration) -> Self;

    /// Records one request, waiting up to `acquire_wait` when the window
    /// is full. `Err(BudgetExceeded)` → the request is NOT sent.
    pub async fn acquire(&self) -> Result<(), DispatchError>;
}

// Pure helpers, unit-testable without sleeping:
fn bucket_index(epoch_second: u64) -> usize;                       // % 60
fn advance(buckets: &mut [u32; 60], last: &mut u64, now_second: u64); // zeroes stale buckets on rotation
fn window_total(buckets: &[u32; 60]) -> u32;
```

Rotation rule: when `now_second / 60 != *last / 60` the whole ring resets;
otherwise only slots strictly between `*last % 60` and `now % 60` (forward)
are zeroed — 1 s granularity, so ≤ 1.1 s wait always frees ≥ `cap/60` slots.

### 1.7 The bundle

```rust
/// Everything the dispatcher needs beyond request shaping. Clone-cheap
/// (budget behind `Arc`), owned by `AllegroServer`.
///
/// Derives (pinned): `#[derive(Debug, Clone)]` — REQUIRED, not stylistic:
/// `AllegroServer` is `#[derive(Clone)]` (`server.rs:12`) and
/// `test_allegro_server_is_clone` exercises that clone, so every field of
/// `AllegroServer` — including `resilience` — must be `Clone` (and `Debug`
/// for the derived server `Debug`). Field-level support:
/// `BackoffPolicy: Clone` (§1.5 note), `Arc<RateBudget>: Clone` always,
/// `Arc<RateBudget>: Debug` via `RateBudget: Debug` (§1.6 note).
#[derive(Debug, Clone)]
pub struct Resilience {
    pub backoff: BackoffPolicy,
    pub budget: Option<Arc<RateBudget>>,
}

impl Resilience {
    /// Production: real backoff + shared budget at `rate_limit_rpm`
    /// (`0` → budget disabled).
    pub fn production(rate_limit_rpm: u32) -> Self;
    /// Legacy-wrapper default: real backoff, no budget (no handle exists
    /// on the `dispatch()`/`dispatch_with_base()` signatures).
    pub fn unguarded() -> Self;
    /// Integration tests: zero sleeps, no budget.
    pub fn test_instant() -> Self;
}

/// 5xx retry gate — RFC 9110 §9.2.2 idempotent methods (§2.5).
pub fn is_idempotent(method: &reqwest::Method) -> bool; // GET HEAD PUT DELETE
```

### 1.8 Unit tests (in-module)

- `backoff_delay_grows_exponentially` — retry_no 1..=4 → 500/1000/2000/4000 ms (seeded, jitter sliced off by asserting `delay - jitter == base`… simpler: with `with_seed`, compute expected = base + jitter_from_seed by exposing `jitter` determinism; alternatively assert `base <= d < base + JITTER_MAX`).
- `backoff_delay_capped_at_backoff_max` — retry_no 10 → ≤ BACKOFF_MAX_MS + jitter.
- `retry_after_overrides_and_clamps` — `Some(120 s)` → 30 s; `Some(2 s)` → 2 s; no jitter added.
- `retry_after_parsing_seconds_zero_and_garbage` — `"1"`→1 s, `"0"`→Some(0), `"soon"`→None, absent→None, `"999999"`→30 s (clamped).
- `retry_after_parsing_http_date` — future date → delta (clamped), past date → Some(0).
- `trace_id_extraction` — present / absent / lowercase-inserted name (HTTP/2 shape) / non-UTF-8 → None.
- `allegro_error_body_parsing` — full camelCase body; null `path`/`details`; empty `errors` array; HTML garbage → None.
- `oauth_error_body_parsing` — both fields; `error` only; garbage → None.
- `report_renders_trace_id_only_when_present` (all variants; asserts `contains("Trace-Id:")` iff Some, `contains("Retry-After:")` iff known).
- `report_client_prefers_user_message_and_lists_codes` — `userMessage` in User line, `code (path)` in Dev line, raw body still present.
- `report_unreachable_has_actionable_copy` — contains "could not be reached".
- `rate_budget_blocks_at_cap` — `new_for_tests(3, ZERO)`: 3 acquires Ok, 4th → `BudgetExceeded { cap: 3 }`.
- `rate_budget_window_rotates` — pure `advance`/`window_total`: same-second accumulation; +61 s → window empty.
- `is_idempotent_table` — GET/HEAD/PUT/DELETE true; POST/PATCH false.
- `render_auth_error_keeps_prefix_and_appends_trace_id`.

---

## Phase 2: DONE — `src/dispatcher.rs` (attempt loop)

### 2.1 New entry point (kept next to the legacy pair)

```rust
/// Full-resilience dispatch. `pub` + `#[doc(hidden)]` so integration tests
/// (separate crate) can inject wiremock + `Resilience::test_instant()`.
/// Production callers reach it through `AllegroServer::call_tool`.
#[doc(hidden)]
pub async fn dispatch_with_resilience(
    auth: &crate::auth::AllegroAuth,
    http: &reqwest::Client,
    api_base: &str,
    tool_def: &crate::tool_registry::ToolDef,
    arguments: serde_json::Map<String, Value>,
    res: &crate::resilience::Resilience,
) -> Result<String, crate::resilience::DispatchError>
```

### 2.2 Loop (replaces `dispatcher.rs:204-247`)

Keep everything up to and including the `build` closure (`dispatcher.rs:160-202`)
unchanged — plan freeze, path/query/body shaping, `with_versioned_content_type`
all stay. That includes the `Method::from_bytes` check at `dispatcher.rs:143-144`
(after token acquisition, before request building), whose `String` error now
maps through the passthrough variant:

```rust
let method = reqwest::Method::from_bytes(tool_def.method.to_uppercase().as_bytes())
    .map_err(|_| DispatchError::Bad(format!("unsupported HTTP method: {}", tool_def.method)))?;
```

(byte-identical message ⇒ `test_dispatch_unsupported_method_returns_error`
stays green through the §2.3 wrappers). Then:

```
let mut token = auth.token().await
    .map_err(|e| DispatchError::Auth(crate::resilience::render_auth_error(&e)))?;

let mut retries_429: u32 = 0;
let mut retries_5xx: u32 = 0;
let mut sends: u32 = 0;                     // total API sends (incl. 401-refresh re-send)
let mut refreshed = false;                  // one 401-refresh per dispatch (existing semantics)
let method_idempotent = resilience::is_idempotent(&method);

loop {
    if let Some(budget) = &res.budget {
        budget.acquire().await?;            // BudgetExceeded propagates; request never sent
    }

    let response = match build(&token).send().await {
        Ok(r) => r,
        Err(e) => return Err(DispatchError::Unreachable { source: e.to_string() }),
    };
    sends += 1;
    // Headers first (§7.9 discipline): capture everything before `.text()`.
    let trace_id    = resilience::extract_trace_id(response.headers());
    let retry_after = resilience::parse_retry_after(response.headers());

    // 401 → forced refresh + re-send, once per dispatch (Phase 5 logic,
    // moved inside the loop; the first 401's body is still discarded).
    if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
        refreshed = true;
        token = auth.refresh_now().await
            .map_err(|e| DispatchError::Auth(render_auth_error(&e)))?;
        continue;                          // budget re-acquired on the re-send
    }

    let status = response.status();

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS && retries_429 < MAX_429_RETRIES {
        retries_429 += 1;
        tracing::warn!(retries_429, ?retry_after, ?trace_id, "429 — backing off");
        res.backoff.sleep(res.backoff.delay_for(retries_429, retry_after)).await;
        continue;                          // any method (§2.5: not yet processed)
    }

    if status.is_server_error() && retries_5xx < MAX_5XX_RETRIES && method_idempotent {
        retries_5xx += 1;
        tracing::warn!(status = %status, ?trace_id, "5xx — single retry");
        res.backoff.sleep(res.backoff.delay_for(retries_5xx, None)).await;
        continue;
    }

    let body = response.text().await
        .map_err(|e| DispatchError::Unreachable { source: e.to_string() })?;

    if status.is_success() {
        return Ok(truncate_body(body));
    }
    return Err(match status.as_u16() {
        429 => DispatchError::RateLimited {
            attempts: sends,               // total sends for this dispatch — see note
            retry_after,                   // captured from the LAST 429 response's headers
            trace_id, body,
        },
        s if s >= 500 => DispatchError::Server { status: s, retries: retries_5xx, trace_id, body },
        s => DispatchError::Client { status: s, trace_id,
              allegro: resilience::parse_allegro_errors(&body), body },
    });
}
```

Implementation notes for the implementer:

- `RateLimited.attempts` is pinned to the **total number of API sends made
  for this dispatch** (the `sends` counter — initial send + 429 retries +
  5xx retries + any 401-refresh re-send), *not* a 429-only count: it is
  exactly the number wiremock's `received_requests()` observes, so test
  assertions (`dispatch_persistent_429_clean_error_with_trace_id` expects
  `attempts == 4` and 4 hits) can never disagree with the report. The
  per-class counts stay separately observable via the Dev line
  (`retries exhausted (backoff 500ms/1s/2ms)`) and tracing.
- `retry_after` (and `trace_id`) are read once per response **before**
  `.text()` consumes the body; the `RateLimited` variant carries the last
  429's values — the same ones the final backoff decision would have used;
- the loop is bounded: ≤ 1 + 3 + 1 sends;
- a 401 after the spent refresh falls to `Client { status: 401, body }`
  — the report embeds the body verbatim, so
  `test_dispatch_with_base_persistent_401_returns_second_body`
  (`contains("still unauthorized after refresh")` + `contains("401)`)
  stays green.

### 2.3 Legacy wrappers (signatures unchanged)

```rust
pub async fn dispatch(...)            -> Result<String, String>  // unchanged sig
#[doc(hidden)]
pub async fn dispatch_with_base(...)  -> Result<String, String>  // unchanged sig
```

Both become one-liners over the loop with `Resilience::unguarded()`
(backoff on, no budget — those signatures cannot carry a shared budget) and
`.map_err(|e| e.report())`. `dispatch()` still derives the base via
`allegro_api_base(sandbox)`. Production traffic flows through
`AllegroServer` → `dispatch_with_resilience` with the real budget.

Behavior change note (intended, per ticket): the legacy paths now retry
429s with **real** sleeps. Verified: no existing test mocks an API-level
429/5xx (only token-endpoint 500/401, which fail before the loop —
`dispatcher.rs` tests + `mcp_server_integration.rs`).

### 2.4 Dispatcher-level wiremock unit tests (`#[cfg(test)]`, `Resilience::test_instant()`)

Reuse `get_offers_tool()` / `mount_token_ok()` helpers. New:

- `test_429_twice_then_200_succeeds` — limited 429×2 mock first, 200 fallback; assert Ok + exactly 3 `/sale/offers` hits, 1 token hit.
- `test_persistent_429_fails_cleanly_with_trace_id` — always-429 with `Trace-Id` + `Retry-After: 1` headers; assert Err report `contains("429")`, `contains("Trace-Id: tr-429")`, `contains("rate limited")`; exactly 4 API hits (1 + 3 retries).
- `test_5xx_once_then_200_retries_get` — 503×1 then 200; Ok, 2 hits.
- `test_persistent_5xx_fails_after_single_retry` — always-503 + Trace-Id; Err `contains("503")` + `Trace-Id:`; exactly 2 hits.
- `test_5xx_post_is_not_retried` — POST tool, always-500; Err, exactly 1 hit (method gate).
- `test_network_error_maps_to_unreachable` — token mock OK, `api_base = "http://127.0.0.1:9"` (discarded port); Err report `contains("could not be reached")`.
- `test_budget_guard_blocks_second_dispatch` — `Resilience` with `Some(Arc::new(RateBudget::new_for_tests(1, ZERO)))`; first dispatch Ok (1 API hit), second → Err `contains("rate budget")`; still only 1 API hit total.
- `test_token_mint_429_is_never_retried` — token endpoint mock: 200 once, then 429 (+`Retry-After`) — API always 401 so the post-401 re-resolve hits the 429; assert Err `contains("auth error")` + `contains("token churn")`, token endpoint hit exactly 2× (initial + the single forced re-resolve — no further retry).
- `test_oauth_error_body_mapped_from_token_endpoint` — token endpoint 400 `{"error":"invalid_client","error_description":"bad id"}`; Err report `contains("invalid_client")` + `contains("bad id")`.
- `test_allegro_error_body_mapped_from_api` — API 422 with the guideline body; Err report `contains("MissingDeliveryPointException")`, User line `contains("Nie wybrano punktu")`, `contains("Trace-Id: …")`.
- Existing dispatcher tests must stay green unchanged through the `unguarded` wrapper: the 401 trio (`test_dispatch_with_base_401_once_then_200_succeeds_with_two_api_hits`, `…_persistent_401_…`, `…_auth_failure_on_re_resolve_…`) and the pre-flight pair (`test_dispatch_unsupported_method_returns_error`, `test_dispatch_auth_failure_returns_auth_error`) — the former via the §2.2 `Bad` mapping, the latter via `Auth(render_auth_error(...))` (prefix "auth error" preserved).

---

## Phase 3: DONE — `src/auth/mod.rs` (mint-path errors)

### 3.1 New `AuthError` variants (append to the enum, `auth/mod.rs:47-90`)

```rust
/// Token endpoint responded non-2xx (other than 429): carries the parsed
/// OAuth `{"error","error_description"}` when the body had one.
#[error("token endpoint error (HTTP {status}): {error:?} {error_description:?}")]
TokenEndpoint {
    status: u16,
    error: Option<String>,
    error_description: Option<String>,
    trace_id: Option<String>,
},

/// 429 from the token endpoint — NEVER auto-retried (ticket). Surfaces the
/// "token churn too high" actionable error.
#[error(
    "token endpoint rate limit (HTTP 429): token churn too high — not retried \
     automatically. Reduce request frequency and make sure no other \
     allegro-mcp instance shares this client_id"
)]
TokenMintRateLimited {
    retry_after: Option<std::time::Duration>,
    trace_id: Option<String>,
},
```

### 3.2 Shared non-2xx reader (module-private)

```rust
/// Reads status/headers/body of a failed token-endpoint response into the
/// structured AuthError (429 → TokenMintRateLimited; else TokenEndpoint
/// with the OAuth body parsed when present).
async fn read_token_error(resp: reqwest::Response) -> AuthError { … }
```

`fetch_token` (`auth/mod.rs:393-402`) and `refresh_grant`
(`auth/mod.rs:591-599`): replace

```rust
.send().await?.error_for_status()?;
```

with

```rust
let resp = …send().await?;
if !resp.status().is_success() {
    return Err(read_token_error(resp).await);
}
```

`AuthError::Http(reqwest::Error)` remains for **transport** failures only
(design intent unchanged — `classify`-style body-before-status discipline
from `device.rs` applied to the mint paths).

### 3.3 Stale-store guard re-typing

`is_definitive_rejection` (`auth/mod.rs:729-731`) becomes:

```rust
/// `true` for a *definitive* token-endpoint rejection (OAuth 4xx other
/// than 429): rotated away / revoked / expired. 429 is its own variant and
/// is transient — it must never trigger the stale-store wipe.
fn is_definitive_rejection(e: &AuthError) -> bool {
    matches!(e, AuthError::TokenEndpoint { status: 400..500, .. })
}
```

Update the two call sites in `resolve_device_token`:

- `auth/mod.rs:490`: `Err(AuthError::Http(e)) if is_definitive_rejection(&e)` → `Err(e) if is_definitive_rejection(&e)`;
- `auth/mod.rs:515`: `!matches!(&retry_err, AuthError::Http(h) if is_definitive_rejection(h))` → `!is_definitive_rejection(&retry_err)`.

Semantics preserved: 400 `invalid_grant` → wipe + `ReauthRequired`
(`tests/device_flow_integration.rs::refresh_rejection_forces_reauth` stays
green); 5xx/transport/429 → transient arm → forced-mode fallback to the
stored live token (a flaky auth server must not take down a possibly-good
token — existing philosophy, now extended to mint-side 429s).

### 3.4 `src/auth/device.rs` — 429 classify fix

`classify` (`device.rs:127-139`): insert before the `(400..500)` arm:

```rust
if status == 429 {
    // Transient rate limit — polling faster must not read as a dead grant.
    return PollOutcome::Transient(format!("HTTP 429: {body}"));
}
```

Unit test: `classify_429_is_transient_not_expired_or_invalid`.

---

## Phase 4: DONE — `src/config.rs` (rate-limit knob)

```rust
/// `[resilience]` table — client-side protections (Phase 9).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResilienceConfig {
    /// Soft per-minute request cap per client_id. `0` disables the guard.
    /// Must stay under Allegro's 9000/min hard limit — validated in
    /// [`Config::validate`].
    pub rate_limit_per_minute: u32,
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self { rate_limit_per_minute: crate::resilience::DEFAULT_RATE_LIMIT_RPM }
    }
}
```

- `Config` gains `#[serde(default)] pub resilience: ResilienceConfig`
  (plain field, not `Option` — it is consumed this phase; container-level
  `#[serde(default)]` already on `Config` keeps partial files valid).
- New `ConfigError::InvalidRateLimit { value: u32 }`:
  `#[error("invalid rate_limit_per_minute {value}: must be 0 (disabled) or 1..=8999 — Allegro's hard limit is 9000/min")]`.
- `Config::validate`: `if r >= ALLEGRO_HARD_LIMIT_RPM && r != 0-disallowed…`
  — precisely: `matches!(r, 1..=8999) || r == 0` else error.
- `Config::apply_env`: parse `ALLEGRO_MCP_RATE_LIMIT` (u32; non-numeric →
  `InvalidEnv`-style error via a new `parse_env_u32` sibling of
  `parse_env_bool`, same trim/case hygiene).
- Helper `impl Config { pub fn rate_limit_rpm(&self) -> u32 { self.resilience.rate_limit_per_minute } }`.

Unit/integration tests (in `config.rs` tests + `tests/config_integration.rs`):
default is 8000; `rate_limit_per_minute = 100` parses; `9000`/`99999` →
`InvalidRateLimit`; `0` accepted (disabled); unknown key under
`[resilience]` → `UnknownKey` (deny_unknown_fields); env override applies;
env garbage errors.

**Env-test hygiene (mandatory, `tests/config_integration.rs`)**: the moment
`Config::apply_env` learns `ALLEGRO_MCP_RATE_LIMIT`, a developer's real
export of that var leaks into every existing `load`-based test. Therefore:

- add `("ALLEGRO_MCP_RATE_LIMIT", None)` to the pinned set
  `LOAD_ENV_VARS` (`tests/config_integration.rs:47-54`) — every
  `with_env(LOAD_ENV_VARS, …)` `load` test then scrubs it like the rest;
- the new env tests (`env_override_sets_rate_limit`,
  `env_override_garbage_errors`) must follow the file's convention exactly:
  `#[serial]` + `with_env(&[("ALLEGRO_MCP_RATE_LIMIT", Some("100"))], …)`
  so they set/restore their own var and never race parallel env-mutators;
- pure `from_toml_str` / `validate` tests (no `load`, no env) need neither.

---

## Phase 5: DONE — `src/server.rs` + `src/main.rs` + `src/lib.rs` + `Cargo.toml`

### 5.1 `src/server.rs`

```rust
pub struct AllegroServer {
    …existing…
    resilience: crate::resilience::Resilience,
}
```

- `AllegroServer::new` sets `resilience: Resilience::production(DEFAULT_RATE_LIMIT_RPM)`
  (main refines it from config; tests that never trip the cap are
  unaffected — 8000/min ≫ anything a test does).
- New `#[doc(hidden)] pub fn with_resilience(mut self, res: Resilience) -> Self`
  (integration tests inject `test_instant()`).
- `call_tool` (`server.rs:144-150`) — both branches collapse to:

```rust
let base = self.api_base_url.as_deref()
    .unwrap_or_else(|| crate::config::api_base_url(self.sandbox));
let dispatch_result = crate::dispatcher::dispatch_with_resilience(
    &self.auth, &self.http, base, tool_def, arguments, &self.resilience,
).await.map_err(|e| e.report());
```

(the `Ok`/`Err` → `CallToolResult` mapping at `server.rs:152-159` is
untouched — the report string becomes the error content block).

### 5.2 `src/main.rs`

`build_allegro_server` (`main.rs:430-431`):

```rust
let server = server::AllegroServer::new(registry, auth, cfg.sandbox)
    .with_http_client(api_client)
    .with_resilience(resilience::Resilience::production(cfg.rate_limit_rpm()));
```

Startup `info!` (`main.rs:166-173`) gains `rate_limit_rpm = cfg.rate_limit_rpm()`.

### 5.3 `src/lib.rs`

Add `pub mod resilience;` (alphabetical order in the existing list).

### 5.4 `Cargo.toml`

```toml
# Retry-After HTTP-date parsing (already transitive via hyper — promoted)
httpdate        = "1.0"
```

---

## Phase 6: DONE — `tests/resilience_integration.rs` (create)

File header mirrors `tests/device_flow_integration.rs`: module doc listing
conventions (own wiremock server per test; `Resilience::test_instant()` —
zero sleeps; limited mocks mounted first). Imports:

```rust
use allegro_mcp::auth::AllegroAuth;
use allegro_mcp::dispatcher::dispatch_with_resilience;
use allegro_mcp::resilience::{RateBudget, Resilience};
use allegro_mcp::server::AllegroServer;
use allegro_mcp::tool_registry::{ToolDef, ToolRegistry};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
```

Local helpers (copy the shape of `dispatcher.rs`'s `mount_token_ok` /
`get_offers_tool`; a `post_offer_tool()` for method-gating; the duplex
JSON-RPC `spawn_server`/`initialize`/`send_json`/`recv_json` quartet from
`tests/mcp_server_integration.rs:42-89` for the MCP-level tests).

Suite (names in repo style):

| # | Test | Mocks (wiremock) | Asserts |
|---|---|---|---|
| 1 | `dispatch_429_backoff_then_success` | 429×2 (limited, first) then 200 | `Ok`; exactly 3 API hits, 1 token hit |
| 2 | `dispatch_persistent_429_clean_error_with_trace_id` | always 429 + `Trace-Id: tr-persist` + `Retry-After: 1` | Err report contains `429`, `Trace-Id: tr-persist`, `Retry-After`, actionable user copy; 4 API hits |
| 3 | `dispatch_401_refresh_retry_from_phase5` | API 401×1 then 200; token 200 always | `Ok`; 2 API hits, 2 token hits (acceptance: "401→refresh→retry (from Phase 5 logic)") |
| 4 | `dispatch_5xx_single_retry_then_success` | 503×1 then 200 (GET) | `Ok`; 2 hits |
| 5 | `dispatch_persistent_5xx_clean_error` | always 503 + Trace-Id | Err contains `503`, `Trace-Id:`; 2 hits |
| 6 | `dispatch_network_error_unreachable_message` | token OK; `api_base = "http://127.0.0.1:9"` | Err contains `could not be reached`; no retry |
| 7 | `dispatch_maps_oauth_error_body` | token endpoint 400 `{"error":"invalid_client","error_description":"Client authentication failed"}` | Err contains `invalid_client`, `Client authentication failed`, `auth error` |
| 8 | `dispatch_token_mint_429_not_retried` | token 200 once then 429+Retry-After; API always 401 | Err contains `token churn`; token endpoint exactly 2 hits (initial + single forced re-resolve, no retry loop) |
| 9 | `dispatch_maps_allegro_error_body_dev_and_user` | API 422 guideline body + Trace-Id | Err: Dev line has `MissingDeliveryPointException` + raw body; User line has `userMessage`; `Trace-Id:` present |
| 10 | `rate_budget_guard_trips_second_call` | token OK; API 200 | `Resilience` with `RateBudget::new_for_tests(1, ZERO)`; 1st Ok, 2nd Err `rate budget`; 1 API hit |
| 11 | `mcp_call_tool_429_persists_returns_structured_error` (server-level, duplex JSON-RPC) | as #2; `AllegroServer::new(..).with_api_base_url(mock.uri()).with_resilience(Resilience::test_instant())` | `tools/call` response `is_error: true`, content text contains `Trace-Id: tr-persist` (proves the MCP tool-error surface end-to-end) |
| 12 | `mcp_call_tool_429_recovery_still_serves_next_call` (server-level) | 429×1 then 200, then another 200 | first call Ok (after in-process backoff), second call Ok — session survives errors |

No `serial_test` needed *in this file* (no env mutation —
`AllegroAuth::with_base_url` injects the auth host directly). The env
tests for `ALLEGRO_MCP_RATE_LIMIT` live in `tests/config_integration.rs`
where the `#[serial]`/`with_env` machinery already exists — see Phase 4.

---

## Phase 7: DONE — docs

- `README.md`: new "Resilience & error reporting" section — retry matrix
  (429 ×3 exponential+jitter honoring Retry-After≤30 s; 5xx ×1
  idempotent-only; 401 refresh ×1; mint-429 never), the report format with
  a sample, `[resilience] rate_limit_per_minute` + env var, and "include
  the Trace-Id line when contacting Allegro support".
- `CHANGELOG.md`: Phase 9 entry under Unreleased.

---

## 4. Tests to write (summary)

**Unit — `src/resilience.rs`** (§1.8): backoff math (growth/cap/Retry-After
override+clamp/jitter bounds, seeded), `parse_retry_after` (seconds/zero/
HTTP-date past+future/garbage/huge-clamp), `extract_trace_id`
(present/absent/lowercase/non-UTF-8), body parsers (Allegro camelCase +
nullables + garbage; OAuth both/one/garbage), `report()` per variant
(Trace-Id/Retry-After lines iff present, User copy, Dev raw body),
`RateBudget` (cap trip with zero wait; pure window rotation), `is_idempotent`,
`render_auth_error`.

**Unit — `src/dispatcher.rs`** (§2.4): the eleven wiremock scenarios
(instant policy) + existing 401 trio stays green.

**Unit — `src/auth/mod.rs` / `device.rs`**: `classify_429_is_transient`;
`token_mint_429_display_mentions_churn_and_no_retry`;
`token_endpoint_error_carries_status_and_body`; guard:
`refresh_grant_429_does_not_wipe_store` (seeded live pair + 429 mock →
error propagates, store file still exists).

**Integration — `tests/resilience_integration.rs`** (§Phase 6): the 12-test
suite incl. two MCP-level duplex tests.

**Integration — `tests/config_integration.rs`**: `[resilience]` parse /
default / invalid / disabled / env override.

---

## 5. Acceptance criteria mapping (ticket → plan)

| Ticket bullet | Verification |
|---|---|
| wiremock: 429→backoff→success | `dispatch_429_backoff_then_success` (+ dispatcher unit twin); backoff math proven by `backoff_delay_grows_exponentially` since tests run with zero-sleep override |
| wiremock: persistent 429→clean error | `dispatch_persistent_429_clean_error_with_trace_id` — 4 attempts, then structured report |
| wiremock: 401→refresh→retry (Phase 5 logic) | `dispatch_401_refresh_retry_from_phase5` + existing `test_dispatch_with_base_401_once_then_200_succeeds_with_two_api_hits` (must stay green — the loop preserves it) |
| All error paths return structured text containing Trace-Id when present | `report_renders_trace_id_only_when_present` (every variant) + `dispatch_persistent_429_clean_error_with_trace_id`, `dispatch_persistent_5xx_clean_error`, `dispatch_maps_allegro_error_body_dev_and_user`, `mcp_call_tool_429_persists_returns_structured_error` (end-to-end MCP surface); token-endpoint Trace-Id via `render_auth_error_keeps_prefix_and_appends_trace_id` + `dispatch_token_mint_429_not_retried` |
| 429 exponential backoff + jitter, max 3 retries, Retry-After respected | `MAX_429_RETRIES=3` const; unit tests §1.8; retry-count asserted via wiremock hit counts (exactly 4 sends) |
| Token-mint 429 never retried, "token churn too high" | `AuthError::TokenMintRateLimited` + `dispatch_token_mint_429_not_retried` (exactly 2 token hits: initial + the single Phase-5 forced re-resolve) + no store wipe test |
| Allegro error body mapped (dev+user) | `parse_allegro_errors` units + `dispatch_maps_allegro_error_body_dev_and_user` (userMessage → User line, code/message/path → Dev line) |
| OAuth error body mapped (dev+user) | `parse_oauth_error` units + `dispatch_maps_oauth_error_body` |
| Trace-Id captured → in every error report | `extract_trace_id` + report tests above |
| 5xx single retry w/ backoff; network → "unreachable" | `dispatch_5xx_single_retry_then_success`, `dispatch_persistent_5xx_clean_error`, `dispatch_network_error_unreachable_message` |
| Client-side rate budget (soft cap < 9000/min per client_id) | `RateBudget` units + `rate_budget_guard_trips_second_call`; default 8000 enforced `< 9000` by `Config::validate` (config tests) |

---

## 6. Out of scope (explicitly NOT doing)

- **No metrics/telemetry** (no histograms, no counters export; tracing
  `warn!` lines only).
- **No circuit breaker** beyond the spec'd retry bounds.
- **No retries for non-401/429/5xx statuses** (403/404/422 stay terminal).
- **No Retry-After honoring on 5xx** or on the device-poll loop (the
  device-grant `interval`/`slow_down` protocol governs that loop).
- **No dynamic rate-limit discovery** from response headers (undocumented).
- **No cross-process/distributed budget** — the guard is per process; the
  ticket's "well under in practice" makes per-process sufficient. Document
  the caveat for multi-instance deployments sharing one `client_id`.
- **No schema-download retries** (10 s timeout stands; separate concern).
- **No MCP protocol changes** — errors remain text content blocks
  (`CallToolResult::error`); no structured-content / JSON error payloads.
- **No changes to token single-flight semantics, store format, or
  `/auth/status`**.
- **No retry of token minting** for non-429 either (mint failures keep
  today's one-shot semantics inside a dispatch).

---

## 7. Risks and edge cases

1. **Retry-After parsing**: delta-seconds vs HTTP-date handled
   (`parse_retry_after`); clock skew on HTTP-dates clamps at
   `Some(ZERO)` (past) / `RETRY_AFTER_CAP_SECS` (far future) — a skewed
   clock degrades to "retry soon", never to a multi-minute hang.
2. **Jitter bounds**: xorshift64* seeded per `BackoffPolicy` (constructed
   once per server) — sufficient for herd decorrelation across concurrent
   dispatches sharing one policy; unit tests pin `[base, base+250 ms)`.
3. **Method gating on 5xx**: POST/PATCH tools lose 5xx retries — a
   duplicate-offer risk is worse than a rare spurious failure, and the MCP
   client (an agent) can re-invoke deliberately. GET-dominated registry
   keeps most tools retryable. 429 retries all methods (request provably
   unprocessed).
4. **401-refresh × 429 ordering**: 401 wins inside an attempt (refresh
   first, then classify) — a rate-limited response with a stale token
   refreshes before backing off; harmless (one extra refresh per dispatch
   max) and keeps Phase-5 semantics byte-compatible. A 429 arriving after
   the refresh was spent still consumes 429 retries normally.
5. **Budget granularity**: 1 s buckets → worst-case wait 1.1 s before
   `BudgetExceeded`; cap frees `cap/60` slots per second. At the 8000
   default that's ~133 req/s sustained — far above any realistic MCP
   session, so the guard only trips pathological agent loops (intended).
6. **Store-wipe regression** (the bug fixed in §3.3): must be covered by
   `refresh_grant_429_does_not_wipe_store`; reviewer should double-check
   both `resolve_device_token` match arms were re-typed (grep for
   `AuthError::Http(h)` leftovers).
7. **String-report stability**: tests must assert on stable substrings
   (`"Trace-Id:"`, `"HTTP 429"`, `userMessage` text), never whole-string
   equality — the report format will evolve.
8. **Legacy-path behavior change**: `dispatch`/`dispatch_with_base` now
   sleep on 429 (real backoff). No existing test hits an API-level 429
   (verified), but future unit tests on those wrappers must use
   `dispatch_with_resilience` + `test_instant()` instead.
9. **Body-before-status discipline**: headers/Trace-Id/Retry-After are read
   before `.text()` consumes the response — implementer keeps the
   `device.rs` classify pattern (never `error_for_status()` first).
10. **Non-UTF-8 / HTML error bodies** (proxies): parsers return `None`,
    raw body still lands in the Dev line (4 KB char-boundary truncation) —
    error reporting never panics on hostile bodies.
11. **wiremock ordering**: limited (`up_to_n_times`) mocks mounted first,
    fallback last — equal-priority registration order (existing repo
    convention, restated in the new file's doc comment).
12. **`AllegroServer::new` default budget (8000) in tests**: harmless —
    integration tests drive O(1) requests; the budget only trips when a
    test *chooses* a tiny cap via `new_for_tests`.

---

## 8. Cross-phase verification checklist (run after all phases land)

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                       # unit + integration incl. resilience suite
cargo test --test resilience_integration
# Regression targets (Phase 5 + Phase 8 surfaces this phase must not break):
cargo test --test device_flow_integration
cargo test --test mcp_server_integration
cargo test test_dispatch_with_base              # 401 trio stays green
```

- [ ] Every error report contains `Trace-Id:` when the mock sent one, and
      omits the line when not (unit + integration).
- [ ] Token-endpoint 429 produces exactly 2 token requests (no retry loop)
      and leaves `tokens.json` untouched (device mode).
- [ ] 429 scenario sends exactly 4 API requests; 5xx GET exactly 2; 5xx
      POST exactly 1 (wiremock `received_requests` counts).
- [ ] `Config` rejects `rate_limit_per_minute ≥ 9000`; default TOML-less
      run logs `rate_limit_rpm=8000`.
- [ ] stdio + HTTP transports both surface the new reports (they share
      `AllegroServer::call_tool` — one manual smoke each is enough).
