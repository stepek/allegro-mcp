//! Integration tests for the Phase 9 resilience layer: 429 backoff,
//! Retry-After/Trace-Id handling, 5xx single-retry method gating, error
//! body mapping (Allegro + OAuth), the client-side rate budget, network
//! failure copy, and the 401-refresh interaction — end-to-end through the
//! public `allegro_mcp` API surface.
//!
//! Conventions (mirroring `tests/device_flow_integration.rs`):
//! - every test gets its own wiremock server — no shared mock state;
//! - [`Resilience::test_instant`] zeroes the backoff sleeps so tests don't
//!   real-sleep (the backoff math itself is unit-covered in
//!   `src/resilience.rs`);
//! - wiremock checks equal-priority mocks in **registration order** and an
//!   exhausted `up_to_n_times` mock is skipped — so the *limited* mock is
//!   always mounted first and the fallback last.
//!
//! No `serial_test` needed in this file: no env mutation happens here
//! (`AllegroAuth::with_base_url` injects the auth host directly).

use std::sync::Arc;
use std::time::Duration;

use allegro_mcp::auth::AllegroAuth;
use allegro_mcp::dispatcher::dispatch_with_resilience;
use allegro_mcp::resilience::{BackoffPolicy, RateBudget, Resilience};
use allegro_mcp::server::AllegroServer;
use allegro_mcp::tool_registry::{ToolDef, ToolRegistry};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// A minimal GET tool hitting `/sale/offers`.
fn get_offers_tool() -> ToolDef {
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

async fn mount_token_ok(mock: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "tok", "token_type": "bearer", "expires_in": 43199
        })))
        .mount(mock)
        .await;
}

async fn dispatch(mock: &MockServer, tool: &ToolDef, res: &Resilience) -> Result<String, String> {
    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri());
    dispatch_with_resilience(
        &auth,
        &reqwest::Client::new(),
        &mock.uri(),
        tool,
        serde_json::Map::new(),
        res,
    )
    .await
    .map_err(|e| e.report())
}

async fn count_hits(mock: &MockServer, path_fragment: &str) -> usize {
    mock.received_requests()
        .await
        .expect("recorded requests")
        .iter()
        .filter(|r| r.url.path() == path_fragment)
        .count()
}

// ── Dispatch-level suite ──────────────────────────────────────────────────────

/// 429 → backoff → 429 → backoff → 200: the acceptance path. Three API
/// hits total (two rate-limited sends + the successful one) and a single
/// token fetch (the token caches for 12 h).
#[tokio::test]
async fn dispatch_429_backoff_then_success() {
    let mock = MockServer::start().await;
    mount_token_ok(&mock).await;

    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .insert_header("Trace-Id", "tr-429"),
        )
        .up_to_n_times(2)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&mock)
        .await;

    let out = dispatch(&mock, &get_offers_tool(), &Resilience::test_instant())
        .await
        .expect("429s must be absorbed by backoff");
    assert_eq!(out, "[]");

    assert_eq!(count_hits(&mock, "/sale/offers").await, 3);
    assert_eq!(count_hits(&mock, "/auth/oauth/token").await, 1);
}

/// Persistent 429 fails cleanly after the retry budget with a structured
/// report: status, Trace-Id, Retry-After, and actionable user copy.
#[tokio::test]
async fn dispatch_persistent_429_clean_error_with_trace_id() {
    let mock = MockServer::start().await;
    mount_token_ok(&mock).await;

    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .insert_header("Trace-Id", "tr-persist"),
        )
        .mount(&mock)
        .await;

    let err = dispatch(&mock, &get_offers_tool(), &Resilience::test_instant())
        .await
        .expect_err("persistent 429 must fail after 3 retries");
    assert!(err.contains("429"), "got: {err}");
    assert!(err.contains("Trace-Id: tr-persist"), "got: {err}");
    assert!(err.contains("Retry-After:"), "got: {err}");
    assert!(
        err.contains("limiting how often"),
        "actionable user copy must be present, got: {err}"
    );

    assert_eq!(
        count_hits(&mock, "/sale/offers").await,
        4,
        "initial send + 3 retries"
    );
}

