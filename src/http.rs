//! Central HTTP client factory — the single place in the process that
//! constructs [`reqwest::Client`] instances.
//!
//! Every outbound request (OAuth token, Allegro API, schema download) must
//! carry a ToS-compliant `User-Agent` (Allegro REST API ToS art. 3.4(c)) and
//! the configured `Accept-Language`. Routing every client through this
//! module guarantees that no request can leave the process without those
//! headers — see [`build_client`] / [`build_schema_client`] and their
//! `default_*` counterparts.

use std::sync::LazyLock;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT_LANGUAGE};
use reqwest::Client;

/// Default `User-Agent`, generated at compile time from `Cargo.toml` so it
/// can never drift from the published version.
///
/// Format required by Allegro: `AppName/Version (+URL)` — see
/// <https://apps.developer.allegro.pl/user-agent>.
pub const DEFAULT_USER_AGENT: &str = concat!(
    "allegro-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/stepek/allegro-mcp)"
);

/// Default `Accept` media type — Allegro public API, version 1.
pub const DEFAULT_ACCEPT: &str = "application/vnd.allegro.public.v1+json";

/// Default `Accept-Language` — Polish, per the Allegro tutorial's example.
pub const DEFAULT_ACCEPT_LANGUAGE: &str = "pl-PL";

/// Timeout applied to schema-download clients (unchanged from the previous
/// `src/schema/fetch.rs` static client).
pub const SCHEMA_TIMEOUT_SECS: u64 = 10;

/// Errors constructing an HTTP client.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// `Accept-Language` (or any header we build ourselves via
    /// `HeaderValue::from_str`) was not a valid header value.
    #[error("invalid header value: {0}")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),

    /// `reqwest::ClientBuilder::build()` failed — this is also where an
    /// invalid `User-Agent` string surfaces, since reqwest stores the
    /// conversion error internally and defers it to `build()`.
    #[error("client build failed: {0}")]
    Build(#[from] reqwest::Error),
}

/// Shared builder setup: `User-Agent` + `Accept-Language` default header.
fn builder(user_agent: &str, accept_language: &str) -> Result<reqwest::ClientBuilder, HttpError> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_str(accept_language)?);
    Ok(Client::builder()
        .user_agent(user_agent)
        .default_headers(headers))
}

/// Builds a client for OAuth token requests and Allegro API calls — no
/// timeout (matches existing auth/dispatcher behavior).
pub fn build_client(user_agent: &str, accept_language: &str) -> Result<Client, HttpError> {
    Ok(builder(user_agent, accept_language)?.build()?)
}

/// Builds a client for schema downloads — same headers as [`build_client`],
/// plus a [`SCHEMA_TIMEOUT_SECS`]-second timeout (preserves the guard that
/// used to live in `src/schema/fetch.rs`'s static client).
pub fn build_schema_client(user_agent: &str, accept_language: &str) -> Result<Client, HttpError> {
    Ok(builder(user_agent, accept_language)?
        .timeout(Duration::from_secs(SCHEMA_TIMEOUT_SECS))
        .build()?)
}

/// The default (no-config) client for OAuth/API calls, built once and
/// pooled. Backs [`crate::auth::AllegroAuth::with_base_url`] and
/// [`crate::server::AllegroServer::new`] so they always carry a UA even
/// without going through [`crate::config::Config`].
pub fn default_client() -> Client {
    static CLIENT: LazyLock<Client> = LazyLock::new(|| {
        build_client(DEFAULT_USER_AGENT, DEFAULT_ACCEPT_LANGUAGE)
            .expect("statically valid default client")
    });
    CLIENT.clone()
}

/// The default (no-config) client for schema downloads, built once and
/// pooled. Backs [`crate::schema::fetch::fetch_bytes`] /
/// [`crate::schema::load`] backward-compat wrappers.
pub fn default_schema_client() -> Client {
    static CLIENT: LazyLock<Client> = LazyLock::new(|| {
        build_schema_client(DEFAULT_USER_AGENT, DEFAULT_ACCEPT_LANGUAGE)
            .expect("statically valid default schema client")
    });
    CLIENT.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_user_agent_matches_required_format() {
        assert!(
            DEFAULT_USER_AGENT.starts_with("allegro-mcp/"),
            "default UA must start with 'allegro-mcp/', got: {DEFAULT_USER_AGENT}"
        );
        assert!(
            DEFAULT_USER_AGENT.contains(" (+https://"),
            "default UA must contain ' (+https://', got: {DEFAULT_USER_AGENT}"
        );
        assert!(
            DEFAULT_USER_AGENT.ends_with(')'),
            "default UA must end with ')', got: {DEFAULT_USER_AGENT}"
        );
    }

    #[test]
    fn test_build_client_returns_client_for_valid_inputs() {
        let result = build_client(DEFAULT_USER_AGENT, DEFAULT_ACCEPT_LANGUAGE);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[test]
    fn test_build_client_invalid_accept_language_errors_at_build() {
        // A newline is not a valid HeaderValue byte.
        let result = build_client(DEFAULT_USER_AGENT, "pl-PL\n");
        assert!(
            matches!(result, Err(HttpError::InvalidHeader(_))),
            "invalid Accept-Language must surface as InvalidHeader, got: {result:?}"
        );
    }

    #[test]
    fn test_build_client_invalid_user_agent_errors_at_build() {
        // reqwest defers the invalid UA conversion error to `.build()`.
        let result = build_client("allegro-mcp/0.1.0\n", DEFAULT_ACCEPT_LANGUAGE);
        assert!(
            matches!(result, Err(HttpError::Build(_))),
            "invalid User-Agent must surface as Build, got: {result:?}"
        );
    }

    #[test]
    fn test_build_schema_client_returns_client_for_valid_inputs() {
        let result = build_schema_client(DEFAULT_USER_AGENT, DEFAULT_ACCEPT_LANGUAGE);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[test]
    fn test_default_client_does_not_panic() {
        let _ = default_client();
    }

    #[test]
    fn test_default_schema_client_does_not_panic() {
        let _ = default_schema_client();
    }
}
