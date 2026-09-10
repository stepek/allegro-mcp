//! HTTP (Streamable HTTP) MCP transport — the default transport in this
//! release; `--stdio` (see `main.rs`) remains for local dev / editor
//! clients. Open WebUI is the primary client for this transport (see
//! `docs/open-webui.md`).
//!
//! Auth/schema/registry construction happens in `main.rs::build_allegro_server`
//! (shared with the stdio transport, see Phase 3.0 of the plan) — this
//! module only owns transport-specific concerns: the eager startup
//! auth-check banner, the axum router/middleware wiring
//! ([`build_router`]), and binding/serving.
//!
//! Exposes three routes:
//! - `POST/GET/DELETE /mcp` — the MCP Streamable HTTP endpoint (rmcp).
//! - `GET /health` — unauthenticated liveness probe (Docker HEALTHCHECK,
//!   load balancers).
//! - `GET /auth/status` — admin visibility into the Allegro token cache
//!   (see `crate::auth::AuthStatus`).
//!
//! `/mcp` and `/auth/status` are gated behind an optional static bearer
//! token (`ALLEGRO_MCP_SERVER_TOKEN`) — off by default, reverse-proxy
//! friendly. `/health` is always public (health checks must not need
//! credentials).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Request, State};
use axum::http::{header::AUTHORIZATION, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tracing::{error, info, warn};

use crate::auth::AllegroAuth;

/// Env var enabling the optional static bearer-token guard. Off by
/// default — this server is designed to sit behind a reverse proxy /
/// Docker-internal network (Open WebUI talking to `allegro-mcp:8080` on a
/// private compose network) where TLS + access control is handled
/// upstream. Set this to require `Authorization: Bearer <token>` on
/// `/mcp` and `/auth/status`.
const SERVER_TOKEN_ENV: &str = "ALLEGRO_MCP_SERVER_TOKEN";

/// Comma-separated `host` / `host:port` allow-list for the Streamable HTTP
/// transport's DNS-rebinding guard. Defaults to loopback-only (rmcp's
/// default), which rejects every request arriving via a Docker service
/// name or reverse-proxy hostname. Set this to the hostname(s) clients
/// actually use, e.g. `allegro-mcp,allegro-mcp:8080` on a compose network,
/// or your public reverse-proxy hostname. Set to `*` to disable the check
/// entirely (not recommended for public deployments — see SECURITY.md).
const ALLOWED_HOSTS_ENV: &str = "ALLEGRO_MCP_ALLOWED_HOSTS";

#[derive(Clone)]
pub struct AppState {
    auth: Arc<AllegroAuth>,
    server_token: Option<Arc<str>>,
}

impl AppState {
    /// Constructs the shared HTTP-transport state.
    ///
    /// `#[doc(hidden)]` — internal wiring for `run_http_server` /
    /// [`build_router`], exposed only so
    /// `tests/http_server_integration.rs` can build a router directly
    /// (Phase 3.5's mandated design: exercise routing/middleware without
    /// going through `run_http_server`'s env-var-driven startup).
    #[doc(hidden)]
    pub fn new(auth: Arc<AllegroAuth>, server_token: Option<Arc<str>>) -> Self {
        Self { auth, server_token }
    }
}

/// Runs the MCP server over Streamable HTTP on an already-built
/// `handler` (see `main.rs::build_allegro_server`, Phase 3.0): runs the
/// eager startup auth-check banner, builds the router ([`build_router`]),
/// then binds and serves `/mcp`, `/health`, and `/auth/status` on
/// `0.0.0.0:{port}` until the process is signalled to stop.
///
/// Takes an already-built `handler` (not `cfg`/`api_client`/
/// `schema_client`/`source`) rather than duplicating
/// `build_allegro_server`'s auth/schema/registry construction — this
/// module never references `main.rs` directly (it can't: `main.rs` and
/// `lib.rs` are separate crate roots that both compile this file, see
/// Phase 3.0's rationale), so the caller in `main.rs` builds the handler
/// first and hands it over.
pub async fn run_http_server(
    handler: crate::server::AllegroServer,
    sandbox: bool,
    port: u16,
) -> Result<()> {
    info!(
        port,
        sandbox, "starting MCP server (Streamable HTTP transport)"
    );

    let auth_handle = handler.auth_handle();

    // See the module doc's "Discrepancy" note in the plan: no device-flow
    // exists, so this eager fetch + banner is the closest honest
    // equivalent to "first-run auth UX" — a broken client_id/secret pair
    // is caught at startup (visible in `docker logs`) instead of silently
    // failing on the first tool call from Open WebUI.
    match auth_handle.token().await {
        Ok(_) => info!(
            "=================================================================\n \
             allegro-mcp: Allegro OAuth2 client_credentials check OK (sandbox={})\n \
             ================================================================",
            sandbox
        ),
        Err(e) => {
            error!(
                "=================================================================\n \
                 allegro-mcp: STARTUP AUTH CHECK FAILED: {e}\n \
                 Check ALLEGRO_CLIENT_ID / ALLEGRO_CLIENT_SECRET and retry.\n \
                 =================================================================",
            );
            anyhow::bail!("startup Allegro auth check failed: {e}");
        }
    }

    let server_token = std::env::var(SERVER_TOKEN_ENV).ok().map(Arc::from);
    if server_token.is_some() {
        info!("bearer token auth ENABLED for /mcp and /auth/status");
    } else {
        warn!(
            "bearer token auth DISABLED (set {SERVER_TOKEN_ENV} to enable) — \
             relying on network isolation / reverse proxy for access control"
        );
    }

    let allowed_hosts = resolve_allowed_hosts();
    let mcp_service: StreamableHttpService<crate::server::AllegroServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts),
        );

    let state = AppState::new(auth_handle, server_token);
    let app = build_router(state, mcp_service);

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;
    info!(%addr, "MCP server ready, serving /mcp, /health, /auth/status");

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("HTTP server error: {e}"))?;

    info!("MCP server shut down");
    Ok(())
}