/// The Phase 5 acceptance path, preserved inside the Phase 9 loop: a
/// single 401 triggers exactly one forced token re-resolution and the
/// re-send wins.
#[tokio::test]
async fn dispatch_401_refresh_retry_from_phase5() {
    let mock = MockServer::start().await;
    mount_token_ok(&mock).await;

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

    let out = dispatch(&mock, &get_offers_tool(), &Resilience::test_instant())
        .await
        .expect("single 401 must be recovered by refresh + re-send");
    assert_eq!(out, "[]");

    assert_eq!(count_hits(&mock, "/sale/offers").await, 2);
    assert_eq!(
        count_hits(&mock, "/auth/oauth/token").await,
        2,
        "initial fetch + the forced re-resolution"
    );
}

/// A single 5xx on an idempotent method is retried once and succeeds.
#[tokio::test]
async fn dispatch_5xx_single_retry_then_success() {
    let mock = MockServer::start().await;
    mount_token_ok(&mock).await;

    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&mock)
        .await;

    let out = dispatch(&mock, &get_offers_tool(), &Resilience::test_instant())
        .await
        .expect("a single 5xx on a GET must be retried");
    assert_eq!(out, "[]");
    assert_eq!(count_hits(&mock, "/sale/offers").await, 2);
}

/// Persistent 5xx: exactly one retry, then a structured report with the
/// status and the Trace-Id.
#[tokio::test]
async fn dispatch_persistent_5xx_clean_error() {
    let mock = MockServer::start().await;
    mount_token_ok(&mock).await;

    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(503).insert_header("Trace-Id", "tr-5xx"))
        .mount(&mock)
        .await;

    let err = dispatch(&mock, &get_offers_tool(), &Resilience::test_instant())
        .await
        .expect_err("persistent 5xx must fail after a single retry");
    assert!(err.contains("503"), "got: {err}");
    assert!(err.contains("Trace-Id: tr-5xx"), "got: {err}");
    assert_eq!(count_hits(&mock, "/sale/offers").await, 2);
}

/// Transport failure (nothing listening) maps to the clear "Allegro could
/// not be reached" copy — reqwest's raw "error sending request" says
/// nothing about reachability to an end user.
#[tokio::test]
async fn dispatch_network_error_unreachable_message() {
    let mock = MockServer::start().await;
    mount_token_ok(&mock).await;

    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri());
    // Port 9 (discard) on loopback: connection refused — no wiremock
    // server answers, and network errors are never retried.
    let err = dispatch_with_resilience(
        &auth,
        &reqwest::Client::new(),
        "http://127.0.0.1:9",
        &get_offers_tool(),
        serde_json::Map::new(),
        &Resilience::test_instant(),
    )
    .await
    .map_err(|e| e.report())
    .expect_err("an unreachable API must fail");
    assert!(
        err.contains("Allegro unreachable"),
        "the report header must name the failure mode (ticket wording), got: {err}"
    );
    assert!(
        err.contains("could not be reached"),
        "user-facing reachability copy, got: {err}"
    );
}

/// OAuth error body from the token endpoint is mapped dev+user: the
/// `error` code and `error_description` both land in the report.
#[tokio::test]
async fn dispatch_maps_oauth_error_body() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_client",
            "error_description": "Client authentication failed"
        })))
        .mount(&mock)
        .await;

    let err = dispatch(&mock, &get_offers_tool(), &Resilience::test_instant())
        .await
        .expect_err("a token-endpoint 400 must fail the dispatch");
    assert!(err.contains("invalid_client"), "got: {err}");
    assert!(err.contains("Client authentication failed"), "got: {err}");
    assert!(
        err.contains("auth error"),
        "the historical prefix is kept, got: {err}"
    );
}

