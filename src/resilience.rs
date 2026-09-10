//! Resilience layer — retry policy, rate budget, and error reporting for
//! Allegro API calls. Pure policy/parse/format (+ the budget primitive):
//! the dispatcher performs the I/O, this module decides and renders.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

// ── Error-body parsers ────────────────────────────────────────────────────────

/// One entry of Allegro's `{"errors":[...]}` body (guideline §0.1).
///
/// Every field optional — the guideline marks `path`/`details` nullable
/// and malformed bodies must never break error reporting.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllegroErrorItem {
    pub code: Option<String>,
    pub message: Option<String>,
    pub user_message: Option<String>,
    pub path: Option<String>,
    /// Extra dev detail — **null in production** per the guideline: parsed
    /// opportunistically, never relied on (hence not rendered in reports).
    #[allow(dead_code)]
    pub details: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AllegroApiErrorBody {
    #[serde(default)]
    pub errors: Vec<AllegroErrorItem>,
}

/// Parses an Allegro API error body; `None` when the body is not the
/// documented shape (HTML error pages, proxies, empty bodies).
pub fn parse_allegro_errors(body: &str) -> Option<AllegroApiErrorBody> {
    serde_json::from_str(body).ok()
}

/// RFC 6749 §5.2 token-endpoint error body.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OAuthErrorBody {
    pub error: String,
    pub error_description: Option<String>,
}

/// Parses an OAuth token-endpoint error body; `None` when the body is not
/// the documented shape (lenient: unknown fields ignored).
pub fn parse_oauth_error(body: &str) -> Option<OAuthErrorBody> {
    serde_json::from_str(body).ok()
}

// ── Header extraction ─────────────────────────────────────────────────────────

/// `Trace-Id` from any response (case-insensitive; HTTP/2-safe — reqwest's
/// `HeaderMap` lookup lowercases, and `to_str().ok()` filters non-UTF-8).
pub fn extract_trace_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(TRACE_ID_HEADER)?
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// `Retry-After` as delta-seconds OR HTTP-date (via `httpdate`), relative
/// to `now`; past dates clamp to `Some(Duration::ZERO)`; unparsable → None.
/// Result is clamped to `[0, RETRY_AFTER_CAP_SECS]`.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER_HEADER)?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    let now = SystemTime::now();
    let delta = if let Ok(secs) = raw.parse::<u64>() {
        // RFC 7231 delta-seconds form.
        Duration::from_secs(secs)
    } else if let Ok(http_date) = httpdate::parse_http_date(raw) {
        // RFC 7231 HTTP-date form: delta relative to now; a past date
        // (clock skew included) degrades to "retry soon", never negative.
        http_date.duration_since(now).unwrap_or(Duration::ZERO)
    } else {
        return None;
    };
    Some(delta.min(Duration::from_secs(RETRY_AFTER_CAP_SECS)))
}

// ── DispatchError taxonomy + report renderer ──────────────────────────────────

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
    /// hence no trace-id. (Field is named `source_text`, not `source`:
    /// thiserror treats a field literally named `source` as the error
    /// source, which would require it to implement `std::error::Error` —
    /// a plain `String` transport message does not.)
    #[error("Allegro unreachable: {source_text}")]
    Unreachable { source_text: String },
    /// Client-side rate budget exhausted — request never sent.
    #[error("client-side rate budget exceeded ({cap} req/min per client_id)")]
    BudgetExceeded { cap: u32 },
    /// Pre-formatted auth error (dispatcher's "auth error: …" path),
    /// Trace-Id already embedded by `render_auth_error`.
    #[error("{0}")]
    Auth(String),
    /// Pre-flight request-shaping failure (e.g. the unsupported-HTTP-method
    /// check). Rendered **verbatim** — the historical `String` error,
    /// byte-identical — so `test_dispatch_unsupported_method_returns_error`
    /// (asserts `contains("unsupported HTTP method")`) stays green when the
    /// legacy wrappers are one-liners over `dispatch_with_resilience`.
    ///
    /// `Bad` is also the documented home for any future pre-flight shaping
    /// error (path/argument problems surfacing before a send): no Trace-Id
    /// (no response happened), no User line — the shaping messages are
    /// already end-user-safe.
    #[error("{0}")]
    Bad(String),
}

/// Best-effort HTTP reason phrase for the report header line (RFC 9110
/// §15 subset — the statuses Allegro actually returns).
fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

/// 4 KB char-boundary truncation for the raw body in the `Dev:` line (same
/// technique as `dispatcher::truncate_body` — remote error bodies are
/// hostile input and must never panic the renderer).
const DEV_BODY_MAX_BYTES: usize = 4096;

fn truncate_dev_body(body: &str) -> &str {
    if body.len() <= DEV_BODY_MAX_BYTES {
        return body;
    }
    let mut pos = DEV_BODY_MAX_BYTES;
    while pos > 0 && !body.is_char_boundary(pos) {
        pos -= 1;
    }
    &body[..pos]
}

