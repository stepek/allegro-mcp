//! Device authorization flow (RFC 8628-style, per Allegro's docs): request a
//! device code, show it to the user, poll the token endpoint until the user
//! approves/denies or the codes expire.
//!
//! Everything here is disk-free and independently testable — persistence
//! lives in [`super::token_store`], the façade that composes both lives in
//! [`super`]. The Allegro-specific quirks handled here:
//!
//! - `expires_in` / `interval` may arrive as JSON numbers **or** numeric
//!   strings (the docs' own samples are inconsistent) → [`u64_lenient`];
//! - polling faster than `interval` yields `400 slow_down` → back off by
//!   [`SLOW_DOWN_INCREMENT_SECS`] (+1 s, matching Allegro's own sample; RFC
//!   8628 §3.5 suggests +5 s — one const to change if needed);
//! - a `400` with the non-standard literal `"Invalid device code"` (or any
//!   other unrecognized 4xx error code) is **terminal**: the device code was
//!   consumed, expired, or the request is malformed.
//!
//! The `device_code` never appears in user-facing output
//! ([`banner_text`]) — only the short `user_code` does.

use std::ops::ControlFlow;
use std::time::Duration;

use serde::Deserialize;

use super::{AuthError, TokenResponse};

/// One helper, used twice (device response + the façade's `TokenResponse`):
/// deserializes a `u64` from either a JSON number or a numeric string
/// ("3600"). Whitespace-tolerant; anything else is a hard error.
pub(crate) fn u64_lenient<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(n) => n.as_u64().ok_or_else(|| {
            serde::de::Error::custom(format!("expected a non-negative integer, got {n}"))
        }),
        serde_json::Value::String(s) => s
            .trim()
            .parse::<u64>()
            .map_err(|e| serde::de::Error::custom(format!("expected a numeric string: {e}"))),
        other => Err(serde::de::Error::custom(format!(
            "expected a number or numeric string, got {other}"
        ))),
    }
}

// ── Device authorization response ─────────────────────────────────────────────

/// `POST /auth/oauth/device` response (RFC 8628 §3.2 subset Allegro returns).
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuthorizationResponse {
    /// Short code displayed to the user (Allegro renders it grouped in
    /// threes, e.g. `XXX XXX XXX` — see [`format_user_code`]).
    pub user_code: String,
    /// Polling handle — single-use; never logged or displayed.
    pub device_code: String,
    /// Seconds both codes stay valid (docs sample: 3600).
    #[serde(deserialize_with = "u64_lenient")]
    pub expires_in: u64,
    /// Required minimum seconds between polls; polling faster yields
    /// `400 slow_down`.
    #[serde(deserialize_with = "u64_lenient")]
    pub interval: u64,
    /// Page where the user enters the code.
    pub verification_uri: String,
    /// Same page with the code pre-filled — preferred for a "clickable"
    /// banner. Optional per spec.
    pub verification_uri_complete: Option<String>,
}

// ── Polling taxonomy ──────────────────────────────────────────────────────────

/// Extra seconds added to the poll interval on each `slow_down` response.
///
/// The ticket (and Allegro's own PHP sample) say **+1 s**; RFC 8628 §3.5
/// recommends +5 s. Named const so tightening it is a one-line change.
pub const SLOW_DOWN_INCREMENT_SECS: u64 = 1;

/// Give up after this many *consecutive* transient failures (5xx / transport
/// errors) — a bounded cap so a dead network cannot poll forever.
pub const MAX_TRANSIENT_ERRORS: u32 = 5;

/// The outcome of one token-endpoint poll, per Allegro's five-response
/// enumeration (plus transport-level surprises).
#[derive(Debug, Clone)]
pub enum PollOutcome {
    /// `200` + token JSON — the grant completed (the `device_code` can
    /// return a token only once).
    Token(TokenResponse),
    /// `400 {"error":"authorization_pending"}` — keep polling.
    AuthorizationPending,
    /// `400 {"error":"slow_down"}` — back off, keep polling.
    SlowDown,
    /// `400 {"error":"access_denied"}` — the user denied; terminal.
    AccessDenied,
    /// `400 expired_token` / `invalid_grant` / `"Invalid device code"` /
    /// any other 4xx error code — the grant is dead; terminal (generate a
    /// new code).
    ExpiredOrInvalid,
    /// 5xx / transport error / unparsable 2xx body — retry, capped at
    /// [`MAX_TRANSIENT_ERRORS`] consecutive failures.
    Transient(String),
}