/// A token-mint 429 is NEVER auto-retried: exactly two token requests (the
/// initial fetch + the single Phase 5 forced re-resolution after the API
/// 401) and the "token churn too high" actionable error.
#[tokio::test]
async fn dispatch_token_mint_429_not_retried() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "tok", "token_type": "bearer", "expires_in": 43199
        })))
        .up_to_n_times(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .insert_header("Trace-Id", "tr-mint"),
        )
        .mount(&mock)
        .await;
    // API: always 401, so the forced re-resolution runs.
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&mock)
        .await;

    let err = dispatch(&mock, &get_offers_tool(), &Resilience::test_instant())
        .await
        .expect_err("a mint-side 429 must surface, not loop");
    assert!(err.contains("token churn"), "got: {err}");

    assert_eq!(
        count_hits(&mock, "/auth/oauth/token").await,
        2,
        "initial fetch + the single forced re-resolution — no retry loop"
    );
}

/// Allegro's `{"errors":[…]}` body maps into the report: the Dev line
/// carries the code + raw body, the User line carries Allegro's localized
/// `userMessage`, and the Trace-Id line is present.
#[tokio::test]
async fn dispatch_maps_allegro_error_body_dev_and_user() {
    let mock = MockServer::start().await;
    mount_token_ok(&mock).await;

    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(
            ResponseTemplate::new(422)
                .insert_header("Trace-Id", "tr-422")
                .set_body_json(json!({
                    "errors": [{
                        "message": "Delivery point data not passed",
                        "code": "MissingDeliveryPointException",
                        "details": null,
                        "path": "Endpoint.getDeliveries.arg1",
                        "userMessage": "Nie wybrano punktu dla odbioru osobistego."
                    }]
                })),
        )
        .mount(&mock)
        .await;

    let err = dispatch(&mock, &get_offers_tool(), &Resilience::test_instant())
        .await
        .expect_err("a 422 must fail the dispatch");
    assert!(
        err.contains("MissingDeliveryPointException"),
        "Dev line must carry the code, got: {err}"
    );
    assert!(
        err.contains("Delivery point data not passed"),
        "Dev line must carry the raw body, got: {err}"
    );
    assert!(
        err.contains("Nie wybrano punktu"),
        "User line must prefer the localized userMessage, got: {err}"
    );
    assert!(err.contains("Trace-Id: tr-422"), "got: {err}");
}

/// The client-side budget guard: a cap-1 budget lets the first dispatch
/// through and blocks the second before any send.
#[tokio::test]
async fn rate_budget_guard_trips_second_call() {
    let mock = MockServer::start().await;
    mount_token_ok(&mock).await;

    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&mock)
        .await;

    let res = Resilience {
        backoff: BackoffPolicy::test_instant(),
        budget: Some(Arc::new(RateBudget::new_for_tests(1, Duration::ZERO))),
    };

    let first = dispatch(&mock, &get_offers_tool(), &res).await;
    assert_eq!(first.expect("the first dispatch fits the cap"), "[]");

    let err = dispatch(&mock, &get_offers_tool(), &res)
        .await
        .expect_err("the second dispatch exceeds the cap");
    assert!(
        err.contains("rate budget"),
        "the budget error must be identifiable, got: {err}"
    );
    assert_eq!(
        count_hits(&mock, "/sale/offers").await,
        1,
        "the blocked request was never sent"
    );
}

// ── MCP-level suite (duplex JSON-RPC, same shape as mcp_server_integration) ──

type Client = BufReader<DuplexStream>;

async fn send_json(client: &mut Client, value: Value) {
    let mut line = serde_json::to_vec(&value).expect("serialize request");
    line.push(b'\n');
    client.write_all(&line).await.expect("write request");
    client.flush().await.expect("flush request");
}

async fn recv_json(client: &mut Client) -> Value {
    let mut line = String::new();
    client.read_line(&mut line).await.expect("read response");
    serde_json::from_str(line.trim_end()).expect("parse response JSON")
}

