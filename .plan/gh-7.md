# Plan: gh-7 — Phase 7: User-Agent middleware + configuration system

## Status: PENDING

---

## Context Analysis

**Issue #7 goal**: every request (API + OAuth) carries a ToS-compliant User-Agent; the whole server is driven by one config.

This phase touches four areas of the stack:

1. **HTTP layer** — header injection (User-Agent, Authorization, Accept, Accept-Language) must become centralized instead of scattered per call-site.
2. **Configuration** — no config system exists today (`toml`/`figment`/`config` are absent from `Cargo.toml`); hosts and env handling are ad-hoc.
3. **Tool registry** — per-operation Accept version override requires new data flow (`operation.responses` → `ToolDef` → dispatcher).
4. **Wiring** — `main.rs` currently hardcodes host selection and constructs three independent `reqwest::Client`s.

**Context-file note**: `package.json`, `architecture.md`, `design.md`, `claude.md`, `agents.md` do **not exist** in this repo (it is a Rust project). Conventions were derived from `PLAN.md`, `README.md`, `CONTRIBUTING.md`, `.github/workflows/ci.yml`, the existing `.plan/gh-*.md` documents, and `Cargo.toml`.

---

## Current State (verified, with file:line references)

### Three uncoordinated HTTP clients

| Client | Location | Headers configured | Used for |
|---|---|---|---|
| `static LazyLock<Client>` | `src/schema/fetch.rs:9-14` | none (timeout only) | schema download |
| `AllegroAuth.http` | `src/auth/mod.rs:150` (`Client::new()`) | **none** | OAuth token fetch |
| `AllegroServer.http` | `src/server.rs:33` (`Client::new()`) | **none** | API dispatch |

### Header status today

- **OAuth token request** (`src/auth/mod.rs:215-222`): sets only `Authorization: Basic …`. **No User-Agent at all** — direct ToS compliance gap (UA is an Allegro whitelisting factor; art. 3.4(c)).
- **API requests** (`src/dispatcher.rs:134-138`): hardcodes the literal `"allegro-mcp/0.1.0"` (drifts from `CARGO_PKG_VERSION`; missing the mandatory `(+URL)` part) and the literal `"application/vnd.allegro.public.v1+json"` (no per-op override).
- **Accept-Language**: not set anywhere in the codebase (0 grep hits).

### Host bug (must fix in this phase)

| Host | Value in code | Value per official docs |
|---|---|---|
| Auth prod | `https://allegro.pl` (`auth/mod.rs:131`) | ✅ correct |
| Auth sandbox | `https://allegro.pl.allegrosandbox.pl` (`auth/mod.rs:129`) | ✅ correct |
| API prod | `https://api.allegro.pl` (`dispatcher.rs:15`) | ✅ correct |
| API sandbox | `https://api.allegrosandbox.pl` (`dispatcher.rs:13` + its test at `dispatcher.rs:266`) | ❌ **wrong** — official value is `https://api.allegro.pl.allegrosandbox.pl` |

`README.md:41` documents the correct value. The config system becomes the **single source of truth** for hosts, eliminating this class of bug.

### ToolDef / Accept versioning

- `ToolDef` (`src/tool_registry/mod.rs:9-23`) has fields `id, name, description, input_schema, method, path` — nothing derived from `operation.responses`.
- `builder.rs` / `schema_builder.rs` never read `operation.responses` (0 grep hits). The dispatcher's Accept value is a hardcoded literal, fully disconnected from the schema.

### Environment variables today

| Var | Location | Failure handling |
|---|---|---|
| `ALLEGRO_CLIENT_ID` | `auth/mod.rs:113` | hard error |
| `ALLEGRO_CLIENT_SECRET` | `auth/mod.rs:115` | hard error |
| `ALLEGRO_MCP_CACHE_DIR` | `schema/cache.rs:8` | silent fallback to `dirs::cache_dir()/allegro-mcp` |

### Auth flows

Only `client_credentials` exists (`src/auth/mod.rs`, single file). Phases 5 (device flow) and 6 (PKCE) are **not implemented** — confirmed via `git log` (only gh-1, gh-2, gh-3, gh-4, gh-8 merged). Config fields for those flows are **reserved, not functional**, in this phase.

