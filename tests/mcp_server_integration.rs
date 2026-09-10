//! Integration tests for the MCP server handler, driven over an in-process
//! `tokio::io::duplex` transport using raw newline-delimited JSON-RPC (the
//! same wire format `rmcp`'s stdio transport uses). This avoids depending on
//! the `client` feature of `rmcp`, which is not enabled in this crate.

use std::path::PathBuf;

use allegro_mcp::auth::AllegroAuth;
use allegro_mcp::http;
use allegro_mcp::schema::{self, SchemaSource};
use allegro_mcp::server::AllegroServer;
use allegro_mcp::tool_registry::ToolRegistry;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// `openapiv3` and `serde_yaml` are regular (non-dev) dependencies of the
// library crate, so they are available here without being listed separately
// in [dev-dependencies].
use openapiv3::OpenAPI;

/// The client side of the in-process transport: a single `BufReader`
/// wrapping the raw duplex stream, used for both reading and writing (no
/// concurrent access is needed since the test drives request/response
/// pairs sequentially).
type Client = BufReader<DuplexStream>;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

async fn build_registry() -> ToolRegistry {
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _raw) = schema::load(&source).await.expect("load fixture failed");
    ToolRegistry::from_openapi(&api).expect("build registry failed")
}

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

/// Performs the MCP `initialize` handshake, consuming its response and
/// sending the `notifications/initialized` notification.
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

/// Spawns the `AllegroServer` over an in-process duplex transport and
/// returns the client-side handle plus the server task's `JoinHandle`.
fn spawn_server(handler: AllegroServer) -> (Client, tokio::task::JoinHandle<()>) {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server_handle = tokio::spawn(async move {
        let running = handler.serve(server_transport).await.expect("serve failed");
        let _ = running.waiting().await;
    });
    (BufReader::new(client_transport), server_handle)
}

#[tokio::test]
async fn test_list_tools_returns_registry_tools() {
    let registry = build_registry().await;
    let expected_len = registry.len();
    let auth = AllegroAuth::new("test-id".to_string(), "test-secret".to_string(), false);
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    let response = recv_json(&mut client).await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("expected tools array in response");
    assert_eq!(
        tools.len(),
        expected_len,
        "tools/list must return all registry tools"
    );

    server_handle.abort();
}

#[tokio::test]
async fn test_call_tool_unknown_name_returns_method_not_found() {
    let registry = build_registry().await;
    let auth = AllegroAuth::new("test-id".to_string(), "test-secret".to_string(), false);
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "allegro_does_not_exist", "arguments": {} }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;
    assert_eq!(
        response["error"]["code"], -32601,
        "unknown tool must return METHOD_NOT_FOUND (-32601), got: {response}"
    );

    server_handle.abort();
}

#[tokio::test]
async fn test_call_tool_error_does_not_crash_session() {
    // Mock the Allegro OAuth token endpoint to always fail, so the
    // dispatcher's `auth.token()` call errors out before any real network
    // request is made — this deterministically exercises the tool-error
    // path without needing real credentials or network access.
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock_server)
        .await;

    let registry = build_registry().await;
    let tool_id = registry
        .list_tools()
        .first()
        .expect("fixture must have at least one tool")
        .id
        .clone();
    let auth = AllegroAuth::with_base_url(
        "test-id".to_string(),
        "test-secret".to_string(),
        mock_server.uri(),
    );
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": tool_id, "arguments": {} }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;
    assert_eq!(
        response["result"]["isError"], true,
        "HTTP/auth failure must surface as a tool-level error, got: {response}"
    );

    // The session must survive the tool error: a subsequent request must
    // still succeed.
    send_json(
        &mut client,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
    )
    .await;
    let response = recv_json(&mut client).await;
    assert!(
        response["result"]["tools"].is_array(),
        "session must remain usable after a tool-level error, got: {response}"
    );

    server_handle.abort();
}

// ── Additional integration tests ──────────────────────────────────────────────

/// `initialize` response must identify the server as "allegro-mcp".
#[tokio::test]
async fn test_initialize_response_identifies_server() {
    let registry = build_registry().await;
    let auth = AllegroAuth::new("test-id".to_string(), "test-secret".to_string(), false);
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);

    send_json(
        &mut client,
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
    let response = recv_json(&mut client).await;

    let server_name = response["result"]["serverInfo"]["name"]
        .as_str()
        .unwrap_or("");
    assert_eq!(
        server_name, "allegro-mcp",
        "initialize response must identify server as 'allegro-mcp', got: {response}"
    );

    server_handle.abort();
}

