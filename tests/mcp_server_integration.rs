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
