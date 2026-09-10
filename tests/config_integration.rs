//! Integration tests for the configuration system: TOML file parsing,
//! discovery, env overrides, precedence (CLI > env > file > defaults), and
//! User-Agent validation — the gate that must abort startup before any
//! network I/O.
//!
//! Env-mutating tests are `#[serial]` (same pattern as `src/schema/cache.rs`)
//! and explicitly set/remove every `ALLEGRO_MCP_*` variable they depend on,
//! so parallel test threads and the developer's real environment cannot
//! perturb the results.

use std::path::{Path, PathBuf};

use allegro_mcp::config::{self, AuthFlow, CliOverrides, Config, ConfigError};
use serial_test::serial;

/// Runs `f` with the given env vars set (`None` → removed), restoring the
/// prior values afterwards. Callers must be `#[serial]`.
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

/// The full set of env vars `Config::load` reads — pinned (set or removed)
/// by every `load`-based test so the developer's environment cannot leak in.
const LOAD_ENV_VARS: &[(&str, Option<&str>)] = &[
    ("ALLEGRO_MCP_SANDBOX", None),
    ("ALLEGRO_MCP_USER_AGENT", None),
    ("ALLEGRO_MCP_ACCEPT_LANGUAGE", None),
    ("ALLEGRO_MCP_SCHEMA_URL", None),
    ("ALLEGRO_MCP_SCHEMA_FILE", None),
    ("ALLEGRO_MCP_CONFIG", None),
    ("ALLEGRO_MCP_RATE_LIMIT", None),
];

fn write_config(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("allegro-mcp.toml");
    std::fs::write(&path, contents).expect("write config file");
    path
}

fn cli_with_config(path: &Path) -> CliOverrides {
    CliOverrides {
        config: Some(path.to_path_buf()),
        ..CliOverrides::default()
    }
}

// ── Config::load with an explicit --config file ───────────────────────────────

#[test]
#[serial]
fn load_explicit_config_file_applies_all_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        dir.path(),
        concat!(
            "sandbox = true\n",
            "user_agent = \"MyApp/1.2.3 (+https://example.com/app)\"\n",
            "accept_language = \"en-US\"\n",
            "schema_url = \"https://example.com/schema.yaml\"\n",
            "scopes = [\"allegro:api:read\"]\n",
        ),
    );

    with_env(LOAD_ENV_VARS, || {
        let cfg = Config::load(&cli_with_config(&path)).expect("load must succeed");
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
        assert_eq!(cfg.scopes, vec!["allegro:api:read"]);
    });
}

#[test]
#[serial]
fn load_partial_config_file_uses_defaults_for_missing_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "sandbox = true\n");

    with_env(LOAD_ENV_VARS, || {
        let cfg = Config::load(&cli_with_config(&path)).expect("partial file must load");
        assert!(cfg.sandbox);
        assert_eq!(cfg.user_agent, None, "no UA override → default UA in main");
        assert_eq!(cfg.accept_language, "pl-PL");
        assert!(cfg.scopes.is_empty());
    });
}

#[test]
#[serial]
fn load_missing_explicit_config_file_is_io_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist.toml");

    with_env(LOAD_ENV_VARS, || {
        let err = Config::load(&cli_with_config(&missing))
            .expect_err("explicit --config must not silently fall back");
        assert!(matches!(err, ConfigError::Io(_)), "got: {err:?}");
    });
}

#[test]
#[serial]
fn load_malformed_config_file_names_the_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "sandbox = \n");

    with_env(LOAD_ENV_VARS, || {
        let err = Config::load(&cli_with_config(&path)).expect_err("malformed TOML must fail");
        match &err {
            ConfigError::Toml { path: p, .. } => assert_eq!(p, &path),
            other => panic!("expected Toml error naming the path, got: {other:?}"),
        }
    });
}

#[test]
#[serial]
fn load_unknown_key_names_the_path_and_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "useragent = \"typo\"\n");

    with_env(LOAD_ENV_VARS, || {
        let err = Config::load(&cli_with_config(&path)).expect_err("typo'd key must fail");
        match &err {
            ConfigError::UnknownKey { path: p, key } => {
                assert_eq!(p, &path);
                assert_eq!(key, "useragent");
            }
            other => panic!("expected UnknownKey, got: {other:?}"),
        }
    });
}