/// `initialize` response must advertise the `tools` capability.
#[tokio::test]
async fn test_initialize_response_advertises_tools_capability() {
    let registry = build_registry().await;
    let auth = AllegroAuth::new("test-id".to_string(), "test-secret".to_string(), false);
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);

    send_json(
        &mut client,
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
    let response = recv_json(&mut client).await;

    assert!(
        !response["result"]["capabilities"]["tools"].is_null(),
        "initialize response must advertise tools capability, got: {response}"
    );

    server_handle.abort();
}

/// `tools/list` on an empty registry must return an empty tools array.
#[tokio::test]
async fn test_list_tools_empty_registry_returns_empty_array() {
    // Build an empty registry from an inline schema with no paths.
    let api: OpenAPI =
        serde_yaml::from_str("openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n")
            .expect("parse inline schema");
    let registry = ToolRegistry::from_openapi(&api).expect("build registry failed");
    assert_eq!(registry.len(), 0, "empty schema must produce 0 tools");

    let auth = AllegroAuth::new("test-id".to_string(), "test-secret".to_string(), false);
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    let response = recv_json(&mut client).await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("expected tools array in response");
    assert_eq!(
        tools.len(),
        0,
        "tools/list on empty registry must return empty array"
    );

    server_handle.abort();
}

/// Each tool in `tools/list` must have a non-empty name and description.
#[tokio::test]
async fn test_list_tools_each_tool_has_name_and_description() {
    let registry = build_registry().await;
    let auth = AllegroAuth::new("test-id".to_string(), "test-secret".to_string(), false);
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    let response = recv_json(&mut client).await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("expected tools array in response");

    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("");
        let description = tool["description"].as_str().unwrap_or("");
        assert!(
            !name.is_empty(),
            "every tool must have a non-empty name, got: {tool}"
        );
        assert!(
            !description.is_empty(),
            "every tool must have a non-empty description, got: {tool}"
        );
    }

    server_handle.abort();
}

/// GET tools in `tools/list` must have `readOnlyHint = true`.
#[tokio::test]
async fn test_list_tools_get_tools_have_read_only_hint() {
    let registry = build_registry().await;
    let auth = AllegroAuth::new("test-id".to_string(), "test-secret".to_string(), false);
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    let response = recv_json(&mut client).await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("expected tools array in response");

    // The fixture has GET tools — verify at least one has readOnlyHint=true.
    let read_only_tools: Vec<&Value> = tools
        .iter()
        .filter(|t| t["annotations"]["readOnlyHint"] == json!(true))
        .collect();
    assert!(
        !read_only_tools.is_empty(),
        "at least one GET tool must have readOnlyHint=true in tools/list"
    );

    server_handle.abort();
}

/// `tools/call` with a tool error must return a result (not a JSON-RPC error)
/// with `isError=true` and a non-empty content array containing the error text.
///
/// NOTE: `AllegroServer::dispatch` hardcodes the Allegro API base URL, so it
/// is not possible to intercept the API-level HTTP call with wiremock in
/// integration tests. This test exercises the tool-error response shape by
/// triggering an auth failure (which fires before any API call), verifying
/// that the MCP response envelope is correct regardless of the error source.
#[tokio::test]
async fn test_call_tool_error_response_has_content_array() {
    let mock_server = MockServer::start().await;

    // Token endpoint — always fail so dispatch returns an auth error.
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock_server)
        .await;

    let registry = build_registry().await;
    let tool_name = registry
        .list_tools()
        .first()
        .expect("fixture must have at least one tool")
        .name
        .clone();

    let auth = AllegroAuth::with_base_url(
        "test-id".to_string(),
        "test-secret".to_string(),
        mock_server.uri(),
    );
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": tool_name, "arguments": {} }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;

    // Tool errors must be returned as a result (not a JSON-RPC error).
    assert!(
        response["error"].is_null(),
        "tool error must not be a JSON-RPC error, got: {response}"
    );
    assert_eq!(
        response["result"]["isError"],
        json!(true),
        "tool error must set isError=true, got: {response}"
    );
    // The content array must be present and non-empty.
    let content = response["result"]["content"]
        .as_array()
        .expect("tool error response must have content array");
    assert!(
        !content.is_empty(),
        "tool error response must have non-empty content array"
    );
    // The first content item must be a text block with the error message.
    assert_eq!(
        content[0]["type"].as_str().unwrap_or(""),
        "text",
        "tool error content must be a text block"
    );
    let text = content[0]["text"].as_str().unwrap_or("");
    assert!(
        !text.is_empty(),
        "tool error content text must not be empty"
    );

    server_handle.abort();
}

