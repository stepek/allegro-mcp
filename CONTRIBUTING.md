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