/// Terminal reasons for the device flow. Mapped to `anyhow` at the CLI
/// boundary; the façade maps the re-auth cases into
/// [`AuthError::ReauthRequired`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeviceFlowError {
    #[error("authorization denied by the user")]
    AccessDenied,
    /// [`PollOutcome::ExpiredOrInvalid`] or the wall-clock deadline passed.
    #[error("device/user code expired or invalid — request a new one")]
    Expired,
    #[error("gave up after {MAX_TRANSIENT_ERRORS} consecutive transient failures: {0}")]
    Network(String),
}

/// Classifies one token-endpoint response (status + body) into a
/// [`PollOutcome`]. Pure — no I/O — so the full matrix is unit-testable.
///
/// The body is read **before** any status check: the 400 bodies carry the
/// taxonomy, and `error_for_status()` would discard them.
pub fn classify(status: u16, body: &str) -> PollOutcome {
    if (400..500).contains(&status) {
        return classify_error_body(body);
    }
    if status >= 500 {
        return PollOutcome::Transient(format!("HTTP {status}: {body}"));
    }
    // 2xx (any unexpected 3xx lands here too) — try to parse the token.
    match serde_json::from_str::<TokenResponse>(body) {
        Ok(tokens) => PollOutcome::Token(tokens),
        Err(e) => PollOutcome::Transient(format!("unparsable token body (HTTP {status}): {e}")),
    }
}

/// `4xx` bodies: `{"error": "<code>"}` where the code picks the branch.
/// Per Allegro's docs, *any* unrecognized 4xx error code means "codes
/// expired or your request is malformed" → terminal, not retryable.
fn classify_error_body(body: &str) -> PollOutcome {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned));

    match code.as_deref() {
        Some("authorization_pending") => PollOutcome::AuthorizationPending,
        Some("slow_down") => PollOutcome::SlowDown,
        Some("access_denied") => PollOutcome::AccessDenied,
        // Allegro's non-standard literal for RFC 8628's `expired_token` —
        // spaces, not snake_case. Matched case-insensitively as a substring
        // because the docs show it verbatim and casing may vary.
        Some(other) if other.to_ascii_lowercase().contains("invalid device code") => {
            PollOutcome::ExpiredOrInvalid
        }
        // `expired_token` / `invalid_grant` / anything else → terminal.
        Some(other) => {
            tracing::debug!(error_code = other, "device poll: terminal 4xx error");
            PollOutcome::ExpiredOrInvalid
        }
        None => {
            tracing::debug!(
                body = &body[..body.len().min(200)],
                "device poll: 4xx without a parseable error field"
            );
            PollOutcome::ExpiredOrInvalid
        }
    }
}

// ── Poll state machine (pure) ─────────────────────────────────────────────────

/// Mutable polling state: current interval, wall-clock deadline, and the
/// consecutive-transient-failure counter. Pure logic — `advance` maps a
/// [`PollOutcome`] to continue-polling or a terminal [`DeviceFlowError`] —
/// so the backoff/cap semantics are unit-testable without network.
#[derive(Debug, Clone)]
pub struct PollState {
    interval: Duration,
    deadline: std::time::Instant,
    transient_errors: u32,
}

impl PollState {
    /// `deadline = now + expires_in_secs` — wall-clock-deadline-by-`Instant`,
    /// which (unlike epoch arithmetic) also covers sleep/suspend correctly
    /// for the lifetime of this process.
    pub fn new(interval_secs: u64, expires_in_secs: u64) -> Self {
        Self {
            interval: Duration::from_secs(interval_secs),
            deadline: std::time::Instant::now() + Duration::from_secs(expires_in_secs),
            transient_errors: 0,
        }
    }

