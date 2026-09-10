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
- Device-flow tokens (`auth_flow = "device_code"`) **are persisted to disk**:
  one versioned JSON file at `token_path` (default
  `~/.config/allegro-mcp/tokens.json`, overridable via `ALLEGRO_MCP_TOKEN_PATH`
  or `auth device --token-path`). The file holds the bearer access token and
  the single-use refresh token. It is written **atomically** (temp file in
  the same directory + `rename`, no torn state) with `0600` permissions, and
  the `allegro-mcp/` directory is created `0700` if it doesn't already exist
  (a pre-existing directory's permissions are never modified). The refresh
  token is single-use and rotated by Allegro on every refresh; the rotated
  pair is persisted *before* the in-memory cache is updated, so the on-disk
  copy is always the only live one. Tokens are environment-labelled
  (`production` / `sandbox`) and refuse to load under the wrong flag. Run
  the container with a mounted volume for this path (the
  `allegro-mcp-tokens` named volume in `docker-compose.yml`), and treat the
  file with the same sensitivity as the client secret above. In
  `client_credentials` mode nothing is ever written to disk.

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