/// `tools/call` with no `arguments` field (omitted) must not crash and must
/// return METHOD_NOT_FOUND for an unknown tool name.
#[tokio::test]
async fn test_call_tool_null_arguments_returns_error_not_crash() {
    let registry = build_registry().await;
    let auth = AllegroAuth::new("test-id".to_string(), "test-secret".to_string(), false);
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    // Send a tools/call with no "arguments" field at all.
    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "allegro_does_not_exist" }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;

    // Must return METHOD_NOT_FOUND, not crash.
    assert_eq!(
        response["error"]["code"], -32601,
        "call with no arguments field must return METHOD_NOT_FOUND for unknown tool, got: {response}"
    );

    server_handle.abort();
}

/// `tools/call` error message must include the unknown tool name.
#[tokio::test]
async fn test_call_tool_unknown_name_error_message_includes_tool_name() {
    let registry = build_registry().await;
    let auth = AllegroAuth::new("test-id".to_string(), "test-secret".to_string(), false);
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "allegro_nonexistent_tool_xyz", "arguments": {} }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;

    let error_message = response["error"]["message"].as_str().unwrap_or("");
    assert!(
        error_message.contains("allegro_nonexistent_tool_xyz"),
        "error message must include the unknown tool name, got: {response}"
    );

    server_handle.abort();
}

// ── Issue #3: write-method readOnlyHint=false assertion ──────────────────────

/// POST/PUT/DELETE tools must have `readOnlyHint=false` in `tools/list`.
/// The fixture has `createOffer` (POST), `updateOffer` (PUT), `deleteOffer`
/// (DELETE) — all must be non-read-only.
///
/// Tool names are derived from the registry (not hard-coded) to stay robust
/// against sanitization changes in `builder.rs`.
#[tokio::test]
async fn test_list_tools_write_methods_have_no_read_only_hint() {
    let registry = build_registry().await;

    // Collect the names of all write-method tools from the registry so we can
    // look them up in the wire response without hard-coding sanitized names.
    let write_tool_names: std::collections::HashSet<String> = registry
        .list_tools()
        .iter()
        .filter(|t| matches!(t.method.as_str(), "post" | "put" | "delete" | "patch"))
        .map(|t| t.name.clone())
        .collect();

    assert!(
        !write_tool_names.is_empty(),
        "fixture must contain at least one write tool (POST/PUT/DELETE)"
    );

    let auth = AllegroAuth::new("test-id".to_string(), "test-secret".to_string(), false);
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    let response = recv_json(&mut client).await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("expected tools array in response");

    // Every write-method tool must have readOnlyHint explicitly set to false.
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("");
        if write_tool_names.contains(name) {
            let hint = &tool["annotations"]["readOnlyHint"];
            assert_eq!(
                hint,
                &json!(false),
                "write tool '{name}' must have readOnlyHint=false (not absent/null), got: {tool}"
            );
        }
    }

    server_handle.abort();
}

// ── Issue #4: known tool called with no `arguments` field ────────────────────