    /// Current poll interval (grows by [`SLOW_DOWN_INCREMENT_SECS`] per
    /// `slow_down`).
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// `true` once the codes' lifetime is up — checked at the top of every
    /// poll iteration.
    pub fn deadline_exceeded(&self) -> bool {
        std::time::Instant::now() >= self.deadline
    }

    /// [`ControlFlow::Break`] with the terminal reason, or [`ControlFlow::Continue`]
    /// to poll again. A successful [`PollOutcome::Token`] never reaches here
    /// (the caller returns it directly); the arm stays defensive.
    pub fn advance(&mut self, outcome: &PollOutcome) -> ControlFlow<DeviceFlowError> {
        match outcome {
            PollOutcome::Token(_) => ControlFlow::Continue(()),
            PollOutcome::AuthorizationPending => ControlFlow::Continue(()),
            PollOutcome::SlowDown => {
                self.interval += Duration::from_secs(SLOW_DOWN_INCREMENT_SECS);
                ControlFlow::Continue(())
            }
            PollOutcome::AccessDenied => ControlFlow::Break(DeviceFlowError::AccessDenied),
            PollOutcome::ExpiredOrInvalid => ControlFlow::Break(DeviceFlowError::Expired),
            PollOutcome::Transient(reason) => {
                self.transient_errors += 1;
                if self.transient_errors >= MAX_TRANSIENT_ERRORS {
                    ControlFlow::Break(DeviceFlowError::Network(reason.clone()))
                } else {
                    ControlFlow::Continue(())
                }
            }
        }
    }
}

// ── Driver ────────────────────────────────────────────────────────────────────

/// HTTP + credential dependencies for the device flow, bundled so the
/// request/poll functions stay pure with respect to configuration.
#[derive(Debug, Clone)]
pub struct DeviceFlowDeps {
    /// Shared HTTP client — built by `crate::http` so the ToS-compliant
    /// User-Agent applies to device-flow requests too.
    pub http: reqwest::Client,
    pub auth_base_url: String,
    pub client_id: String,
    pub client_secret: String,
    /// Space-joined into the optional `scope` form param when non-empty
    /// (single source of truth with the client_credentials flow).
    pub scopes: Vec<String>,
    pub policy: PollingPolicy,
}

/// Test/production knob for the poll sleep. `production()` sleeps the
/// server-mandated interval; `test_instant()` never sleeps so wiremock
/// tests run at full speed.
#[derive(Debug, Clone, Copy)]
pub struct PollingPolicy {
    pub interval_override: Option<Duration>,
}

impl PollingPolicy {
    /// Real-world policy: honor the server's `interval` (and the slow_down
    /// backoff accumulated on top of it).
    pub fn production() -> Self {
        Self {
            interval_override: None,
        }
    }

    /// Test policy: zero sleep everywhere (initial interval *and* per
    /// iteration) so integration tests don't real-sleep.
    /// Zero-latency policy for tests: both sleeps and intervals collapse
    /// to zero so polling loops run instantly (slow_down accumulation is
    /// still tracked in `PollState`).
    // Test-only; unused inside the non-test bin pass.
    #[allow(dead_code)]
    pub fn test_instant() -> Self {
        Self {
            interval_override: Some(Duration::ZERO),
        }
    }

    /// The interval to seed a [`PollState`] with.
    pub fn effective_interval(&self, server_interval_secs: u64) -> Duration {
        self.interval_override
            .unwrap_or_else(|| Duration::from_secs(server_interval_secs))
    }

    /// The per-iteration sleep. In production this is the state's current
    /// interval (`advance` grows it on slow_down); the override wins in
    /// tests regardless of accumulated backoff.
    fn effective_sleep(&self, state_interval: Duration) -> Duration {
        self.interval_override.unwrap_or(state_interval)
    }
}