/// Invalid UA in the config file must abort load with the doc link in the
/// message — before any client building or network I/O can happen.
#[test]
#[serial]
fn load_invalid_user_agent_aborts_with_doc_link() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "user_agent = \"not a valid ua\"\n");

    with_env(LOAD_ENV_VARS, || {
        let err = Config::load(&cli_with_config(&path)).expect_err("invalid UA must fail load");
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::InvalidUserAgent { .. }));
        assert!(
            msg.contains("https://apps.developer.allegro.pl/user-agent"),
            "error must link the UA docs, got: {msg}"
        );
        assert!(
            msg.contains("not a valid ua"),
            "error must include the offending value, got: {msg}"
        );
    });
}

#[test]
#[serial]
fn load_invalid_scope_entry_aborts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "scopes = [\"allegro api read\"]\n");

    with_env(LOAD_ENV_VARS, || {
        let err = Config::load(&cli_with_config(&path)).expect_err("whitespace scope must fail");
        assert!(
            matches!(err, ConfigError::InvalidScopes { .. }),
            "got: {err:?}"
        );
    });
}

#[test]
#[serial]
fn load_without_any_config_source_gives_defaults() {
    with_env(LOAD_ENV_VARS, || {
        let cfg = Config::load(&CliOverrides::default()).expect("defaults must load");
        assert!(!cfg.sandbox);
        assert_eq!(cfg.user_agent, None);
        assert_eq!(cfg.accept_language, "pl-PL");
        assert_eq!(cfg.auth_flow, AuthFlow::ClientCredentials);
        assert!(cfg.schema_file.is_none() && cfg.schema_url.is_none());
    });
}

// ── Env overrides ─────────────────────────────────────────────────────────────

#[test]
#[serial]
fn load_env_overrides_config_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        dir.path(),
        concat!(
            "sandbox = false\n",
            "user_agent = \"FileApp/1.0.0 (+https://file.example/app)\"\n",
            "accept_language = \"en-US\"\n",
        ),
    );

    with_env(
        &[
            ("ALLEGRO_MCP_SANDBOX", Some("true")),
            (
                "ALLEGRO_MCP_USER_AGENT",
                Some("EnvApp/2.0.0 (+https://env.example/app)"),
            ),
            ("ALLEGRO_MCP_ACCEPT_LANGUAGE", Some("pl-PL")),
            ("ALLEGRO_MCP_CONFIG", None),
            ("ALLEGRO_MCP_SCHEMA_URL", None),
            ("ALLEGRO_MCP_SCHEMA_FILE", None),
            ("ALLEGRO_MCP_RATE_LIMIT", None),
        ],
        || {
            let cfg = Config::load(&cli_with_config(&path)).expect("load");
            assert!(cfg.sandbox, "env sandbox=true must beat file's false");
            assert_eq!(
                cfg.user_agent.as_deref(),
                Some("EnvApp/2.0.0 (+https://env.example/app)"),
                "env UA must beat the file's UA"
            );
            assert_eq!(cfg.accept_language, "pl-PL");
        },
    );
}

#[test]
#[serial]
fn load_invalid_sandbox_env_value_aborts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "sandbox = true\n");

    with_env(
        &[
            ("ALLEGRO_MCP_SANDBOX", Some("maybe")),
            ("ALLEGRO_MCP_USER_AGENT", None),
            ("ALLEGRO_MCP_ACCEPT_LANGUAGE", None),
            ("ALLEGRO_MCP_CONFIG", None),
            ("ALLEGRO_MCP_SCHEMA_URL", None),
            ("ALLEGRO_MCP_SCHEMA_FILE", None),
            ("ALLEGRO_MCP_RATE_LIMIT", None),
        ],
        || {
            let err =
                Config::load(&cli_with_config(&path)).expect_err("'maybe' must be a hard error");
            assert!(
                matches!(err, ConfigError::InvalidEnv { .. }),
                "got: {err:?}"
            );
        },
    );
}

#[test]
#[serial]
fn load_user_agent_env_is_trimmed() {
    with_env(
        &[
            (
                "ALLEGRO_MCP_USER_AGENT",
                Some("  EnvApp/3.0.0 (+https://env.example/app)  "),
            ),
            ("ALLEGRO_MCP_SANDBOX", None),
            ("ALLEGRO_MCP_ACCEPT_LANGUAGE", None),
            ("ALLEGRO_MCP_CONFIG", None),
            ("ALLEGRO_MCP_SCHEMA_URL", None),
            ("ALLEGRO_MCP_SCHEMA_FILE", None),
            ("ALLEGRO_MCP_RATE_LIMIT", None),
        ],
        || {
            let cfg = Config::load(&CliOverrides::default()).expect("trimmed UA must load");
            assert_eq!(
                cfg.user_agent.as_deref(),
                Some("EnvApp/3.0.0 (+https://env.example/app)"),
                "UA must be trimmed before validation and use"
            );
        },
    );
}