### Token persistence

None exists (in-memory cache only). `token_path` config field is reserved for gh-5/gh-6.

### Test-injection points that must survive

- `AllegroAuth::with_base_url` (`auth/mod.rs:144-153`, `#[doc(hidden)] pub` — needed by `tests/` as a separate crate)
- `dispatcher::dispatch_with_base` (`dispatcher.rs:106-120`)
- `AllegroServer::with_api_base_url` (`server.rs:39-49`)

### CI gate (must keep passing)

`cargo fmt --all -- --check` · `cargo clippy --all-targets -- -D warnings` · `cargo build --locked` · `cargo test --locked` (ubuntu/macos/windows; rustfmt `max_width = 100`).

---

## Research Findings (sourced)

### 1. Allegro User-Agent policy

Source: https://developer.allegro.pl/tutorials/informacje-podstawowe-b21569boAI1 (section "User-Agent")

- Legal basis: REST API ToS art. 3.4(c) — apps must be reliably and unambiguously identifiable.
- **Required format**: `AppName/Version (+URL)` — e.g. `TestApplication/1.1.0 (+https://firma.com/TestApplication-info)`.
- AppName must match the name registered at https://apps.developer.allegro.pl (client-side unverifiable — document only).
- The `(+URL)` part is presented as required, pointing to a page/repo describing the app.
- UA content is used as a **whitelisting factor** — "do not mutate the header at runtime (except version)".
- Official validator: https://apps.developer.allegro.pl/user-agent (requires login; HTTP 403 for automated fetch — link it in error messages, do not scrape it).
- No official character whitelist/regex is published → we define a conservative structural validator (below).

### 2. Accept media types in the real swagger.yaml

Grep of https://developer.allegro.pl/swagger.yaml (1.5 MB, 41k lines):

| Media type | Occurrences |
|---|---|
| `application/vnd.allegro.public.v1+json` | 712 |
| `application/vnd.allegro.beta.v1+json` | 58 |

No other version tokens exist today. Per-op override therefore only needs to *read* the schema — never hardcode an endpoint list.

### 3. Sandbox hosts (official, from the "Środowisko testowe" section)

| | Production | Sandbox |
|---|---|---|
| API | `https://api.allegro.pl/` | `https://api.allegro.pl.allegrosandbox.pl/` |
| OAuth | `https://allegro.pl/auth/oauth/` | `https://allegro.pl.allegrosandbox.pl/auth/oauth/` |

### 4. Accept-Language

Documented values (swagger enum): `en-US, pl-PL, uk-UA, sk-SK, cs-CZ, hu-HU`. Default for us: `pl-PL` (per issue).

### 5. reqwest 0.12 middleware options

- `reqwest-middleware` **0.5.x requires `reqwest ^0.13.1`** — incompatible with our pinned `0.12.15`. Last 0.12-compatible line: **0.4.2** (2025-04), now stale relative to upstream; adds `async-trait`, `thiserror` v1 (duplicate major), `tower-service`.
- reqwest 0.12 has **no native middleware chain**, but `ClientBuilder::user_agent()` and `ClientBuilder::default_headers()` apply to every request, with per-request `.header()` taking precedence — sufficient for UA / Accept-Language / default Accept.
- `tower_http::SetRequestHeaderLayer` requires a new dep + low-level `connector_layer` plumbing — no advantage here.

### 6. Config crates

| Crate | Latest | Status | Verdict |
|---|---|---|---|
| `toml` | 1.1.5 (2026-09) | very active, serde-first | **adopt** (only new dep) |
| `figment` | 0.10.19 (2024-05) | ~2 years without release | reject |
| `config-rs` | 0.15.25 | active but heavyweight abstraction | reject (overkill for ~10 keys) |

---

## Design Constraints

- Exact version pins, few dependencies, rustls-only, `--locked` builds must not break (adding `toml` regenerates `Cargo.lock` — commit it).
- No `regex` dependency for UA validation — hand-rolled structural validator (the format is trivially parseable).
- Secrets (`ALLEGRO_CLIENT_ID`, `ALLEGRO_CLIENT_SECRET`) stay **env-only** — never in the config file.
- `#[doc(hidden)] pub` test constructors must keep working (`tests/` is a separate crate).
- Phases 5/6/10 fields are placeholders: parsed, validated, but produce a clear "not yet implemented" error/warning when actually used.
- Startup ordering: **config load + UA validation happen before any HTTP client or server is constructed** → invalid config means no request can leave the process (acceptance criterion).