/// A known tool called with no `arguments` field must exercise the
/// `request.arguments.unwrap_or_default()` path in `call_tool` and return a
/// valid MCP response (tool-level error due to auth failure, not a crash).
#[tokio::test]
async fn test_call_known_tool_with_no_arguments_field_does_not_crash() {
    let mock_server = MockServer::start().await;

    // Auth always fails — we just want to confirm the server handles the
    // missing `arguments` field gracefully before reaching the network.
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock_server)
        .await;

    let registry = build_registry().await;
    let tool_name = registry
        .list_tools()
        .first()
        .expect("fixture must have at least one tool")
        .name
        .clone();

    let auth = AllegroAuth::with_base_url(
        "test-id".to_string(),
        "test-secret".to_string(),
        mock_server.uri(),
    );
    let handler = AllegroServer::new(registry, auth, false);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    // No "arguments" key in params — exercises `unwrap_or_default()`.
    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": tool_name }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;

    // Must return a tool-level error result, not a JSON-RPC protocol error,
    // and must not crash the server.
    assert!(
        response["error"].is_null(),
        "known tool with no arguments must not return a JSON-RPC error, got: {response}"
    );
    assert_eq!(
        response["result"]["isError"],
        json!(true),
        "known tool with no arguments must return isError=true (auth failure), got: {response}"
    );

    // Session must still be usable.
    send_json(
        &mut client,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
    )
    .await;
    let list_response = recv_json(&mut client).await;
    assert!(
        list_response["result"]["tools"].is_array(),
        "session must remain usable after call with no arguments, got: {list_response}"
    );

    server_handle.abort();
}

// ── Issue #1: happy-path tools/call with mocked API endpoint ─────────────────

/// `tools/call` with a mocked OAuth token AND a mocked API endpoint must
/// return `isError` absent/false and `content[0].text` containing the mocked
/// response body.
///
/// Uses `AllegroServer::with_api_base_url` to inject the wiremock server URL
/// so that the dispatcher hits the mock instead of the real Allegro API.
/// Both mocks require the full ToS header set (User-Agent, Authorization,
/// Accept, Accept-Language) — any request missing a header falls through to
/// wiremock's unmatched-response and fails the test, the wiremock equivalent
/// of the ticket's tcpdump requirement.
#[tokio::test]
async fn test_call_tool_happy_path_returns_mocked_body() {
    let mock_server = MockServer::start().await;

    // Mock the OAuth token endpoint (UA comes from the shared default client).
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .and(header("Accept-Language", "pl-PL"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "test-token", "expires_in": 3600 })),
        )
        .mount(&mock_server)
        .await;

    // Mock the GET /sale/offers endpoint with full header matchers.
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .and(header("Authorization", "Bearer test-token"))
        .and(header("Accept", "application/vnd.allegro.public.v1+json"))
        .and(header("Accept-Language", "pl-PL"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"offers":[],"count":0}"#))
        .mount(&mock_server)
        .await;

    let registry = build_registry().await;
    // Find the getListingOffers tool (GET /sale/offers).
    let tool_name = registry
        .list_tools()
        .iter()
        .find(|t| t.method == "get" && t.path == "/sale/offers")
        .expect("fixture must have GET /sale/offers tool")
        .name
        .clone();

    let auth = AllegroAuth::with_base_url(
        "test-id".to_string(),
        "test-secret".to_string(),
        mock_server.uri(),
    );
    // Inject the mock server as the API base URL so dispatch hits the mock.
    let handler = AllegroServer::new(registry, auth, false).with_api_base_url(mock_server.uri());

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": tool_name, "arguments": {} }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;

    // Must be a successful result, not an error.
    assert!(
        response["error"].is_null(),
        "happy-path tool call must not return a JSON-RPC error, got: {response}"
    );
    assert_ne!(
        response["result"]["isError"],
        json!(true),
        "happy-path tool call must not set isError=true, got: {response}"
    );

    // content[0].text must contain the mocked response body.
    let content = response["result"]["content"]
        .as_array()
        .expect("successful tool call must have content array");
    assert!(
        !content.is_empty(),
        "successful tool call must have non-empty content"
    );
    let text = content[0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("offers"),
        "content[0].text must contain the mocked response body, got: {text}"
    );

    server_handle.abort();
}

// ── Issue #2: 100 KB truncation integration test ──────────────────────────────

