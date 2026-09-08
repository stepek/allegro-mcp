# Plan: Phase 4 — Auth v1: client_credentials flow (GH-4)

## Overview

**What**: Implement the simplest working OAuth2 `client_credentials` flow for the Allegro REST API. This provides application tokens suitable for public endpoints (e.g. `GET /sale/categories`) and sandbox smoke tests.

**Current state**: Branch `4` has a CLI skeleton (`src/main.rs`) with synchronous `fn main()`, clap, and tracing. No auth module exists.

**Root cause / scope**: Phase 4 of the project plan. Two files need to change: create `src/auth/mod.rs` and modify `src/main.rs`.

**No new Cargo.toml dependencies needed**: `reqwest::RequestBuilder::basic_auth()` handles `Authorization: Basic base64(id:secret)` natively. `tokio::sync::RwLock` is part of tokio (already in deps). All other needs are covered by existing deps.

---

## Files to Create / Modify

```
src/
├── auth/
│   └── mod.rs    ← CREATE (new module)
└── main.rs       ← MODIFY (async main, --sandbox flag, smoke call)
```

---

## File 1: `src/auth/mod.rs` (CREATE)

```rust
//! Allegro OAuth2 `client_credentials` flow.
//!
//! Provides [`AllegroAuth`] — a thread-safe, async-friendly token manager
//! that fetches application tokens and caches them in memory, refreshing
//! 60 seconds before expiry.

use std::time::{Duration, Instant};

use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can occur during authentication.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("missing environment variable: {0}")]
    MissingEnvVar(&'static str),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("token response missing required field: {0}")]
    MalformedResponse(&'static str),
}

// ── Token response ────────────────────────────────────────────────────────────

/// Raw token response from `POST /auth/oauth/token`.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    /// Lifetime in seconds (Allegro issues 12-hour tokens = 43 200 s).
    pub expires_in: u64,
    pub scope: Option<String>,
    pub jti: Option<String>,
}

// ── Cached token ──────────────────────────────────────────────────────────────

/// An in-memory cached token with its expiry instant.
struct CachedToken {
    access_token: String,
    /// The instant at which the token expires (wall-clock approximation).
    expires_at: Instant,
}

impl CachedToken {
    /// Returns `true` if the token has more than 60 s remaining before expiry.
    ///
    /// The 60 s buffer ensures we refresh before the token actually expires,
    /// avoiding 401 errors on in-flight requests.
    fn is_valid(&self) -> bool {
        // `checked_duration_since` returns `None` if `expires_at` is in the past.
        self.expires_at
            .checked_duration_since(Instant::now())
            .map(|remaining| remaining > Duration::from_secs(60))
            .unwrap_or(false)
    }
}

// ── AllegroAuth ───────────────────────────────────────────────────────────────

/// Thread-safe Allegro application-token manager.
///
/// Fetches tokens via the `client_credentials` OAuth2 grant and caches them
/// in memory, refreshing 60 seconds before expiry.
///
/// # Example
/// ```no_run
/// # tokio_test::block_on(async {
/// let auth = AllegroAuth::from_env(false).unwrap();
/// let token = auth.token().await.unwrap();
/// println!("Bearer {token}");
/// # });
/// ```
pub struct AllegroAuth {
    client_id: String,
    client_secret: String,
    /// Base URL for the auth endpoint, e.g. `https://allegro.pl`.
    auth_base_url: String,
    http: Client,
    cache: RwLock<Option<CachedToken>>,
}

/// Manual `Debug` impl — redacts `client_secret` to prevent accidental logging.
impl std::fmt::Debug for AllegroAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AllegroAuth")
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("auth_base_url", &self.auth_base_url)
            .finish_non_exhaustive()
    }
}

impl AllegroAuth {
    /// Constructs an [`AllegroAuth`] from environment variables.
    ///
    /// Reads `ALLEGRO_CLIENT_ID` and `ALLEGRO_CLIENT_SECRET`.
    ///
    /// Set `sandbox = true` to target `allegro.pl.allegrosandbox.pl`.
    pub fn from_env(sandbox: bool) -> Result<Self, AuthError> {
        let client_id = std::env::var("ALLEGRO_CLIENT_ID")
            .map_err(|_| AuthError::MissingEnvVar("ALLEGRO_CLIENT_ID"))?;
        let client_secret = std::env::var("ALLEGRO_CLIENT_SECRET")
            .map_err(|_| AuthError::MissingEnvVar("ALLEGRO_CLIENT_SECRET"))?;
        Ok(Self::new(client_id, client_secret, sandbox))
    }