---

## Recommendations (ranked)

### Option A — RECOMMENDED: Config module + single shared configured client ("functional middleware")

Add `toml` (only new dep). Create `src/config/` (types + layered load + validation) and `src/http.rs` (one factory building a single `reqwest::Client` pre-configured with UA + default headers). "Middleware" = one choke point: all API/OAuth requests flow through the shared client and the dispatcher's single request-builder block.

- ✅ Zero middleware deps; no reqwest 0.13 conflict; no stale 0.4.2 pin.
- ✅ Header defaults live in exactly one place; hosts derive from one `sandbox` flag.
- ✅ Trivially testable (pure validator; wiremock sees final headers).
- ⚠️ "Every request flows through it" is convention, not type-enforced — mitigated by deleting all other `Client::new()` sites in production paths and (optional) clippy `disallowed-methods` on `reqwest::Client::new`.

### Option B — reqwest-middleware 0.4.2 layer

Real composable middleware chain (`Middleware trait`), natural home for Phase 9 retry/backoff.

- ⚠️ Pinned to a stale line; upstream moved to reqwest 0.13 → future upgrade trap.
- ⚠️ Adds `async-trait` + duplicate `thiserror` v1.
- Verdict: revisit at Phase 9; not needed for static headers.

### Option C — config-rs or figment for config layering

- ⚠️ figment unmaintained-ish; config-rs pulls a framework for ~10 keys.
- Verdict: manual env overlay (~30 lines of code) over `toml` + serde is simpler and dependency-light.

---

## Recommended Architecture — Detailed Design

### 1. Config file contract (`allegro-mcp.toml`)

```toml
# Discovery order (first found wins):
#   1. --config <PATH> CLI flag or ALLEGRO_MCP_CONFIG env var
#   2. ./allegro-mcp.toml (current working directory)
#   3. dirs::config_dir()/allegro-mcp/config.toml
# No file found → defaults only (not an error).

[auth]
flow = "client_credentials"   # ONLY accepted value today.
                              # "device_code" | "authorization_code" → clear error
                              # pointing to gh-5/gh-6 (not yet implemented).
# scopes = ["offer.read"]     # reserved for gh-5/gh-6; parsed, warn-unused

[server]
sandbox      = false
user_agent   = "MyApp/1.2.3 (+https://example.com/myapp-info)"
              # optional; default: allegro-mcp/<CARGO_PKG_VERSION> (+https://github.com/stepek/allegro-mcp)
accept_language = "pl-PL"     # en-US | pl-PL | uk-UA | sk-SK | cs-CZ | hu-HU
# token_path = "/custom/dir/token.json"   # optional; default <cache>/allegro-mcp/token.json
                                            # reserved for gh-5/gh-6; warn-unused now

[schema]
url  = "https://developer.allegro.pl/swagger.yaml"   # exactly one of url|file
# file = "/path/to/swagger.yaml"

[tools]                      # reserved for Phase 10 — parsed, warn-unused
# allow = ["allegro_get_*"]
# deny   = ["allegro_delete_*"]
```

**Precedence (low → high)**: built-in defaults < config file < env vars < CLI flags.
Existing `--sandbox` / `--schema-url` / `--schema-file` flags keep working and land in the CLI layer (highest precedence). New flag: `--config <PATH>`.

### 2. Env override map

| Env var | Overrides | Validation |
|---|---|---|
| `ALLEGRO_MCP_CONFIG` | (file discovery) | path must exist, else clear error |
| `ALLEGRO_MCP_USER_AGENT` | `server.user_agent` | UA validator (below) |
| `ALLEGRO_MCP_SANDBOX` | `server.sandbox` | must parse as bool |
| `ALLEGRO_MCP_ACCEPT_LANGUAGE` | `server.accept_language` | member of enum |
| `ALLEGRO_MCP_TOKEN_PATH` | `server.token_path` | path string |
| `ALLEGRO_MCP_SCHEMA_URL` / `ALLEGRO_MCP_SCHEMA_FILE` | `schema.*` | both set → error |
| `ALLEGRO_CLIENT_ID` / `ALLEGRO_CLIENT_SECRET` | — | unchanged, env-only (secrets policy) |
| `ALLEGRO_MCP_CACHE_DIR` | — | unchanged (schema cache only) |