/// A mocked API endpoint returning >100 KB of data must result in
/// `content[0].text` ending with `[truncated]`.
#[tokio::test]
async fn test_call_tool_response_over_100kb_is_truncated() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "test-token", "expires_in": 3600 })),
        )
        .mount(&mock_server)
        .await;

    // Return a body that is clearly over 100 KB (102_400 bytes).
    let large_body = "x".repeat(200_000);
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .and(header("Authorization", "Bearer test-token"))
        .and(header("Accept", "application/vnd.allegro.public.v1+json"))
        .and(header("Accept-Language", "pl-PL"))
        .respond_with(ResponseTemplate::new(200).set_body_string(large_body))
        .mount(&mock_server)
        .await;

    let registry = build_registry().await;
    let tool_name = registry
        .list_tools()
        .iter()
        .find(|t| t.method == "get" && t.path == "/sale/offers")
        .expect("fixture must have GET /sale/offers tool")
        .name
        .clone();

    let auth = AllegroAuth::with_base_url(
        "test-id".to_string(),
        "test-secret".to_string(),
        mock_server.uri(),
    );
    let handler = AllegroServer::new(registry, auth, false).with_api_base_url(mock_server.uri());

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": tool_name, "arguments": {} }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;

    assert!(
        response["error"].is_null(),
        "truncation test must not return a JSON-RPC error, got: {response}"
    );
    assert_ne!(
        response["result"]["isError"],
        json!(true),
        "truncation test must not set isError=true, got: {response}"
    );

    let content = response["result"]["content"]
        .as_array()
        .expect("truncation test must have content array");
    let text = content[0]["text"].as_str().unwrap_or("");
    assert!(
        text.ends_with("[truncated]"),
        "response over 100 KB must end with '[truncated]', got last 50 chars: {:?}",
        &text[text.len().saturating_sub(50)..]
    );

    server_handle.abort();
}

// ── Issue #6: API base URL override ──────────────────────────────────────────

/// `AllegroServer::with_api_base_url` must route all API calls to the
/// injected URL regardless of the `sandbox` flag. Verified by injecting the
/// wiremock server URL and confirming the call reaches the mock and returns
/// the mocked body.
#[tokio::test]
async fn test_call_tool_with_api_base_url_override_succeeds() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "sandbox-token", "expires_in": 3600 })),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .and(header("Authorization", "Bearer sandbox-token"))
        .and(header("Accept", "application/vnd.allegro.public.v1+json"))
        .and(header("Accept-Language", "pl-PL"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"sandbox":true}"#))
        .mount(&mock_server)
        .await;

    let registry = build_registry().await;
    let tool_name = registry
        .list_tools()
        .iter()
        .find(|t| t.method == "get" && t.path == "/sale/offers")
        .expect("fixture must have GET /sale/offers tool")
        .name
        .clone();

    let auth = AllegroAuth::with_base_url(
        "test-id".to_string(),
        "test-secret".to_string(),
        mock_server.uri(),
    );
    // sandbox=true, API base overridden to the mock server — the override
    // must take precedence over the sandbox flag's default URL.
    let handler = AllegroServer::new(registry, auth, true).with_api_base_url(mock_server.uri());

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": tool_name, "arguments": {} }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;

    assert!(
        response["error"].is_null(),
        "tool call with overridden base URL must not return a JSON-RPC error, got: {response}"
    );
    assert_ne!(
        response["result"]["isError"],
        json!(true),
        "tool call with overridden base URL must not set isError=true, got: {response}"
    );
    let content = response["result"]["content"]
        .as_array()
        .expect("tool call with overridden base URL must have content array");
    let text = content[0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("sandbox"),
        "tool call with overridden base URL must return mocked body, got: {text}"
    );

    server_handle.abort();
}

// ── Issue #3 (new): 4xx/5xx API-level error integration test ─────────────────

