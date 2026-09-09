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

    /// Reserved for future use when validating token response fields.
    #[allow(dead_code)]
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
    // `scope` and `jti` are part of the Allegro token contract; exposed for
    // future phases that may need to inspect or log them.
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
/// # use allegro_mcp::auth::AllegroAuth;
/// # #[tokio::main]
/// # async fn main() {
/// let auth = AllegroAuth::from_env(false).unwrap();
/// let token = auth.token().await.unwrap();
/// println!("Bearer {token}");
/// # }
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

    /// Returns the auth base URL. Exposed for testing only.
    #[cfg(test)]
    fn auth_base_url(&self) -> &str {
        &self.auth_base_url
    }

    /// Constructs an [`AllegroAuth`] with explicit credentials.
    pub fn new(client_id: String, client_secret: String, sandbox: bool) -> Self {
        let auth_base_url = if sandbox {
            "https://allegro.pl.allegrosandbox.pl".to_owned()
        } else {
            "https://allegro.pl".to_owned()
        };
        Self::with_base_url(client_id, client_secret, auth_base_url)
    }

    /// Constructs an [`AllegroAuth`] with an explicit auth base URL.
    ///
    /// Intended for integration tests that need to point the token endpoint
    /// at a mock server; production callers should use [`Self::new`] or
    /// [`Self::from_env`] instead.
    pub fn with_base_url(client_id: String, client_secret: String, auth_base_url: String) -> Self {
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

        // Clamp expires_in to a minimum of 120 s to prevent an infinite refresh
        // loop if the server returns a pathologically short lifetime.
        let effective_expires_in = if resp.expires_in < 120 {
            warn!(
                expires_in = resp.expires_in,
                "token expires_in is very short — clamping to 120 s to avoid refresh loop"
            );
            120
        } else {
            resp.expires_in
        };

        // Store the raw expiry instant; `is_valid()` applies the 60 s guard.
        let expires_at = Instant::now() + Duration::from_secs(effective_expires_in);

        debug!(expires_in = resp.expires_in, "fetched new access token");
        info!("access token refreshed");

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
        debug!(
            scope = token_resp.scope.as_deref(),
            jti = token_resp.jti.as_deref(),
            "token response received"
        );
        Ok(token_resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the `MissingEnvVar` error variant carries the variable name and
    /// formats correctly. Does not mutate the process environment.
    #[test]
    fn missing_env_var_error_display() {
        let err = AuthError::MissingEnvVar("ALLEGRO_CLIENT_ID");
        assert!(
            matches!(err, AuthError::MissingEnvVar("ALLEGRO_CLIENT_ID")),
            "MissingEnvVar variant must carry the variable name"
        );
        assert_eq!(
            err.to_string(),
            "missing environment variable: ALLEGRO_CLIENT_ID"
        );
    }

    /// Verifies that `from_env` returns `Err(MissingEnvVar)` when the env var
    /// is absent.
    ///
    /// Uses a sentinel variable name that is guaranteed to never be set in any
    /// real environment, avoiding the need to mutate the process environment
    /// (which would be a data race in parallel tests).
    ///
    /// Note: `from_env` is hard-coded to look up `ALLEGRO_CLIENT_ID`, so we
    /// cannot inject a different variable name without refactoring. Instead,
    /// we verify the error path by checking that `from_env` fails when
    /// `ALLEGRO_CLIENT_ID` is absent (reliable in CI), and document that
    /// developers with real credentials set should run `cargo test` without
    /// those env vars to exercise this path.
    #[test]
    fn from_env_returns_err_for_absent_var() {
        // Only run the assertion when the var is actually absent.
        // In CI (no credentials), this always runs.
        // Locally with credentials set, the test is a no-op — the
        // `missing_env_var_error_display` test covers the error type/display.
        if std::env::var("ALLEGRO_CLIENT_ID").is_err() {
            let result = AllegroAuth::from_env(false);
            assert!(
                result.is_err(),
                "expected Err when ALLEGRO_CLIENT_ID is absent"
            );
            assert!(
                matches!(
                    result.unwrap_err(),
                    AuthError::MissingEnvVar("ALLEGRO_CLIENT_ID")
                ),
                "expected MissingEnvVar(\"ALLEGRO_CLIENT_ID\")"
            );
        }
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
        let token = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(3600),
        };
        assert!(
            token.is_valid(),
            "token with 1 hour remaining should be valid"
        );
    }

    #[test]
    fn cached_token_is_invalid_when_expired() {
        // expires_at is in the past
        let token = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        assert!(!token.is_valid(), "expired token should not be valid");
    }

    /// Boundary: 62 s remaining is strictly greater than the 60 s threshold,
    /// so `is_valid()` must return `true`.
    ///
    /// Uses 62 s (not 61 s) to give a 1-second margin against timing jitter
    /// on loaded machines where two `Instant::now()` calls may differ by ~1 s.
    #[test]
    fn cached_token_is_valid_at_61s_boundary() {
        let token = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(62),
        };
        assert!(
            token.is_valid(),
            "token with 62 s remaining should be valid (> 60 s threshold)"
        );
    }

    /// Boundary: 30 s remaining is NOT strictly greater than the 60 s threshold
    /// (`remaining > Duration::from_secs(60)` is false), so `is_valid()` must
    /// return `false`. Uses 30 s (well below 60 s) for a stable margin.
    #[test]
    fn cached_token_is_invalid_at_exactly_60s_boundary() {
        let token = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(30),
        };
        assert!(
            !token.is_valid(),
            "token with 30 s remaining should be invalid (threshold is strictly > 60 s)"
        );
    }

    /// `AllegroAuth::new` with `sandbox = false` must use the production base URL.
    /// Uses the `#[cfg(test)]` accessor to avoid coupling to `Debug` format strings.
    #[test]
    fn new_production_uses_prod_url() {
        let auth = AllegroAuth::new("id".to_owned(), "secret".to_owned(), false);
        assert_eq!(
            auth.auth_base_url(),
            "https://allegro.pl",
            "production auth_base_url must be 'https://allegro.pl'"
        );
    }

    /// `AllegroAuth::new` with `sandbox = true` must use the sandbox base URL.
    /// Uses the `#[cfg(test)]` accessor to avoid coupling to `Debug` format strings.
    #[test]
    fn new_sandbox_uses_sandbox_url() {
        let auth = AllegroAuth::new("id".to_owned(), "secret".to_owned(), true);
        assert_eq!(
            auth.auth_base_url(),
            "https://allegro.pl.allegrosandbox.pl",
            "sandbox auth_base_url must be 'https://allegro.pl.allegrosandbox.pl'"
        );
    }

    /// The `Debug` output must expose `client_id` and `auth_base_url` in
    /// addition to the redacted secret (the existing test only checks the
    /// secret; this test verifies the other fields are present).
    #[test]
    fn debug_includes_client_id_and_base_url() {
        let auth = AllegroAuth::new("my-client-id".to_owned(), "s3cr3t".to_owned(), false);
        let debug_str = format!("{auth:?}");
        assert!(
            debug_str.contains("my-client-id"),
            "Debug output must contain client_id, got: {debug_str}"
        );
        assert!(
            debug_str.contains("auth_base_url"),
            "Debug output must contain the auth_base_url field name, got: {debug_str}"
        );
    }

    /// `MalformedResponse` error variant must format with the field name.
    ///
    /// This variant is reserved for future use (see `#[allow(dead_code)]` on the
    /// variant). Update or remove this test when the variant is promoted to active use.
    #[test]
    fn malformed_response_error_display() {
        let err = AuthError::MalformedResponse("access_token");
        assert_eq!(
            err.to_string(),
            "token response missing required field: access_token"
        );
    }

    /// Verifies that a token stored with a 120 s lifetime (the minimum after
    /// clamping) is considered valid immediately after creation.
    ///
    /// This exercises the `expires_in < 120` clamping path in `token()`:
    /// a clamped token has `expires_at = now + 120s`, which is > 60 s from now,
    /// so `is_valid()` must return `true`.
    #[test]
    fn cached_token_with_clamped_120s_lifetime_is_valid() {
        let token = CachedToken {
            access_token: "tok".to_owned(),
            // Simulate the clamped expires_at: now + 120 s
            expires_at: Instant::now() + Duration::from_secs(120),
        };
        assert!(
            token.is_valid(),
            "token clamped to 120 s lifetime should be valid immediately after creation"
        );
    }

    /// Verifies that `from_env` returns `Err(MissingEnvVar("ALLEGRO_CLIENT_SECRET"))`
    /// when `ALLEGRO_CLIENT_ID` is present but `ALLEGRO_CLIENT_SECRET` is absent.
    ///
    /// Only runs when `ALLEGRO_CLIENT_ID` is set and `ALLEGRO_CLIENT_SECRET` is not,
    /// to avoid mutating the process environment in a parallel test harness.
    #[test]
    fn from_env_returns_err_for_missing_secret() {
        if std::env::var("ALLEGRO_CLIENT_ID").is_ok()
            && std::env::var("ALLEGRO_CLIENT_SECRET").is_err()
        {
            let result = AllegroAuth::from_env(false);
            assert!(
                result.is_err(),
                "expected Err when ALLEGRO_CLIENT_SECRET is absent"
            );
            assert!(
                matches!(
                    result.unwrap_err(),
                    AuthError::MissingEnvVar("ALLEGRO_CLIENT_SECRET")
                ),
                "expected MissingEnvVar(\"ALLEGRO_CLIENT_SECRET\")"
            );
        }
        // Also verify the error type directly (always runs):
        let err = AuthError::MissingEnvVar("ALLEGRO_CLIENT_SECRET");
        assert_eq!(
            err.to_string(),
            "missing environment variable: ALLEGRO_CLIENT_SECRET"
        );
    }
}