/// Builds the axum [`Router`]: `/health` (always public, added *after*
/// the merge so the bearer-auth `route_layer` never touches it) merged
/// with the bearer-auth-gated `/mcp` + `/auth/status` routes.
///
/// **Required extraction, not optional** (Phase 3.5): split out from
/// `run_http_server` specifically so
/// `tests/http_server_integration.rs` can construct a real router —
/// bearer-auth middleware, `/health`, `/auth/status`, and a real `/mcp`
/// `StreamableHttpService` — and drive it over real HTTP on an ephemeral
/// port, without going through `run_http_server`'s env-var-driven
/// `ALLEGRO_CLIENT_ID`/`ALLEGRO_CLIENT_SECRET` lookup or the eager
/// startup auth-check network call.
#[doc(hidden)]
pub fn build_router(
    state: AppState,
    mcp_service: StreamableHttpService<crate::server::AllegroServer, LocalSessionManager>,
) -> Router {
    let protected = Router::new()
        .nest_service("/mcp", mcp_service)
        .route("/auth/status", get(auth_status_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), bearer_auth));

    Router::new()
        .route("/health", get(health_handler))
        .merge(protected)
        .with_state(state)
}

/// Parses [`ALLOWED_HOSTS_ENV`] into rmcp's allow-list format. Absent env
/// var ⇒ rmcp's loopback-only default (safe default for `docker run -p`
/// local testing); `*` ⇒ disabled (documented tradeoff, see
/// SECURITY.md); otherwise the comma-separated list verbatim (each entry
/// may be `host` or `host:port`, matching `StreamableHttpServerConfig`'s
/// own format).
fn resolve_allowed_hosts() -> Vec<String> {
    match std::env::var(ALLOWED_HOSTS_ENV) {
        Ok(v) if v.trim() == "*" => {
            warn!("{ALLOWED_HOSTS_ENV}=* — Host-header DNS-rebinding protection DISABLED");
            Vec::new() // empty allow-list == rmcp "allow all" (see tower.rs host_is_allowed)
        }
        Ok(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(_) => vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
    }
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn auth_status_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.auth.status().await)
}

/// Optional static bearer-token guard. No-op (passes every request
/// through) when `ALLEGRO_MCP_SERVER_TOKEN` was not set at startup.
async fn bearer_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = &state.server_token else {
        return next.run(req).await;
    };

    let provided = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
            next.run(req).await
        }
        _ => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    }
}

/// Constant-time byte comparison — avoids a timing side-channel on the
/// bearer-token check. No new dependency: this is a ~5-line primitive,
/// not worth pulling in `subtle`/`constant_time_eq` for.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_equal_slices() {
        assert!(constant_time_eq(b"secret", b"secret"));
    }

    #[test]
    fn constant_time_eq_rejects_different_slices() {
        assert!(!constant_time_eq(b"secret", b"wrong-token"));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq(b"short", b"much-longer-value"));
    }

    #[test]
    fn resolve_allowed_hosts_defaults_to_loopback_when_env_absent() {
        // SAFETY-of-intent: serial_test avoids cross-test env races (see
        // Cargo.toml [dev-dependencies]); this test only reads the var, so
        // relies on the CI/dev shell not exporting it. Mirrors the
        // env-absent test pattern in src/auth/mod.rs.
        if std::env::var(ALLOWED_HOSTS_ENV).is_err() {
            let hosts = resolve_allowed_hosts();
            assert_eq!(hosts, vec!["localhost", "127.0.0.1", "::1"]);
        }
    }
}
