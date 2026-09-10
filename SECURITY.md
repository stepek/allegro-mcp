# Security

## Threat model for this deployment mode

`allegro-mcp` running in HTTP mode holds a live Allegro `client_credentials`
token capable of acting on the linked Allegro account/application — read
access and, depending on the requested scopes (`cfg.scopes`), write access
to offers, orders, and other account data. **Anyone who can reach `/mcp`
unauthenticated can call any tool the server exposes.** Treat this port
like you'd treat direct database access, not like a public API endpoint.

## Secrets handling

- `ALLEGRO_CLIENT_ID` / `ALLEGRO_CLIENT_SECRET`: never commit these to a
  `.env` file checked into git. This repo's `.gitignore` already excludes
  `.env` and `.env.*` (except `.env.example`) — verify that still holds if
  you fork or modify it. In production, prefer your orchestrator's secret
  store (Docker Swarm secrets, a Kubernetes `Secret`, etc.) over a mounted
  `.env` file; if you do use a mounted file, set its permissions to `600`.
- `ALLEGRO_MCP_SERVER_TOKEN`: generate it with `openssl rand -hex 32` (or
  an equivalent). Handle it with the same care as the client secret above.
  Rotate it by changing the env var and recreating the container — there
  is no other state to clean up, since the token is only ever checked
  in-memory and never written anywhere.
- Today, **no credential or token is ever written to disk** by this
  process. The `token_path` config field (`src/config.rs`) is reserved for
  a future phase. The `allegro-mcp-tokens` named volume in
  `docker-compose.yml` currently only matters if you choose to mount an
  optional `allegro-mcp.toml` config file there — it's forward-compatible
  with a future on-disk token cache, but it is not load-bearing for
  security today.

## Network exposure

- Default posture: `allegro-mcp` binds `0.0.0.0:8080` *inside* the
  container, but you should only publish that port to a private Docker
  network (e.g. the same compose network as Open WebUI). **Do not**
  publish `8080:8080` to a public interface without a reverse proxy in
  front terminating TLS and enforcing access control.
- `ALLEGRO_MCP_SERVER_TOKEN` is a defense-in-depth static-token guard, not
  a substitute for TLS. It's sent as a plain bearer header, so it must
  only ever traverse TLS-terminated or otherwise-trusted network segments
  — a Docker-internal network, a VPN, or behind a TLS-terminating reverse
  proxy.
- `ALLEGRO_MCP_ALLOWED_HOSTS=*` disables the Streamable HTTP transport's
  DNS-rebinding protection entirely. Only set this if you already have
  equivalent protection upstream (a reverse proxy enforcing the expected
  `Host` header) — never on a directly-published port.

## Container hardening notes

- The runtime image is `gcr.io/distroless/cc-debian12`: no shell, no
  package manager, no `apt`. This drastically reduces what an attacker
  with code execution inside the container can do next (no `curl`/`wget`
  to exfiltrate data, no shell to pivot with).
- It runs as **root** (uid 0) today, to match the documented
  `/root/.config/allegro-mcp` volume mount path. If you need a non-root
  container for compliance reasons, switch the runtime base image to
  `gcr.io/distroless/cc-debian12:nonroot` in your own fork of the
  `Dockerfile`, and update `HOME` / the volume mount path to
  `/home/nonroot/.config/allegro-mcp` accordingly. That's a
  straightforward follow-up, not attempted here to keep this release's
  scope matching the original issue.

## Revoking Allegro-side access

If `ALLEGRO_CLIENT_SECRET` is compromised, or you're decommissioning a
deployment:

- Log into [apps.developer.allegro.pl](https://apps.developer.allegro.pl),
  find the application, and either regenerate its client secret or delete
  the app registration outright.
- From the *account* side, an Allegro user who authorized the app (this
  matters for `authorization_code` / device-flow-based scopes, if a future
  phase ever adds them) can revoke it under **allegro.pl → Ustawienia →
  Aplikacje → Powiązane aplikacje** ("Settings → Applications → Linked
  applications"). Revoking there invalidates any tokens issued under that
  authorization immediately.
- `client_credentials` app-tokens — what this server uses today — are tied
  to the app registration itself, not to a per-user authorization. Secret
  rotation at the app-registration level is therefore the relevant control
  for the current auth flow.

## Reporting a vulnerability

Please report security vulnerabilities privately using [GitHub Security
Advisories](https://github.com/stepek/allegro-mcp/security/advisories/new)
on this repository, rather than opening a public issue. We'll acknowledge
reports as soon as reasonably possible and work with you on a fix and
coordinated disclosure timeline before any public write-up.