async fn initialize(client: &mut Client) {
    send_json(
        client,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.0.1" }
            }
        }),
    )
    .await;
    let _init_response = recv_json(client).await;
    send_json(
        client,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
}

fn spawn_server(handler: AllegroServer) -> (Client, tokio::task::JoinHandle<()>) {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server_handle = tokio::spawn(async move {
        let running = handler.serve(server_transport).await.expect("serve failed");
        let _ = running.waiting().await;
    });
    (BufReader::new(client_transport), server_handle)
}

/// A one-tool registry with the `GET /sale/offers` operation, so MCP-level
/// `tools/call` requests can name the tool deterministically.
fn registry_with_offers_tool() -> ToolRegistry {
    let api: openapiv3::OpenAPI = serde_yaml::from_str(concat!(
        "openapi: \"3.0.3\"\n",
        "info:\n  title: t\n  version: v\n",
        "paths:\n",
        "  /sale/offers:\n",
        "    get:\n",
        "      operationId: getListingOffers\n",
        "      summary: List offers\n",
        "      responses:\n",
        "        \"200\":\n          description: OK\n",
    ))
    .expect("parse inline schema");
    ToolRegistry::from_openapi(&api).expect("build registry")
}

async fn call_tool(client: &mut Client, id: i64) -> Value {
    send_json(
        client,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "allegro_getlistingoffers", "arguments": {} }
        }),
    )
    .await;
    recv_json(client).await
}

/// End-to-end MCP surface: a persistent 429 comes back as a tool-level
/// error (`isError: true` on the wire) whose text block carries the
/// structured report — Trace-Id included.
#[tokio::test]
async fn mcp_call_tool_429_persists_returns_structured_error() {
    let mock = MockServer::start().await;
    mount_token_ok(&mock).await;

    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .insert_header("Trace-Id", "tr-persist"),
        )
        .mount(&mock)
        .await;

    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri());
    let handler = AllegroServer::new(registry_with_offers_tool(), auth, false)
        .with_api_base_url(mock.uri())
        .with_resilience(Resilience::test_instant());

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    let response = call_tool(&mut client, 2).await;
    assert!(
        response["error"].is_null(),
        "a tool error is a result, not a JSON-RPC error, got: {response}"
    );
    assert_eq!(
        response["result"]["isError"],
        json!(true),
        "persistent 429 must set isError=true, got: {response}"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        text.contains("Trace-Id: tr-persist"),
        "the MCP tool-error surface must carry the Trace-Id end-to-end, got: {text}"
    );
    assert!(text.contains("429"), "got: {text}");

    server_handle.abort();
}

/// Error recovery at the session level: after a 429-absorbing call, the
/// next call still succeeds on the same session (and the in-process
/// backoff actually recovered the first call).
#[tokio::test]
async fn mcp_call_tool_429_recovery_still_serves_next_call() {
    let mock = MockServer::start().await;
    mount_token_ok(&mock).await;

    // 429 exactly once, then 200 forever (limited mock mounted FIRST).
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .insert_header("Trace-Id", "tr-429"),
        )
        .up_to_n_times(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"offers":[]}"#))
        .mount(&mock)
        .await;

    let auth = AllegroAuth::with_base_url("id".to_owned(), "secret".to_owned(), mock.uri());
    let handler = AllegroServer::new(registry_with_offers_tool(), auth, false)
        .with_api_base_url(mock.uri())
        .with_resilience(Resilience::test_instant());

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    // First call: 429 → in-process backoff → 200 → Ok.
    let first = call_tool(&mut client, 2).await;
    assert_eq!(
        first["result"]["isError"],
        json!(false),
        "the first call must recover via backoff, got: {first}"
    );

    // Second call on the same session: plain 200 → Ok.
    let second = call_tool(&mut client, 3).await;
    assert_eq!(
        second["result"]["isError"],
        json!(false),
        "the session must keep serving after a recovered 429, got: {second}"
    );

    server_handle.abort();
}
