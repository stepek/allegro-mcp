//! Integration tests for the OAuth2 device flow (Phase 5 / GH-5): the
//! wiremock-observed request shapes, the polling sequences (pending /
//! slow_down / denied / expired), resume-from-persisted-grant, refresh
//! rotation, and the dispatcher's single 401 retry — end-to-end through the
//! public `allegro_mcp` API surface.
//!
//! Conventions:
//! - every test gets its own tempdir token store — no shared filesystem
//!   state;
//! - [`PollingPolicy::test_instant`] zeroes the poll sleeps so tests don't
//!   real-sleep (the +1 s slow_down accumulation itself is unit-covered in
//!   `src/auth/device.rs`);
//! - wiremock checks equal-priority mocks in **registration order** and an
//!   exhausted `up_to_n_times` mock is skipped — so the *limited* mock is
//!   always mounted first and the fallback last.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use allegro_mcp::auth::device::{
    poll_for_token, request_device_code, DeviceAuthorizationResponse, DeviceFlowDeps,
    DeviceFlowError, PollState, PollingPolicy,
};
use allegro_mcp::auth::token_store::TokenStore;
use allegro_mcp::auth::{AllegroAuth, AuthError, TokenResponse};
use allegro_mcp::dispatcher::dispatch_with_base;
use allegro_mcp::tool_registry::ToolDef;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Wall-clock epoch seconds — mirrors `token_store::epoch_now`, which is
/// `pub(crate)` and therefore invisible to this separate test crate.
fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

fn device_json() -> serde_json::Value {
    json!({
        "user_code": "cbt3zdu4g",
        "device_code": "645629715",
        "expires_in": 3600,
        "interval": 5,
        "verification_uri": "https://allegro.pl/skojarz-aplikacje",
        "verification_uri_complete": "https://allegro.pl/skojarz-aplikacje?code=cbt3zdu4g"
    })
}

fn token_json(access: &str, refresh: &str) -> serde_json::Value {
    json!({
        "access_token": access,
        "token_type": "bearer",
        "refresh_token": refresh,
        "expires_in": 43199,
        "scope": "allegro:api:read"
    })
}

fn ok_token_template(access: &str, refresh: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(token_json(access, refresh))
}

fn error_template(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "error": code }))
}

fn deps_for(mock: &MockServer, policy: PollingPolicy) -> DeviceFlowDeps {
    DeviceFlowDeps {
        http: reqwest::Client::new(),
        auth_base_url: mock.uri(),
        client_id: "id".to_owned(),
        client_secret: "secret".to_owned(),
        scopes: vec![],
        policy,
    }
}

fn store_in(dir: &Path) -> TokenStore {
    TokenStore::new(dir.join("tokens.json"), false)
}

/// Drives the CLI-shaped sequence: request → persist pending → poll →
/// persist tokens.
async fn run_flow(
    deps: &DeviceFlowDeps,
    store: &TokenStore,
) -> Result<TokenResponse, DeviceFlowError> {
    let grant = request_device_code(deps)
        .await
        .expect("device code request");
    store.save_pending(&grant).expect("save pending");
    let interval = deps.policy.effective_interval(grant.interval);
    let state = PollState::new(interval.as_secs(), grant.expires_in);
    let tokens = poll_for_token(deps, &grant.device_code, state)
        .await
        .expect("poll success");
    store
        .save_tokens(&tokens, tokens.expires_in)
        .expect("save tokens");
    Ok(tokens)
}

/// Seeds an *expired* token pair with a refresh token directly (the public
/// `save_tokens` always persists a live pair).
fn seed_expired_pair(store: &TokenStore, refresh: &str) {
    let envelope = json!({
        "version": 1,
        "env": "production",
        "tokens": {
            "access_token": "old-access",
            "refresh_token": refresh,
            "expires_at_epoch": epoch_now() - 10,
            "scope": "allegro:api:read",
            "updated_at_epoch": epoch_now()
        }
    });
    std::fs::write(store.path(), envelope.to_string()).expect("seed expired pair");
}

/// Seeds a *live* (unexpired) pair with a refresh token — the state a
/// server would have restored at startup, before anything expires.
fn seed_live_pair(store: &TokenStore, access: &str, refresh: &str) {
    let envelope = json!({
        "version": 1,
        "env": "production",
        "tokens": {
            "access_token": access,
            "refresh_token": refresh,
            "expires_at_epoch": epoch_now() + 3600,
            "scope": "allegro:api:read",
            "updated_at_epoch": epoch_now()
        }
    });
    std::fs::write(store.path(), envelope.to_string()).expect("seed live pair");
}

