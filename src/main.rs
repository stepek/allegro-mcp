//! allegro-mcp — MCP server for the Allegro REST API.
//!
//! Phase 1: skeleton that compiles and exits cleanly.
//! Real server logic is added in subsequent phases.

use anyhow::Result;
use clap::Parser;
use tracing::info;

/// allegro-mcp: MCP server for the Allegro REST API.
#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Increase log verbosity (repeat for more: -v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let level = match cli.verbose {
        0 => tracing::Level::WARN,
        1 => tracing::Level::INFO,
        2 => tracing::Level::DEBUG,
        _ => tracing::Level::TRACE,
    };

    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .init();

    info!("allegro-mcp starting (phase 1 skeleton)");

    // TODO(gh-2): initialise MCP server and connect stdio transport
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        // Placeholder — verifies the test harness works.
        assert_eq!(2 + 2, 4);
    }
}
