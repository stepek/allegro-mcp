# Plan: Phase 1 — Bootstrap (GH-1)

## Overview

**What**: Bootstrap an empty-but-professional Rust binary crate (`allegro-mcp`) that compiles cleanly, passes `cargo test`, and has CI green on Ubuntu, macOS, and Windows.

**Current state**: The repo contains only `README.md` on `master`. Branch `feat/gh-1-bootstrap` exists but is identical to `master`.

**Root cause / scope**: Nothing exists yet — this is greenfield scaffolding. Every file listed below must be created from scratch.

**Key dependency research**: The official Rust MCP SDK crate is `rmcp` (published by `modelcontextprotocol`), currently at **v3.2.0** on crates.io. The `rmcp` crate itself is written in edition 2024 and requires Rust ≥ 1.88 — this is an internal detail of the SDK, **not** a requirement on consumers. Our binary crate uses **edition 2021**, which is correct and intentional. The `server` feature (default) pulls in `schemars`, `uuid`, and `transport-async-rw`. For stdio transport the `transport-io` feature is needed.

---

## Files to Create

```
allegro-mcp/
├── .github/
│   └── workflows/
│       └── ci.yml
├── .plan/
│   └── gh-1.md          ← this file
├── src/
│   └── main.rs
├── .gitignore
├── Cargo.toml
├── Cargo.lock            (committed — binary crate convention)
├── rustfmt.toml
├── clippy.toml
├── CHANGELOG.md
└── CONTRIBUTING.md
```

---

## Exact File Contents

### `Cargo.toml`

```toml
[package]
name        = "allegro-mcp"
version     = "0.1.0"
edition     = "2021"
rust-version = "1.88"
description = "MCP server for Allegro REST API — tools generated at runtime from the official OpenAPI 3.0 schema"
license     = "MIT"
repository  = "https://github.com/stepek/allegro-mcp"
readme      = "README.md"

[[bin]]
name = "allegro-mcp"
path = "src/main.rs"

[dependencies]
# MCP SDK — official Rust implementation
rmcp            = { version = "3.2.0", features = ["server", "transport-io"] }

# OpenAPI schema parsing
openapiv3       = "2.2.0"

# HTTP client (rustls — no OpenSSL dependency)
reqwest         = { version = "0.12.15", default-features = false, features = ["rustls-tls", "json"] }

# Async runtime
tokio           = { version = "1.45.1", features = ["full"] }

# Serialisation
serde           = { version = "1.0.219", features = ["derive"] }
serde_json      = "1.0.140"
serde_yaml      = "0.9.34"

# Error handling
thiserror       = "2.0.12"
anyhow          = "1.0.98"

# CLI
clap            = { version = "4.5.38", features = ["derive"] }

# Logging / tracing
tracing             = "0.1.41"
tracing-subscriber  = { version = "0.3.19", features = ["env-filter", "fmt"] }

# Platform directories (config/cache paths)
dirs            = "6.0.0"

[profile.release]
strip    = true
opt-level = 3
lto      = true
codegen-units = 1
```

> **Version pinning rationale**: All versions above are the latest stable at the time of writing (2026-09-08). They are pinned at the minor level (e.g. `"1.45.1"`) so `Cargo.lock` is the true pin and `Cargo.toml` communicates intent. `Cargo.lock` must be committed for a binary crate.

> **`openapiv3` version note**: The crate is at `2.2.0` on crates.io. Verify with `cargo search openapiv3` before committing — if a newer patch exists, use it.

> **`reqwest` version note**: Both `rmcp` and our direct dep use `reqwest 0.12.x`. Cargo unifies them automatically — no conflict. The real risk is accidentally enabling rmcp's optional `reqwest` feature (e.g. `transport-streamable-http-client-reqwest`), which would pull in rmcp's own reqwest configuration. Avoid enabling any rmcp HTTP transport features; stick to `server` and `transport-io` only.

> **Edition note**: `rmcp` is internally written in edition 2024 and requires Rust ≥ 1.88. This is an implementation detail of the SDK crate itself. Our consumer binary uses edition 2021, which is fully compatible — Rust editions are per-crate and do not propagate to dependents.

---

### `src/main.rs`

```rust
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
```

---

### `.github/workflows/ci.yml`

```yaml
name: CI

on:
  push:
    branches: ["**"]
  pull_request:
    branches: ["**"]

env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1

jobs:
  ci:
    name: ${{ matrix.os }} / ${{ matrix.toolchain }}
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
        toolchain: [stable]

    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          toolchain: ${{ matrix.toolchain }}
          components: rustfmt, clippy

      - name: Cache cargo registry + build artefacts
        uses: Swatinem/rust-cache@v2

      - name: cargo fmt --check
        run: cargo fmt --all -- --check

      - name: cargo clippy -D warnings
        run: cargo clippy --all-targets -- -D warnings

      - name: cargo build
        run: cargo build --locked

      - name: cargo test
        run: cargo test --locked
```

