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

/// In device mode the 401 retry re-resolves from the *store* (never a
/// client_credentials fallback): invalidate the cache → `token()` re-reads
/// the persisted pair → rebuild the request → succeed. The token endpoint
/// is therefore never contacted.
#[tokio::test]
async fn dispatcher_retries_once_on_401() {
    let mock = MockServer::start().await;

    // Token endpoint must stay silent — device mode never falls back to it.
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(500).set_body_string("must never be hit"))
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
    // Seed the restored session the server would have loaded at startup.
    let envelope = json!({
        "version": 1,
        "env": "production",
        "tokens": {
            "access_token": "tok",
            "refresh_token": "rfr",
            "expires_at_epoch": epoch_now() + 3600,
            "scope": "allegro:api:read",
            "updated_at_epoch": epoch_now()
        }
    });
    std::fs::write(store.path(), envelope.to_string()).expect("seed live pair");

    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri())
        .with_token_store(store_in(dir.path()));

    let tool_def = ToolDef {
        id: "allegro_get_offers".to_owned(),
        name: "allegro_get_offers".to_owned(),
        description: "List offers".to_owned(),
        input_schema: json!({}),
        method: "get".to_owned(),
        path: "/sale/offers".to_owned(),
        accept_media_type: None,
    };

    let out = dispatch_with_base(
        &auth,
        &reqwest::Client::new(),
        &mock.uri(),
        &tool_def,
        serde_json::Map::new(),
    )
    .await
    .expect("single 401 must be retried and succeed");
    assert_eq!(out, "[]");

    let reqs = mock.received_requests().await.expect("recorded requests");
    assert_eq!(
        reqs.iter()
            .filter(|r| r.url.path() == "/sale/offers")
            .count(),
        2,
        "the API must be hit twice (401 then 200)"
    );
    assert_eq!(
        reqs.iter()
            .filter(|r| r.url.path() == "/auth/oauth/token")
            .count(),
        0,
        "device-mode re-resolve reads the store, never the token endpoint"
    );
}
