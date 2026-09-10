# allegro-mcp

**MCP server for the Allegro REST API, written in Rust.** Tools are generated **at runtime** from the official OpenAPI 3.0 schema ([swagger.yaml](https://developer.allegro.pl/swagger.yaml)) — no codegen, new Allegro endpoints work without recompiling.

Every outgoing request carries:
- `Authorization: Bearer <token>` — OAuth2 (device flow / authorization code + PKCE / client_credentials)
- `User-Agent: allegro-mcp/<version> (+https://github.com/stepek/allegro-mcp)` — **required** by [Allegro REST API ToS art. 3.4(c)](https://developer.allegro.pl/tutorials/informacje-podstawowe-b21569boAI1#user-agent); used as a whitelist factor, never mutated at runtime
- `Accept: application/vnd.allegro.public.v1+json`

> ⚠️ The app name in `User-Agent` must match the name of your registered Allegro application. Validate yours at https://apps.developer.allegro.pl/user-agent

## Architecture

```
┌────────────── stdio MCP ───────────────┐
│  Claude / MCP client                   │
└──────────────┬─────────────────────────┘
               ▼
┌─ allegro-mcp (Rust) ───────────────────────────────────┐
│ rmcp server (stdio)                                    │
│  └─ Tool registry ← built from swagger.yaml (runtime)  │
│      filters: allowlist/denylist, read-only mode       │
│  Auth manager: OAuth2 (device / code+PKCE /            │
│   client_credentials) + token file, auto-refresh       │
│  HTTP client (reqwest middleware):                     │
│   • Authorization: Bearer …                            │
│   • User-Agent: allegro-mcp/x.y.z (+repo URL)          │
│   • Accept: application/vnd.allegro.public.v1+json     │
│   • 429 backoff, Trace-Id surfaced in errors           │
└────────────────────────────────────────────────────────┘
```

## Deployment: HTTP MCP server (Docker)

Starting with this release, `allegro-mcp` defaults to serving MCP over
**Streamable HTTP** (`POST/GET/DELETE /mcp`) instead of stdio — built for
long-running deployments with **Open WebUI** as the primary client. The
stdio transport (Claude Desktop, other local/editor MCP clients) is still
available via `--stdio`.

```bash
docker run --rm \
  -e ALLEGRO_CLIENT_ID=... \
  -e ALLEGRO_CLIENT_SECRET=... \
  -p 8080:8080 \
  ghcr.io/stepek/allegro-mcp:latest
```

Or via `docker-compose.yml` (see the file in this repo — includes a named
volume for config/future token-cache persistence and the
`ALLEGRO_MCP_ALLOWED_HOSTS` setting needed for Docker-network deployments):

```bash
cp .env.example .env   # fill in ALLEGRO_CLIENT_ID / ALLEGRO_CLIENT_SECRET
docker compose up -d
```

See [`docs/open-webui.md`](docs/open-webui.md) for wiring this up as an
Open WebUI External Tool Server, and [`SECURITY.md`](SECURITY.md) before
exposing the port beyond a private network.

| Env var | Purpose | Default |
|---|---|---|
| `PORT` | HTTP listen port | `8080` |
| `ALLEGRO_MCP_SERVER_TOKEN` | Optional static bearer-token guard on `/mcp` and `/auth/status` | unset (disabled) |
| `ALLEGRO_MCP_ALLOWED_HOSTS` | Comma-separated `Host` header allow-list (DNS-rebinding guard); `*` disables it | `localhost,127.0.0.1,::1` |

`GET /health` (unauthenticated) and `GET /auth/status` (admin visibility
into the Allegro token cache) are also served alongside `/mcp`.

## Headless login: `allegro-mcp auth device`

For **user-scoped** access (your own account's data — offers, orders,
bills), allegro-mcp supports the OAuth2 **device flow**
(`auth_flow = "device_code"` in `allegro-mcp.toml`):

```bash
export ALLEGRO_CLIENT_ID=... ALLEGRO_CLIENT_SECRET=...
allegro-mcp --sandbox auth device        # production: drop --sandbox
```

The command prints a verification link (the user code is pre-filled) to
**stderr**, polls in the background, and — once you approve in the browser —
persists the token pair atomically (`0600`) to
`~/.config/allegro-mcp/tokens.json` (override with `token_path` in the
config, `ALLEGRO_MCP_TOKEN_PATH`, or `--token-path`). The server then
restores and auto-refreshes those tokens; a single-use refresh token is
rotated on every refresh and persisted *before* the cache is touched.

Re-running the command is safe: if a previous run was killed mid-poll, the
still-valid pending grant is resumed instead of asking for a fresh
authorization.

> ⚠️ **Device flow requires a device-type app registration.** When creating
> the app at [apps.developer.allegro.pl](https://apps.developer.allegro.pl),
> choose *"Aplikacja będzie działać w środowisku bez dostępu do
> przeglądarki…"* ("the application will run in an environment without
> browser access"). An app's type **cannot** be changed after registration —
> a browser-based (`client_credentials`/`authorization_code`) registration
> cannot run the device flow.

Serving with `auth_flow = "device_code"`: startup restores the persisted
tokens; if a pending grant exists it prints the banner to stderr/docker
logs and completes the authorization in the background; with nothing stored
it refuses to start and points at `allegro-mcp auth device`.

| Env var | Purpose | Default |
|---|---|---|
| `ALLEGRO_MCP_TOKEN_PATH` | Overrides where device-flow tokens are persisted (beats `token_path` in the config) | unset |
| `ALLEGRO_MCP_AUTH_FLOW` | Selects the OAuth2 flow (`client_credentials` / `device_code`); beats `auth_flow` in the config file | unset |

#### Compose / Portainer (no CLI)

There is no CLI flag for the flow — the config file or the
`ALLEGRO_MCP_AUTH_FLOW` environment variable select it, so
compose/Portainer deployments can enable the device flow without
hand-writing a TOML file:

- **A1 (one stack, temp command):** in the stack editor set
  `command: ["--sandbox", "auth", "device"]` and
  `ALLEGRO_MCP_AUTH_FLOW: "device_code"`, deploy, open the
  `verification_uri_complete` URL from the container logs, approve in the
  browser, then remove the `command:` line and redeploy. Tokens land in
  the `allegro-mcp-tokens` volume; the server restores/refreshes them on
  start.
- **A2 (one-off container):** Containers → Add container → same image,
  command `--sandbox auth device`, same env vars, mount the stack's
  volume (`<project>_allegro-mcp-tokens`, visible in the stack's volumes
  tab) at `/root/.config/allegro-mcp`, read the logs, approve, then
  remove the container.

The runtime image is distroless (no shell) — the Portainer "Console" tab
cannot be used; the container logs and the browser approval are all
that's needed.

## Resilience & error reporting

allegro-mcp is a good Allegro citizen on the wire and fails loudly but
cleanly. Every tool call runs through one resilience loop:

| Condition | Behavior |
|---|---|
| `429 Too Many Requests` | Exponential backoff **+ jitter**, up to **3 retries** (500 ms → 1 s → 2 s, capped at 30 s). A server-sent `Retry-After` replaces the computed delay (clamped to 30 s — a tool call never hangs minutes). Retried for **every** method: a 429 means the request was rejected before processing. |
| `5xx` server error | **Single retry**, idempotent methods only (`GET`/`HEAD`/`PUT`/`DELETE`) — a replayed POST could create a duplicate offer. |
| `401 Unauthorized` | One forced token re-resolution (device flow: the single-use refresh grant) + re-send — Phase 5 semantics, unchanged. |
| Token-mint `429` | **Never auto-retried.** Surfaces immediately as an actionable *"token churn too high"* error — hammering the token endpoint would make it worse. |
| Network / transport error | Clear **"Allegro could not be reached"** message instead of reqwest's raw transport noise. |
| Rate-limit budget | Client-side sliding 60 s window keeps the process under a soft cap (default **8000 req/min**, ~11 % headroom under Allegro's 9000/min per-`client_id` quota). Past the cap the call fails fast with a *"paused itself to protect your Allegro quota"* error. |

Every API error is reported as structured text with a dev line, a
user-friendly line, and — when Allegro sent one — the response's
`Trace-Id`:

```text
Allegro API error: HTTP 429 Too Many Requests — rate limited after 4 attempts
Trace-Id: 1311db4f-fe65-4cb2-b514-1bb47f781aa7
Retry-After: 30s
Dev: 429; retries exhausted (backoff 500ms/1s/2s + jitter, server Retry-After honored); body: {"errors":[…]}
User: Allegro is limiting how often this app can call the API. Wait a moment and retry; if it persists, reduce how many requests you make at once.
```

Allegro error bodies (`{"errors":[{code,message,userMessage,path}]}`) and
OAuth token-endpoint errors (`{error,error_description}`) are mapped into
those dev + user lines automatically.

> 💡 **When contacting Allegro support, include the `Trace-Id:` line** —
> it is the correlation key their support asks for.

### Configuring the rate budget

```toml
# allegro-mcp.toml
[resilience]
rate_limit_per_minute = 8000   # default; 0 disables the guard; must be < 9000
```

| Env var | Purpose | Default |
|---|---|---|
| `ALLEGRO_MCP_RATE_LIMIT` | Overrides `rate_limit_per_minute` | `8000` |

The budget is per process (= per `client_id` in every supported
deployment). Running **multiple instances behind one `client_id`**? Lower
the per-instance cap accordingly — the guard does not coordinate across
processes.

## Key design decisions

| # | Decision | Why |
|---|----------|-----|
| 1 | Runtime schema parsing (`openapiv3` crate), not codegen | "Configurable" tool surface — new Allegro endpoints appear without recompiling |
| 2 | Device flow as primary auth; `client_credentials` fallback | MCP servers are headless; auth code lives 10 s & needs a browser redirect loop |
| 3 | refresh_token is **single-use** → persisted atomically (temp file + rename) under a mutex | Allegro rotates refresh tokens; losing one = full re-auth |
| 4 | User-Agent injected via reqwest middleware on every call, validated at startup | ToS 3.4(c) whitelist factor — never mutated after startup |
| 5 | Sandbox switch: `api.allegro.pl` ⇄ `api.allegro.pl.allegrosandbox.pl` | Same binary, two environments; tokens are NOT interchangeable |
| 6 | Default tool surface = read-only GET; writes behind explicit config | 196+ tools would flood model context; safety by default |

## Build phases

Tracked as GitHub issues — see the [issue tracker](https://github.com/stepek/allegro-mcp/issues).

## Known Allegro traps

- Token minting is rate-limited → never fetch a token per request; reuse until expiry (access: 12 h).
- `redirect_uri` must match the registered value **exactly**.
- Sandbox and production tokens are **not** interchangeable.
- `Trace-Id` response header — include it when reporting problems.

## License

MIT