/// When the Allegro API returns HTTP 404, `tools/call` must surface it as a
/// tool-level error (`isError=true`) with the HTTP status code in the content
/// text. The session must remain usable afterwards.
#[tokio::test]
async fn test_call_tool_api_4xx_returns_tool_level_error() {
    let mock_server = MockServer::start().await;

    // Valid token so dispatch proceeds past auth.
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "test-token", "expires_in": 3600 })),
        )
        .mount(&mock_server)
        .await;

    // API endpoint returns 404 — but only for fully header-compliant requests.
    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .and(header("Authorization", "Bearer test-token"))
        .and(header("Accept", "application/vnd.allegro.public.v1+json"))
        .and(header("Accept-Language", "pl-PL"))
        .respond_with(
            ResponseTemplate::new(404).set_body_string(
                r#"{"errors":[{"code":"NotFound","message":"Resource not found"}]}"#,
            ),
        )
        .mount(&mock_server)
        .await;

    let registry = build_registry().await;
    let tool_name = registry
        .list_tools()
        .iter()
        .find(|t| t.method == "get" && t.path == "/sale/offers")
        .expect("fixture must have GET /sale/offers tool")
        .name
        .clone();

    let auth = AllegroAuth::with_base_url(
        "test-id".to_string(),
        "test-secret".to_string(),
        mock_server.uri(),
    );
    let handler = AllegroServer::new(registry, auth, false).with_api_base_url(mock_server.uri());

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": tool_name, "arguments": {} }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;

    // Must be a tool-level error, not a JSON-RPC protocol error.
    assert!(
        response["error"].is_null(),
        "API 404 must not produce a JSON-RPC error, got: {response}"
    );
    assert_eq!(
        response["result"]["isError"],
        json!(true),
        "API 404 must set isError=true, got: {response}"
    );

    // The error text must mention the HTTP status code.
    let content = response["result"]["content"]
        .as_array()
        .expect("API 404 response must have content array");
    assert!(
        !content.is_empty(),
        "API 404 response must have non-empty content array"
    );
    let text = content[0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("404"),
        "API 404 error text must contain '404', got: {text}"
    );

    // Session must remain usable after the tool-level error.
    send_json(
        &mut client,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
    )
    .await;
    let list_response = recv_json(&mut client).await;
    assert!(
        list_response["result"]["tools"].is_array(),
        "session must remain usable after API 404 error, got: {list_response}"
    );

    server_handle.abort();
}

// ── Issue #5: graceful EOF shutdown test ─────────────────────────────────────

/// Dropping the client transport (simulating stdin EOF) must cause the server
/// task to exit cleanly — not via `abort()` — within a reasonable timeout.
#[tokio::test]
async fn test_server_exits_cleanly_on_client_eof() {
    let registry = build_registry().await;
    let auth = AllegroAuth::new("test-id".to_string(), "test-secret".to_string(), false);
    let handler = AllegroServer::new(registry, auth, false);

    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server_handle = tokio::spawn(async move {
        let running = handler.serve(server_transport).await.expect("serve failed");
        let _ = running.waiting().await;
    });

    let mut client: Client = BufReader::new(client_transport);
    initialize(&mut client).await;

    // Drop the client — this closes the write end of the duplex, which the
    // server sees as EOF on its read end, triggering graceful shutdown.
    drop(client);

    // The server task must exit on its own within 2 seconds.
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), server_handle).await;

    assert!(
        result.is_ok(),
        "server task must exit cleanly within 2 s after client EOF"
    );
    // The JoinHandle result must not be a panic.
    assert!(
        result.unwrap().is_ok(),
        "server task must not panic on client EOF"
    );
}

// ── Issue #7: stdout purity note ─────────────────────────────────────────────

// NOTE: "strict JSON-RPC on stdout, logs → stderr only" is enforced by the
// `tracing_subscriber` configuration in `src/main.rs` (line 86, inside
// `main()`):
//
//     tracing_subscriber::fmt()
//         .with_writer(std::io::stderr)   // <-- all log output goes to stderr
//         .init();
//
// This property cannot be easily tested in-process because the in-process
// duplex transport used here bypasses stdout/stderr entirely. The guarantee
// is structural: the MCP server writes only via `rmcp`'s stdio transport
// (which writes to stdout), and all `tracing` events are routed to stderr
// by the subscriber initialised in `main`. Any future change that adds a
// `tracing_subscriber` writing to stdout would break this invariant and
// should be caught in code review.

// ── Phase 7: User-Agent / Accept / scopes on the wire ─────────────────────────

/// Builds a registry from an inline OpenAPI YAML document (operations with
/// versioned `application/vnd.allegro.*+json` content keys, which the local
/// fixture deliberately lacks).
fn registry_from_yaml(yaml: &str) -> ToolRegistry {
    let api: OpenAPI = serde_yaml::from_str(yaml).expect("parse inline schema");
    ToolRegistry::from_openapi(&api).expect("build registry")
}