/// Starts the flow: `POST {auth_base_url}/auth/oauth/device` with Basic auth
/// and `client_id` (+ optional space-joined `scope`) in the form body — the
/// form body per the official PHP sample (cleaner URL than the docs' curl
/// variant that puts `client_id` in the query string; both work).
pub async fn request_device_code(
    deps: &DeviceFlowDeps,
) -> Result<DeviceAuthorizationResponse, AuthError> {
    let url = format!("{}/auth/oauth/device", deps.auth_base_url);
    debug_scopes(deps);
    tracing::debug!(url, "requesting device authorization code");

    let scope_param = deps.scopes.join(" ");
    let mut form: Vec<(&str, &str)> = vec![("client_id", deps.client_id.as_str())];
    if !scope_param.is_empty() {
        form.push(("scope", scope_param.as_str()));
    }

    let resp = deps
        .http
        .post(&url)
        .basic_auth(&deps.client_id, Some(&deps.client_secret))
        .form(&form)
        .send()
        .await?
        // The device endpoint's errors carry no taxonomy worth parsing —
        // a 4xx here is almost always a non-device-type app registration
        // (docs: an app's type cannot be changed after registration).
        .error_for_status()?;

    let parsed: DeviceAuthorizationResponse = resp.json().await?;
    tracing::debug!(
        interval = parsed.interval,
        expires_in = parsed.expires_in,
        "device authorization code issued"
    );
    Ok(parsed)
}

/// Polls `POST {auth_base_url}/auth/oauth/token` with
/// `grant_type=urn:ietf:params:oauth:grant-type:device_code` until the user
/// approves ([`Ok`]) or a terminal condition is reached ([`Err`]).
///
/// Loop shape: deadline check → sleep → POST → classify → advance. The
/// status/body pair is classified *after* reading the body (400s carry the
/// taxonomy); `tokio::time::sleep` keeps the loop pausable in tests.
pub async fn poll_for_token(
    deps: &DeviceFlowDeps,
    device_code: &str,
    mut state: PollState,
) -> Result<TokenResponse, DeviceFlowError> {
    loop {
        if state.deadline_exceeded() {
            return Err(DeviceFlowError::Expired);
        }
        tokio::time::sleep(deps.policy.effective_sleep(state.interval())).await;

        let outcome = post_token_poll(deps, device_code).await;
        match outcome {
            PollOutcome::Token(tokens) => return Ok(tokens),
            other => {
                if let ControlFlow::Break(err) = state.advance(&other) {
                    return Err(err);
                }
            }
        }
    }
}

/// One poll POST. Transport errors and non-2xx statuses become
/// [`PollOutcome`]s — never `error_for_status()` before the body is read.
async fn post_token_poll(deps: &DeviceFlowDeps, device_code: &str) -> PollOutcome {
    let url = format!("{}/auth/oauth/token", deps.auth_base_url);
    let form = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("device_code", device_code),
    ];

    let response = match deps
        .http
        .post(&url)
        .basic_auth(&deps.client_id, Some(&deps.client_secret))
        .form(&form)
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => return PollOutcome::Transient(format!("transport error: {e}")),
    };

    let status = response.status().as_u16();
    match response.text().await {
        Ok(body) => classify(status, &body),
        Err(e) => PollOutcome::Transient(format!("body read failed: {e}")),
    }
}

fn debug_scopes(deps: &DeviceFlowDeps) {
    tracing::debug!(scopes = deps.scopes.len(), "device flow request prepared");
}

// ── User-facing banner ────────────────────────────────────────────────────────

