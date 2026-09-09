# allegro-mcp build plan

Source docs: [Auth & OAuth](https://developer.allegro.pl/tutorials/uwierzytelnianie-i-autoryzacja-zlq9e75GdIR) · [Basics incl. User-Agent](https://developer.allegro.pl/tutorials/informacje-podstawowe-b21569boAI1#user-agent) · [OpenAPI schema](https://developer.allegro.pl/swagger.yaml)

## Distribution decision

**Docker image on GHCR + Claude Code integration (`claude mcp add`) ONLY.**
No crates.io publish, no cargo-dist binary matrix.

## Phases

1. **Bootstrap** — cargo project, CI, repo hygiene
2. **Schema pipeline** — download/cache swagger.yaml, parse & validate, endpoint inventory
3. **Tool registry engine** — paths+params+schemas → MCP tool defs (JSON Schema)
4. **Auth v1: client_credentials** — token fetch/cache, Bearer injection
5. **Auth v2: device flow** — polling loop, token-file persistence, auto-refresh on 401
6. **Auth v3: authorization code + PKCE** (out-of-band code entry)
7. **User-Agent + config** — middleware, startup validation, sandbox toggle
8. **MCP wiring** — rmcp server, list-tools / call-tool handlers
9. **Resilience** — 429 backoff, Allegro error JSON → tool errors, Trace-Id passthrough
10. **Scoping & safety** — allow/deny lists, read-only mode, tool-count guard
11. **Tests** — wiremock-based integration tests per auth flow + engine
12. **Release** — Docker image on GHCR + Claude Code integration (`claude mcp add`); no crates.io, no binary matrix

Each phase = one GitHub issue with acceptance criteria.