/// A config-driven custom User-Agent (config file / env / --user-agent end
/// up here via `with_http_client`) must be sent on BOTH the OAuth token
/// request and the API request — asserted by UA matchers on both mocks.
#[tokio::test]
async fn test_custom_user_agent_applies_to_token_and_api_requests() {
    const CUSTOM_UA: &str = "MyApp/2.0.0 (+https://example.com/app)";
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .and(header("User-Agent", CUSTOM_UA))
        .and(header("Accept-Language", "pl-PL"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "custom-ua-token", "expires_in": 3600 })),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/sale/offers"))
        .and(header("User-Agent", CUSTOM_UA))
        .and(header("Authorization", "Bearer custom-ua-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&mock_server)
        .await;

    let registry = build_registry().await;
    let tool_name = registry
        .list_tools()
        .iter()
        .find(|t| t.method == "get" && t.path == "/sale/offers")
        .expect("fixture must have GET /sale/offers tool")
        .name
        .clone();

    let custom_client =
        http::build_client(CUSTOM_UA, http::DEFAULT_ACCEPT_LANGUAGE).expect("valid client");
    let auth = AllegroAuth::with_http_client(
        "test-id".to_string(),
        "test-secret".to_string(),
        mock_server.uri(),
        custom_client.clone(),
    );
    let handler = AllegroServer::new(registry, auth, false)
        .with_api_base_url(mock_server.uri())
        .with_http_client(custom_client);

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": tool_name, "arguments": {} }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;

    // The request only succeeded if BOTH mocks matched — i.e. the custom UA
    // was present on both the token and the API request.
    assert!(
        response["error"].is_null(),
        "custom-UA tool call must succeed, got: {response}"
    );
    assert_ne!(
        response["result"]["isError"],
        json!(true),
        "got: {response}"
    );

    server_handle.abort();
}

/// An operation whose schema declares `application/vnd.allegro.beta.v1+json`
/// in `responses.'200'.content` must be dispatched with exactly that Accept
/// header instead of the dispatcher default.
#[tokio::test]
async fn test_per_operation_accept_override_sent_for_get() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "beta-token", "expires_in": 3600 })),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/beta/thing"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .and(header("Authorization", "Bearer beta-token"))
        .and(header("Accept", "application/vnd.allegro.beta.v1+json"))
        .and(header("Accept-Language", "pl-PL"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"beta":true}"#))
        .mount(&mock_server)
        .await;

    let registry = registry_from_yaml(concat!(
        "openapi: \"3.0.3\"\n",
        "info:\n  title: t\n  version: v\n",
        "paths:\n",
        "  /beta/thing:\n",
        "    get:\n",
        "      operationId: getBetaThing\n",
        "      summary: Get a beta thing\n",
        "      responses:\n",
        "        \"200\":\n",
        "          description: OK\n",
        "          content:\n",
        "            application/vnd.allegro.beta.v1+json:\n",
        "              schema:\n                type: object\n",
    ));

    let auth = AllegroAuth::with_base_url(
        "test-id".to_string(),
        "test-secret".to_string(),
        mock_server.uri(),
    );
    let handler = AllegroServer::new(registry, auth, false).with_api_base_url(mock_server.uri());

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "allegro_getbetathing", "arguments": {} }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;

    assert!(
        response["error"].is_null(),
        "per-op Accept tool call must succeed (mock matched), got: {response}"
    );
    assert_ne!(
        response["result"]["isError"],
        json!(true),
        "got: {response}"
    );

    server_handle.abort();
}

