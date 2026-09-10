# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial Cargo project scaffold (`allegro-mcp` binary crate, edition 2021).
- GitHub Actions CI: `fmt` + `clippy -D warnings` + `test` on Ubuntu, macOS, Windows.
- `CONTRIBUTING.md` with branching convention and code-style guide.
- `CHANGELOG.md` (this file).
- Minimal `src/main.rs` with `clap` CLI skeleton and `tracing` initialisation.
- Phase 9 resilience: 429 exponential backoff with jitter (max 3 retries,
  server `Retry-After` honored up to a 30 s ceiling), single 5xx retry for
  idempotent methods, network errors mapped to a clear "Allegro could not
  be reached" message, Allegro/OAuth error-body mapping into structured
  dev + user report lines, `Trace-Id` capture in every error report, and a
  client-side rate-limit budget guard (`[resilience] rate_limit_per_minute`,
  default 8000 req/min, env `ALLEGRO_MCP_RATE_LIMIT`). Token-mint 429s are
  never auto-retried and never wipe the device-flow token store.

[Unreleased]: https://github.com/stepek/allegro-mcp/compare/v0.1.0...HEAD