Error messages on invalid values must name the offending source (`env ALLEGRO_MCP_USER_AGENT` vs `allegro-mcp.toml [server].user_agent @ <path>`).

### 3. User-Agent specification & validator

Default (also validated at startup — no exemptions):

```
allegro-mcp/{CARGO_PKG_VERSION} (+https://github.com/stepek/allegro-mcp)
```

Validator rules (hand-rolled in `src/config/user_agent.rs`, pure fn, no `regex` dep):

1. Non-empty; contains exactly one `/`.
2. AppName (before `/`): 1+ chars from `[A-Za-z0-9._-]`, no whitespace.
3. Version (after `/`): 1+ chars from `[A-Za-z0-9._-]`.
4. Exactly one space between version and the parenthesized part.
5. Parenthesized part: starts `(+`, ends `)`, is the final char of the string.
6. URL inside: non-empty, starts `http://` or `https://`, no whitespace.

Failure message includes: which rule failed, the offending value, and both links (https://apps.developer.allegro.pl/user-agent + https://developer.allegro.pl/tutorials/informacje-podstawowe-b21569boAI1#user-agent). Also log a reminder that AppName must match the registered Allegro application name (unverifiable client-side).

### 4. Hosts — single source of truth

Derived fields on the resolved config (not serialized): `api_base` + `auth_base` computed **only** from `sandbox`:

| | `sandbox = false` | `sandbox = true` |
|---|---|---|
| `api_base` | `https://api.allegro.pl` | `https://api.allegro.pl.allegrosandbox.pl` |
| `auth_base` | `https://allegro.pl` | `https://allegro.pl.allegrosandbox.pl` |

The duplicated host literals in `dispatcher.rs:11-17` and `auth/mod.rs:127-134` are **deleted**; both consumers receive hosts from config. Test-only base-URL injection constructors survive unchanged. **Bugfix included**: the shipped API sandbox host changes from `api.allegrosandbox.pl` (wrong) to `api.allegro.pl.allegrosandbox.pl` (official); the dispatcher unit test asserting the old value must be updated.

### 5. Header injection matrix ("the middleware")

One shared `reqwest::Client` built by `src/http.rs` from resolved config via `ClientBuilder`:
`.user_agent(<validated UA>)` + `.default_headers({ Accept: application/vnd.allegro.public.v1+json, Accept-Language: <configured> })`.

| Outgoing request | User-Agent | Accept | Accept-Language | Authorization |
|---|---|---|---|---|
| OAuth `POST /auth/oauth/token` | client default ✅ | explicit `application/json` (override) | client default | `Basic` (existing) |
| API op, no versioned content | client default | client default (`public.v1`) | client default | `Bearer` (dispatcher) |
| API op, versioned (`beta.v1`) | client default | **per-op override** from `ToolDef.accept` | client default | `Bearer` (dispatcher) |

reqwest semantics: request-level `.header()` overrides client defaults per header — exactly the override mechanism needed for per-op Accept and the token endpoint's `Accept: application/json`.

`AllegroAuth` and `AllegroServer` are refactored to **accept the shared client** (instead of building their own). The schema-fetch client (`src/schema/fetch.rs`) stays separate — it targets `developer.allegro.pl`, is neither an API nor an OAuth call, and already uses a builder with timeout.

### 6. Per-op Accept extraction (tool registry change)

- `ToolDef` gains `accept: Option<String>`.
- `builder.rs::build_one_tool` scans `operation.responses`: among 2xx responses that have a `content` map, collect media-type keys starting with `application/vnd.allegro.`; if exactly one distinct value → store it; if multiple distinct values → pick the first and log at TRACE; none → `None` (dispatcher falls back to the client default).
- Dispatcher uses `tool_def.accept` when present instead of the client default.
- Fixture `fixtures/allegro_sample.yaml` gains one operation declaring `application/vnd.allegro.beta.v1+json` response content, so the path is testable without the network.

### 7. Module layout

```
src/
├── config/
│   ├── mod.rs          ← CREATE — Config types, layered load (defaults→file→env→CLI), validation
│   └── user_agent.rs   ← CREATE — validator + default UA builder (pure)
├── http.rs             ← CREATE — shared configured Client factory
├── auth/mod.rs         ← MODIFY — hosts injected, shared client, no header literals
├── dispatcher.rs       ← MODIFY — hosts from caller, per-op Accept, delete UA/Accept/host literals
├── server.rs           ← MODIFY — hold config-derived hosts + shared client
├── tool_registry/
│   ├── mod.rs          ← MODIFY — ToolDef.accept
│   └── builder.rs      ← MODIFY — response content-type extraction
├── main.rs             ← MODIFY — --config flag, config load first, wiring
└── lib.rs              ← MODIFY — pub mod config; pub mod http;
```

New tests: `tests/config_integration.rs`. Extended: `tests/mcp_server_integration.rs`, dispatcher/registry unit tests. Docs: `README.md` (config + env table + hosts), `CHANGELOG.md`.

**Cargo.toml**: add `toml` (1.x, serde support). No other new dependencies. Regenerate & commit `Cargo.lock` (build without `--locked` once, then verify `--locked`).

---

## Implementation Steps (ordered, high-level)

1. **Cargo.toml** — add `toml` 1.x; `cargo build` to regenerate `Cargo.lock`; commit lock.
2. **`src/config/user_agent.rs`** — validator + default-UA builder; exhaustive unit tests (see Testing).
3. **`src/config/mod.rs`** — `Config`/`FileConfig`/env-overlay types; discovery; precedence merge; `Hosts` derivation; validation orchestration (UA, accept-language enum, auth-flow gate, schema url/file exclusivity); reserved-field warn-unused behavior. Unit tests with `#[serial]` for env mutation (existing pattern in `schema/cache.rs`).
4. **`src/http.rs`** — client factory taking resolved config; unit test asserting default header map contents.
5. **Tool registry** — `ToolDef.accept` + builder extraction + fixture extension + unit/integration tests.
6. **`src/dispatcher.rs`** — delete `allegro_api_base` literals and header literals; accept hosts + accept-override via parameters; update the sandbox-host unit test to the official value.
7. **`src/auth/mod.rs`** — accept base URL + shared client via constructor; token request gains explicit `Accept: application/json`; UA now inherited from shared client (fixes the ToS gap).
8. **`src/server.rs` / `src/main.rs`** — thread resolved config through; `--config` flag; load-and-validate **before** any client/server construction; keep subcommands unchanged.
9. **Integration tests** — header-assertion wiremock suite (below) + config precedence suite.
10. **Docs** — README config/env/UA sections, CHANGELOG entry.
11. **Verify**: `cargo fmt --all` · `cargo clippy --all-targets -- -D warnings` · `cargo build --locked` · `cargo test --locked`.

---

## Testing Impact

### New unit tests

- **UA validator** (table-driven): valid default; valid custom; missing `(+URL)`; URL not http(s); no space; double space; empty name; empty version; whitespace in name; slash in URL; trailing garbage after `)`; `+` without URL. Default UA built from `CARGO_PKG_VERSION` validates.
- **Config**: precedence (defaults < file < env < CLI); file discovery order; malformed TOML → error names file path; unknown auth flow → error mentions gh-5/gh-6; both `schema.url` and `schema.file` → error; accept-language enum enforcement; sandbox host derivation for both flags (both hosts swap together — issue requirement).
- **Registry**: beta fixture op → `accept == Some("application/vnd.allegro.beta.v1+json")`; legacy fixture ops → `accept == None`; multiple distinct content types → deterministic pick + TRACE.

### New/extended integration tests (wiremock — the issue's acceptance criteria)

Header-assertion suite in `tests/mcp_server_integration.rs` using mocks that **match on required headers** (`wiremock::matchers::header`), so a missing header = no mock match = request fails the test:

1. Token `POST /auth/oauth/token` carries configured UA (+ Basic auth; no Bearer).
2. Default API GET carries UA + Bearer + `Accept: application/vnd.allegro.public.v1+json` + `Accept-Language: pl-PL`.
3. Versioned op GET carries `Accept: application/vnd.allegro.beta.v1+json` (per-op override via ToolDef).
4. POST op carries the full matrix.
5. `ALLEGRO_MCP_USER_AGENT` override propagates to the observed header value.
6. Invalid UA config → config load errors; client/server never constructed ("no requests leave the process" is structural: the shared client is the only production client and is built only after validation).
7. Sandbox config → token URL and API URL both point at the `allegro.pl.allegrosandbox.pl` hosts (existing test-injection constructors).

### Touched existing tests

- Dispatcher sandbox-host unit test (wrong value today) → official value.
- `tests/tool_registry_integration.rs` → assert on new `accept` field where fixtures allow; real-swagger test (if network-enabled) asserts every `accept ∈ {None} ∪ {application/vnd.allegro.*}` and ≥ 1 beta tool exists.
- Env-mutating tests get `#[serial]` following the `schema/cache.rs` pattern.

CI commands unchanged.

---

## Acceptance Criteria Mapping

| Issue criterion | Verification |
|---|---|
| Middleware injects UA + Bearer + Accept on every request | Wiremock suite 1–4 (matcher-enforced, 100% of request types) |
| Per-op version override from schema `content` keys | Registry unit tests + integration suite 3 (beta fixture + swagger scan) |
| UA format validation at startup, reject invalid, link docs | Validator unit table; config-load error message contains both URLs; main exits before client construction |
| Default UA + `ALLEGRO_MCP_USER_AGENT` override | Unit (default from `CARGO_PKG_VERSION`) + integration 5 |
| `allegro-mcp.toml` with env override driving whole server | Config precedence suite (`#[serial]` env) |
| Sandbox swaps BOTH api and auth hosts | Hosts derivation unit test + integration 7 (bugfix: `api.allegro.pl.allegrosandbox.pl`) |
| `Accept-Language` configurable, pl-PL default | Config validation + integration suite 2 (header observed) |

---

## Edge Cases

1. **UA valid format but unregistered app name** — unverifiable client-side; warn in logs and README ("must match the registered app name").
2. **Multiple distinct vnd.allegro content types on one op** — pick first, TRACE log; never invent values.
3. **204/empty responses** — no content map → `accept = None` → default.
4. **`auth.flow = "device_code"` today** — startup error naming the field, file/env source, and gh-5; never silently fall back.
5. **Reserved fields (`scopes`, `token_path`, `tools.*`) set** — one WARN per field at startup; otherwise ignored.
6. **Both `ALLEGRO_MCP_SCHEMA_URL` and `ALLEGRO_MCP_SCHEMA_FILE`** — hard error (ambiguous).
7. **`ALLEGRO_MCP_CONFIG` points to missing file** — hard error (explicit intent).
8. **Malformed TOML** — error includes file path + toml error span.
9. **Config file exists but empty** — treated as defaults (valid empty tables).
10. **Env var invalid value (bad bool / bad language)** — error names the env var, not the file.
11. **CLI `--sandbox` + config `sandbox = false`** — CLI wins; log the effective value + both host bases at startup.
12. **Windows path handling for `--config` / token_path** — `PathBuf` throughout; CI runs windows-latest.
13. **Secrets never in config** — `Config` has no credential fields by design; document in README + example config comment.
14. **`Cargo.lock` drift** — adding `toml` requires one unlocked build; commit the lock so CI `--locked` passes.
15. **Duplicate `Client::new()` regression** — optional hardening: clippy `disallowed-methods` for `reqwest::Client::new` with `#[allow]` on the single factory (decision left to implementer; note tests use their own clients).
16. **UA header mutated mid-session** — UA is fixed at client construction; no runtime mutation path exists (whitelisting-factor requirement).

---

## Notes for implementer

- Implementation delegated to a **sonnet** coding agent (user preference for this repo's workflow).
- One new dependency only: `toml` 1.x. Everything else is std/reqwest/serde.
- Do not reformat unrelated code (rustfmt `max_width = 100`); keep `--locked` green.
- Keep all `#[doc(hidden)] pub` test constructors working — the integration suite depends on them.
- `schema/fetch.rs` client is intentionally out of scope (not an API/OAuth request).