/// A minimal GET tool hitting `/sale/offers`, for the dispatcher tests.
fn offers_tool() -> ToolDef {
    ToolDef {
        id: "allegro_get_offers".to_owned(),
        name: "allegro_get_offers".to_owned(),
        description: "List offers".to_owned(),
        input_schema: json!({}),
        method: "get".to_owned(),
        path: "/sale/offers".to_owned(),
        accept_media_type: None,
    }
}

// ── 1. Happy path ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn device_flow_happy_path_persists_tokens() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(device_json()))
        .mount(&mock)
        .await;
    // Token endpoint: authorization_pending twice, then the grant.
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(error_template(400, "authorization_pending"))
        .up_to_n_times(2)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ok_token_template("dev-access-token", "dev-refresh-token"))
        .mount(&mock)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_in(dir.path());
    let deps = deps_for(&mock, PollingPolicy::test_instant());

    let tokens = run_flow(&deps, &store).await.expect("flow completes");
    assert_eq!(tokens.access_token, "dev-access-token");
    assert_eq!(tokens.refresh_token.as_deref(), Some("dev-refresh-token"));

    // Persisted pair: access + refresh + epoch expiry; pending cleared.
    let stored = store.load().expect("load store");
    let saved = stored.tokens.expect("tokens persisted");
    assert_eq!(saved.access_token, "dev-access-token");
    assert_eq!(saved.refresh_token.as_deref(), Some("dev-refresh-token"));
    assert!(
        saved.expires_at_epoch > epoch_now(),
        "epoch expiry must be in the future"
    );
    assert!(
        stored.pending.is_none(),
        "grant completion must clear the pending grant"
    );

    // Request shapes: form body carries the device grant; Basic auth present.
    let reqs = mock.received_requests().await.expect("recorded requests");
    let token_reqs: Vec<_> = reqs
        .iter()
        .filter(|r| r.url.path() == "/auth/oauth/token")
        .collect();
    assert_eq!(
        token_reqs.len(),
        3,
        "two pending polls + one successful grant"
    );
    for req in &token_reqs {
        let body = String::from_utf8_lossy(&req.body);
        assert!(
            body.contains("grant_type=urn"),
            "device poll must send the device_code grant_type, got: {body}"
        );
        assert!(
            body.contains("device_code=645629715"),
            "device poll must carry the device_code, got: {body}"
        );
        let authz = req
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            authz.starts_with("Basic "),
            "polls must use Basic auth, got: {authz:?}"
        );
    }
    let device_req: Vec<_> = reqs
        .iter()
        .filter(|r| r.url.path() == "/auth/oauth/device")
        .collect();
    assert_eq!(device_req.len(), 1);
    assert!(
        String::from_utf8_lossy(&device_req[0].body).contains("client_id=id"),
        "the device endpoint gets client_id in the form body"
    );

    // `auth.token()` restores from the store without hitting the network
    // again (fresh façade instance, same store, dead-simple assertion: the
    // token endpoint request count must not grow).
    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri())
        .with_token_store(store_in(dir.path()));
    let token = auth.token().await.expect("restore from store");
    assert_eq!(token, "dev-access-token");

    let reqs_after = mock.received_requests().await.expect("recorded requests");
    let token_hits_after = reqs_after
        .iter()
        .filter(|r| r.url.path() == "/auth/oauth/token")
        .count();
    assert_eq!(
        token_hits_after, 3,
        "restoring a live stored token must not make network calls"
    );
}

// ── 2. slow_down ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn device_flow_slow_down_backs_off() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(device_json()))
        .mount(&mock)
        .await;
    // slow_down twice, then the grant.
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(error_template(400, "slow_down"))
        .up_to_n_times(2)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ok_token_template("sd-access", "sd-refresh"))
        .mount(&mock)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_in(dir.path());
    let deps = deps_for(&mock, PollingPolicy::test_instant());

    let tokens = run_flow(&deps, &store)
        .await
        .expect("slow_down must not abort");
    assert_eq!(tokens.access_token, "sd-access");
    // (The +1 s accumulation itself is asserted by PollState unit tests.)
}

// ── 3. access_denied ──────────────────────────────────────────────────────────