/// A POST operation with a versioned `requestBody` media type must send the
/// versioned `Content-Type` (set BEFORE `.json()`), the versioned `Accept`,
/// and — verified on the recorded request — exactly ONE `Content-Type`
/// header (single-header regression guard for the ordering rule).
#[tokio::test]
async fn test_post_carries_versioned_content_type_and_accept_single_header() {
    const MEDIA_TYPE: &str = "application/vnd.allegro.public.v1+json";
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "post-token", "expires_in": 3600 })),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/offers"))
        .and(header("User-Agent", http::DEFAULT_USER_AGENT))
        .and(header("Authorization", "Bearer post-token"))
        .and(header("Accept", MEDIA_TYPE))
        .and(header("Content-Type", MEDIA_TYPE))
        .respond_with(ResponseTemplate::new(201).set_body_string(r#"{"id":"offer-1"}"#))
        .mount(&mock_server)
        .await;

    let registry = registry_from_yaml(concat!(
        "openapi: \"3.0.3\"\n",
        "info:\n  title: t\n  version: v\n",
        "paths:\n",
        "  /offers:\n",
        "    post:\n",
        "      operationId: createOfferVnd\n",
        "      summary: Create an offer\n",
        "      requestBody:\n",
        "        required: true\n",
        "        content:\n",
        "          application/vnd.allegro.public.v1+json:\n",
        "            schema:\n              type: object\n",
        "      responses:\n",
        "        \"201\":\n          description: Created\n",
    ));

    let auth = AllegroAuth::with_base_url(
        "test-id".to_string(),
        "test-secret".to_string(),
        mock_server.uri(),
    );
    let handler = AllegroServer::new(registry, auth, false).with_api_base_url(mock_server.uri());

    let (mut client, server_handle) = spawn_server(handler);
    initialize(&mut client).await;

    send_json(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "allegro_createoffervnd", "arguments": { "body": { "name": "x" } } }
        }),
    )
    .await;
    let response = recv_json(&mut client).await;

    assert!(
        response["error"].is_null(),
        "versioned-Content-Type POST must succeed (mock matched), got: {response}"
    );
    assert_ne!(
        response["result"]["isError"],
        json!(true),
        "got: {response}"
    );

    // Single-header guarantee: inspect the recorded API request and count
    // its Content-Type values (`.header()` appends; `.json()` must not add a
    // second one).
    let requests = mock_server.received_requests().await.expect("requests");
    let api_post = requests
        .iter()
        .find(|r| r.method == "POST" && r.url.path() == "/offers")
        .expect("API POST request must have been recorded");
    let content_types: Vec<&str> = api_post
        .headers
        .get_all("content-type")
        .iter()
        .map(|v| v.to_str().expect("ascii header value"))
        .collect();
    assert_eq!(
        content_types,
        vec![MEDIA_TYPE],
        "exactly one versioned Content-Type header must be sent"
    );

    server_handle.abort();
}

/// Configured scopes must be sent as the space-joined OAuth2 `scope` form
/// param on the token request; without scopes the body must stay
/// byte-identical to the classic flow (no `scope=` at all).
#[tokio::test]
async fn test_token_request_carries_scope_param_only_when_configured() {
    /// Calls any fixture tool against a fresh mock server and returns the
    /// recorded token-request body for the given scopes.
    async fn token_body_for(scopes: Vec<String>, tool_name: String) -> Vec<u8> {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/oauth/token"))
            .and(header("User-Agent", http::DEFAULT_USER_AGENT))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "access_token": "scoped-token", "expires_in": 3600 })),
            )
            .mount(&mock_server)
            .await;

        let registry = build_registry().await;
        let auth = AllegroAuth::with_base_url(
            "test-id".to_string(),
            "test-secret".to_string(),
            mock_server.uri(),
        )
        .with_scopes(scopes);
        let handler =
            AllegroServer::new(registry, auth, false).with_api_base_url(mock_server.uri());

        let (mut client, server_handle) = spawn_server(handler);
        initialize(&mut client).await;
        send_json(
            &mut client,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": tool_name, "arguments": {} }
            }),
        )
        .await;
        let _ = recv_json(&mut client).await;
        server_handle.abort();

        let requests = mock_server.received_requests().await.expect("requests");
        requests
            .iter()
            .find(|r| r.method == "POST" && r.url.path() == "/auth/oauth/token")
            .expect("token request must have been recorded")
            .body
            .to_vec()
    }

    let registry = build_registry().await;
    let tool_name = registry
        .list_tools()
        .first()
        .expect("fixture must have at least one tool")
        .name
        .clone();

    // With scopes: `scope=` + url-encoded, space-joined value (':' → %3A).
    let body = token_body_for(
        vec!["allegro:api:sale:offers:read".to_owned()],
        tool_name.clone(),
    )
    .await;
    let body = String::from_utf8(body).expect("form body is ascii");
    assert!(
        body.contains("scope=allegro%3Aapi%3Asale%3Aoffers%3Aread"),
        "token body must carry the url-encoded scope param, got: {body}"
    );
    assert!(
        body.starts_with("grant_type=client_credentials"),
        "grant_type stays first, got: {body}"
    );

    // Without scopes: byte-identical to the classic flow.
    let body = token_body_for(Vec::new(), tool_name).await;
    assert_eq!(
        body, b"grant_type=client_credentials",
        "empty scopes must keep the token request body byte-identical"
    );
}