    /// Constructs an [`AllegroAuth`] with explicit credentials.
    pub fn new(client_id: String, client_secret: String, sandbox: bool) -> Self {
        let auth_base_url = if sandbox {
            "https://allegro.pl.allegrosandbox.pl".to_owned()
        } else {
            "https://allegro.pl".to_owned()
        };
        info!(auth_base_url, "AllegroAuth initialised");
        Self {
            client_id,
            client_secret,
            auth_base_url,
            http: Client::new(),
            cache: RwLock::new(None),
        }
    }

    /// Returns a valid access token, fetching a new one if necessary.
    ///
    /// Uses a double-check pattern to avoid thundering-herd re-fetches when
    /// multiple async tasks race to refresh an expired token.
    pub async fn token(&self) -> Result<String, AuthError> {
        // Fast path — shared read lock.
        {
            let guard = self.cache.read().await;
            if let Some(cached) = guard.as_ref() {
                if cached.is_valid() {
                    debug!("reusing cached token");
                    return Ok(cached.access_token.clone());
                }
            }
        }

        // Slow path — exclusive write lock with double-check.
        let mut guard = self.cache.write().await;
        if let Some(cached) = guard.as_ref() {
            if cached.is_valid() {
                debug!("reusing cached token (post-lock double-check)");
                return Ok(cached.access_token.clone());
            }
        }

        // Fetch a fresh token.
        let resp = self.fetch_token().await?;

        if resp.expires_in < 120 {
            warn!(
                expires_in = resp.expires_in,
                "token expires_in is very short — cache refresh may loop"
            );
        }

        // Store the raw expiry instant; `is_valid()` applies the 60 s guard.
        let expires_at = Instant::now() + Duration::from_secs(resp.expires_in);

        let token_preview = &resp.access_token[..resp.access_token.len().min(8)];
        info!(
            expires_in = resp.expires_in,
            token_prefix = token_preview,
            "fetched new access token"
        );

        let token = resp.access_token.clone();
        *guard = Some(CachedToken {
            access_token: resp.access_token,
            expires_at,
        });

        Ok(token)
    }

    /// Performs the actual HTTP call to the token endpoint.
    async fn fetch_token(&self) -> Result<TokenResponse, AuthError> {
        let url = format!("{}/auth/oauth/token", self.auth_base_url);
        debug!(url, "fetching token");

        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await?
            .error_for_status()?;

        let token_resp: TokenResponse = resp.json().await?;
        Ok(token_resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_returns_err_when_vars_missing() {
        // Ensure the env vars are not set for this test.
        // SAFETY: single-threaded test; no other thread reads these vars concurrently.
        unsafe {
            std::env::remove_var("ALLEGRO_CLIENT_ID");
            std::env::remove_var("ALLEGRO_CLIENT_SECRET");
        }
        let result = AllegroAuth::from_env(false);
        assert!(result.is_err(), "expected Err when env vars are missing");
        let err = result.unwrap_err();
        assert!(
            matches!(err, AuthError::MissingEnvVar("ALLEGRO_CLIENT_ID")),
            "expected MissingEnvVar for ALLEGRO_CLIENT_ID, got: {err}"
        );
    }

    #[test]
    fn debug_redacts_client_secret() {
        let auth = AllegroAuth::new(
            "test-client-id".to_owned(),
            "super-secret".to_owned(),
            false,
        );
        let debug_str = format!("{auth:?}");
        assert!(
            !debug_str.contains("super-secret"),
            "Debug output must not contain the client secret"
        );
        assert!(
            debug_str.contains("[REDACTED]"),
            "Debug output must contain [REDACTED]"
        );
    }

    #[test]
    fn cached_token_is_valid_with_plenty_of_time() {
        use std::time::{Duration, Instant};
        let token = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(3600),
        };
        assert!(token.is_valid(), "token with 1 hour remaining should be valid");
    }

    #[test]
    fn cached_token_is_invalid_when_expired() {
        use std::time::{Duration, Instant};
        // expires_at is in the past
        let token = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        assert!(!token.is_valid(), "expired token should not be valid");
    }

    #[test]
    fn cached_token_is_invalid_within_60s_buffer() {
        use std::time::{Duration, Instant};
        // expires_at is 30 s in the future — within the 60 s buffer
        let token = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(30),
        };
        assert!(
            !token.is_valid(),
            "token with only 30 s remaining should be considered invalid (60 s buffer)"
        );
    }
}
```

---

## File 2: `src/main.rs` (MODIFY)

Replace the existing `src/main.rs` with:

```rust
//! allegro-mcp — MCP server for the Allegro REST API.
//!
//! Phase 4: client_credentials auth with in-memory token cache.

