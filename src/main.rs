//! allegro-mcp — MCP server for the Allegro REST API.

mod auth;
mod dispatcher;
mod schema;
mod server;
mod tool_registry;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

/// allegro-mcp: MCP server for the Allegro REST API.
#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Increase log verbosity (repeat for more: -v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Use the Allegro sandbox environment instead of production.
    #[arg(long, default_value_t = false)]
    sandbox: bool,

    /// Override the schema URL (default: https://developer.allegro.pl/swagger.yaml)
    #[arg(long, global = true)]
    schema_url: Option<String>,

    /// Use a local schema file instead of fetching from URL
    #[arg(long, global = true)]
    schema_file: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Schema inspection commands
    Schema {
        #[command(subcommand)]
        action: SchemaAction,
    },
    /// Tool registry commands
    Tools {
        #[command(subcommand)]
        action: ToolsAction,
    },
}

#[derive(Debug, Subcommand)]
enum SchemaAction {
    /// Print path/operation/parameter counts from the schema
    Stats,
}

#[derive(Debug, Subcommand)]
enum ToolsAction {
    /// List all tools derived from the schema
    List,
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

    info!(sandbox = cli.sandbox, "allegro-mcp starting");

    let source = if let Some(path) = cli.schema_file {
        schema::SchemaSource::File(path)
    } else if let Some(url) = cli.schema_url {
        schema::SchemaSource::Url(url)
    } else {
        schema::SchemaSource::default()
    };

    match cli.command {
        Some(Commands::Schema {
            action: SchemaAction::Stats,
        }) => {
            let (api, raw) = schema::load(&source).await?;
            let stats = schema::compute_stats(&api, &raw);
            println!("Paths:      {}", stats.path_count);
            println!("Operations: {}", stats.operation_count);
            println!("Parameters: {}", stats.parameter_count);
            println!("SHA-256:    {}", stats.sha256);
        }
        Some(Commands::Tools {
            action: ToolsAction::List,
        }) => {
            let (api, _raw) = schema::load(&source).await?;
            let registry = tool_registry::ToolRegistry::from_openapi(&api)?;
            println!("Tools: {}", registry.len());
            for tool in registry.list_tools().iter().take(5) {
                println!("  - {} ({})", tool.id, tool.description);
            }
            if registry.len() > 5 {
                println!("  ... and {} more", registry.len() - 5);
            }
        }
        None => {
            run_mcp_server(cli.sandbox, source).await?;
        }
    }

    Ok(())
}

/// Runs the MCP server over stdio: builds auth, loads the schema, builds the
/// tool registry, and serves `tools/list` + `tools/call` until stdin EOF.
async fn run_mcp_server(sandbox: bool, source: schema::SchemaSource) -> Result<()> {
    tracing::info!(sandbox, "starting MCP server (stdio transport)");

    let auth = auth::AllegroAuth::from_env(sandbox)
        .map_err(|e| anyhow::anyhow!("auth init failed: {e}"))?;

    let (api, _raw) = schema::load(&source)
        .await
        .map_err(|e| anyhow::anyhow!("schema load failed: {e}"))?;
    let registry = tool_registry::ToolRegistry::from_openapi(&api)
        .map_err(|e| anyhow::anyhow!("registry build failed: {e}"))?;

    tracing::info!(tool_count = registry.len(), "tool registry built");

    let handler = server::AllegroServer::new(registry, auth, sandbox);

    let transport = rmcp::transport::io::stdio();

    let running = rmcp::serve_server(handler, transport)
        .await
        .map_err(|e| anyhow::anyhow!("MCP server init failed: {e}"))?;

    tracing::info!("MCP server ready");

    if let Err(e) = running.waiting().await {
        tracing::warn!("MCP server exited with error: {e}");
    }

    tracing::info!("MCP server shut down");
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