#[tokio::test]
async fn device_flow_access_denied_is_terminal() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(device_json()))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(error_template(400, "access_denied"))
        .mount(&mock)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_in(dir.path());
    let deps = deps_for(&mock, PollingPolicy::test_instant());

    let grant = request_device_code(&deps).await.expect("device code");
    store.save_pending(&grant).expect("save pending");
    let interval = deps.policy.effective_interval(grant.interval);
    let state = PollState::new(interval.as_secs(), grant.expires_in);

    let err = poll_for_token(&deps, &grant.device_code, state)
        .await
        .expect_err("denial is terminal");
    assert!(
        matches!(err, DeviceFlowError::AccessDenied),
        "expected AccessDenied, got: {err:?}"
    );

    // The file still holds only the pending grant — no tokens were granted.
    let stored = store.load().expect("load store");
    assert!(stored.tokens.is_none());
    assert!(stored.pending.is_some());
}

// ── 4. expired / invalid device code ─────────────────────────────────────────

#[tokio::test]
async fn device_flow_expired_is_terminal() {
    for error_code in ["Invalid device code", "expired_token"] {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/auth/oauth/device"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_json()))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/oauth/token"))
            .respond_with(error_template(400, error_code))
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path());
        let deps = deps_for(&mock, PollingPolicy::test_instant());

        let grant = request_device_code(&deps).await.expect("device code");
        store.save_pending(&grant).expect("save pending");
        let state = PollState::new(0, grant.expires_in);

        let err = poll_for_token(&deps, &grant.device_code, state)
            .await
            .expect_err("expired/invalid is terminal");
        assert!(
            matches!(err, DeviceFlowError::Expired),
            "error code {error_code:?} must map to Expired, got: {err:?}"
        );
    }
}

// ── 5. Resume from the persisted pending grant ───────────────────────────────

#[tokio::test]
async fn resume_uses_persisted_pending_grant() {
    let mock = MockServer::start().await;

    // Only the token endpoint exists in the "restarted" world; if anything
    // re-requested a device code it would land here and be counted.
    Mock::given(method("POST"))
        .and(path("/auth/oauth/device"))
        .respond_with(ResponseTemplate::new(500).set_body_string("must never be hit"))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ok_token_template("resumed-access", "resumed-refresh"))
        .mount(&mock)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_in(dir.path());

    // Pre-seed the pending grant a killed process left behind.
    let pending = DeviceAuthorizationResponse {
        user_code: "cbt3zdu4g".to_owned(),
        device_code: "645629715".to_owned(),
        expires_in: 3600,
        interval: 5,
        verification_uri: "https://allegro.pl/skojarz-aplikacje".to_owned(),
        verification_uri_complete: None,
    };
    store.save_pending(&pending).expect("seed pending");

    let deps = deps_for(&mock, PollingPolicy::test_instant());
    let state = PollState::new(0, 3600);
    let tokens = poll_for_token(&deps, "645629715", state)
        .await
        .expect("resume poll completes");
    assert_eq!(tokens.access_token, "resumed-access");
    store
        .save_tokens(&tokens, tokens.expires_in)
        .expect("save tokens");

    let reqs = mock.received_requests().await.expect("recorded requests");
    assert!(
        reqs.iter().all(|r| r.url.path() == "/auth/oauth/token"),
        "resume must poll the token endpoint only — the device endpoint was hit {} times",
        reqs.iter()
            .filter(|r| r.url.path() == "/auth/oauth/device")
            .count()
    );
}

// ── 6. Refresh rotation ───────────────────────────────────────────────────────

#[tokio::test]
async fn refresh_rotates_and_persists() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ok_token_template("new-access", "new-refresh"))
        .mount(&mock)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_in(dir.path());
    seed_expired_pair(&store, "old-refresh");

    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri())
        .with_token_store(store_in(dir.path()));
    let token = auth
        .token()
        .await
        .expect("expired access must trigger refresh");
    assert_eq!(token, "new-access");

    // The rotated pair is on disk: NEW refresh token, old one gone.
    let stored = store_in(dir.path()).load().expect("load store");
    let saved = stored.tokens.expect("tokens persisted");
    assert_eq!(saved.refresh_token.as_deref(), Some("new-refresh"));
    assert_eq!(saved.access_token, "new-access");

    // Cache seeded: the second call must not hit the network.
    assert_eq!(auth.token().await.expect("cached"), "new-access");
    let reqs = mock.received_requests().await.expect("recorded requests");
    assert_eq!(reqs.len(), 1, "exactly one refresh grant call");

    let body = String::from_utf8_lossy(&reqs[0].body);
    assert!(
        body.contains("grant_type=refresh_token") && body.contains("refresh_token=old-refresh"),
        "refresh grant request shape, got: {body}"
    );
}