mod auth;

use anyhow::Result;
use clap::Parser;
use tracing::{info, warn};

use crate::auth::AllegroAuth;

/// allegro-mcp: MCP server for the Allegro REST API.
#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Increase log verbosity (repeat for more: -v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Use the Allegro sandbox environment instead of production.
    #[arg(long, default_value_t = false)]
    sandbox: bool,
}

/// Maps the `-v` count to a [`tracing::Level`].
///
/// | flags | level |
/// |-------|-------|
/// | (none) | WARN  |
/// | `-v`   | INFO  |
/// | `-vv`  | DEBUG |
/// | `-vvv` or more | TRACE |
fn verbosity_level(verbose: u8) -> tracing::Level {
    match verbose {
        0 => tracing::Level::WARN,
        1 => tracing::Level::INFO,
        2 => tracing::Level::DEBUG,
        _ => tracing::Level::TRACE,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_max_level(verbosity_level(cli.verbose))
        .with_writer(std::io::stderr)
        .init();

    info!(sandbox = cli.sandbox, "allegro-mcp starting (phase 4 auth)");

    // Initialise auth — reads ALLEGRO_CLIENT_ID / ALLEGRO_CLIENT_SECRET from env.
    let auth = match AllegroAuth::from_env(cli.sandbox) {
        Ok(a) => a,
        Err(e) => {
            warn!("Auth not configured ({e}); skipping smoke test");
            return Ok(());
        }
    };

    // Smoke test: first call — should fetch a new token.
    let token1 = auth.token().await?;
    info!(token_prefix = &token1[..token1.len().min(8)], "first token obtained");

    // Smoke test: second call — should reuse cached token.
    let token2 = auth.token().await?;
    info!(token_prefix = &token2[..token2.len().min(8)], "second token obtained (should be cached)");

    assert_eq!(token1, token2, "second call must reuse the cached token");

    // Smoke test: call GET /sale/categories with the token.
    let api_host = if cli.sandbox {
        "https://api.allegro.pl.allegrosandbox.pl"
    } else {
        "https://api.allegro.pl"
    };

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{api_host}/sale/categories"))
        .bearer_auth(&token1)
        .header("Accept", "application/vnd.allegro.public.v1+json")
        .send()
        .await?;

    let status = resp.status();
    info!(status = status.as_u16(), "GET /sale/categories response");

    if status.is_success() {
        info!("Smoke test PASSED: categories endpoint returned {status}");
    } else {
        let body = resp.text().await.unwrap_or_default();
        warn!(status = status.as_u16(), body, "Smoke test: unexpected status");
    }

    // TODO(gh-5): initialise MCP server and connect stdio transport
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::verbosity_level;

    #[test]
    fn default_verbosity_is_warn() {
        assert_eq!(verbosity_level(0), tracing::Level::WARN);
    }

    #[test]
    fn single_v_gives_info() {
        assert_eq!(verbosity_level(1), tracing::Level::INFO);
    }

    #[test]
    fn double_v_gives_debug() {
        assert_eq!(verbosity_level(2), tracing::Level::DEBUG);
    }

    #[test]
    fn triple_v_and_above_gives_trace() {
        assert_eq!(verbosity_level(3), tracing::Level::TRACE);
        assert_eq!(verbosity_level(255), tracing::Level::TRACE);
    }
}
```

**Note**: `reqwest` is already in `Cargo.toml` but not yet used in `main.rs`. This change uses it directly for the smoke test HTTP call.

---

## New Dependencies

**None required.** All needed crates are already in `Cargo.toml`:
- `reqwest` (with `rustls-tls`, `json`, `multipart` — `form` is included in default features)
- `tokio` (full features — includes `sync::RwLock`)
- `serde` (derive)
- `thiserror`
- `tracing`
- `anyhow`

**Verify `form` feature**: `reqwest` 0.12.x includes `multipart` and `form` in default features. Since we use `default-features = false`, we must add `"form"` to the features list in `Cargo.toml`. Keep the existing exact version pin `"0.12.15"` and preserve `"multipart"` (it may be needed by future phases):

```toml
reqwest = { version = "0.12.15", default-features = false, features = ["rustls-tls", "json", "multipart", "form"] }
```

---

## Implementation Steps

### Step 1 — Add `form` feature to reqwest in `Cargo.toml`

Edit `Cargo.toml` — change the reqwest line to (preserve exact version pin and `multipart`):
```toml
reqwest = { version = "0.12.15", default-features = false, features = ["rustls-tls", "json", "multipart", "form"] }
```

### Step 2 — Create `src/auth/mod.rs`

Create the directory `src/auth/` and write `src/auth/mod.rs` with the exact content from File 1 above.

### Step 3 — Replace `src/main.rs`

Replace `src/main.rs` with the exact content from File 2 above.

### Step 4 — Build without `--locked` to regenerate `Cargo.lock`

Adding the `form` feature changes the dependency graph, so `Cargo.lock` must be regenerated:
```bash
cargo build
```

Fix any compilation errors. Then stage the updated lock file:
```bash
git add Cargo.lock
```

### Step 5 — Verify with `--locked`

```bash
cargo build --locked
```

This must succeed now that `Cargo.lock` is up to date.

### Step 6 — Run tests

```bash
cargo test --locked
```

All existing tests must pass.

### Step 7 — Run clippy

```bash
cargo clippy --all-targets -- -D warnings
```

Fix any warnings.

### Step 8 — Run rustfmt

```bash
cargo fmt --all
```

### Step 9 — Commit

```bash
git add src/auth/mod.rs src/main.rs Cargo.toml Cargo.lock
git commit -m "feat(auth): add client_credentials OAuth2 flow with in-memory token cache (#4)"
```

---

## Acceptance Criteria

| Criterion | How to verify |
|---|---|
| `cargo build --locked` passes | Exit code 0 |
| `cargo test --locked` passes | All tests green |
| `cargo clippy -D warnings` passes | No warnings |
| `cargo fmt --check` passes | No diff |
| `AllegroAuth::from_env(false)` returns `Err` when env vars missing | `from_env_returns_err_when_vars_missing` unit test |
| `AllegroAuth` Debug output redacts `client_secret` | `debug_redacts_client_secret` unit test |
| `CachedToken::is_valid()` returns `false` within 60 s buffer | `cached_token_is_invalid_within_60s_buffer` unit test |
| `CachedToken::is_valid()` returns `true` with plenty of time | `cached_token_is_valid_with_plenty_of_time` unit test |
| Live `GET /sale/categories` returns 2xx on sandbox | Manual smoke test with real credentials |

---

## Edge Cases and Risks

### 1. `expires_in < 60` — short-lived tokens
**Risk**: If the server returns `expires_in = 30`, the token will expire before the 60 s buffer in `is_valid()` is satisfied, causing every call to `token()` to fetch a new token.
**Mitigation**: `warn!` if `expires_in < 120`. The `expires_at` is stored as `Instant::now() + Duration::from_secs(expires_in)` (raw, no subtraction). The 60 s guard lives only in `is_valid()`, avoiding double-subtraction.

### 2. Thundering herd
**Risk**: Multiple async tasks simultaneously find the cache empty and all try to fetch a new token.
**Mitigation**: Double-check pattern — after acquiring the write lock, re-validate the cache before fetching.

### 3. Secret leakage in logs
**Risk**: `#[derive(Debug)]` on `AllegroAuth` would print `client_secret` in plain text.
**Mitigation**: Manual `Debug` impl that replaces `client_secret` with `"[REDACTED]"`.

### 4. `form` feature missing from reqwest
**Risk**: `.form(&[...])` panics or fails to compile if the `form` feature is not enabled.
**Mitigation**: Add `"form"` to reqwest features in `Cargo.toml`.

### 5. Clock jumps
**Risk**: `Instant::now()` is monotonic on all platforms — no risk of negative duration from NTP adjustments.
**Mitigation**: `checked_duration_since` returns `None` if `expires_at` is in the past, which correctly triggers a refresh.

### 6. Token string too short for preview
**Risk**: `&token[..8]` panics if token is shorter than 8 bytes.
**Mitigation**: Use `token.len().min(8)` as the slice end.

### 7. Sandbox vs production confusion
**Risk**: Using production credentials against sandbox or vice versa.
**Mitigation**: Log `auth_base_url` at `INFO` level during `AllegroAuth::new()`.

### 8. `reqwest::Client` per-request vs shared
**Risk**: Creating a new `Client` per request wastes connection pool resources.
**Mitigation**: `AllegroAuth` owns a single `Client` instance reused across all token fetches. The smoke test in `main.rs` creates its own `Client` for the categories call — acceptable for a smoke test.

### 9. Missing `mod auth;` declaration
**Risk**: Forgetting to add `mod auth;` to `main.rs` causes a compile error.
**Mitigation**: The `main.rs` content above includes `mod auth;` at the top.

### 10. `#[tokio::main]` on `fn main`
**Risk**: Forgetting to change `fn main()` to `async fn main()` when adding `#[tokio::main]`.
**Mitigation**: The `main.rs` content above uses `async fn main()`.
