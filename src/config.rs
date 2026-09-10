//! Configuration system — one merged [`Config`] (file → env → CLI) driving
//! the whole server: sandbox flag, User-Agent, Accept-Language, schema
//! source, OAuth flow, scopes, and tool filters (Phase 10).
//!
//! Precedence is CLI > env > file > defaults. The User-Agent is validated at
//! load time (see [`validate_user_agent`]) so an invalid value aborts startup
//! before any network I/O happens, per Allegro's ToS art. 3.4(c) — see
//! <https://apps.developer.allegro.pl/user-agent>.
//!
//! This module also owns the single source of truth for environment hosts:
//! [`api_base_url`] / [`auth_base_url`]. Tokens are NOT interchangeable
//! between production and sandbox, so one flag must always swap BOTH hosts.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Link to Allegro's User-Agent documentation (referenced by every
/// User-Agent validation error so misconfigured users land on the right page).
const USER_AGENT_DOC_URL: &str = "https://apps.developer.allegro.pl/user-agent";

/// Errors that can occur while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Malformed TOML — the path names the offending file.
    #[error("failed to parse config file {}: {source}", path.display())]
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },

    /// `deny_unknown_fields` rejection — a typo'd key must never be silently
    /// ignored (users would silently lose ToS compliance).
    #[error(
        "unknown key `{key}` in config file {} — every key must match the schema",
        path.display()
    )]
    UnknownKey { path: PathBuf, key: String },

    /// User-Agent failed structural validation. The message carries the
    /// offending value, the expected format, and the Allegro doc link.
    #[error(
        "invalid User-Agent {value:?}: {reason}\n  expected format: `AppName/Version (+URL)`, e.g. \
         `TestApplication/1.1.0 (+https://firma.com/TestApplication-info)`\n  see {USER_AGENT_DOC_URL}"
    )]
    InvalidUserAgent { value: String, reason: String },

    /// Unparseable `ALLEGRO_MCP_*` env value — never silently defaulted
    /// (a wrong-guess sandbox/prod mix would send prod tokens to sandbox).
    #[error(
        "invalid value for environment variable {var}: {value:?} \
         (expected one of true/1/yes or false/0/no)"
    )]
    InvalidEnv { var: &'static str, value: String },

    /// A `scopes` entry would corrupt the space-joined OAuth2 `scope` param.
    #[error("invalid scope entry {value:?}: {reason}")]
    InvalidScopes { value: String, reason: String },
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// OAuth2 flow used to obtain application tokens.
///
/// Only `client_credentials` exists today; the enum (with
/// `#[serde(rename_all = "snake_case")]`, so TOML writes
/// `auth_flow = "client_credentials"`) keeps the config file key
/// forward-compatible with future flows. Unknown values are startup errors
/// listing the supported ones.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthFlow {
    #[default]
    ClientCredentials,
}

/// Tool name filters (`allow` / `deny` prefix lists) — parsed here, consumed
/// by the registry filtering hooks in Phase 10.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolFilters {
    /// Allowed tool-name prefixes. Bin-tree dead code until Phase 10 wires
    /// the filters into the registry (derived `Debug` reads don't count for
    /// dead-code analysis).
    #[allow(dead_code)]
    pub allow: Vec<String>,
    /// Denied tool-name prefixes — see [`Self::allow`].
    #[allow(dead_code)]
    pub deny: Vec<String>,
}

/// The effective server configuration.
///
/// Container-level `#[serde(default)]` (backed by the manual [`Default`]
/// impl below) means **any subset of TOML keys parses** — `sandbox = true`
/// alone is a valid file — while `deny_unknown_fields` turns typos into hard
/// errors.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Target the sandbox environment — swaps BOTH the API and the auth host
    /// (see [`api_base_url`] / [`auth_base_url`]).
    pub sandbox: bool,
    /// Custom `User-Agent`; `None` → [`crate::http::DEFAULT_USER_AGENT`].
    /// Validated by [`validate_user_agent`] before any network I/O.
    pub user_agent: Option<String>,
    /// `Accept-Language` sent on every request (`pl-PL` per the Allegro
    /// tutorial by default).
    pub accept_language: String,
    /// Schema download URL override.
    pub schema_url: Option<String>,
    /// Local schema file override (wins over `schema_url` at the same layer).
    pub schema_file: Option<PathBuf>,
    /// OAuth2 flow selection (see [`AuthFlow`]).
    pub auth_flow: AuthFlow,
    /// OAuth2 scopes requested on the token fetch (space-joined into the
    /// `scope` form param when non-empty).
    pub scopes: Vec<String>,
    /// On-disk token cache location — reserved for a future phase.
    pub token_path: Option<PathBuf>,
    /// Tool filters — parsed now, consumed in Phase 10.
    pub tools: Option<ToolFilters>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sandbox: false,
            user_agent: None,
            accept_language: crate::http::DEFAULT_ACCEPT_LANGUAGE.to_owned(),
            schema_url: None,
            schema_file: None,
            auth_flow: AuthFlow::default(),
            scopes: Vec::new(),
            token_path: None,
            tools: None,
        }
    }
}