// ── 7. Refresh rejection → re-auth ────────────────────────────────────────────

#[tokio::test]
async fn refresh_rejection_forces_reauth() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(error_template(400, "invalid_grant"))
        .mount(&mock)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_in(dir.path());
    seed_expired_pair(&store, "dead-refresh");

    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri())
        .with_token_store(store_in(dir.path()));
    let err = auth
        .token()
        .await
        .expect_err("a rejected refresh must surface as re-auth");
    let msg = err.to_string();
    assert!(
        matches!(err, AuthError::ReauthRequired { .. }),
        "expected ReauthRequired, got: {err:?}"
    );
    assert!(
        msg.contains("refresh token rejected"),
        "the reason must say why, got: {msg}"
    );

    // The dead pair was wiped from disk.
    assert!(
        !store.path().exists(),
        "a rejected refresh must clear the store"
    );
    let state = store_in(dir.path()).load().expect("store usable again");
    assert!(state.tokens.is_none() && state.pending.is_none());
}

// ── 8. Dispatcher 401 retry (device-mode end-to-end) ──────────────────────────

/// In device mode the 401 retry must *force the refresh grant*: the API
/// just rejected the stored access token out-of-band (revocation, password
/// change, session cap), and a plain store re-read would hand back the very
/// token that failed — an out-of-band revocation never changes the stored
/// `expires_at_epoch`. `refresh_now` skips the stored live token, rotates
/// the pair through the token endpoint, persists the NEW pair, and the
/// retried request succeeds with the new access token.
#[tokio::test]
async fn dispatcher_401_exercises_the_refresh_grant_and_rotates_the_pair() {
    let mock = MockServer::start().await;

    // Token endpoint: the forced refresh grant — old refresh in, rotated
    // pair out.
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ok_token_template("new-access", "new-refresh"))
        .mount(&mock)
        .await;
    // API: 401 exactly once, then 200 (limited mock mounted FIRST).
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(401).set_body_string("stale token"))
        .up_to_n_times(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&mock)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_in(dir.path());
    // Seed a *live* pair — the pre-fix bug short-circuited on it and never
    // contacted the token endpoint.
    seed_live_pair(&store, "stale-access", "old-refresh");

    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri())
        .with_token_store(store_in(dir.path()));

    let out = dispatch_with_base(
        &auth,
        &reqwest::Client::new(),
        &mock.uri(),
        &offers_tool(),
        serde_json::Map::new(),
    )
    .await
    .expect("single 401 must be retried and succeed");
    assert_eq!(out, "[]");

    let reqs = mock.received_requests().await.expect("recorded requests");
    let api_reqs: Vec<_> = reqs
        .iter()
        .filter(|r| r.url.path() == "/sale/offers")
        .collect();
    assert_eq!(
        api_reqs.len(),
        2,
        "the API must be hit twice (401 then 200)"
    );
    assert_eq!(
        reqs.iter()
            .filter(|r| r.url.path() == "/auth/oauth/token")
            .count(),
        1,
        "the 401 must be recovered through the refresh grant (exactly one token call)"
    );

    // The refresh grant ran with the OLD refresh token.
    let refresh_req = reqs
        .iter()
        .find(|r| r.url.path() == "/auth/oauth/token")
        .expect("the refresh grant call");
    let refresh_body = String::from_utf8_lossy(&refresh_req.body);
    assert!(
        refresh_body.contains("grant_type=refresh_token")
            && refresh_body.contains("refresh_token=old-refresh"),
        "the 401 recovery must run the refresh grant with the stored token, got: {refresh_body}"
    );

    // The first attempt carried the stored token, the retry the NEW token.
    let authz = |r: &wiremock::Request| {
        r.headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(
        authz(api_reqs[0]),
        "Bearer stale-access",
        "the first attempt carries the stored token"
    );
    assert_eq!(
        authz(api_reqs[1]),
        "Bearer new-access",
        "the retry must carry the refreshed token"
    );

    // The rotated pair is persisted (old refresh token gone).
    let stored = store_in(dir.path()).load().expect("load store");
    let saved = stored.tokens.expect("tokens persisted");
    assert_eq!(saved.access_token, "new-access");
    assert_eq!(saved.refresh_token.as_deref(), Some("new-refresh"));
}

