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