/// CLI flags re-packaged for [`Config::load`] — keeps this module independent
/// of clap while preserving "CLI always wins" precedence.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `Some(true)` only when `--sandbox` was passed (a bare flag cannot
    /// force `false`, so `None` means "not specified").
    pub sandbox: Option<bool>,
    pub user_agent: Option<String>,
    pub schema_url: Option<String>,
    pub schema_file: Option<PathBuf>,
    pub config: Option<PathBuf>,
}

// ── Host helpers (single source of truth) ─────────────────────────────────────

/// The Allegro REST API base URL for the given environment.
///
/// Production and sandbox app registrations (and therefore tokens) are
/// separate; this and [`auth_base_url`] must always be flipped together via
/// the single `sandbox` flag.
pub fn api_base_url(sandbox: bool) -> &'static str {
    if sandbox {
        "https://api.allegrosandbox.pl"
    } else {
        "https://api.allegro.pl"
    }
}

/// The Allegro OAuth2 authorization-server base URL for the given environment.
pub fn auth_base_url(sandbox: bool) -> &'static str {
    if sandbox {
        "https://allegro.pl.allegrosandbox.pl"
    } else {
        "https://allegro.pl"
    }
}

// ── User-Agent validation ─────────────────────────────────────────────────────

/// Validates a User-Agent against Allegro's required structure:
/// `name "/" version " (+" url ")"`.
///
/// Rules (deliberately structural — Allegro's own validator is login-gated
/// JS, so exact character-level rules are not machine-readable):
/// - surrounding whitespace is trimmed first;
/// - **whole-string charset:** every byte must be visible ASCII
///   (`0x21..=0x7E`) — control characters and non-ASCII are rejected
///   anywhere — with exactly one exception: the single structural space
///   (`0x20`) preceding `(+`. This guarantees anything that passes is also a
///   valid `HeaderValue`, so a config UA can never pass startup and then
///   fail at client-build time with a worse message;
/// - `name`: 1..=64 visible-ASCII chars excluding the structural breakers
///   `/ ( ) +`;
/// - `version`: semver `MAJOR.MINOR.PATCH` (three non-empty ASCII-digit
///   groups), with an optional `-prerelease` suffix that is not deeply
///   validated;
/// - the string must end with `)`, and the URL between `(+` and `)` must
///   start with `http://` or `https://` and be non-empty afterwards.
///
/// Errors include the offending value, the expected format + example, and
/// <https://apps.developer.allegro.pl/user-agent>.
pub fn validate_user_agent(ua: &str) -> Result<(), ConfigError> {
    let ua = ua.trim();
    let invalid = |reason: &str| ConfigError::InvalidUserAgent {
        value: ua.to_owned(),
        reason: reason.to_owned(),
    };

    if ua.is_empty() {
        return Err(invalid("User-Agent must not be empty"));
    }

    // Structural markers: ` (+` introduces the URL, `)` closes the string.
    let marker = ua.find(" (+").ok_or_else(|| {
        invalid(
            "missing ` (+` before the documentation URL — the format is `AppName/Version (+URL)`",
        )
    })?;
    if !ua.ends_with(')') {
        return Err(invalid("must end with `)` closing the documentation URL"));
    }

    // Name / version split at the FIRST '/' (the name excludes '/').
    let (name, _) = ua
        .split_once('/')
        .ok_or_else(|| invalid("missing `/` between the app name and the version"))?;
    if marker < name.len() {
        // The ` (+` we found sits inside the name region → the name contains
        // a space / parens / plus, all of which are structural breakers.
        return Err(invalid(
            "app name must not contain spaces, parentheses, or `+` (first ` (+` found before the version)",
        ));
    }
    if name.is_empty() {
        return Err(invalid(
            "app name must be 1..=64 characters, got an empty name",
        ));
    }
    if name.len() > 64 {
        return Err(invalid("app name must be 1..=64 characters"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_graphic() && b != b'/' && b != b'(' && b != b')' && b != b'+')
    {
        return Err(invalid(
            "app name must be visible ASCII without the structural breakers `/ ( ) +`",
        ));
    }

    // Version runs from after the first '/' up to the structural space.
    let version = &ua[name.len() + 1..marker];
    if !is_valid_semver(version) {
        return Err(invalid(
            "version must be semver MAJOR.MINOR.PATCH (e.g. 1.1.0, 0.1.0-rc.1)",
        ));
    }

    // URL runs from after ` (+` to before the final `)`.
    let url = &ua[marker + " (+".len()..ua.len() - 1];
    let scheme_rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| invalid("documentation URL must start with `https://` or `http://`"))?;
    if scheme_rest.is_empty() {
        return Err(invalid(
            "documentation URL must not be empty after the scheme",
        ));
    }

    // Whole-string charset (last, so structural errors take priority):
    // everything visible ASCII except the single structural space at `marker`
    // (guaranteed to be `0x20` by the `" (+"` find above).
    for (i, b) in ua.as_bytes().iter().enumerate() {
        if i == marker {
            continue;
        }
        if !b.is_ascii_graphic() {
            return Err(invalid(
                "must contain only visible ASCII characters — control characters, \
                 non-ASCII, and extra spaces are rejected anywhere in the string",
            ));
        }
    }

    Ok(())
}

/// `MAJOR.MINOR.PATCH` with an optional `-prerelease` suffix (not deeply
/// validated). Three dot-separated, non-empty, all-ASCII-digit groups.
fn is_valid_semver(version: &str) -> bool {
    let core = match version.split_once('-') {
        Some((core, prerelease)) => {
            if prerelease.is_empty() {
                return false;
            }
            core
        }
        None => version,
    };
    let groups: Vec<&str> = core.split('.').collect();
    groups.len() == 3
        && groups
            .iter()
            .all(|g| !g.is_empty() && g.bytes().all(|b| b.is_ascii_digit()))
}

// ── Loading pipeline ──────────────────────────────────────────────────────────

impl Config {
    /// Config-file discovery, first hit wins:
    /// `--config PATH` → `$ALLEGRO_MCP_CONFIG` → `./allegro-mcp.toml` (CWD)
    /// → `dirs::config_dir()/allegro-mcp/allegro-mcp.toml` → none.
    ///
    /// Explicit sources (CLI, env) are returned unconditionally so a typo'd
    /// path is a startup error, never a silent fallback; implicit slots
    /// (CWD, platform config dir) only match when the file exists.
    /// `dirs::config_dir() == None` skips that slot silently.
    pub fn discover_path(cli_config: Option<&Path>) -> Option<PathBuf> {
        if let Some(path) = cli_config {
            return Some(path.to_path_buf());
        }
        if let Ok(env_path) = std::env::var("ALLEGRO_MCP_CONFIG") {
            if !env_path.trim().is_empty() {
                return Some(PathBuf::from(env_path));
            }
        }
        let cwd = Path::new("allegro-mcp.toml");
        if cwd.is_file() {
            return Some(cwd.to_path_buf());
        }
        dirs::config_dir()
            .map(|d| d.join("allegro-mcp").join("allegro-mcp.toml"))
            .filter(|p| p.is_file())
    }

    /// Parses a [`Config`] from a TOML string (no discovery, no env/CLI
    /// merge). Any subset of keys parses (`#[serde(default)]`); unknown keys
    /// are hard errors (`deny_unknown_fields`).
    ///
    /// Exposed `pub` for tests and library users; the binary always goes
    /// through [`Config::load`], hence the `dead_code` allow for the bin
    /// target's module tree (same pattern as `AllegroServer::with_api_base_url`).
    #[allow(dead_code)]
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        Self::from_toml_str_at(s, Path::new(""))
    }

    /// [`Config::from_toml_str`] with the file path attached to parse errors.
    fn from_toml_str_at(s: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(s).map_err(|source| {
            // serde's deny_unknown_fields message is
            // `unknown field `<key>`, expected ...` — lift the key out of it
            // so the error names both the path and the offending key.
            if let Some(key) = source
                .message()
                .strip_prefix("unknown field `")
                .and_then(|rest| rest.split('`').next())
            {
                return ConfigError::UnknownKey {
                    path: path.to_path_buf(),
                    key: key.to_owned(),
                };
            }
            ConfigError::Toml {
                path: path.to_path_buf(),
                source,
            }
        })
    }

    /// Applies `ALLEGRO_MCP_*` environment overrides on top of `cfg`.
    ///
    /// Setting one schema-source variable shadows the other field (env file
    /// wins over env url, and both beat anything from the config file),
    /// mirroring the fixed resolution order — see [`Config::load`].
    pub fn apply_env(cfg: &mut Config) -> Result<(), ConfigError> {
        if let Ok(v) = std::env::var("ALLEGRO_MCP_SANDBOX") {
            cfg.sandbox = parse_env_bool("ALLEGRO_MCP_SANDBOX", &v)?;
        }
        if let Ok(v) = std::env::var("ALLEGRO_MCP_USER_AGENT") {
            cfg.user_agent = Some(v);
        }
        if let Ok(v) = std::env::var("ALLEGRO_MCP_ACCEPT_LANGUAGE") {
            cfg.accept_language = v.trim().to_owned();
        }
        // URL before FILE so that, when both env vars are set, the file wins
        // (env file > env url); each step clears the opposite field because a
        // higher layer's source choice shadows everything below it.
        if let Ok(v) = std::env::var("ALLEGRO_MCP_SCHEMA_URL") {
            if let Some(shadowed) = cfg.schema_file.take() {
                tracing::warn!(
                    shadowed = ?shadowed,
                    "ALLEGRO_MCP_SCHEMA_URL shadows schema_file"
                );
            }
            cfg.schema_url = Some(v);
        }
        if let Ok(v) = std::env::var("ALLEGRO_MCP_SCHEMA_FILE") {
            if let Some(shadowed) = cfg.schema_url.take() {
                tracing::warn!(
                    shadowed = ?shadowed,
                    "ALLEGRO_MCP_SCHEMA_FILE shadows schema_url"
                );
            }
            cfg.schema_file = Some(PathBuf::from(v));
        }
        Ok(())
    }

    /// Applies CLI overrides on top of `cfg` — CLI flags always win over env
    /// and file values. Same schema-source shadowing semantics as
    /// [`Config::apply_env`].
    pub fn apply_cli(cfg: &mut Config, cli: &CliOverrides) {
        if let Some(sandbox) = cli.sandbox {
            cfg.sandbox = sandbox;
        }
        if let Some(user_agent) = &cli.user_agent {
            cfg.user_agent = Some(user_agent.clone());
        }
        if let Some(url) = &cli.schema_url {
            if let Some(shadowed) = cfg.schema_file.take() {
                tracing::warn!(shadowed = ?shadowed, "--schema-url shadows schema_file");
            }
            cfg.schema_url = Some(url.clone());
        }
        if let Some(file) = &cli.schema_file {
            if let Some(shadowed) = cfg.schema_url.take() {
                tracing::warn!(shadowed = ?shadowed, "--schema-file shadows schema_url");
            }
            cfg.schema_file = Some(file.clone());
        }
    }

    /// Validates the merged configuration: custom User-Agent structure and
    /// scope-entry hygiene. Called by [`Config::load`] before anything
    /// network-facing happens.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(ua) = &self.user_agent {
            validate_user_agent(ua)?;
        }
        for scope in &self.scopes {
            if scope.is_empty() {
                return Err(ConfigError::InvalidScopes {
                    value: scope.clone(),
                    reason: "scope entries must be non-empty".to_owned(),
                });
            }
            if scope.chars().any(char::is_whitespace) {
                return Err(ConfigError::InvalidScopes {
                    value: scope.clone(),
                    reason:
                        "scope entries must not contain whitespace (they are space-joined into one OAuth2 parameter)"
                            .to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Full loading pipeline: file (discovered or explicit) → env → CLI →
    /// validation. This is the User-Agent validation gate — `main` calls it
    /// before schema fetch, auth construction, and client building, so an
    /// invalid UA aborts startup with zero network syscalls.
    ///
    /// Effective schema source resolution (applied by `main`):
    /// CLI file > CLI url > env file > env url > config file > config url >
    /// `SchemaSource::default()` — enforced by the shadow-on-set semantics of
    /// [`Config::apply_env`] / [`Config::apply_cli`].
    pub fn load(cli: &CliOverrides) -> Result<Self, ConfigError> {
        let mut cfg = if let Some(path) = Self::discover_path(cli.config.as_deref()) {
            let s = std::fs::read_to_string(&path).map_err(ConfigError::Io)?;
            tracing::info!(path = %path.display(), "using config file");
            Self::from_toml_str_at(&s, &path)?
        } else {
            Self::default()
        };

        Self::apply_env(&mut cfg)?;
        Self::apply_cli(&mut cfg, cli);

        // Normalize the UA: trimmed before validation AND before use, so
        // `ALLEGRO_MCP_USER_AGENT=" allegro-mcp/0.1.0 (+…)"` works.
        if let Some(ua) = cfg.user_agent.as_mut() {
            *ua = ua.trim().to_owned();
        }

        cfg.validate()?;
        Ok(cfg)
    }
}

/// Parses an `ALLEGRO_MCP_SANDBOX`-style boolean: `true/1/yes` /
/// `false/0/no` (case-insensitive, surrounding whitespace tolerated).
/// Anything else is [`ConfigError::InvalidEnv`] — never silently `false`.
fn parse_env_bool(var: &'static str, value: &str) -> Result<bool, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(ConfigError::InvalidEnv {
            var,
            value: value.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ── User-Agent validation matrix ──────────────────────────────────────────

    #[test]
    fn validate_user_agent_accepts_documented_example() {
        let ua = "TestApplication/1.1.0 (+https://firma.com/TestApplication-info)";
        assert!(
            validate_user_agent(ua).is_ok(),
            "documented example must pass"
        );
    }

    #[test]
    fn validate_user_agent_accepts_default() {
        assert!(
            validate_user_agent(crate::http::DEFAULT_USER_AGENT).is_ok(),
            "the compile-time default UA must always be valid"
        );
    }

    #[test]
    fn validate_user_agent_accepts_semver_prerelease() {
        assert!(validate_user_agent("App/0.1.0-rc.1 (+https://x.com)").is_ok());
    }

    #[test]
    fn validate_user_agent_trims_surrounding_whitespace() {
        assert!(
            validate_user_agent("  App/1.0.0 (+https://x.com)  ").is_ok(),
            "surrounding whitespace must be trimmed before validation"
        );
    }

    #[test]
    fn validate_user_agent_rejects_missing_plus_before_parens() {
        assert!(validate_user_agent("App/1.0.0 (https://x.com)").is_err());
    }

    #[test]
    fn validate_user_agent_rejects_missing_parens() {
        assert!(validate_user_agent("App/1.0.0 +https://x.com").is_err());
    }

    #[test]
    fn validate_user_agent_rejects_missing_url_entirely() {
        assert!(validate_user_agent("App/1.0.0").is_err());
    }

    #[test]
    fn validate_user_agent_rejects_bad_semver() {
        assert!(validate_user_agent("App/1.0 (+https://x.com)").is_err());
        assert!(validate_user_agent("App/latest (+https://x.com)").is_err());
        assert!(validate_user_agent("App/1.0.0.0 (+https://x.com)").is_err());
        assert!(validate_user_agent("App/1..0 (+https://x.com)").is_err());
    }

    #[test]
    fn validate_user_agent_rejects_empty_name() {
        assert!(validate_user_agent("/1.0.0 (+https://x.com)").is_err());
    }

    #[test]
    fn validate_user_agent_rejects_missing_url_scheme() {
        assert!(validate_user_agent("App/1.0.0 (+ftp://x.com)").is_err());
        assert!(validate_user_agent("App/1.0.0 (+x.com)").is_err());
    }

    #[test]
    fn validate_user_agent_rejects_empty_url_after_scheme() {
        assert!(validate_user_agent("App/1.0.0 (+https://)").is_err());
    }

    #[test]
    fn validate_user_agent_rejects_slash_in_name() {
        assert!(validate_user_agent("My/App/1.0.0 (+https://x.com)").is_err());
    }

    #[test]
    fn validate_user_agent_rejects_paren_in_name() {
        assert!(validate_user_agent("Ap(p/1.0.0 (+https://x.com)").is_err());
    }

    #[test]
    fn validate_user_agent_rejects_non_ascii_anywhere() {
        // Non-ASCII in the name…
        assert!(validate_user_agent("Ąpp/1.0.0 (+https://x.com)").is_err());
        // …and in the URL.
        assert!(validate_user_agent("App/1.0.0 (+https://ą.com)").is_err());
        // Control characters.
        assert!(validate_user_agent("App/1.0.0\t(+https://x.com)").is_err());
        assert!(validate_user_agent("App/1.\x000 (+https://x.com)").is_err());
        // Extra internal spaces (only ONE structural space is allowed).
        assert!(validate_user_agent("My App/1.0.0 (+https://x.com)").is_err());
        assert!(validate_user_agent("App/1.0.0  (+https://x.com)").is_err());
    }

    #[test]
    fn validate_user_agent_rejects_overlong_name() {
        let long_name = "a".repeat(65);
        assert!(validate_user_agent(&format!("{long_name}/1.0.0 (+https://x.com)")).is_err());
        // 64 chars is the documented maximum.
        let ok_name = "a".repeat(64);
        assert!(validate_user_agent(&format!("{ok_name}/1.0.0 (+https://x.com)")).is_ok());
    }

    #[test]
    fn validate_user_agent_error_message_contains_doc_link() {
        let err = validate_user_agent("nope").expect_err("must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("https://apps.developer.allegro.pl/user-agent"),
            "error must link the UA docs, got: {msg}"
        );
        assert!(
            msg.contains("nope"),
            "error must include the offending value, got: {msg}"
        );
    }

    #[test]
    fn is_valid_semver_matrix() {
        assert!(is_valid_semver("1.1.0"));
        assert!(is_valid_semver("0.1.0"));
        assert!(is_valid_semver("12.34.56"));
        assert!(is_valid_semver("1.0.0-rc.1"));
        assert!(!is_valid_semver("1.1"));
        assert!(!is_valid_semver("1.1.0.0"));
        assert!(!is_valid_semver("latest"));
        assert!(!is_valid_semver("1.a.0"));
        assert!(!is_valid_semver("1..0"));
        assert!(!is_valid_semver("1.0."));
        assert!(!is_valid_semver("1.0.0-")); // empty prerelease
    }

    // ── Host helpers ──────────────────────────────────────────────────────────

    #[test]
    fn host_helpers_prod_pair() {
        assert_eq!(api_base_url(false), "https://api.allegro.pl");
        assert_eq!(auth_base_url(false), "https://allegro.pl");
    }

    #[test]
    fn host_helpers_sandbox_pair() {
        assert_eq!(api_base_url(true), "https://api.allegrosandbox.pl");
        assert_eq!(auth_base_url(true), "https://allegro.pl.allegrosandbox.pl");
    }

    // ── from_toml_str ─────────────────────────────────────────────────────────

    #[test]
    fn from_toml_str_full_file_parses_all_keys() {
        let cfg = Config::from_toml_str(concat!(
            "sandbox = true\n",
            "user_agent = \"MyApp/1.2.3 (+https://example.com/app)\"\n",
            "accept_language = \"en-US\"\n",
            "schema_url = \"https://example.com/schema.yaml\"\n",
            "auth_flow = \"client_credentials\"\n",
            "scopes = [\"allegro:api:sale:offers:read\"]\n",
            "[tools]\n",
            "allow = [\"allegro_get\"]\n",
            "deny = [\"allegro_delete\"]\n",
        ))
        .expect("full file must parse");
        assert!(cfg.sandbox);
        assert_eq!(
            cfg.user_agent.as_deref(),
            Some("MyApp/1.2.3 (+https://example.com/app)")
        );
        assert_eq!(cfg.accept_language, "en-US");
        assert_eq!(
            cfg.schema_url.as_deref(),
            Some("https://example.com/schema.yaml")
        );
        assert_eq!(cfg.auth_flow, AuthFlow::ClientCredentials);
        assert_eq!(cfg.scopes, vec!["allegro:api:sale:offers:read"]);
        let tools = cfg.tools.expect("tools section must parse");
        assert_eq!(tools.allow, vec!["allegro_get"]);
        assert_eq!(tools.deny, vec!["allegro_delete"]);
    }

    #[test]
    fn from_toml_str_partial_file_sandbox_only() {
        let cfg = Config::from_toml_str("sandbox = true\n").expect("partial file must parse");
        assert!(cfg.sandbox);
        assert_eq!(
            cfg.accept_language, "pl-PL",
            "unset keys fall back to defaults"
        );
        assert_eq!(cfg.user_agent, None);
    }

    #[test]
    fn from_toml_str_partial_file_user_agent_only() {
        let cfg = Config::from_toml_str("user_agent = \"App/1.0.0 (+https://x.com)\"\n")
            .expect("partial file must parse");
        assert_eq!(
            cfg.user_agent.as_deref(),
            Some("App/1.0.0 (+https://x.com)")
        );
        assert!(!cfg.sandbox);
    }

    #[test]
    fn from_toml_str_empty_input_gives_defaults() {
        let cfg = Config::from_toml_str("").expect("empty input must parse as defaults");
        assert_eq!(cfg.accept_language, "pl-PL");
        assert_eq!(cfg.auth_flow, AuthFlow::ClientCredentials);
        assert!(cfg.scopes.is_empty());
        assert!(cfg.tools.is_none());
    }

    #[test]
    fn from_toml_str_unknown_key_is_hard_error() {
        let err =
            Config::from_toml_str("useragent = \"x\"\n").expect_err("typo'd key must be rejected");
        assert!(
            matches!(&err, ConfigError::UnknownKey { key, .. } if key == "useragent"),
            "expected UnknownKey(useragent), got: {err:?}"
        );
    }

    #[test]
    fn from_toml_str_unknown_nested_key_is_hard_error() {
        let err = Config::from_toml_str("[tools]\nallowed = [\"x\"]\n")
            .expect_err("typo'd nested key must be rejected");
        assert!(
            matches!(&err, ConfigError::UnknownKey { key, .. } if key == "allowed"),
            "expected UnknownKey(allowed), got: {err:?}"
        );
    }

    #[test]
    fn from_toml_str_malformed_input_is_toml_error() {
        let err = Config::from_toml_str("sandbox = \n").expect_err("malformed TOML must fail");
        assert!(matches!(err, ConfigError::Toml { .. }), "got: {err:?}");
    }

    #[test]
    fn from_toml_str_client_credentials_flow_parses() {
        let cfg = Config::from_toml_str("auth_flow = \"client_credentials\"\n")
            .expect("client_credentials must parse");
        assert_eq!(cfg.auth_flow, AuthFlow::ClientCredentials);
    }

    #[test]
    fn from_toml_str_unknown_auth_flow_errors_listing_supported_values() {
        let err = Config::from_toml_str("auth_flow = \"device_code\"\n")
            .expect_err("unknown flow must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("client_credentials"),
            "error must list the supported value(s), got: {msg}"
        );
    }

    #[test]
    fn from_toml_str_token_path_parses() {
        let cfg = Config::from_toml_str("token_path = \"/tmp/tokens.json\"\n")
            .expect("token_path must parse");
        assert_eq!(
            cfg.token_path.as_deref(),
            Some(Path::new("/tmp/tokens.json"))
        );
    }

    // ── discover_path ─────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn discover_path_cli_path_wins_over_everything() {
        unsafe { std::env::set_var("ALLEGRO_MCP_CONFIG", "/nonexistent-from-env.toml") };
        let found = Config::discover_path(Some(Path::new("/explicit/path.toml")));
        unsafe { std::env::remove_var("ALLEGRO_MCP_CONFIG") };
        assert_eq!(found, Some(PathBuf::from("/explicit/path.toml")));
    }

    #[test]
    #[serial]
    fn discover_path_env_var_wins_over_cwd_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join("from-env.toml");
        std::fs::write(&env_file, "sandbox = true").expect("write env file");

        let cwd_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(cwd_dir.path().join("allegro-mcp.toml"), "sandbox = true")
            .expect("write cwd file");

        let orig = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(cwd_dir.path()).expect("chdir");
        unsafe { std::env::set_var("ALLEGRO_MCP_CONFIG", &env_file) };
        let found = Config::discover_path(None);
        unsafe { std::env::remove_var("ALLEGRO_MCP_CONFIG") };
        std::env::set_current_dir(&orig).expect("restore cwd");

        assert_eq!(found, Some(env_file));
    }

    #[test]
    #[serial]
    fn discover_path_finds_cwd_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("allegro-mcp.toml"), "sandbox = true")
            .expect("write cwd file");

        let orig = std::env::current_dir().expect("cwd");
        // Pin the platform config dir to an empty tempdir so that slot never
        // interferes (Linux/macOS: XDG_CONFIG_HOME).
        let xdg = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::remove_var("ALLEGRO_MCP_CONFIG");
            std::env::set_var("XDG_CONFIG_HOME", xdg.path());
        }
        std::env::set_current_dir(dir.path()).expect("chdir");
        let found = Config::discover_path(None);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        std::env::set_current_dir(&orig).expect("restore cwd");

        assert_eq!(found, Some(PathBuf::from("allegro-mcp.toml")));
    }

    #[test]
    #[serial]
    fn discover_path_returns_none_when_nothing_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let xdg = tempfile::tempdir().expect("tempdir");
        let orig = std::env::current_dir().expect("cwd");
        unsafe {
            std::env::remove_var("ALLEGRO_MCP_CONFIG");
            std::env::set_var("XDG_CONFIG_HOME", xdg.path());
        }
        std::env::set_current_dir(dir.path()).expect("chdir");
        let found = Config::discover_path(None);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        std::env::set_current_dir(&orig).expect("restore cwd");

        assert_eq!(found, None, "no explicit source and no files → None");
    }

    // ── apply_env ─────────────────────────────────────────────────────────────

    /// Runs `f` with the given `ALLEGRO_MCP_*` vars set (None → removed),
    /// restoring them afterwards. Callers must be `#[serial]`.
    fn with_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        // SAFETY: callers are marked #[serial] so only one thread mutates the
        // environment at a time.
        unsafe {
            for (k, v) in vars {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        f();
        // SAFETY: see above.
        unsafe {
            for (k, v) in saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    #[serial]
    fn apply_env_sandbox_true_variants() {
        for v in ["true", "1", "yes", "TRUE", "Yes"] {
            with_env(&[("ALLEGRO_MCP_SANDBOX", Some(v))], || {
                let mut cfg = Config::default();
                Config::apply_env(&mut cfg).expect("valid bool must apply");
                assert!(cfg.sandbox, "ALLEGRO_MCP_SANDBOX={v} must set sandbox=true");
            });
        }
    }

    #[test]
    #[serial]
    fn apply_env_sandbox_false_variants() {
        for v in ["false", "0", "no", "FALSE", "No"] {
            with_env(&[("ALLEGRO_MCP_SANDBOX", Some(v))], || {
                let mut cfg = Config {
                    sandbox: true,
                    ..Config::default()
                };
                Config::apply_env(&mut cfg).expect("valid bool must apply");
                assert!(
                    !cfg.sandbox,
                    "ALLEGRO_MCP_SANDBOX={v} must set sandbox=false"
                );
            });
        }
    }

    #[test]
    #[serial]
    fn apply_env_sandbox_maybe_is_invalid_env() {
        with_env(&[("ALLEGRO_MCP_SANDBOX", Some("maybe"))], || {
            let mut cfg = Config::default();
            let err = Config::apply_env(&mut cfg).expect_err("'maybe' must be rejected");
            assert!(
                matches!(&err, ConfigError::InvalidEnv { var, value } if *var == "ALLEGRO_MCP_SANDBOX" && value == "maybe"),
                "expected InvalidEnv, got: {err:?}"
            );
            // Never silently false: the config must not have been flipped.
            assert!(!cfg.sandbox);
        });
    }

    #[test]
    #[serial]
    fn apply_env_overrides_user_agent_and_language() {
        with_env(
            &[
                (
                    "ALLEGRO_MCP_USER_AGENT",
                    Some("EnvApp/2.0.0 (+https://env.com)"),
                ),
                ("ALLEGRO_MCP_ACCEPT_LANGUAGE", Some("en-US")),
            ],
            || {
                let mut cfg = Config {
                    user_agent: Some("FileApp/1.0.0 (+https://file.com)".to_owned()),
                    ..Config::default()
                };
                Config::apply_env(&mut cfg).expect("apply_env");
                assert_eq!(
                    cfg.user_agent.as_deref(),
                    Some("EnvApp/2.0.0 (+https://env.com)")
                );
                assert_eq!(cfg.accept_language, "en-US");
            },
        );
    }

    #[test]
    #[serial]
    fn apply_env_url_shadows_config_schema_file() {
        with_env(
            &[(
                "ALLEGRO_MCP_SCHEMA_URL",
                Some("https://env.example/schema.yaml"),
            )],
            || {
                let mut cfg = Config {
                    schema_file: Some(PathBuf::from("/from/file.yaml")),
                    ..Config::default()
                };
                Config::apply_env(&mut cfg).expect("apply_env");
                assert!(cfg.schema_file.is_none(), "env url must shadow config file");
                assert_eq!(
                    cfg.schema_url.as_deref(),
                    Some("https://env.example/schema.yaml")
                );
            },
        );
    }

    #[test]
    #[serial]
    fn apply_env_file_shadows_config_schema_url() {
        with_env(
            &[("ALLEGRO_MCP_SCHEMA_FILE", Some("/from/env.yaml"))],
            || {
                let mut cfg = Config {
                    schema_url: Some("https://file.example/schema.yaml".to_owned()),
                    ..Config::default()
                };
                Config::apply_env(&mut cfg).expect("apply_env");
                assert!(cfg.schema_url.is_none(), "env file must shadow config url");
                assert_eq!(
                    cfg.schema_file.as_deref(),
                    Some(Path::new("/from/env.yaml"))
                );
            },
        );
    }

    #[test]
    #[serial]
    fn apply_env_file_beats_url_within_env_layer() {
        with_env(
            &[
                (
                    "ALLEGRO_MCP_SCHEMA_URL",
                    Some("https://env.example/schema.yaml"),
                ),
                ("ALLEGRO_MCP_SCHEMA_FILE", Some("/from/env.yaml")),
            ],
            || {
                let mut cfg = Config::default();
                Config::apply_env(&mut cfg).expect("apply_env");
                assert!(cfg.schema_url.is_none(), "env file > env url");
                assert_eq!(
                    cfg.schema_file.as_deref(),
                    Some(Path::new("/from/env.yaml"))
                );
            },
        );
    }

    // ── apply_cli ─────────────────────────────────────────────────────────────

    #[test]
    fn apply_cli_overrides_env_and_file() {
        let mut cfg = Config {
            sandbox: true,
            user_agent: Some("FileApp/1.0.0 (+https://file.com)".to_owned()),
            ..Config::default()
        };
        cfg.sandbox = false; // pretend env flipped it
        Config::apply_cli(
            &mut cfg,
            &CliOverrides {
                sandbox: Some(true),
                user_agent: Some("CliApp/1.0.0 (+https://cli.com)".to_owned()),
                ..CliOverrides::default()
            },
        );
        assert!(cfg.sandbox, "CLI --sandbox must win");
        assert_eq!(
            cfg.user_agent.as_deref(),
            Some("CliApp/1.0.0 (+https://cli.com)")
        );
    }

    #[test]
    fn apply_cli_none_leaves_values_untouched() {
        let mut cfg = Config {
            sandbox: true,
            user_agent: Some("EnvApp/1.0.0 (+https://env.com)".to_owned()),
            ..Config::default()
        };
        Config::apply_cli(&mut cfg, &CliOverrides::default());
        assert!(
            cfg.sandbox,
            "absent flags must not override env/file values"
        );
        assert_eq!(
            cfg.user_agent.as_deref(),
            Some("EnvApp/1.0.0 (+https://env.com)")
        );
    }

    #[test]
    fn apply_cli_url_shadows_lower_layers() {
        let mut cfg = Config {
            schema_file: Some(PathBuf::from("/from/env-or-file.yaml")),
            ..Config::default()
        };
        Config::apply_cli(
            &mut cfg,
            &CliOverrides {
                schema_url: Some("https://cli.example/schema.yaml".to_owned()),
                ..CliOverrides::default()
            },
        );
        assert!(
            cfg.schema_file.is_none(),
            "CLI url must shadow lower-layer file"
        );
        assert_eq!(
            cfg.schema_url.as_deref(),
            Some("https://cli.example/schema.yaml")
        );
    }

    #[test]
    fn apply_cli_file_beats_cli_url() {
        let mut cfg = Config::default();
        Config::apply_cli(
            &mut cfg,
            &CliOverrides {
                schema_url: Some("https://cli.example/schema.yaml".to_owned()),
                schema_file: Some(PathBuf::from("/from/cli.yaml")),
                ..CliOverrides::default()
            },
        );
        assert!(cfg.schema_url.is_none(), "CLI file > CLI url");
        assert_eq!(
            cfg.schema_file.as_deref(),
            Some(Path::new("/from/cli.yaml"))
        );
    }

    // ── validate ──────────────────────────────────────────────────────────────

    #[test]
    fn validate_accepts_default_config() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_invalid_user_agent() {
        let cfg = Config {
            user_agent: Some("not a valid ua".to_owned()),
            ..Config::default()
        };
        let err = cfg.validate().expect_err("invalid UA must fail validation");
        assert!(matches!(err, ConfigError::InvalidUserAgent { .. }));
    }

    #[test]
    fn validate_accepts_valid_custom_user_agent() {
        let cfg = Config {
            user_agent: Some("MyApp/2.0.0 (+https://example.com/app)".to_owned()),
            ..Config::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_scope_entry() {
        let cfg = Config {
            scopes: vec!["allegro:api:read".to_owned(), String::new()],
            ..Config::default()
        };
        let err = cfg.validate().expect_err("empty scope must fail");
        assert!(
            matches!(&err, ConfigError::InvalidScopes { value, .. } if value.is_empty()),
            "expected InvalidScopes, got: {err:?}"
        );
    }

    #[test]
    fn validate_rejects_scope_with_internal_whitespace() {
        let cfg = Config {
            scopes: vec!["allegro api read".to_owned()],
            ..Config::default()
        };
        let err = cfg.validate().expect_err("whitespace scope must fail");
        assert!(
            matches!(&err, ConfigError::InvalidScopes { value, reason } if value == "allegro api read" && !reason.is_empty()),
            "expected InvalidScopes with reason, got: {err:?}"
        );
    }

    #[test]
    fn validate_accepts_clean_scopes() {
        let cfg = Config {
            scopes: vec![
                "allegro:api:sale:offers:read".to_owned(),
                "allegro:api:offers:write".to_owned(),
            ],
            ..Config::default()
        };
        assert!(cfg.validate().is_ok());
    }
}
