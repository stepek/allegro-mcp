//! allegro-mcp — MCP server for the Allegro REST API.
//!
//! Phase 4: client_credentials auth with in-memory token cache.

mod auth;

use anyhow::Result;
use clap::Parser;
use tracing::{debug, info, warn};

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
    // Use chars().take(8) to safely handle any UTF-8 token prefix.
    // Logged at DEBUG to avoid leaking token material at INFO level.
    debug!(
        token_prefix = token1.chars().take(8).collect::<String>(),
        "first token obtained"
    );
    info!("first token obtained");

    // Smoke test: second call — should reuse cached token.
    let token2 = auth.token().await?;
    debug!(
        token_prefix = token2.chars().take(8).collect::<String>(),
        "second token obtained (should be cached)"
    );
    info!("second token obtained (should be cached)");

    if token1 != token2 {
        warn!("second token call returned a different token — cache may not be working");
    }

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
        warn!(
            status = status.as_u16(),
            body, "Smoke test: unexpected status"
        );
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
