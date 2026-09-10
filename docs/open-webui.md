# Wiring `allegro-mcp` up to Open WebUI

`allegro-mcp` defaults to serving MCP over **Streamable HTTP**
(`POST/GET/DELETE /mcp`) — the transport Open WebUI's native MCP client
speaks. This guide walks through connecting the two.

## 1. Prerequisites

- **Open WebUI ≥ v0.6.31.** This is the first release with native
  Streamable HTTP MCP support. Earlier versions have no native MCP client
  at all — you'd need [`mcpo`](https://github.com/open-webui/mcpo), a
  stdio/SSE→OpenAPI bridge, in front of an `allegro-mcp --stdio` process
  instead. If you're on an older Open WebUI and can't upgrade, point
  `mcpo` at `allegro-mcp --stdio`, not at the HTTP transport described
  here.
- `allegro-mcp` running and reachable from the Open WebUI container:
  either the same Docker Compose network (reach it as
  `http://allegro-mcp:8080/mcp`), or `http://host.docker.internal:8080/mcp`
  if Open WebUI runs in Docker and `allegro-mcp` runs directly on the
  host — Open WebUI's own docs call out `host.docker.internal` explicitly
  for this case, since a `localhost` URL inside the Open WebUI container
  would resolve to the container itself, not the host.
- `ALLEGRO_CLIENT_ID` / `ALLEGRO_CLIENT_SECRET` set on the `allegro-mcp`
  container, and the startup auth-check banner showing `OK` in
  `docker logs allegro-mcp` (see the "Discrepancy" note in the design
  plan for why this eager check exists instead of a device-flow banner).

## 2. Add the connection

Open WebUI's MCP connections are **admin-only** by design — regular users
cannot self-register one. As an admin:

1. Go to **Settings → Admin → Integrations**.
2. Under **External Tool Servers**, click **+ Add Connection**.
3. Set **Type** to **MCP (Streamable HTTP)** — *not* "OpenAPI". Picking
   the wrong type here is Open WebUI's own documented gotcha: the UI hangs
   on an infinite loading screen instead of showing a clear error.
4. **Server URL**:
   - `http://allegro-mcp:8080/mcp` if Open WebUI and `allegro-mcp` share a
     Docker Compose network.
   - `http://host.docker.internal:8080/mcp` if `allegro-mcp` runs on the
     host and Open WebUI runs in Docker.
5. **Auth**:
   - `None` if `ALLEGRO_MCP_SERVER_TOKEN` is unset on the `allegro-mcp`
     container (the default — off).
   - `Bearer`, with **Key** set to the same value as
     `ALLEGRO_MCP_SERVER_TOKEN`, if the bearer guard is enabled. **Do not**
     select `Bearer` and leave the key empty — Open WebUI will send a bare
     `Authorization: Bearer` header with no token in that case, which this
     server's middleware (like most servers) rejects outright with `401`.
6. **Save**.
7. Use **Access Control** on the connection to scope which users/groups
   can see the Allegro tools — there's no separate per-user opt-in step.

## 3. Streamable HTTP vs SSE — quirks worth knowing

- Open WebUI's native MCP client speaks **Streamable HTTP only** — it has
  no native stdio or legacy-SSE support. That's exactly the transport
  `allegro-mcp` implements by default (no `--stdio` flag), so no bridge
  layer is needed for this integration.
- If you ever need to expose a *stdio*-only `allegro-mcp` process (e.g.
  running `allegro-mcp --stdio` for local testing) to Open WebUI, put
  [`mcpo`](https://github.com/open-webui/mcpo) in front of it as a
  translation layer. Don't try to point Open WebUI's MCP connection type
  directly at a stdio process — it will not work.
- `allegro-mcp`'s Streamable HTTP transport defaults to **legacy session
  mode** (rmcp's `StreamableHttpServerConfig::legacy_session_mode = true`):
  it issues an `Mcp-Session-Id` on `initialize` and expects it on
  subsequent requests. This is exactly what Open WebUI's client already
  does — no configuration needed on either side.
- **Cold starts**: `allegro-mcp` loads the full Allegro OpenAPI schema and
  builds the tool registry (~200 tools) once at startup, *before* it binds
  the port. If Open WebUI's `session.initialize()` handshake times out
  against a slow or cold container, raise Open WebUI's
  `MCP_INITIALIZE_TIMEOUT` (default 10 s) rather than assuming the server
  is broken.

## 4. Verifying the connection

After saving the connection, open a chat, click **+ → Integrations →
Tools**, and confirm `allegro-mcp` tools show up in the list. There will
be many — the Allegro OpenAPI schema generates roughly 200 tools. If that
list is unwieldy for a given model, use the tool allow/deny filters
(`ToolFilters` in `src/config.rs`, configurable via `allegro-mcp.toml` or
CLI flags) to narrow the surface `allegro-mcp` exposes in the first place,
rather than trying to hide tools on the Open WebUI side.

## 5. Re-auth procedure

This server does not persist tokens to disk today (see the "Discrepancy"
note in the design plan — there's no device-authorization flow, so
there's no persisted refresh token to manage either). Rotating
`ALLEGRO_CLIENT_SECRET` — after a leak, or an Allegro-side rotation — is
simply:

1. Update the env var (your compose `.env` file, or your secret store).
2. `docker compose up -d --force-recreate allegro-mcp`.

The fresh process re-runs the startup auth check against the new
credentials, and the banner in `docker logs` tells you immediately
whether it worked. No manual token-file cleanup is needed — nothing is
cached on disk to clean up.

## 6. Troubleshooting

If tool calls fail with auth-shaped errors coming back from the Allegro
API side, the first thing to check is `/auth/status`:

```bash
curl http://allegro-mcp:8080/auth/status
# or, if ALLEGRO_MCP_SERVER_TOKEN is set:
curl -H "Authorization: Bearer $ALLEGRO_MCP_SERVER_TOKEN" http://allegro-mcp:8080/auth/status
```

A healthy response looks like:

```json
{"auth_flow":"client_credentials","token_cached":true,"token_valid":true,"expires_in_secs":43199}
```

If `token_cached` is `false` or `token_valid` is `false`, the container's
credentials are wrong or the token has expired and hasn't been refreshed
yet — check `docker logs allegro-mcp` for the startup banner and any
`access token refreshed` / auth-error lines.

See [`SECURITY.md`](../SECURITY.md) for the broader threat model of
running `allegro-mcp` in HTTP mode, network-exposure guidance, and secret
rotation/revocation procedures.