/// A *definitively* rejected forced refresh (revoked/expired authorization)
/// must surface the re-auth guidance — `ReauthRequired` naming
/// `allegro-mcp auth device` — instead of silently retrying with the dead
/// token, and wipe the dead pair so the next run starts clean.
#[tokio::test]
async fn dispatcher_401_with_rejected_refresh_surfaces_reauth_guidance() {
    let mock = MockServer::start().await;

    // Token endpoint: the forced refresh is definitively rejected.
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(error_template(400, "invalid_grant"))
        .mount(&mock)
        .await;
    // API: always 401 — but the retry must never happen: re-resolution
    // fails first.
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(401).set_body_string("revoked"))
        .mount(&mock)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_in(dir.path());
    seed_live_pair(&store, "dead-access", "dead-refresh");

    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri())
        .with_token_store(store_in(dir.path()));

    let err = dispatch_with_base(
        &auth,
        &reqwest::Client::new(),
        &mock.uri(),
        &offers_tool(),
        serde_json::Map::new(),
    )
    .await
    .expect_err("a definitively rejected refresh must fail the dispatch");
    assert!(
        err.contains("auth error"),
        "auth failures surface with the 'auth error' prefix, got: {err}"
    );
    assert!(
        err.contains("re-authorization required"),
        "the ReauthRequired guidance must be surfaced, got: {err}"
    );
    assert!(
        err.contains("refresh token rejected"),
        "the reason must say why, got: {err}"
    );
    assert!(
        err.contains("allegro-mcp auth device"),
        "the guidance must name the CLI command, got: {err}"
    );

    // The dead pair was wiped — and no second API call was made with the
    // dead token.
    assert!(
        !store.path().exists(),
        "a definitively rejected refresh must clear the store"
    );
    let reqs = mock.received_requests().await.expect("recorded requests");
    assert_eq!(
        reqs.iter()
            .filter(|r| r.url.path() == "/sale/offers")
            .count(),
        1,
        "no retry with the dead token — re-resolution fails first"
    );
    assert_eq!(
        reqs.iter()
            .filter(|r| r.url.path() == "/auth/oauth/token")
            .count(),
        1,
        "exactly one refresh attempt"
    );
}

/// A *transient* forced-refresh failure (5xx) must neither fail the
/// dispatch nor wipe the store: the stored access token is the fallback
/// for the single retry.
#[tokio::test]
async fn dispatcher_401_with_transient_refresh_failure_falls_back_to_stored_token() {
    let mock = MockServer::start().await;

    // Token endpoint: the forced refresh fails transiently — every time
    // (so only the stored-token fallback can recover).
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(500).set_body_string("auth server hiccup"))
        .mount(&mock)
        .await;
    // API: 401 exactly once, then 200 (limited mock mounted FIRST).
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(401).set_body_string("stale token"))
        .up_to_n_times(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&mock)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_in(dir.path());
    seed_live_pair(&store, "stale-access", "fallback-refresh");

    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri())
        .with_token_store(store_in(dir.path()));

    let out = dispatch_with_base(
        &auth,
        &reqwest::Client::new(),
        &mock.uri(),
        &offers_tool(),
        serde_json::Map::new(),
    )
    .await
    .expect("transient refresh failure must fall back to the stored token");
    assert_eq!(out, "[]");

    // The store survived the transient failure (pair intact, not wiped).
    let stored = store_in(dir.path()).load().expect("load store");
    let saved = stored.tokens.expect("pair kept on a transient failure");
    assert_eq!(saved.access_token, "stale-access");
    assert_eq!(saved.refresh_token.as_deref(), Some("fallback-refresh"));

    // The retried request carried the stored access token.
    let reqs = mock.received_requests().await.expect("recorded requests");
    let api_reqs: Vec<_> = reqs
        .iter()
        .filter(|r| r.url.path() == "/sale/offers")
        .collect();
    assert_eq!(api_reqs.len(), 2);
    let retried_authz = api_reqs[1]
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        retried_authz, "Bearer stale-access",
        "the fallback must retry with the stored token"
    );
}