> **Notes**:
> - `--locked` ensures CI uses the committed `Cargo.lock` exactly.
> - `dtolnay/rust-toolchain@stable` is the canonical action; it respects `rust-toolchain.toml` if present.
> - `Swatinem/rust-cache@v2` caches `~/.cargo/registry` and `target/` keyed on `Cargo.lock`, dramatically speeding up subsequent runs.
> - `fail-fast: false` lets all three OS jobs run even if one fails, giving full signal.
> - Clippy uses `--all-targets` (not `--all-features`) to avoid activating optional heavy dependencies (e.g. rmcp HTTP transports, native-tls backends) that may fail to compile on some platforms during bootstrap.

---

### `rustfmt.toml`

```toml
edition = "2021"
max_width = 100
use_small_heuristics = "Default"
imports_granularity = "Crate"
group_imports = "StdExternalCrate"
```

---

### `clippy.toml`

```toml
# Minimum Rust version for Clippy MSRV lints
msrv = "1.88"
```

---

### `.gitignore`

```gitignore
/target
**/*.rs.bk
.env
.env.*
!.env.example
*.pem
*.key
```

---

### `CONTRIBUTING.md`

```markdown
# Contributing to allegro-mcp

Thank you for your interest in contributing!

## Prerequisites

- Rust stable ≥ 1.88 (`rustup update stable`)
- `rustfmt` and `clippy` components (`rustup component add rustfmt clippy`)

## Development workflow

```bash
# Clone and build
git clone https://github.com/stepek/allegro-mcp.git
cd allegro-mcp
cargo build

# Run tests
cargo test

# Check formatting
cargo fmt --all -- --check

# Run lints (must be warning-free)
cargo clippy --all-targets -- -D warnings
```

## Branching convention

| Branch pattern | Purpose |
|---|---|
| `feat/gh-<N>-<slug>` | New feature tied to issue #N |
| `fix/gh-<N>-<slug>` | Bug fix tied to issue #N |
| `chore/<slug>` | Maintenance / tooling |

## Pull requests

1. Open an issue first (or pick an existing one).
2. Create a branch from `master` using the naming convention above.
3. Ensure `cargo fmt`, `cargo clippy -D warnings`, and `cargo test` all pass locally.
4. Open a PR — CI must be green before merge.

## Code style

- Edition 2021, `max_width = 100` (see `rustfmt.toml`).
- All `clippy` warnings are errors in CI.
- Prefer `thiserror` for library-style errors, `anyhow` for application-level propagation.
- Every public item must have a doc comment.

## Commit messages

Follow [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(auth): add device-code OAuth2 flow
fix(schema): handle nullable oneOf correctly
chore(ci): pin rust-cache to v2.8
```

## Licence

By contributing you agree that your contributions will be licensed under the MIT licence.
```

---

### `CHANGELOG.md`

```markdown
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