/// Appends the `Trace-Id:` / `Retry-After:` lines when the error carries
/// them (ticket: every error report includes Trace-Id when present).
fn push_trace_and_retry_after(
    out: &mut String,
    trace_id: &Option<String>,
    retry_after: &Option<Duration>,
) {
    if let Some(id) = trace_id {
        out.push_str(&format!("\nTrace-Id: {id}"));
    }
    if let Some(d) = retry_after {
        out.push_str(&format!("\nRetry-After: {}s", d.as_secs()));
    }
}

/// `code: message (path)` per Allegro error item — parts omitted when the
/// body left them null.
fn format_allegro_items(items: &[AllegroErrorItem]) -> String {
    items
        .iter()
        .map(|item| {
            let mut s = String::new();
            if let Some(code) = &item.code {
                s.push_str(code);
            }
            if let Some(message) = &item.message {
                if !s.is_empty() {
                    s.push_str(": ");
                }
                s.push_str(message);
            }
            if let Some(path) = &item.path {
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&format!("({path})"));
            }
            s
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

fn plural_retries(n: u32) -> String {
    if n == 1 {
        "1 retry".to_owned()
    } else {
        format!("{n} retries")
    }
}

impl DispatchError {
    /// Renders the structured error report — the string the MCP client
    /// sees as the tool-error content block:
    ///
    /// ```text
    /// Allegro API error: HTTP 429 Too Many Requests — rate limited after 4 attempts
    /// Trace-Id: 1311db4f-fe65-4cb2-b514-1bb47f781aa7      ← only when present
    /// Retry-After: 30s                                     ← only when known
    /// Dev: 429; retries exhausted (backoff 500ms/1s/2s); body: {"errors":[…]}
    /// User: Allegro is limiting how often this app can call the API. …
    /// ```
    ///
    /// Tests must assert on stable substrings (`"Trace-Id:"`, `"HTTP 429"`,
    /// `userMessage` text), never whole-string equality — the format will
    /// evolve.
    pub fn report(&self) -> String {
        match self {
            DispatchError::Auth(msg) | DispatchError::Bad(msg) => msg.clone(),
            DispatchError::RateLimited {
                attempts,
                retry_after,
                trace_id,
                body,
            } => {
                let mut out = format!(
                    "Allegro API error: HTTP 429 Too Many Requests — \
                     rate limited after {attempts} attempts"
                );
                push_trace_and_retry_after(&mut out, trace_id, retry_after);
                out.push_str(&format!(
                    "\nDev: 429; retries exhausted (backoff 500ms/1s/2s + jitter, \
                     server Retry-After honored); body: {}",
                    truncate_dev_body(body)
                ));
                out.push_str(
                    "\nUser: Allegro is limiting how often this app can call the API. \
                     Wait a moment and retry; if it persists, reduce how many requests \
                     you make at once.",
                );
                out
            }
            DispatchError::Server {
                status,
                retries,
                trace_id,
                body,
            } => {
                let mut out = format!(
                    "Allegro API error: HTTP {status} {} — server error after {}",
                    reason_phrase(*status),
                    plural_retries(*retries)
                );
                push_trace_and_retry_after(&mut out, trace_id, &None);
                out.push_str(&format!(
                    "\nDev: 5xx persisted after {}; body: {}",
                    plural_retries(*retries),
                    truncate_dev_body(body)
                ));
                out.push_str(
                    "\nUser: Allegro had a temporary problem handling the request. \
                     Try again in a few seconds; if it keeps happening, wait before \
                     retrying.",
                );
                out
            }
            DispatchError::Client {
                status,
                trace_id,
                allegro,
                body,
            } => {
                let mut out = format!(
                    "Allegro API error: HTTP {status} {}",
                    reason_phrase(*status)
                );
                push_trace_and_retry_after(&mut out, trace_id, &None);
                let items = allegro
                    .as_ref()
                    .map(|b| format_allegro_items(&b.errors))
                    .unwrap_or_default();
                if items.is_empty() {
                    out.push_str(&format!("\nDev: body: {}", truncate_dev_body(body)));
                } else {
                    out.push_str(&format!(
                        "\nDev: {items}; body: {}",
                        truncate_dev_body(body)
                    ));
                }
                let user = allegro
                    .as_ref()
                    .and_then(|b| b.errors.iter().find_map(|e| e.user_message.clone()))
                    .unwrap_or_else(|| {
                        format!(
                            "Allegro rejected the request (HTTP {status}). \
                             Check the Dev details — often a missing or invalid parameter."
                        )
                    });
                out.push_str(&format!("\nUser: {user}"));
                out
            }
            DispatchError::Unreachable { source_text } => {
                let mut out =
                    String::from("Allegro API error: Allegro unreachable (transport failure)");
                push_trace_and_retry_after(&mut out, &None, &None);
                out.push_str(&format!("\nDev: transport error: {source_text}"));
                out.push_str(
                    "\nUser: Allegro could not be reached — the network or the service \
                     may be down. Check connectivity and try again.",
                );
                out
            }
            DispatchError::BudgetExceeded { cap } => {
                let mut out = format!(
                    "Allegro API error: client-side rate budget exceeded \
                     ({cap} req/min per client_id)"
                );
                push_trace_and_retry_after(&mut out, &None, &None);
                out.push_str(&format!(
                    "\nDev: sliding 60 s window reached the {cap} req/min cap; \
                     the request was not sent"
                ));
                out.push_str(&format!(
                    "\nUser: This app made more than {cap} requests in the last minute \
                     and paused itself to protect your Allegro quota. Wait a few seconds \
                     and send fewer requests."
                ));
                out
            }
        }
    }
}

/// Renders an [`crate::auth::AuthError`] for the dispatcher's error path:
/// keeps the historical `"auth error: "` prefix (tests assert it) and
/// appends the Trace-Id line when the error carries one.
pub fn render_auth_error(e: &crate::auth::AuthError) -> String {
    let trace_id = match e {
        crate::auth::AuthError::TokenEndpoint { trace_id, .. }
        | crate::auth::AuthError::TokenMintRateLimited { trace_id, .. } => trace_id.clone(),
        _ => None,
    };
    let mut out = format!("auth error: {e}");
    if let Some(id) = trace_id {
        out.push_str(&format!("\nTrace-Id: {id}"));
    }
    out
}

// ── Backoff policy ────────────────────────────────────────────────────────────

/// Seeded xorshift64* — jitter only (not crypto); seedable for tests.
#[derive(Debug, Clone)]
struct JitterRng {
    state: u64,
}

impl JitterRng {
    /// Seed from wall-clock nanos — decorrelates concurrently constructed
    /// policies; never zero (xorshift requires a nonzero state).
    fn seeded() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self { state: nanos | 1 }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform `U[0, max_ms)` — modulo bias at 250 ms is irrelevant for
    /// herd decorrelation.
    fn jitter_ms(&mut self, max_ms: u64) -> u64 {
        if max_ms == 0 {
            return 0;
        }
        self.next_u64() % max_ms
    }
}

/// Retry-delay policy: exponential backoff + jitter, with an injectable
/// sleep override for tests (the same pattern as
/// `auth::device::PollingPolicy`).
///
/// Clone semantics: a cloned policy is a **snapshot** — it continues the
/// RNG stream from the copied state; clones do not share entropy. Server
/// clones share the *budget* (the thing that must be shared) via `Arc`,
/// while independent jitter streams still decorrelate concurrent
/// dispatches. (`Clone` is manual — std does not implement `Clone` for
/// `Mutex<T>` — locking and copying the RNG state is exactly the snapshot
/// described above.)
#[derive(Debug)]
pub struct BackoffPolicy {
    /// `Some(d)` overrides every sleep with `d` (tests: `Some(ZERO)`),
    /// matching `auth::device::PollingPolicy`.
    sleep_override: Option<Duration>,
    jitter: Mutex<JitterRng>,
}

impl Clone for BackoffPolicy {
    fn clone(&self) -> Self {
        Self {
            sleep_override: self.sleep_override,
            jitter: Mutex::new(
                self.jitter
                    .lock()
                    .expect("jitter rng mutex poisoned")
                    .clone(),
            ),
        }
    }
}

impl BackoffPolicy {
    /// Real-world policy: seed from wall-clock nanos, honor computed delays.
    pub fn production() -> Self {
        Self {
            sleep_override: None,
            jitter: Mutex::new(JitterRng::seeded()),
        }
    }

    /// Test policy: zero sleeps everywhere (deterministic fixed seed) so
    /// wiremock suites run at full speed.
    // Test-only; unused inside the non-test bin pass.
    #[allow(dead_code)]
    pub fn test_instant() -> Self {
        Self {
            sleep_override: Some(Duration::ZERO),
            jitter: Mutex::new(JitterRng {
                state: 0xA5A5_1234 | 1,
            }),
        }
    }

    /// Deterministic seed for the jitter-bounds unit tests.
    // Unit-test-only; unused inside the non-test bin pass.
    #[allow(dead_code)]
    pub fn with_seed(seed: u64) -> Self {
        Self {
            sleep_override: None,
            jitter: Mutex::new(JitterRng { state: seed | 1 }),
        }
    }

    /// Delay before retry #`retry_no` (1-based).
    ///
    /// `retry_after = Some(d)` (429 only) replaces the computed delay,
    /// clamped to `RETRY_AFTER_CAP_SECS`, **no jitter** (server-explicit).
    /// Otherwise: `min(BACKOFF_BASE_MS << (retry_no-1), BACKOFF_MAX_MS) +
    /// U[0, JITTER_MAX_MS)`.
    pub fn delay_for(&self, retry_no: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(server) = retry_after {
            return server.min(Duration::from_secs(RETRY_AFTER_CAP_SECS));
        }
        // Cap the shift well past the point where BACKOFF_MAX_MS takes over
        // (500 << 16 ms ≈ 9 h) so huge retry numbers cannot overflow.
        let shift = u64::from(retry_no.saturating_sub(1)).min(16);
        let base_ms = (BACKOFF_BASE_MS << shift).min(BACKOFF_MAX_MS);
        let jitter_ms = self
            .jitter
            .lock()
            .expect("jitter rng mutex poisoned")
            .jitter_ms(JITTER_MAX_MS);
        Duration::from_millis(base_ms + jitter_ms)
    }

    /// The sleep the dispatcher awaits (override-aware).
    pub async fn sleep(&self, d: Duration) {
        let effective = self.sleep_override.unwrap_or(d);
        if effective > Duration::ZERO {
            tokio::time::sleep(effective).await;
        }
    }
}

// ── Rate budget (client-side guard) ───────────────────────────────────────────

/// Sliding 60 × 1 s window over API sends, per client_id (= per process).
///
/// Not `Clone` by design — it exists to be shared, so [`Resilience`] holds
/// `Option<Arc<RateBudget>>` and server clones share the single instance.
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
struct BudgetState {
    buckets: [u32; WINDOW_BUCKETS],
    last_second: u64,
}

fn epoch_second() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        // Clock before the epoch is unrecoverable; 0 is the safe direction
        // (every expiry check fails, so the window starts empty).
        .unwrap_or(0)
}

impl RateBudget {
    /// Production budget: waits up to 1.1 s (one second-boundary rotation)
    /// before failing with `BudgetExceeded`.
    pub fn new(cap: u32) -> Self {
        Self::new_for_tests(cap, Duration::from_millis(1100))
    }

    /// Test constructor: explicit `acquire_wait` (zero → fail immediately
    /// when the window is full, no sleeps).
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn new_for_tests(cap: u32, acquire_wait: Duration) -> Self {
        Self {
            cap,
            state: Mutex::new(BudgetState {
                buckets: [0; WINDOW_BUCKETS],
                last_second: 0,
            }),
            acquire_wait,
        }
    }

    /// Records one request, waiting up to `acquire_wait` when the window
    /// is full. `Err(BudgetExceeded)` → the request is NOT sent.
    pub async fn acquire(&self) -> Result<(), DispatchError> {
        let deadline = tokio::time::Instant::now() + self.acquire_wait;
        loop {
            let acquired = {
                let mut guard = self.state.lock().expect("rate budget mutex poisoned");
                // Reborrow through the guard so the two fields below are
                // disjoint borrows of one `&mut BudgetState`.
                let st = &mut *guard;
                let now = epoch_second();
                advance(&mut st.buckets, &mut st.last_second, now);
                if window_total(&st.buckets) < self.cap {
                    st.buckets[bucket_index(now)] += 1;
                    true
                } else {
                    false
                }
            };
            if acquired {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(DispatchError::BudgetExceeded { cap: self.cap });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Pure helpers, unit-testable without sleeping.
///
/// Rotation rule: when `now_second / 60 != *last / 60` the whole ring
/// resets; otherwise only slots strictly between `*last % 60` and
/// `now % 60` (forward) are zeroed — 1 s granularity, so a ≤ 1.1 s wait
/// always spans a rotation that frees `cap/60` slots.
fn bucket_index(epoch_second: u64) -> usize {
    (epoch_second % WINDOW_BUCKETS as u64) as usize
}

fn advance(buckets: &mut [u32; WINDOW_BUCKETS], last: &mut u64, now_second: u64) {
    if now_second <= *last {
        return;
    }
    if now_second / 60 != *last / 60 {
        // A full minute (or more) has passed — the whole ring is stale.
        *buckets = [0; WINDOW_BUCKETS];
    } else {
        // Zero every bucket strictly between *last%60 and now%60, forward
        // with wraparound. Bucket *last%60 itself stays: its second is
        // still inside the 60 s window.
        let mut i = (*last % 60) + 1;
        let end = now_second % 60;
        while i != end {
            buckets[i as usize] = 0;
            i = (i + 1) % 60;
        }
    }
    *last = now_second;
}

fn window_total(buckets: &[u32; WINDOW_BUCKETS]) -> u32 {
    buckets.iter().sum()
}

// ── The bundle ────────────────────────────────────────────────────────────────

/// Everything the dispatcher needs beyond request shaping. Clone-cheap
/// (budget behind `Arc`), owned by `AllegroServer`.
///
/// `#[derive(Debug, Clone)]` is REQUIRED, not stylistic: `AllegroServer`
/// is `#[derive(Clone)]` and `test_allegro_server_is_clone` exercises that
/// clone, so every field of `AllegroServer` — including `resilience` —
/// must be `Clone` (and `Debug` for the derived server `Debug`).
#[derive(Debug, Clone)]
pub struct Resilience {
    pub backoff: BackoffPolicy,
    pub budget: Option<Arc<RateBudget>>,
}

impl Resilience {
    /// Production: real backoff + shared budget at `rate_limit_rpm`
    /// (`0` → budget disabled).
    pub fn production(rate_limit_rpm: u32) -> Self {
        Self {
            backoff: BackoffPolicy::production(),
            budget: (rate_limit_rpm > 0).then(|| Arc::new(RateBudget::new(rate_limit_rpm))),
        }
    }

    /// Legacy-wrapper default: real backoff, no budget (no handle exists
    /// on the `dispatch()`/`dispatch_with_base()` signatures).
    // Bin-tree dead code: the legacy wrappers themselves are bin-dead since
    // the server dispatches through `dispatch_with_resilience` directly.
    #[allow(dead_code)]
    pub fn unguarded() -> Self {
        Self {
            backoff: BackoffPolicy::production(),
            budget: None,
        }
    }

    /// Integration tests: zero sleeps, no budget.
    // Test-only; unused inside the non-test bin pass.
    #[allow(dead_code)]
    pub fn test_instant() -> Self {
        Self {
            backoff: BackoffPolicy::test_instant(),
            budget: None,
        }
    }
}

/// 5xx retry gate — RFC 9110 §9.2.2 idempotent methods: a 5xx means the
/// request *may have been processed*, so only `GET`/`HEAD`/`PUT`/`DELETE`
/// are replayed (a duplicate POST risks duplicate offers/orders; a 429 is
/// rejected before processing, so 429 retries are not method-gated).
pub fn is_idempotent(method: &reqwest::Method) -> bool {
    matches!(
        *method,
        reqwest::Method::GET
            | reqwest::Method::HEAD
            | reqwest::Method::PUT
            | reqwest::Method::DELETE
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
                HeaderValue::from_str(value).expect("valid header value"),
            );
        }
        map
    }

    // ── Backoff math ────────────────────────────────────────────────────────

    #[test]
    fn backoff_delay_grows_exponentially() {
        let policy = BackoffPolicy::with_seed(42);
        for (retry_no, base_ms) in [(1, 500u64), (2, 1000), (3, 2000), (4, 4000)] {
            let d = policy.delay_for(retry_no, None);
            let ms = d.as_millis() as u64;
            assert!(
                ms >= base_ms && ms < base_ms + JITTER_MAX_MS,
                "retry #{retry_no}: expected [{base_ms}, {}) ms with jitter, got {ms}",
                base_ms + JITTER_MAX_MS
            );
        }
    }

    #[test]
    fn backoff_delay_capped_at_backoff_max() {
        let policy = BackoffPolicy::with_seed(7);
        let ms = policy.delay_for(10, None).as_millis() as u64;
        assert!(
            (BACKOFF_MAX_MS..BACKOFF_MAX_MS + JITTER_MAX_MS).contains(&ms),
            "retry #10 must clamp to [{BACKOFF_MAX_MS}, {}) ms, got {ms}",
            BACKOFF_MAX_MS + JITTER_MAX_MS
        );
    }

    #[test]
    fn retry_after_overrides_and_clamps() {
        let policy = BackoffPolicy::with_seed(1);
        // A server-advised 120 s wait clamps to the 30 s ceiling…
        assert_eq!(
            policy.delay_for(1, Some(Duration::from_secs(120))),
            Duration::from_secs(RETRY_AFTER_CAP_SECS)
        );
        // …a short one passes through verbatim — and with NO jitter
        // (the server was explicit), so equality is exact.
        assert_eq!(
            policy.delay_for(2, Some(Duration::from_secs(2))),
            Duration::from_secs(2)
        );
    }

    // ── Retry-After parsing ─────────────────────────────────────────────────

    #[test]
    fn retry_after_parsing_seconds_zero_and_garbage() {
        assert_eq!(
            parse_retry_after(&headers(&[("Retry-After", "1")])),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            parse_retry_after(&headers(&[("Retry-After", "0")])),
            Some(Duration::ZERO)
        );
        assert_eq!(
            parse_retry_after(&headers(&[("Retry-After", "soon")])),
            None
        );
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
        // Huge values clamp to the ceiling instead of hanging the call.
        assert_eq!(
            parse_retry_after(&headers(&[("Retry-After", "999999")])),
            Some(Duration::from_secs(RETRY_AFTER_CAP_SECS))
        );
    }

    #[test]
    fn retry_after_parsing_http_date() {
        let now = SystemTime::now();
        let future = now + Duration::from_secs(3600);
        let future_str = httpdate::fmt_http_date(future);
        assert_eq!(
            parse_retry_after(&headers(&[("Retry-After", future_str.as_str())])),
            Some(Duration::from_secs(RETRY_AFTER_CAP_SECS)),
            "a far-future HTTP-date clamps to the 30 s ceiling"
        );

        let near_future = now + Duration::from_secs(2);
        let near_str = httpdate::fmt_http_date(near_future);
        let parsed = parse_retry_after(&headers(&[("Retry-After", near_str.as_str())]))
            .expect("a parsable HTTP-date must yield a delta");
        assert!(
            parsed <= Duration::from_secs(2),
            "clock skew aside the delta is ~2 s (already clamped), got {parsed:?}"
        );

        let past = now - Duration::from_secs(3600);
        let past_str = httpdate::fmt_http_date(past);
        assert_eq!(
            parse_retry_after(&headers(&[("Retry-After", past_str.as_str())])),
            Some(Duration::ZERO),
            "a past HTTP-date clamps to zero — retry soon, never negative"
        );
    }

    // ── Trace-Id extraction ─────────────────────────────────────────────────

    #[test]
    fn trace_id_extraction() {
        assert_eq!(
            extract_trace_id(&headers(&[("Trace-Id", "tr-123")])),
            Some("tr-123".to_owned())
        );
        // HTTP/2 lowercases header names on the wire; HeaderMap lookup is
        // case-insensitive either way.
        assert_eq!(
            extract_trace_id(&headers(&[("trace-id", "tr-lower")])),
            Some("tr-lower".to_owned())
        );
        assert_eq!(extract_trace_id(&HeaderMap::new()), None);
        // Non-UTF-8 values must yield None, never a panic or lossy string.
        let mut map = HeaderMap::new();
        map.insert(
            HeaderName::from_static("trace-id"),
            HeaderValue::from_bytes(&[0xFF, 0xFE]).expect("bytes are a valid header value"),
        );
        assert_eq!(extract_trace_id(&map), None);
    }

    // ── Error-body parsing ──────────────────────────────────────────────────

    #[test]
    fn allegro_error_body_parsing() {
        let body = parse_allegro_errors(
            r#"{"errors":[{"message":"Delivery point data not passed",
                 "code":"MissingDeliveryPointException","details":null,
                 "path":"Endpoint.getDeliveries.arg1",
                 "userMessage":"Nie wybrano punktu dla odbioru osobistego."}]}"#,
        )
        .expect("guideline body must parse");
        let item = body.errors.first().expect("one error item");
        assert_eq!(item.code.as_deref(), Some("MissingDeliveryPointException"));
        assert_eq!(
            item.user_message.as_deref(),
            Some("Nie wybrano punktu dla odbioru osobistego.")
        );
        assert_eq!(item.path.as_deref(), Some("Endpoint.getDeliveries.arg1"));
        assert_eq!(item.details, None);

        // Empty errors array is the documented shape (nothing to report).
        assert!(parse_allegro_errors(r#"{"errors":[]}"#)
            .expect("empty array parses")
            .errors
            .is_empty());
        // HTML error pages / garbage / empty bodies are not the shape.
        assert!(parse_allegro_errors("<html>bad gateway</html>").is_none());
        assert!(parse_allegro_errors("").is_none());
        assert!(parse_allegro_errors("not json").is_none());
    }

    #[test]
    fn oauth_error_body_parsing() {
        let both = parse_oauth_error(
            r#"{"error":"invalid_client","error_description":"Client authentication failed"}"#,
        )
        .expect("both fields must parse");
        assert_eq!(both.error, "invalid_client");
        assert_eq!(
            both.error_description.as_deref(),
            Some("Client authentication failed")
        );

        let error_only =
            parse_oauth_error(r#"{"error":"invalid_grant"}"#).expect("error-only must parse");
        assert_eq!(error_only.error, "invalid_grant");
        assert_eq!(error_only.error_description, None);

        assert!(parse_oauth_error("<html>").is_none());
        assert!(parse_oauth_error(r#"{"error":42}"#).is_none());
        assert!(parse_oauth_error("").is_none());
    }

    // ── report() per variant ────────────────────────────────────────────────

    fn rate_limited(trace: Option<&str>) -> DispatchError {
        DispatchError::RateLimited {
            attempts: 4,
            retry_after: Some(Duration::from_secs(30)),
            trace_id: trace.map(str::to_owned),
            body: r#"{"errors":[]}"#.to_owned(),
        }
    }

    #[test]
    fn report_renders_trace_id_only_when_present() {
        // With a Trace-Id → the line is present, and Retry-After when known.
        let with = rate_limited(Some("1311db4f-fe65-4cb2-b514-1bb47f781aa7")).report();
        assert!(
            with.contains("Trace-Id: 1311db4f-fe65-4cb2-b514-1bb47f781aa7"),
            "{with}"
        );
        assert!(with.contains("Retry-After: 30s"), "{with}");
        assert!(with.contains("HTTP 429"), "{with}");

        let mut without = rate_limited(None);
        if let DispatchError::RateLimited { retry_after, .. } = &mut without {
            *retry_after = None;
        }
        let text = without.report();
        assert!(!text.contains("Trace-Id:"), "{text}");
        assert!(!text.contains("Retry-After:"), "{text}");

        // Server variant.
        let server = DispatchError::Server {
            status: 503,
            retries: 1,
            trace_id: Some("tr-srv".to_owned()),
            body: "oops".to_owned(),
        }
        .report();
        assert!(server.contains("Trace-Id: tr-srv"), "{server}");
        assert!(server.contains("503"), "{server}");
        let server_none = DispatchError::Server {
            status: 503,
            retries: 0,
            trace_id: None,
            body: "oops".to_owned(),
        }
        .report();
        assert!(!server_none.contains("Trace-Id:"), "{server_none}");

        // Client variant.
        let client = DispatchError::Client {
            status: 422,
            trace_id: Some("tr-cli".to_owned()),
            allegro: None,
            body: String::new(),
        }
        .report();
        assert!(client.contains("Trace-Id: tr-cli"), "{client}");
        assert!(client.contains("422"), "{client}");

        // Variants that can never carry a Trace-Id must not render the line.
        let unreachable = DispatchError::Unreachable {
            source_text: "dns blew up".to_owned(),
        }
        .report();
        assert!(!unreachable.contains("Trace-Id:"), "{unreachable}");
        let budget = DispatchError::BudgetExceeded { cap: 8000 }.report();
        assert!(!budget.contains("Trace-Id:"), "{budget}");

        // Auth/Bad are rendered verbatim — no structural lines at all.
        let auth = DispatchError::Auth("auth error: boom".to_owned()).report();
        assert_eq!(auth, "auth error: boom");
        let bad = DispatchError::Bad("unsupported HTTP method: get post".to_owned()).report();
        assert_eq!(bad, "unsupported HTTP method: get post");
    }

    #[test]
    fn report_client_prefers_user_message_and_lists_codes() {
        let body = r#"{"errors":[{"code":"MissingDeliveryPointException",
            "message":"Delivery point data not passed",
            "path":"Endpoint.getDeliveries.arg1",
            "userMessage":"Nie wybrano punktu dla odbioru osobistego."}]}"#;
        let report = DispatchError::Client {
            status: 422,
            trace_id: Some("tr-422".to_owned()),
            allegro: parse_allegro_errors(body),
            body: body.to_owned(),
        }
        .report();
        // Dev line: code + path, and the raw body verbatim.
        assert!(report.contains("MissingDeliveryPointException"), "{report}");
        assert!(report.contains("(Endpoint.getDeliveries.arg1)"), "{report}");
        assert!(report.contains("body: {"), "{report}");
        // User line: Allegro's localized userMessage is preferred.
        assert!(report.contains("Nie wybrano punktu"), "{report}");
        assert!(report.contains("Trace-Id: tr-422"), "{report}");

        // Without a parseable body → generic user copy names the status.
        let fallback = DispatchError::Client {
            status: 404,
            trace_id: None,
            allegro: None,
            body: "<html/>".to_owned(),
        }
        .report();
        assert!(fallback.contains("HTTP 404"), "{fallback}");
        assert!(fallback.contains("Check the Dev details"), "{fallback}");
    }

    #[test]
    fn report_unreachable_has_actionable_copy() {
        let report = DispatchError::Unreachable {
            source_text: "error sending request".to_owned(),
        }
        .report();
        assert!(report.contains("Allegro unreachable"), "{report}");
        assert!(report.contains("could not be reached"), "{report}");
        assert!(report.contains("error sending request"), "{report}");
    }

    /// Hostile huge bodies (proxy error pages, binary garbage) must never
    /// balloon the report: the Dev line caps at 4 KB, and the cut must land
    /// on a UTF-8 char boundary even when a multi-byte char straddles it.
    #[test]
    fn report_truncates_huge_dev_bodies() {
        let mut body = "x".repeat(DEV_BODY_MAX_BYTES - 1);
        // A 3-byte char occupying bytes [4095, 4098) — byte 4096 is NOT a
        // char boundary, so the truncation helper must walk back.
        body.push('€');
        body.push_str(&"tail-marker-never-rendered".repeat(64));

        let report = DispatchError::Client {
            status: 502,
            trace_id: Some("tr-huge".to_owned()),
            allegro: None,
            body,
        }
        .report();
        assert!(
            report.len() < DEV_BODY_MAX_BYTES + 1024,
            "the report must stay bounded (Dev line capped at {DEV_BODY_MAX_BYTES} bytes \
             + short header/user lines), got {} bytes",
            report.len()
        );
        assert!(
            !report.contains("tail-marker-never-rendered"),
            "everything past the 4 KB cap must be cut"
        );
        assert!(report.contains("Trace-Id: tr-huge"), "{report}");
    }

    // ── Rate budget ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn rate_budget_blocks_at_cap() {
        let budget = RateBudget::new_for_tests(3, Duration::ZERO);
        for i in 1..=3 {
            budget
                .acquire()
                .await
                .unwrap_or_else(|_| panic!("acquire #{i} under the cap must pass"));
        }
        match budget.acquire().await {
            Err(DispatchError::BudgetExceeded { cap }) => assert_eq!(cap, 3),
            other => panic!("expected BudgetExceeded {{ cap: 3 }}, got {other:?}"),
        }
    }

    #[test]
    fn rate_budget_window_rotates() {
        let mut buckets = [0u32; WINDOW_BUCKETS];
        let mut last = 0u64;

        // Same-second accumulation.
        advance(&mut buckets, &mut last, 100);
        buckets[bucket_index(100)] += 1;
        advance(&mut buckets, &mut last, 100);
        buckets[bucket_index(100)] += 1;
        assert_eq!(window_total(&buckets), 2);

        // +3 s in the same minute: the old second survives, the strictly
        // between-seconds buckets were zeroed.
        advance(&mut buckets, &mut last, 103);
        assert_eq!(window_total(&buckets), 2);
        assert_eq!(buckets[bucket_index(100)], 2);
        assert_eq!(buckets[bucket_index(101)], 0);
        assert_eq!(buckets[bucket_index(102)], 0);

        // +61 s total: past the window — the whole ring resets.
        advance(&mut buckets, &mut last, 161);
        assert_eq!(window_total(&buckets), 0, "a 61 s-old window must be empty");

        // Minute-boundary crossing also resets the ring.
        let mut b2 = [7u32; WINDOW_BUCKETS];
        let mut last2 = 59u64;
        advance(&mut b2, &mut last2, 60);
        assert_eq!(window_total(&b2), 0, "crossing the minute boundary resets");
    }

    /// The config contract "0 disables the guard" end-to-end at the wiring
    /// layer: `production(0)` must produce NO budget handle (the
    /// dispatcher's acquire step is skipped entirely), while any positive
    /// cap wires the shared budget the server clones share.
    #[test]
    fn production_rate_limit_zero_disables_the_budget() {
        assert!(
            Resilience::production(0).budget.is_none(),
            "rate_limit_per_minute = 0 must disable the client-side guard"
        );
        assert!(
            Resilience::production(8000).budget.is_some(),
            "a positive cap must wire the shared budget"
        );
        // Unguarded (legacy wrappers) and test-instant bundles never carry
        // a budget — only the production constructor wires one.
        assert!(Resilience::unguarded().budget.is_none());
        assert!(Resilience::test_instant().budget.is_none());
    }

    // ── is_idempotent ───────────────────────────────────────────────────────

    #[test]
    fn is_idempotent_table() {
        assert!(is_idempotent(&reqwest::Method::GET));
        assert!(is_idempotent(&reqwest::Method::HEAD));
        assert!(is_idempotent(&reqwest::Method::PUT));
        assert!(is_idempotent(&reqwest::Method::DELETE));
        assert!(!is_idempotent(&reqwest::Method::POST));
        assert!(!is_idempotent(&reqwest::Method::PATCH));
    }

    // ── render_auth_error ───────────────────────────────────────────────────

    #[test]
    fn render_auth_error_keeps_prefix_and_appends_trace_id() {
        let with_trace = crate::auth::AuthError::TokenEndpoint {
            status: 401,
            error: Some("invalid_client".to_owned()),
            error_description: Some("bad id".to_owned()),
            trace_id: Some("tr-token".to_owned()),
        };
        let rendered = render_auth_error(&with_trace);
        assert!(
            rendered.starts_with("auth error: "),
            "the historical prefix must be kept, got: {rendered}"
        );
        assert!(rendered.contains("invalid_client"), "{rendered}");
        assert!(rendered.contains("\nTrace-Id: tr-token"), "{rendered}");

        let without_trace = crate::auth::AuthError::ReauthRequired {
            reason: "no stored device authorization".to_owned(),
        };
        let rendered = render_auth_error(&without_trace);
        assert!(rendered.starts_with("auth error: "), "{rendered}");
        assert!(!rendered.contains("Trace-Id:"), "{rendered}");

        // The mint-429 variant carries its Trace-Id into the report too.
        let mint = crate::auth::AuthError::TokenMintRateLimited {
            retry_after: Some(Duration::from_secs(1)),
            trace_id: Some("tr-429".to_owned()),
        };
        let rendered = render_auth_error(&mint);
        assert!(rendered.contains("auth error: "), "{rendered}");
        assert!(rendered.contains("token churn"), "{rendered}");
        assert!(rendered.contains("\nTrace-Id: tr-429"), "{rendered}");
    }
}