/// The stale-store guard: after a definitive rejection of *our* refresh
/// token, a concurrently rotated file is retried with the newer token —
/// but a *transient* failure of that retry must keep the newer pair on
/// disk (only a definitive rejection may clear the store).
#[tokio::test]
async fn stale_store_guard_transient_retry_failure_keeps_the_newer_pair() {
    let mock = MockServer::start().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_in(dir.path());
    seed_live_pair(&store, "a1", "r1");

    // The mock itself performs the concurrent rotation, so it lands
    // strictly *between* our store load and the refresh response — exactly
    // the window the guard exists for: call #1 rejects our token
    // (`invalid_grant`) and rotates the file to (b1, r2), what a racing
    // `allegro-mcp auth device` run does; call #2 (the guard's retry with
    // r2) hits a 5xx.
    let rotated = Arc::new(AtomicBool::new(false));
    let rotating_store = store.clone();
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(move |_req: &wiremock::Request| {
            if !rotated.swap(true, Ordering::SeqCst) {
                rotating_store
                    .save_tokens(
                        &TokenResponse {
                            access_token: "b1".to_owned(),
                            expires_in: 3600,
                            token_type: Some("bearer".to_owned()),
                            refresh_token: Some("r2".to_owned()),
                            scope: Some("allegro:api:read".to_owned()),
                            jti: None,
                        },
                        3600,
                    )
                    .expect("concurrent rotation");
                error_template(400, "invalid_grant")
            } else {
                ResponseTemplate::new(500).set_body_string("hiccup")
            }
        })
        .mount(&mock)
        .await;

    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri())
        .with_token_store(store_in(dir.path()));

    // Forced re-resolution: r1 is rejected → guard reloads → r2 → 5xx.
    let err = auth
        .refresh_now()
        .await
        .expect_err("a transient retry failure must propagate");
    assert!(
        matches!(err, AuthError::Http(_)),
        "the transient error must propagate as-is (not be swallowed into a store wipe), got: {err:?}"
    );

    // The newer pair survives on disk.
    let stored = store_in(dir.path()).load().expect("store readable");
    let saved = stored.tokens.expect("the newer pair must be kept");
    assert_eq!(saved.access_token, "b1");
    assert_eq!(saved.refresh_token.as_deref(), Some("r2"));
}

// ── 9. Device authorization request shape ────────────────────────────────────

/// The `POST /auth/oauth/device` contract: Basic auth header, `client_id`
/// in the form body, and the optional space-joined `scope` param **only**
/// when scopes are configured — an empty scope list must keep the body
/// scope-less (byte-identical to the scope-less flow, mirroring the token
/// endpoint's convention). All other device-flow tests exercise the empty
/// scope branch only, so without this test the scope param and the device
/// endpoint's Basic auth were never asserted anywhere.
#[tokio::test]
async fn device_code_request_carries_basic_auth_and_optional_scope() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/oauth/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(device_json()))
        .mount(&mock)
        .await;

    let mut deps = deps_for(&mock, PollingPolicy::test_instant());
    request_device_code(&deps)
        .await
        .expect("scope-less device code request");

    deps.scopes = vec![
        "allegro:api:read".to_owned(),
        "allegro:api:write".to_owned(),
    ];
    request_device_code(&deps)
        .await
        .expect("scoped device code request");

    let reqs = mock.received_requests().await.expect("recorded requests");
    assert_eq!(reqs.len(), 2, "exactly one request per call");
    let (scopeless, scoped) = (&reqs[0], &reqs[1]);

    // Basic auth on both (the credentials ride the header, not the body).
    for req in reqs.iter() {
        let authz = req
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            authz.starts_with("Basic "),
            "the device endpoint must use Basic auth, got: {authz:?}"
        );
    }

    let scopeless_body = String::from_utf8_lossy(&scopeless.body);
    assert!(
        scopeless_body.contains("client_id=id"),
        "client_id rides the form body, got: {scopeless_body}"
    );
    assert!(
        !scopeless_body.contains("scope="),
        "an empty scope list must keep the body scope-less, got: {scopeless_body}"
    );

    let scoped_body = String::from_utf8_lossy(&scoped.body);
    assert!(
        scoped_body.starts_with("client_id=id&scope="),
        "the scope param is appended after client_id, got: {scoped_body}"
    );
    assert!(
        scoped_body.contains("allegro%3Aapi%3Aread")
            && scoped_body.contains("allegro%3Aapi%3Awrite"),
        "both scopes must be URL-encoded in the form body, got: {scoped_body}"
    );
}