[Unreleased]: https://github.com/stepek/allegro-mcp/compare/v0.1.0...HEAD
```

---

## Implementation Phases

### Phase 1 — Scaffold files (single agent, ~10 min)

**Agent**: `implementer`  
**Branch**: `feat/gh-1-bootstrap`

> **Note on CI ordering**: The CI workflow (Phase 2) is created *after* the scaffold files (Phase 1). This means the very first push of Phase 1 files will have no CI running — that is acceptable for bootstrap. CI will be active from the Phase 2 push onward, and will run retroactively on any subsequent push to the branch.

Steps (in order):

1. Create `.gitignore` with the content above.
2. Create `Cargo.toml` with the content above.
3. Create `src/main.rs` with the content above.
4. Create `rustfmt.toml` with the content above.
5. Create `clippy.toml` with the content above.
6. Run `cargo build` locally to generate `Cargo.lock`. Commit `Cargo.lock`.
7. If any dependency version is not found on crates.io, resolve by running `cargo search <crate>` and updating the version in `Cargo.toml`.

**Checkpoint**: `cargo build && cargo test` pass locally.

---

### Phase 2 — CI workflow (single agent, ~5 min)

**Agent**: `implementer`

Steps:

1. Create `.github/workflows/ci.yml` with the content above.
2. Push the branch to GitHub.
3. Verify the Actions tab shows three jobs (ubuntu, macos, windows) all green.

**Checkpoint**: All three CI jobs pass on the first push that includes `ci.yml`.

---

### Phase 3 — Repo hygiene files (single agent, ~5 min)

**Agent**: `implementer`

Steps:

1. Create `CONTRIBUTING.md` with the content above.
2. Create `CHANGELOG.md` with the content above.
3. Commit with message: `chore: add CONTRIBUTING.md and CHANGELOG.md`.
4. Set repository topics on GitHub via **Settings → Topics**: add `rust`, `mcp`, `allegro`, `openapi`. This satisfies the ticket requirement for topic labels on the repository.

**Checkpoint**: Files exist on the branch and render correctly on GitHub. Repository topics are visible on the repo homepage.

---

### Phase 4 — PR and merge (single agent, ~2 min)

**Agent**: `implementer`

Steps:

1. Open a PR from `feat/gh-1-bootstrap` → `master`.
2. PR title: `feat: Phase 1 — Bootstrap cargo project, CI, repo hygiene (#1)`.
3. Wait for CI to go green on the PR.
4. Merge (squash or merge commit — either is fine for phase 1).
5. Close issue #1.

---

## Acceptance Criteria

| Criterion | How to verify |
|---|---|
| `cargo build` passes on fresh clone | `git clone … && cd allegro-mcp && cargo build --locked` exits 0 |
| `cargo test` passes | `cargo test --locked` exits 0, smoke test passes |
| `cargo fmt --check` passes | No diff output |
| `cargo clippy -D warnings` passes | No warnings or errors |
| CI green on Ubuntu | GitHub Actions job `ubuntu-latest / stable` ✅ |
| CI green on macOS | GitHub Actions job `macos-latest / stable` ✅ |
| CI green on Windows | GitHub Actions job `windows-latest / stable` ✅ |
| `CONTRIBUTING.md` exists | File present on `master` after merge |
| `CHANGELOG.md` exists | File present on `master` after merge, keepachangelog format |
| `Cargo.lock` committed | `git ls-files Cargo.lock` returns the file |
| Repository topics set | `rust`, `mcp`, `allegro`, `openapi` visible on GitHub repo homepage |

---

## Edge Cases and Risks

### 1. `reqwest` version unification with `rmcp`

**Risk**: Both `rmcp 3.2.0` and our direct dep use `reqwest 0.12.x`. Cargo unifies them automatically — this is not a conflict. However, if the implementer accidentally enables any of rmcp's optional HTTP transport features (e.g. `transport-streamable-http-client-reqwest`, `reqwest`), rmcp will activate its own reqwest feature flags, which may conflict with our `rustls-tls` configuration or pull in unexpected TLS backends.

**Mitigation**: Only enable `rmcp` features `server` and `transport-io`. Never enable rmcp's `reqwest` or any `transport-streamable-http-*` feature in this crate. Verify with `cargo tree -f "{p} {f}" | grep reqwest` that only one reqwest instance appears in the dependency tree.

### 2. `openapiv3` version

**Risk**: The crate is at `2.2.0`. It is only a placeholder dep in phase 1 (not used in `main.rs`), so compilation is the only concern. If a newer patch exists, it is safe to use.

**Mitigation**: Run `cargo search openapiv3` and use the latest `2.x` patch before committing.

### 3. `serde_yaml` deprecation / YAML parsing

**Risk**: `serde_yaml` 0.9.x is the last maintained version; the upstream author deprecated it. It still compiles and works.

**Mitigation**: Accept 0.9.34 for now. A future phase can migrate to `serde-yaml-ng` or `marked-yaml` if needed.

### 4. Windows CI — `rustls` and `ring`/`aws-lc-rs`

**Risk**: `reqwest` with `rustls-tls` feature pulls in `ring` or `aws-lc-rs` as a crypto backend. `aws-lc-rs` requires NASM on Windows in CI.

**Mitigation**: `reqwest` with `rustls-tls` defaults to `ring` (not `aws-lc-rs`), which is pure Rust and builds on Windows without extra tooling. If the build fails on Windows, switch to `rustls-tls-manual-roots` or `native-tls` for the Windows matrix entry.

### 5. `rmcp` MSRV = 1.88

**Risk**: The GitHub Actions `dtolnay/rust-toolchain@stable` action installs whatever `stable` is at run time. If stable is older than 1.88 (unlikely but possible on a fresh runner image), the build fails.

**Mitigation**: `rust-version = "1.88"` in `Cargo.toml` causes Cargo to emit a clear error. The `dtolnay/rust-toolchain@stable` action always installs the latest stable, which is ≥ 1.88 as of 2026-09-08.

### 6. `Cargo.lock` not committed

**Risk**: If `Cargo.lock` is absent or gitignored, `cargo build --locked` in CI will fail.

**Mitigation**: Explicitly run `cargo build` locally before the first push to generate `Cargo.lock`, then `git add Cargo.lock`. Ensure `.gitignore` does **not** contain `Cargo.lock`.

### 7. `clippy` false positives on empty skeleton

**Risk**: `clippy` may warn about `dead_code` on the `Cli` struct fields or the `verbose` field if they are never read beyond parsing.

**Mitigation**: The `main.rs` skeleton above reads `cli.verbose` in the `match` expression, so no dead-code warning is triggered. The `smoke` test function is `#[cfg(test)]` so it is excluded from release clippy.
