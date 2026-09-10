//! allegro-mcp — MCP server for the Allegro REST API.

mod auth;
mod config;
mod dispatcher;
mod http;
mod http_server;
mod resilience;
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
    ///
    /// `global = true` so it can be given *after* the subcommand too
    /// (`allegro-mcp auth device --sandbox`), not only before it.
    #[arg(long, default_value_t = false, global = true)]
    sandbox: bool,

    /// Override the schema URL (default: https://developer.allegro.pl/swagger.yaml)
    #[arg(long, global = true)]
    schema_url: Option<String>,

    /// Use a local schema file instead of fetching from URL
    #[arg(long, global = true)]
    schema_file: Option<std::path::PathBuf>,

    /// Path to the config file (default: discover allegro-mcp.toml)
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,

    /// Override the User-Agent sent on every request
    /// (format: "AppName/Version (+URL)")
    #[arg(long, global = true)]
    user_agent: Option<String>,

    /// Use the stdio MCP transport instead of the default Streamable HTTP
    /// transport. Intended for local development / editor-embedded clients
    /// (Claude Desktop, etc.) — production deployments use the HTTP transport
    /// (Open WebUI is the primary client).
    #[arg(long, default_value_t = false)]
    stdio: bool,

    /// TCP port for the HTTP transport (ignored when `--stdio` is set).
    /// Falls back to `$PORT`, then `8080`. The bind address is always
    /// `0.0.0.0` — this is a container-first deployment, not a flag.
    #[arg(long, global = true)]
    port: Option<u16>,

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
    /// Authorization commands (OAuth2 device flow)
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Internal: HTTP liveness probe used by the Docker HEALTHCHECK
    /// (distroless images have no shell, so `curl`/`wget` aren't
    /// available — the binary probes itself instead).
    Healthcheck,
}