// ── Precedence: CLI > env > file > defaults ───────────────────────────────────

#[test]
#[serial]
fn load_cli_beats_env_beats_file_for_sandbox_and_ua() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        dir.path(),
        concat!(
            "sandbox = false\n",
            "user_agent = \"FileApp/1.0.0 (+https://file.example/app)\"\n",
        ),
    );

    with_env(
        &[
            ("ALLEGRO_MCP_SANDBOX", Some("true")),
            (
                "ALLEGRO_MCP_USER_AGENT",
                Some("EnvApp/2.0.0 (+https://env.example/app)"),
            ),
            ("ALLEGRO_MCP_ACCEPT_LANGUAGE", None),
            ("ALLEGRO_MCP_CONFIG", None),
            ("ALLEGRO_MCP_SCHEMA_URL", None),
            ("ALLEGRO_MCP_SCHEMA_FILE", None),
            ("ALLEGRO_MCP_RATE_LIMIT", None),
        ],
        || {
            // CLI overrides both env and file.
            let cfg = Config::load(&CliOverrides {
                sandbox: Some(false),
                user_agent: Some("CliApp/9.9.9 (+https://cli.example/app)".to_owned()),
                config: Some(path.clone()),
                ..CliOverrides::default()
            })
            .expect("load");
            assert!(
                !cfg.sandbox,
                "CLI --sandbox=false (from overrides) must win"
            );
            assert_eq!(
                cfg.user_agent.as_deref(),
                Some("CliApp/9.9.9 (+https://cli.example/app)")
            );
        },
    );
}

#[test]
#[serial]
fn load_schema_source_resolution_cli_url_beats_config_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "schema_file = \"/from/config-file.yaml\"\n");

    with_env(LOAD_ENV_VARS, || {
        let cfg = Config::load(&CliOverrides {
            schema_url: Some("https://cli.example/schema.yaml".to_owned()),
            config: Some(path.clone()),
            ..CliOverrides::default()
        })
        .expect("load");
        assert!(
            cfg.schema_file.is_none(),
            "CLI url must shadow the config file's schema_file"
        );
        assert_eq!(
            cfg.schema_url.as_deref(),
            Some("https://cli.example/schema.yaml")
        );
    });
}

#[test]
#[serial]
fn load_schema_source_resolution_env_url_beats_config_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "schema_file = \"/from/config-file.yaml\"\n");

    with_env(
        &[
            (
                "ALLEGRO_MCP_SCHEMA_URL",
                Some("https://env.example/schema.yaml"),
            ),
            ("ALLEGRO_MCP_SANDBOX", None),
            ("ALLEGRO_MCP_USER_AGENT", None),
            ("ALLEGRO_MCP_ACCEPT_LANGUAGE", None),
            ("ALLEGRO_MCP_CONFIG", None),
            ("ALLEGRO_MCP_SCHEMA_FILE", None),
            ("ALLEGRO_MCP_RATE_LIMIT", None),
        ],
        || {
            let cfg = Config::load(&cli_with_config(&path)).expect("load");
            assert!(cfg.schema_file.is_none(), "env url must shadow config file");
            assert_eq!(
                cfg.schema_url.as_deref(),
                Some("https://env.example/schema.yaml")
            );
        },
    );
}

// ── ALLEGRO_MCP_CONFIG discovery ──────────────────────────────────────────────

#[test]
#[serial]
fn load_uses_env_discovered_config_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "sandbox = true\naccept_language = \"en-US\"\n");
    // Write it under a non-default name to prove $ALLEGRO_MCP_CONFIG picks it.
    let renamed = dir.path().join("renamed-config.toml");
    std::fs::rename(&path, &renamed).expect("rename");

    with_env(
        &[
            ("ALLEGRO_MCP_CONFIG", Some(renamed.to_str().unwrap())),
            ("ALLEGRO_MCP_SANDBOX", None),
            ("ALLEGRO_MCP_USER_AGENT", None),
            ("ALLEGRO_MCP_ACCEPT_LANGUAGE", None),
            ("ALLEGRO_MCP_SCHEMA_URL", None),
            ("ALLEGRO_MCP_SCHEMA_FILE", None),
            ("ALLEGRO_MCP_RATE_LIMIT", None),
        ],
        || {
            let cfg = Config::load(&CliOverrides::default()).expect("load");
            assert!(
                cfg.sandbox,
                "settings must come from the env-discovered file"
            );
            assert_eq!(cfg.accept_language, "en-US");
        },
    );
}