/// Groups a user code in threes, space-separated (Allegro's recommended
/// display): `"cbt3zdu4g"` → `"cbt 3zd u4g"`. Char-based, so non-3-multiple
/// lengths keep their tail (`"abcd"` → `"abc d"`).
pub fn format_user_code(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    chars
        .chunks(3)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds the "go open this URL" banner shown by the CLI (stderr) and the
/// server's resume path (docker logs). Names the pre-filled
/// `verification_uri_complete` first, falling back to
/// `verification_uri` + the grouped user code; carries the expiry and the
/// poll cadence. `resumed = true` marks a restart of an earlier grant.
/// Never includes `device_code`.
pub fn banner_text(resp: &DeviceAuthorizationResponse, sandbox: bool, resumed: bool) -> String {
    let rule = "=================================================================";
    let mut out = String::new();
    out.push_str(rule);
    out.push('\n');
    out.push_str(&format!(
        " allegro-mcp: Allegro device authorization{}{}\n",
        if sandbox { " (sandbox)" } else { "" },
        if resumed {
            " — resuming your earlier device authorization"
        } else {
            ""
        },
    ));

    match &resp.verification_uri_complete {
        Some(complete) => {
            out.push_str(" Open this link in a browser and approve the request:\n");
            out.push_str(&format!("   {complete}\n"));
            out.push_str(&format!(
                " (or open {} and enter the code: {})\n",
                resp.verification_uri,
                format_user_code(&resp.user_code),
            ));
        }
        None => {
            out.push_str(" Open this URL in a browser, then enter the code:\n");
            out.push_str(&format!("   {}\n", resp.verification_uri));
            out.push_str(&format!("   code: {}\n", format_user_code(&resp.user_code)));
        }
    }

    out.push_str(&format!(
        " The code expires in {} s — polling every {} s until you approve or deny.\n",
        resp.expires_in, resp.interval
    ));
    out.push_str(rule);
    out
}

// ── Resume bridge ─────────────────────────────────────────────────────────────

impl super::token_store::PendingDeviceGrant {
    /// Rebuilds a display/poll-ready response from a persisted pending
    /// grant (the resume path): `expires_in` becomes the *remaining*
    /// seconds so banners and deadlines reflect the stored wall-clock
    /// expiry rather than the original lifetime.
    pub fn to_authorization_response(&self) -> DeviceAuthorizationResponse {
        DeviceAuthorizationResponse {
            user_code: self.user_code.clone(),
            device_code: self.device_code.clone(),
            expires_in: self
                .expires_at_epoch
                .saturating_sub(super::token_store::epoch_now()),
            interval: self.interval_secs,
            verification_uri: self.verification_uri.clone(),
            verification_uri_complete: self.verification_uri_complete.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── u64_lenient ───────────────────────────────────────────────────────────

    #[test]
    fn u64_lenient_accepts_numbers_and_numeric_strings() {
        #[derive(Deserialize)]
        struct Probe {
            #[serde(deserialize_with = "u64_lenient")]
            v: u64,
        }
        let from_number: Probe = serde_json::from_str(r#"{"v": 3600}"#).expect("json number");
        assert_eq!(from_number.v, 3600);
        let from_string: Probe = serde_json::from_str(r#"{"v": "5"}"#).expect("numeric string");
        assert_eq!(from_string.v, 5);
        let padded: Probe = serde_json::from_str(r#"{"v": " 42 "}"#).expect("trimmed");
        assert_eq!(padded.v, 42);

        assert!(
            serde_json::from_str::<Probe>(r#"{"v": "abc"}"#).is_err(),
            "non-numeric string must be rejected"
        );
        assert!(
            serde_json::from_str::<Probe>(r#"{"v": null}"#).is_err(),
            "null must be rejected"
        );
        assert!(
            serde_json::from_str::<Probe>(r#"{"v": -1}"#).is_err(),
            "negative numbers must be rejected"
        );
    }

    // ── DeviceAuthorizationResponse parsing ───────────────────────────────────

    fn parse_device_response(body: &str) -> DeviceAuthorizationResponse {
        serde_json::from_str(body).expect("valid device response")
    }

    #[test]
    fn device_response_parses_numeric_fields() {
        let resp = parse_device_response(
            r#"{
                "user_code": "cbt3zdu4g",
                "device_code": "645629715",
                "expires_in": 3600,
                "interval": 5,
                "verification_uri": "https://allegro.pl/skojarz-aplikacje",
                "verification_uri_complete": "https://allegro.pl/skojarz-aplikacje?code=cbt3zdu4g"
            }"#,
        );
        assert_eq!(resp.expires_in, 3600);
        assert_eq!(resp.interval, 5);
        assert!(resp.verification_uri_complete.is_some());
    }

    /// The docs' json5 samples quote the numbers — must parse either way.
    #[test]
    fn device_response_parses_quoted_numeric_fields() {
        let resp = parse_device_response(
            r#"{
                "user_code": "cbt3zdu4g",
                "device_code": "645629715",
                "expires_in": "3600",
                "interval": "5",
                "verification_uri": "https://allegro.pl/skojarz-aplikacje"
            }"#,
        );
        assert_eq!(resp.expires_in, 3600);
        assert_eq!(resp.interval, 5);
        assert!(
            resp.verification_uri_complete.is_none(),
            "missing verification_uri_complete must be tolerated"
        );
    }

    // ── classify matrix (§0.2 — all five responses + edge cases) ─────────────

    #[test]
    fn classify_success_yields_token() {
        let outcome = classify(
            200,
            r#"{"access_token":"t","token_type":"bearer","refresh_token":"r","expires_in":43199}"#,
        );
        let PollOutcome::Token(tokens) = outcome else {
            panic!("expected Token, got {outcome:?}");
        };
        assert_eq!(tokens.access_token, "t");
        assert_eq!(tokens.refresh_token.as_deref(), Some("r"));
    }

    #[test]
    fn classify_pending_yields_authorization_pending() {
        assert!(matches!(
            classify(400, r#"{"error":"authorization_pending"}"#),
            PollOutcome::AuthorizationPending
        ));
    }

    #[test]
    fn classify_slow_down_yields_slow_down() {
        assert!(matches!(
            classify(400, r#"{"error":"slow_down"}"#),
            PollOutcome::SlowDown
        ));
    }

    #[test]
    fn classify_access_denied_yields_access_denied() {
        assert!(matches!(
            classify(400, r#"{"error":"access_denied"}"#),
            PollOutcome::AccessDenied
        ));
    }

    #[test]
    fn classify_invalid_device_code_literal_is_terminal() {
        // Case-insensitive substring match on the non-standard literal.
        for body in [
            r#"{"error":"Invalid device code"}"#,
            r#"{"error":"invalid device code"}"#,
            r#"{"error":"Something: Invalid Device Code!"}"#,
        ] {
            assert!(
                matches!(classify(400, body), PollOutcome::ExpiredOrInvalid),
                "body {body} must be terminal"
            );
        }
    }

    #[test]
    fn classify_expired_token_is_terminal() {
        assert!(matches!(
            classify(400, r#"{"error":"expired_token"}"#),
            PollOutcome::ExpiredOrInvalid
        ));
    }

    /// Any *other* 4xx error code means "codes expired or malformed" per the
    /// docs → terminal, with the raw code visible in debug logs.
    #[test]
    fn classify_unknown_4xx_error_code_is_terminal() {
        assert!(matches!(
            classify(400, r#"{"error":"some_future_code"}"#),
            PollOutcome::ExpiredOrInvalid
        ));
        assert!(matches!(
            classify(403, r#"{"error":"forbidden"}"#),
            PollOutcome::ExpiredOrInvalid
        ));
    }

    #[test]
    fn classify_4xx_without_error_field_is_terminal() {
        assert!(matches!(
            classify(400, "<html>bad gateway</html>"),
            PollOutcome::ExpiredOrInvalid
        ));
    }

    #[test]
    fn classify_5xx_is_transient() {
        let outcome = classify(503, "service unavailable");
        assert!(
            matches!(outcome, PollOutcome::Transient(_)),
            "5xx must be transient, got {outcome:?}"
        );
    }

    #[test]
    fn classify_unparsable_2xx_body_is_transient() {
        let outcome = classify(200, "not json");
        assert!(
            matches!(outcome, PollOutcome::Transient(_)),
            "garbage 200 body must be transient, got {outcome:?}"
        );
    }

    // ── PollState ─────────────────────────────────────────────────────────────

    #[test]
    fn poll_state_pending_keeps_interval() {
        let mut state = PollState::new(5, 3600);
        assert!(matches!(
            state.advance(&PollOutcome::AuthorizationPending),
            ControlFlow::Continue(())
        ));
        assert_eq!(state.interval(), Duration::from_secs(5));
    }

    #[test]
    fn poll_state_slow_down_adds_exactly_one_second_each_time() {
        let mut state = PollState::new(5, 3600);
        let _ = state.advance(&PollOutcome::SlowDown);
        assert_eq!(
            state.interval(),
            Duration::from_secs(6),
            "+1 s per slow_down"
        );
        let _ = state.advance(&PollOutcome::SlowDown);
        assert_eq!(state.interval(), Duration::from_secs(7), "twice → +2 s");
    }

    #[test]
    fn poll_state_access_denied_breaks_immediately() {
        let mut state = PollState::new(5, 3600);
        assert!(matches!(
            state.advance(&PollOutcome::AccessDenied),
            ControlFlow::Break(DeviceFlowError::AccessDenied)
        ));
    }

    #[test]
    fn poll_state_expired_breaks_immediately() {
        let mut state = PollState::new(5, 3600);
        assert!(matches!(
            state.advance(&PollOutcome::ExpiredOrInvalid),
            ControlFlow::Break(DeviceFlowError::Expired)
        ));
    }

    #[test]
    fn poll_state_transient_cap_breaks_on_fifth() {
        let mut state = PollState::new(5, 3600);
        for i in 1..MAX_TRANSIENT_ERRORS {
            assert!(
                matches!(
                    state.advance(&PollOutcome::Transient("boom".to_owned())),
                    ControlFlow::Continue(())
                ),
                "transient #{i} must continue"
            );
        }
        assert!(
            matches!(
                state.advance(&PollOutcome::Transient("boom".to_owned())),
                ControlFlow::Break(DeviceFlowError::Network(_))
            ),
            "the {MAX_TRANSIENT_ERRORS}th consecutive transient must break"
        );
    }

    #[test]
    fn poll_state_deadline_boundary() {
        // expires_in = 0 → the deadline is already due.
        let expired = PollState::new(5, 0);
        assert!(expired.deadline_exceeded());

        // An hour of headroom → not exceeded.
        let live = PollState::new(5, 3600);
        assert!(!live.deadline_exceeded());
    }

    // ── banner ────────────────────────────────────────────────────────────────

    fn banner_fixture(complete: Option<&str>) -> DeviceAuthorizationResponse {
        DeviceAuthorizationResponse {
            user_code: "cbt3zdu4g".to_owned(),
            device_code: "645629715".to_owned(),
            expires_in: 3600,
            interval: 5,
            verification_uri: "https://allegro.pl/skojarz-aplikacje".to_owned(),
            verification_uri_complete: complete.map(str::to_owned),
        }
    }

    #[test]
    fn format_user_code_groups_in_threes() {
        assert_eq!(format_user_code("cbt3zdu4g"), "cbt 3zd u4g");
        assert_eq!(format_user_code("abcdefg"), "abc def g");
        assert_eq!(format_user_code("ab"), "ab");
        assert_eq!(format_user_code(""), "");
    }

    #[test]
    fn banner_prefers_the_complete_uri_and_never_leaks_device_code() {
        let resp = banner_fixture(Some("https://allegro.pl/skojarz-aplikacje?code=cbt3zdu4g"));
        let text = banner_text(&resp, false, false);
        assert!(text.contains("https://allegro.pl/skojarz-aplikacje?code=cbt3zdu4g"));
        assert!(text.contains("cbt 3zd u4g"), "grouped user code present");
        assert!(text.contains("3600"), "expiry present");
        assert!(text.contains("every 5 s"), "poll cadence present");
        assert!(
            !text.contains("645629715"),
            "device_code must never appear in the banner, got: {text}"
        );
        assert!(!text.contains("resuming"), "fresh start is not a resume");
    }

    #[test]
    fn banner_falls_back_to_uri_plus_code_and_marks_resumes() {
        let resp = banner_fixture(None);
        let text = banner_text(&resp, true, true);
        assert!(text.contains("https://allegro.pl/skojarz-aplikacje"));
        assert!(text.contains("(sandbox)"), "sandbox flag surfaced");
        assert!(
            text.contains("resuming your earlier device authorization"),
            "resume note present, got: {text}"
        );
    }
}