#[derive(Debug, Subcommand)]
enum AuthAction {
    /// Authorize this machine with Allegro via the OAuth2 device flow.
    ///
    /// Prints a verification URL (+ short user code) to stderr, polls until
    /// you approve or deny in the browser, then persists the token pair
    /// atomically (0600) for server-side use. Safe to re-run: an unexpired
    /// pending grant is resumed instead of re-requesting authorization.
    Device {
        /// Override where tokens are persisted (config file / default
        /// otherwise). Parent directories are created if missing.
        #[arg(long)]
        token_path: Option<std::path::PathBuf>,
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

    if matches!(cli.command, Some(Commands::Healthcheck)) {
        return run_healthcheck().await;
    }

    // Re-package CLI flags for the config pipeline (config.rs stays
    // clap-independent). A bare `--sandbox` flag can only express `true`,
    // so `None` means "not specified" and env/file values survive.
    let overrides = config::CliOverrides {
        sandbox: cli.sandbox.then_some(true),
        user_agent: cli.user_agent.clone(),
        schema_url: cli.schema_url.clone(),
        schema_file: cli.schema_file.clone(),
        config: cli.config.clone(),
    };

    // Config load = the User-Agent validation gate: nothing network-facing
    // (schema fetch, auth, client building) happens before it, so an
    // invalid UA aborts startup with zero network syscalls.
    let cfg = config::Config::load(&overrides)?;

    let user_agent = cfg
        .user_agent
        .clone()
        .unwrap_or_else(|| http::DEFAULT_USER_AGENT.to_owned());
    info!(
        sandbox = cfg.sandbox,
        user_agent = %user_agent,
        auth_flow = ?cfg.auth_flow,
        token_path = ?cfg.token_path,
        tools_filters = ?cfg.tools,
        rate_limit_rpm = cfg.rate_limit_rpm(),
        "allegro-mcp starting"
    );

    // Two config-driven clients: api_client (auth + API, no timeout) and
    // schema_client (10 s timeout) — both shared immutably.
    let api_client = http::build_client(&user_agent, &cfg.accept_language)?;
    let schema_client = http::build_schema_client(&user_agent, &cfg.accept_language)?;

    // Schema-source resolution shared by all three modes; the merged config
    // already encodes CLI file > CLI url > env file > env url > config file
    // > config url, with `file` winning whenever both fields are set.
    let source = resolve_schema_source(&cfg);

    match cli.command {
        Some(Commands::Schema {
            action: SchemaAction::Stats,
        }) => {
            let (api, raw) = schema::load_with_client(&schema_client, &source).await?;
            let stats = schema::compute_stats(&api, &raw);
            println!("Paths:      {}", stats.path_count);
            println!("Operations: {}", stats.operation_count);
            println!("Parameters: {}", stats.parameter_count);
            println!("SHA-256:    {}", stats.sha256);
        }
        Some(Commands::Tools {
            action: ToolsAction::List,
        }) => {
            let (api, _raw) = schema::load_with_client(&schema_client, &source).await?;
            let registry = tool_registry::ToolRegistry::from_openapi(&api)?;
            println!("Tools: {}", registry.len());
            for tool in registry.list_tools().iter().take(5) {
                println!("  - {} ({})", tool.id, tool.description);
            }
            if registry.len() > 5 {
                println!("  ... and {} more", registry.len() - 5);
            }
        }
        Some(Commands::Auth {
            action: AuthAction::Device { token_path },
        }) => {
            // `auth device` must not require the OpenAPI schema: the schema
            // source above is resolved lazily (no I/O), so dispatching here
            // skips schema fetch/registry work entirely.
            run_auth_device(cfg, api_client, token_path).await?;
        }
        Some(Commands::Healthcheck) => {
            // Unreachable: handled by the early return above, before config
            // loading / network setup. Kept as an explicit arm (rather than
            // a wildcard `_ =>`) so adding a future `Commands` variant here
            // forces a compile error instead of silently falling through.
            unreachable!("Commands::Healthcheck is handled by the early return in main()")
        }
        None => {
            if cli.stdio {
                run_mcp_server(cfg, api_client, schema_client, source).await?;
            } else {
                let port = cli
                    .port
                    .or_else(|| std::env::var("PORT").ok().and_then(|v| v.parse().ok()))
                    .unwrap_or(8080);
                let handler =
                    build_allegro_server(&cfg, api_client, &schema_client, &source).await?;
                http_server::run_http_server(handler, cfg.sandbox, port).await?;
            }
        }
    }

    Ok(())
}

/// Resolves the effective schema source from the merged config
/// (file > url > default).
fn resolve_schema_source(cfg: &config::Config) -> schema::SchemaSource {
    if let Some(file) = &cfg.schema_file {
        schema::SchemaSource::File(file.clone())
    } else if let Some(url) = &cfg.schema_url {
        schema::SchemaSource::Url(url.clone())
    } else {
        schema::SchemaSource::default()
    }
}

/// Implements `allegro-mcp auth device`: runs the interactive OAuth2 device
/// flow — request a device code, print the verification banner to stderr,
/// poll until approval/denial/expiry, then persist the token pair atomically
/// to the token store.
///
/// Resume (acceptance criterion: kill mid-poll → restart → continue): an
/// unexpired pending grant from a previous run is reused — the same
/// single-use `device_code` is polled again, and the banner says so. No
/// Ctrl-C handler: a SIGINT kill is fine because the pending grant is on
/// disk before the banner is printed.
async fn run_auth_device(
    cfg: config::Config,
    api_client: reqwest::Client,
    token_path_override: Option<std::path::PathBuf>,
) -> Result<()> {
    // Same credential mapping as `build_allegro_server` — one contract.
    let client_id = std::env::var("ALLEGRO_CLIENT_ID")
        .map_err(|_| anyhow::anyhow!("missing environment variable: ALLEGRO_CLIENT_ID"))?;
    let client_secret = std::env::var("ALLEGRO_CLIENT_SECRET")
        .map_err(|_| anyhow::anyhow!("missing environment variable: ALLEGRO_CLIENT_SECRET"))?;

    // Path precedence: subcommand `--token-path` > config `token_path` >
    // platform default (`dirs::config_dir()/allegro-mcp/tokens.json`).
    let token_path = match token_path_override.or_else(|| cfg.token_path.clone()) {
        Some(p) => p,
        None => {
            auth::token_store::TokenStore::default_path().map_err(|e| anyhow::anyhow!("{e}"))?
        }
    };
    let store = auth::token_store::TokenStore::new(token_path, cfg.sandbox);

    let deps = auth::device::DeviceFlowDeps {
        http: api_client,
        auth_base_url: config::auth_base_url(cfg.sandbox).to_owned(),
        client_id,
        client_secret,
        scopes: cfg.scopes.clone(),
        policy: auth::device::PollingPolicy::production(),
    };

    let now = auth::token_store::epoch_now();
    let stored = store.load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let (grant, resumed) = match stored.pending {
        Some(pending) if now < pending.expires_at_epoch => {
            (pending.to_authorization_response(), true)
        }
        _ => {
            let resp = auth::device::request_device_code(&deps)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            store
                .save_pending(&resp)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            (resp, false)
        }
    };

    // stderr on purpose: the banner must survive `docker logs` and must not
    // corrupt stdout (which carries the machine-readable summary below).
    eprint!(
        "{}",
        auth::device::banner_text(&grant, cfg.sandbox, resumed)
    );

    let interval = deps.policy.effective_interval(grant.interval);
    let state = auth::device::PollState::new(interval.as_secs(), grant.expires_in);
    let tokens = match auth::device::poll_for_token(&deps, &grant.device_code, state).await {
        Ok(tokens) => tokens,
        Err(auth::device::DeviceFlowError::AccessDenied) => {
            anyhow::bail!("authorization denied by user");
        }
        Err(auth::device::DeviceFlowError::Expired) => {
            anyhow::bail!("device/user code expired — rerun `allegro-mcp auth device`");
        }
        Err(auth::device::DeviceFlowError::Network(reason)) => {
            anyhow::bail!("device flow failed after repeated transient errors: {reason}");
        }
    };

    // `save_tokens` atomically replaces the whole envelope (dropping the
    // pending grant), so grant completion + pending clearance is one
    // crash-safe operation.
    let expires_in = tokens.expires_in;
    let scope = tokens.scope.clone();
    store
        .save_tokens(&tokens, expires_in)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("Tokens saved to {}", store.path().display());
    println!(
        "Access token expires in {} (expires_in = {expires_in} s)",
        format_duration_hm(expires_in)
    );
    if let Some(scope) = scope {
        println!("Scope: {scope}");
    }
    Ok(())
}

/// Human-readable seconds: `12h 30m` / `45m` / `45s`.
fn format_duration_hm(secs: u64) -> String {
    if secs >= 3600 {
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Shared server-build logic for both transports: reads
/// `ALLEGRO_CLIENT_ID`/`ALLEGRO_CLIENT_SECRET`, builds `AllegroAuth`,
/// loads the schema, builds the tool registry, and wires up the
/// `AllegroServer` handler. Transport-specific code (`run_mcp_server`'s
/// stdio serve loop; `http_server::run_http_server`'s axum router +
/// eager auth-check banner) picks up from the returned handler.
async fn build_allegro_server(
    cfg: &config::Config,
    api_client: reqwest::Client,
    schema_client: &reqwest::Client,
    source: &schema::SchemaSource,
) -> Result<server::AllegroServer> {
    // Client credentials stay in the classic env vars (auth module contract);
    // everything else about the token request (UA, Accept-Language, host,
    // scopes) comes from the config-driven client below.
    let client_id = std::env::var("ALLEGRO_CLIENT_ID")
        .map_err(|_| anyhow::anyhow!("missing environment variable: ALLEGRO_CLIENT_ID"))?;
    let client_secret = std::env::var("ALLEGRO_CLIENT_SECRET")
        .map_err(|_| anyhow::anyhow!("missing environment variable: ALLEGRO_CLIENT_SECRET"))?;

    // Device mode persists tokens at the effective `token_path` (config
    // wins over the platform default); client_credentials mode has no
    // store. The handle is kept out so the startup policy can inspect the
    // pending grant even though the store itself is owned by `auth`.
    let device_store = match cfg.auth_flow {
        config::AuthFlow::DeviceCode => {
            let token_path = match &cfg.token_path {
                Some(p) => p.clone(),
                None => auth::token_store::TokenStore::default_path()
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
            };
            Some(auth::token_store::TokenStore::new(token_path, cfg.sandbox))
        }
        config::AuthFlow::ClientCredentials => None,
    };

    let auth = {
        let base = auth::AllegroAuth::with_http_client(
            client_id.clone(),
            client_secret.clone(),
            config::auth_base_url(cfg.sandbox).to_owned(),
            api_client.clone(),
        )
        .with_scopes(cfg.scopes.clone());
        match device_store.clone() {
            Some(store) => base.with_token_store(store),
            None => base,
        }
    };

    let (api, _raw) = schema::load_with_client(schema_client, source)
        .await
        .map_err(|e| anyhow::anyhow!("schema load failed: {e}"))?;
    let registry = tool_registry::ToolRegistry::from_openapi(&api)
        .map_err(|e| anyhow::anyhow!("registry build failed: {e}"))?;
    tracing::info!(tool_count = registry.len(), "tool registry built");

    // Cheap clone (Arc internals) — the device resume task must reuse the
    // config-driven client (ToS-compliant UA), never a bare default one.
    let api_client_for_resume = api_client.clone();
    let server = server::AllegroServer::new(registry, auth, cfg.sandbox)
        .with_http_client(api_client)
        .with_resilience(resilience::Resilience::production(cfg.rate_limit_rpm()));

    // Device-mode startup policy (shared by both transports): restore the
    // persisted authorization, resume an unexpired pending grant in the
    // background, or fail loudly with the `auth device` instruction.
    if let Some(store) = &device_store {
        run_device_startup_policy(
            &server,
            store,
            cfg,
            &client_id,
            &client_secret,
            &api_client_for_resume,
        )
        .await?;
    }

    Ok(server)
}

/// Device-mode startup policy:
///
/// - stored tokens resolve → info "restored persisted authorization" — the
///   HTTP transport's eager check then passes unchanged;
/// - nothing cached but an **unexpired pending grant** exists → print the
///   verification banner (stderr / docker logs) and spawn a background
///   resume poll; the server starts anyway and user-scoped tools return
///   per-call `auth error`s until the grant completes;
/// - nothing usable → hard startup failure with the
///   `allegro-mcp auth device` instruction.
async fn run_device_startup_policy(
    server: &server::AllegroServer,
    store: &auth::token_store::TokenStore,
    cfg: &config::Config,
    client_id: &str,
    client_secret: &str,
    api_client: &reqwest::Client,
) -> Result<()> {
    let auth_handle = server.auth_handle();
    match auth_handle.token().await {
        Ok(_) => {
            let status = auth_handle.status().await;
            info!(
                "restored persisted authorization (expires in {} s)",
                status.expires_in_secs.unwrap_or(0)
            );
            Ok(())
        }
        Err(auth::AuthError::ReauthRequired { .. }) => {
            let state = store.load().map_err(|e| anyhow::anyhow!("{e}"))?;
            let pending = state
                .pending
                .filter(|p| auth::token_store::epoch_now() < p.expires_at_epoch);
            let Some(pending) = pending else {
                anyhow::bail!(
                    "no Allegro authorization found — run `allegro-mcp auth device` first"
                );
            };

            let grant = pending.to_authorization_response();
            eprint!("{}", auth::device::banner_text(&grant, cfg.sandbox, true));

            let deps = auth::device::DeviceFlowDeps {
                http: api_client.clone(),
                auth_base_url: config::auth_base_url(cfg.sandbox).to_owned(),
                client_id: client_id.to_owned(),
                client_secret: client_secret.to_owned(),
                scopes: cfg.scopes.clone(),
                policy: auth::device::PollingPolicy::production(),
            };
            let expires_in = grant.expires_in;
            tokio::spawn(async move {
                let interval = deps.policy.effective_interval(grant.interval);
                let state = auth::device::PollState::new(interval.as_secs(), expires_in);
                match auth::device::poll_for_token(&deps, &grant.device_code, state).await {
                    Ok(tokens) => {
                        if let Err(e) = auth_handle.install_tokens(&tokens).await {
                            tracing::error!(
                                "device authorization completed but persisting the tokens failed: {e}"
                            );
                            return;
                        }
                        tracing::info!(
                            "device authorization completed — tokens persisted and cached"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            "device authorization failed: {e} — run `allegro-mcp auth device` to start a new one"
                        );
                    }
                }
            });
            Ok(())
        }
        // Store corruption, env mismatch, version errors, network hiccups —
        // all fatal at startup; the message tells the operator what to fix.
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

/// Runs the MCP server over stdio: builds auth, loads the schema, builds the
/// tool registry, and serves `tools/list` + `tools/call` until stdin EOF.
async fn run_mcp_server(
    cfg: config::Config,
    api_client: reqwest::Client,
    schema_client: reqwest::Client,
    source: schema::SchemaSource,
) -> Result<()> {
    tracing::info!(
        sandbox = cfg.sandbox,
        "starting MCP server (stdio transport)"
    );

    let handler = build_allegro_server(&cfg, api_client, &schema_client, &source).await?;

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

/// Docker `HEALTHCHECK` probe: `GET http://127.0.0.1:$PORT/health` with a
/// short timeout. Reads the same `$PORT` (default 8080) the HTTP server
/// binds to. Exits non-zero (via `anyhow::Error`) on any failure — timeout,
/// connection refused, or non-2xx status — which Docker treats as unhealthy.
async fn run_healthcheck() -> Result<()> {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(8080);
    let url = format!("http://127.0.0.1:{port}/health");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?;
    let resp = client.get(&url).send().await?;

    if resp.status().is_success() {
        Ok(())
    } else {
        anyhow::bail!("healthcheck failed: HTTP {}", resp.status());
    }
}

#[cfg(test)]
mod tests {
    use super::verbosity_level;
    use super::{AuthAction, Cli, Commands};
    use clap::Parser;

    #[test]
    fn stdio_flag_defaults_to_false() {
        let cli = Cli::parse_from(["allegro-mcp"]);
        assert!(!cli.stdio);
    }

    #[test]
    fn stdio_flag_parses() {
        let cli = Cli::parse_from(["allegro-mcp", "--stdio"]);
        assert!(cli.stdio);
    }

    #[test]
    fn port_flag_parses() {
        let cli = Cli::parse_from(["allegro-mcp", "--port", "9090"]);
        assert_eq!(cli.port, Some(9090));
    }

    #[test]
    fn port_flag_defaults_to_none() {
        let cli = Cli::parse_from(["allegro-mcp"]);
        assert_eq!(cli.port, None);
    }

    // ── auth device subcommand ───────────────────────────────────────────────

    #[test]
    fn auth_device_subcommand_parses() {
        let cli = Cli::parse_from(["allegro-mcp", "auth", "device"]);
        assert!(matches!(
            cli.command,
            Some(Commands::Auth {
                action: AuthAction::Device { token_path: None }
            })
        ));
    }

    #[test]
    fn auth_device_token_path_flag_parses() {
        let cli = Cli::parse_from([
            "allegro-mcp",
            "auth",
            "device",
            "--token-path",
            "/tmp/tokens.json",
        ]);
        match cli.command {
            Some(Commands::Auth {
                action: AuthAction::Device { token_path },
            }) => {
                assert_eq!(
                    token_path.as_deref(),
                    Some(std::path::Path::new("/tmp/tokens.json"))
                );
            }
            other => panic!("expected `auth device`, got: {other:?}"),
        }
    }

    /// `--sandbox` is `global = true` — it must parse *after* the
    /// subcommand too, not only before it.
    #[test]
    fn sandbox_flag_parses_after_subcommand() {
        let cli = Cli::parse_from(["allegro-mcp", "auth", "device", "--sandbox"]);
        assert!(
            cli.sandbox,
            "--sandbox must be accepted after the subcommand"
        );
    }

    /// The pre-existing before-subcommand position keeps working.
    #[test]
    fn sandbox_flag_still_parses_before_subcommand() {
        let cli = Cli::parse_from(["allegro-mcp", "--sandbox", "auth", "device"]);
        assert!(cli.sandbox);
        assert!(matches!(
            cli.command,
            Some(Commands::Auth {
                action: AuthAction::Device { token_path: None }
            })
        ));
    }

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