// ── [resilience] rate-limit knob (Phase 9) ────────────────────────────────────

#[test]
#[serial]
fn load_resilience_section_parses_rate_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "[resilience]\nrate_limit_per_minute = 100\n");

    with_env(LOAD_ENV_VARS, || {
        let cfg = Config::load(&cli_with_config(&path)).expect("load must succeed");
        assert_eq!(cfg.rate_limit_rpm(), 100);
    });
}

#[test]
#[serial]
fn load_resilience_defaults_to_8000_without_the_section() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "sandbox = true\n");

    with_env(LOAD_ENV_VARS, || {
        let cfg = Config::load(&cli_with_config(&path)).expect("load must succeed");
        assert_eq!(
            cfg.rate_limit_rpm(),
            8000,
            "soft cap default, under the 9000 hard limit"
        );
    });
}

#[test]
#[serial]
fn load_resilience_invalid_value_aborts() {
    for bad in ["9000", "99999"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            &format!("[resilience]\nrate_limit_per_minute = {bad}\n"),
        );

        with_env(LOAD_ENV_VARS, || {
            let err = Config::load(&cli_with_config(&path))
                .expect_err("values >= 9000 must abort startup");
            assert!(
                matches!(err, ConfigError::InvalidRateLimit { value } if value.to_string() == bad),
                "expected InvalidRateLimit({bad}), got: {err:?}"
            );
        });
    }
}

#[test]
#[serial]
fn load_resilience_zero_disables_the_guard() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "[resilience]\nrate_limit_per_minute = 0\n");

    with_env(LOAD_ENV_VARS, || {
        let cfg = Config::load(&cli_with_config(&path)).expect("0 is explicitly legal");
        assert_eq!(cfg.rate_limit_rpm(), 0, "0 disables the budget guard");
    });
}

#[test]
#[serial]
fn env_override_sets_rate_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "[resilience]\nrate_limit_per_minute = 100\n");

    with_env(
        &[
            ("ALLEGRO_MCP_RATE_LIMIT", Some("250")),
            ("ALLEGRO_MCP_SANDBOX", None),
            ("ALLEGRO_MCP_USER_AGENT", None),
            ("ALLEGRO_MCP_ACCEPT_LANGUAGE", None),
            ("ALLEGRO_MCP_CONFIG", None),
            ("ALLEGRO_MCP_SCHEMA_URL", None),
            ("ALLEGRO_MCP_SCHEMA_FILE", None),
        ],
        || {
            let cfg = Config::load(&cli_with_config(&path)).expect("load must succeed");
            assert_eq!(
                cfg.rate_limit_rpm(),
                250,
                "env ALLEGRO_MCP_RATE_LIMIT must beat the file value"
            );
        },
    );
}

#[test]
#[serial]
fn env_override_garbage_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "sandbox = true\n");

    with_env(
        &[
            ("ALLEGRO_MCP_RATE_LIMIT", Some("fast")),
            ("ALLEGRO_MCP_SANDBOX", None),
            ("ALLEGRO_MCP_USER_AGENT", None),
            ("ALLEGRO_MCP_ACCEPT_LANGUAGE", None),
            ("ALLEGRO_MCP_CONFIG", None),
            ("ALLEGRO_MCP_SCHEMA_URL", None),
            ("ALLEGRO_MCP_SCHEMA_FILE", None),
        ],
        || {
            let err = Config::load(&cli_with_config(&path))
                .expect_err("non-numeric env values must abort, never silently default");
            assert!(
                matches!(&err, ConfigError::InvalidEnv { var, .. } if *var == "ALLEGRO_MCP_RATE_LIMIT"),
                "expected InvalidEnv(ALLEGRO_MCP_RATE_LIMIT), got: {err:?}"
            );
        },
    );
}

// ── Host helpers ──────────────────────────────────────────────────────────────

#[test]
fn host_helpers_swap_both_hosts_with_sandbox_flag() {
    // One flag, BOTH hosts — tokens are not interchangeable between
    // environments, so the pair must never diverge.
    assert_eq!(config::api_base_url(false), "https://api.allegro.pl");
    assert_eq!(config::auth_base_url(false), "https://allegro.pl");
    assert_eq!(config::api_base_url(true), "https://api.allegrosandbox.pl");
    assert_eq!(
        config::auth_base_url(true),
        "https://allegro.pl.allegrosandbox.pl"
    );
}
