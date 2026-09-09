//! Integration tests for the MCP server handler, driven over an in-process
//! `tokio::io::duplex` transport using raw newline-delimited JSON-RPC (the
//! same wire format `rmcp`'s stdio transport uses). This avoids depending on
//! the `client` feature of `rmcp`, which is not enabled in this crate.

use std::path::PathBuf;

use allegro_mcp::auth::AllegroAuth;
use allegro_mcp::schema::{self, SchemaSource};
use allegro_mcp::server::AllegroServer;
use allegro_mcp::tool_registry::ToolRegistry;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use wiremock::matchers::{method, path};
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
    let api: OpenAPI = serde_yaml::from_str(
        "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
    )
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


