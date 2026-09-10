//! Integration test for the Streamable HTTP transport: boots the real
//! axum app (via `allegro_mcp::http_server::build_router`) on an
//! ephemeral port and drives it with `reqwest`, mirroring
//! `tests/mcp_server_integration.rs`'s fixture/mock conventions but over
//! real HTTP instead of an in-process duplex stream. Deliberately does
//! NOT call `run_http_server` (which reads env vars and talks to the
//! real Allegro auth host) — see the plan's Phase 3.5 rationale.

use std::path::PathBuf;
use std::sync::Arc;

use allegro_mcp::auth::AllegroAuth;
use allegro_mcp::http_server::{build_router, AppState};
use allegro_mcp::schema::{self, SchemaSource};
use allegro_mcp::server::AllegroServer;
use allegro_mcp::tool_registry::ToolRegistry;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

async fn mock_token_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "test-token",
            "expires_in": 43200,
            "token_type": "bearer"
        })))
        .mount(&server)
        .await;
    server
}

/// Spawns the router on an ephemeral `127.0.0.1` port and returns the base
/// URL (`http://127.0.0.1:{port}`) plus the server task's `JoinHandle`.
async fn spawn_router(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve failed");
    });
    (format!("http://{addr}"), handle)
}

fn build_mcp_service(
    server: AllegroServer,
) -> StreamableHttpService<AllegroServer, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

#[tokio::test]
async fn health_endpoint_is_public_even_with_bearer_token_configured() {
    let registry = build_registry().await;
    let mock_server = mock_token_server().await;
    let auth =
        AllegroAuth::with_base_url("id".to_string(), "secret".to_string(), mock_server.uri());
    let server = AllegroServer::new(registry, auth, false);
    let auth_handle = server.auth_handle();
    let mcp_service = build_mcp_service(server);

    let state = AppState::new(auth_handle, Some(Arc::from("secret-token")));
    let router = build_router(state, mcp_service);
    let (base_url, handle) = spawn_router(router).await;

    let client = reqwest::Client::new();
    // No Authorization header at all — must still succeed.
    let resp = client
        .get(format!("{base_url}/health"))
        .send()
        .await
        .expect("GET /health");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert_eq!(body, "ok");

    handle.abort();
}

#[tokio::test]
async fn auth_status_without_server_token_is_public() {
    let registry = build_registry().await;
    let mock_server = mock_token_server().await;
    let auth =
        AllegroAuth::with_base_url("id".to_string(), "secret".to_string(), mock_server.uri());
    let server = AllegroServer::new(registry, auth, false);
    let auth_handle = server.auth_handle();
    let mcp_service = build_mcp_service(server);

    let state = AppState::new(auth_handle, None);
    let router = build_router(state, mcp_service);
    let (base_url, handle) = spawn_router(router).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base_url}/auth/status"))
        .send()
        .await
        .expect("GET /auth/status");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse json");
    assert_eq!(body["auth_flow"], "client_credentials");

    handle.abort();
}

#[tokio::test]
async fn auth_status_with_server_token_requires_bearer() {
    let registry = build_registry().await;
    let mock_server = mock_token_server().await;
    let auth =
        AllegroAuth::with_base_url("id".to_string(), "secret".to_string(), mock_server.uri());
    let server = AllegroServer::new(registry, auth, false);
    let auth_handle = server.auth_handle();
    let mcp_service = build_mcp_service(server);

    let state = AppState::new(auth_handle, Some(Arc::from("secret-token")));
    let router = build_router(state, mcp_service);
    let (base_url, handle) = spawn_router(router).await;

    let client = reqwest::Client::new();

    // No Authorization header -> 401.
    let resp = client
        .get(format!("{base_url}/auth/status"))
        .send()
        .await
        .expect("GET /auth/status (no auth)");
    assert_eq!(resp.status(), 401);

    // Wrong token -> 401.
    let resp = client
        .get(format!("{base_url}/auth/status"))
        .header("Authorization", "Bearer wrong")
        .send()
        .await
        .expect("GET /auth/status (wrong token)");
    assert_eq!(resp.status(), 401);

    // Correct token -> 200.
    let resp = client
        .get(format!("{base_url}/auth/status"))
        .header("Authorization", "Bearer secret-token")
        .send()
        .await
        .expect("GET /auth/status (correct token)");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("parse json");
    assert_eq!(body["auth_flow"], "client_credentials");

    // Lowercase "bearer" scheme must be rejected (case-sensitive prefix match).
    let resp = client
        .get(format!("{base_url}/auth/status"))
        .header("Authorization", "bearer secret-token")
        .send()
        .await
        .expect("GET /auth/status (lowercase bearer)");
    assert_eq!(resp.status(), 401);

    handle.abort();
}

#[tokio::test]
async fn mcp_endpoint_requires_bearer_when_server_token_configured() {
    let registry = build_registry().await;
    let mock_server = mock_token_server().await;
    let auth =
        AllegroAuth::with_base_url("id".to_string(), "secret".to_string(), mock_server.uri());
    let server = AllegroServer::new(registry, auth, false);
    let auth_handle = server.auth_handle();
    let mcp_service = build_mcp_service(server);

    let state = AppState::new(auth_handle, Some(Arc::from("secret-token")));
    let router = build_router(state, mcp_service);
    let (base_url, handle) = spawn_router(router).await;

    let client = reqwest::Client::new();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "0.0.1" }
        }
    });

    let resp = client
        .post(format!("{base_url}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .expect("POST /mcp without Authorization header");

    assert_eq!(resp.status(), 401);

    handle.abort();
}

#[tokio::test]
async fn mcp_get_without_session_returns_client_error() {
    let registry = build_registry().await;
    let mock_server = mock_token_server().await;
    let auth =
        AllegroAuth::with_base_url("id".to_string(), "secret".to_string(), mock_server.uri());
    let server = AllegroServer::new(registry, auth, false);
    let auth_handle = server.auth_handle();
    let mcp_service = build_mcp_service(server);

    let state = AppState::new(auth_handle, None);
    let router = build_router(state, mcp_service);
    let (base_url, handle) = spawn_router(router).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base_url}/mcp"))
        .send()
        .await
        .expect("GET /mcp without session");

    assert!(
        resp.status().is_client_error(),
        "GET /mcp without a session ID must return a 4xx status, got: {}",
        resp.status()
    );

    handle.abort();
}

#[tokio::test]
async fn mcp_endpoint_accepts_initialize_request() {
    let registry = build_registry().await;
    let mock_server = mock_token_server().await;
    let auth =
        AllegroAuth::with_base_url("id".to_string(), "secret".to_string(), mock_server.uri());
    let server = AllegroServer::new(registry, auth, false);
    let auth_handle = server.auth_handle();
    let mcp_service = build_mcp_service(server);

    let state = AppState::new(auth_handle, None);
    let router = build_router(state, mcp_service);
    let (base_url, handle) = spawn_router(router).await;

    let client = reqwest::Client::new();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "0.0.1" }
        }
    });

    let resp = client
        .post(format!("{base_url}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .expect("POST /mcp initialize");

    assert_eq!(resp.status(), 200, "initialize must return 200");
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "initialize response must be text/event-stream, got: {content_type}"
    );

    handle.abort();
}
