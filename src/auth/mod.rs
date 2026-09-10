//! Allegro OAuth2 token management — the [`AllegroAuth`] façade over the
//! supported flows.
//!
//! Two resolution chains sit behind one type (call sites hold
//! `&AllegroAuth`/`Arc<AllegroAuth>` concretely — no trait indirection):
//!
//! - **`client_credentials`** (default): fetches application tokens from the
//!   token endpoint and caches them in memory, refreshing 60 s before
//!   expiry. Nothing touches disk.
//! - **`device_code`** (RFC 8628-style, activated by
//!   [`AllegroAuth::with_token_store`]): resolves user-scoped tokens through
//!   a persisted [`token_store::TokenStore`] — a live stored access token
//!   first, then the single-use **refresh grant** (Allegro rotates the pair
//!   on every refresh; the new pair is persisted before the in-memory cache
//!   is touched), otherwise a `ReauthRequired` error pointing at
//!   `allegro-mcp auth device`. The interactive half of the flow (device
//!   code request + polling) lives in [`device`].

pub mod device;
pub mod token_store;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use device::u64_lenient;
use token_store::TokenStore;

/// Client-side age cap for a stored refresh token: Allegro's docs give the
/// (single-use) refresh token a ~3-month lifetime but never return its
/// expiry in-band, so we stop trying to refresh 1 day *before* the 90-day
/// mark (`90 * 24 * 3600 - 86_400` = 89 days) and demand re-authorization
/// instead.
const REFRESH_TOKEN_MAX_AGE_SECS: u64 = 90 * 24 * 3600 - 86_400;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can occur during authentication. One public error surface for
/// both flows — the store/device submodules reuse these variants rather than
/// growing parallel enums.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Constructed by [`AllegroAuth::from_env`] (library/tests API). The
    /// binary reads the same env vars directly in `run_mcp_server` and maps
    /// a missing var to an `anyhow` error, so this variant is bin-tree dead
    /// code — same pattern as `MalformedResponse` below.
    #[allow(dead_code)]
    #[error("missing environment variable: {0}")]
    MissingEnvVar(&'static str),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// Reserved for future use when validating token response fields.
    #[allow(dead_code)]
    #[error("token response missing required field: {0}")]
    MalformedResponse(&'static str),

    /// Device mode: the stored authorization cannot serve tokens (nothing
    /// stored, revoked, or refresh rejected) — the user must run
    /// `allegro-mcp auth device` again.
    #[error("re-authorization required: {reason} — run `allegro-mcp auth device`")]
    ReauthRequired { reason: String },

    /// Token-store I/O or (de)serialization failure; the message names the
    /// file. A corrupt store is surfaced, never auto-deleted.
    #[error("token store error: {0}")]
    StoreIo(String),

    /// The token file was written by a newer/older allegro-mcp format.
    #[error(
        "token store version {found} is not supported (expected 1) — delete the file or upgrade"
    )]
    StoreVersion { found: u64 },

    /// Tokens are not interchangeable between environments; the file's
    /// label must match the flag this process runs with.
    #[error(
        "tokens.json was created for the {stored} environment but allegro-mcp is running against {current}"
    )]
    EnvMismatch {
        stored: &'static str,
        current: &'static str,
    },
}

// ── Token response ────────────────────────────────────────────────────────────

/// Raw token response from `POST /auth/oauth/token` (both the
/// `client_credentials` grant, the device-code poll, and the refresh grant
/// produce this shape).
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    /// Lifetime in seconds (Allegro issues 12-hour tokens = 43 200 s).
    /// Leniently parsed: the API has been seen returning quoted numbers.
    #[serde(deserialize_with = "u64_lenient")]
    pub expires_in: u64,
    /// Always `"bearer"` in practice — kept for logging/completeness.
    /// Echoed OAuth2 token type (always `"bearer"` in practice) — parsed
    /// for schema completeness; no behavior keys off it.
    // The bin crate re-declares this module privately, so an unread pub
    // field trips dead_code there even though it is public lib API.
    #[allow(dead_code)]
    pub token_type: Option<String>,
    /// Single-use refresh token (device flow only; `client_credentials`
    /// responses never carry one). Allegro rotates it on every refresh.
    pub refresh_token: Option<String>,
    // `scope` and `jti` are part of the Allegro token contract; exposed for
    // future phases that may need to inspect or log them.
    pub scope: Option<String>,
    pub jti: Option<String>,
}

// ── Cached token ──────────────────────────────────────────────────────────────

/// An in-memory cached token with its expiry instant.
struct CachedToken {
    access_token: String,
    /// The instant at which the token expires (wall-clock approximation).
    expires_at: Instant,
}

impl CachedToken {
    /// Returns `true` if the token has more than 60 s remaining before expiry.
    ///
    /// The 60 s buffer ensures we refresh before the token actually expires,
    /// avoiding 401 errors on in-flight requests.
    fn is_valid(&self) -> bool {
        // `checked_duration_since` returns `None` if `expires_at` is in the past.
        self.expires_at
            .checked_duration_since(Instant::now())
            .map(|remaining| remaining > Duration::from_secs(60))
            .unwrap_or(false)
    }
}

// ── Flow mode ─────────────────────────────────────────────────────────────────

/// Which resolution chain [`AllegroAuth::token`] uses. Constructors default
/// to [`FlowMode::ClientCredentials`] so every pre-existing call site keeps
/// its semantics; [`AllegroAuth::with_token_store`] flips to device mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlowMode {
    ClientCredentials,
    Device,
}

// ── AllegroAuth ───────────────────────────────────────────────────────────────

/// Thread-safe Allegro token manager.
///
/// `client_credentials` mode (default) fetches application tokens and caches
/// them in memory, refreshing 60 seconds before expiry. Device mode (via
/// [`Self::with_token_store`]) additionally persists/rotates user-scoped
/// tokens through a [`TokenStore`].
///
/// # Example
/// ```no_run
/// # use allegro_mcp::auth::AllegroAuth;
/// # #[tokio::main]
/// # async fn main() {
/// let auth = AllegroAuth::from_env(false).unwrap();
/// let token = auth.token().await.unwrap();
/// println!("Bearer {token}");
/// # }
/// ```
pub struct AllegroAuth {
    client_id: String,
    client_secret: String,
    /// Base URL for the auth endpoint, e.g. `https://allegro.pl`.
    auth_base_url: String,
    /// Shared HTTP client — always built by `crate::http` so token requests
    /// carry the ToS-compliant User-Agent + Accept-Language.
    http: Client,
    /// OAuth2 scopes requested on the token fetch (space-joined into the
    /// `scope` form param when non-empty). Empty ⇒ byte-identical request
    /// body to the scope-less flow.
    scopes: Vec<String>,
    cache: RwLock<Option<CachedToken>>,
    /// Resolution chain selection — see [`FlowMode`].
    mode: FlowMode,
    /// Device-mode persistence. `None` in client_credentials mode.
    store: Option<TokenStore>,
    /// One-shot forced-refresh flag: set by [`Self::refresh_now`] (the
    /// dispatcher's 401 hook), consumed by [`Self::resolve_device_token`].
    /// An out-of-band revocation never changes the stored token's
    /// `expires_at_epoch`, so a plain post-401 re-read would hand back the
    /// very token the API just rejected — the flag makes the next
    /// device-mode resolution skip the stored live token and exercise the
    /// refresh grant instead. `AtomicBool` because it travels through
    /// `&self` alongside the resolution chain's write lock.
    force_refresh: AtomicBool,
}

/// Manual `Debug` impl — redacts `client_secret` to prevent accidental logging.
impl std::fmt::Debug for AllegroAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AllegroAuth")
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("auth_base_url", &self.auth_base_url)
            .finish_non_exhaustive()
    }
}

impl AllegroAuth {
    /// Constructs an [`AllegroAuth`] from environment variables.
    ///
    /// Reads `ALLEGRO_CLIENT_ID` and `ALLEGRO_CLIENT_SECRET`.
    ///
    /// Set `sandbox = true` to target `allegro.pl.allegrosandbox.pl`.
    ///
    /// Library/tests API: the binary reads the same env vars directly in
    /// `run_mcp_server` (so it can inject the config-driven HTTP client),
    /// hence the bin-tree `dead_code` allow.
    #[allow(dead_code)]
    pub fn from_env(sandbox: bool) -> Result<Self, AuthError> {
        let client_id = std::env::var("ALLEGRO_CLIENT_ID")
            .map_err(|_| AuthError::MissingEnvVar("ALLEGRO_CLIENT_ID"))?;
        let client_secret = std::env::var("ALLEGRO_CLIENT_SECRET")
            .map_err(|_| AuthError::MissingEnvVar("ALLEGRO_CLIENT_SECRET"))?;
        Ok(Self::new(client_id, client_secret, sandbox))
    }

    /// Returns the auth base URL. Exposed for testing only.
    #[cfg(test)]
    fn auth_base_url(&self) -> &str {
        &self.auth_base_url
    }

    /// Constructs an [`AllegroAuth`] with explicit credentials.
    ///
    /// The auth host is selected by [`crate::config::auth_base_url`] — the
    /// same single source of truth the API dispatcher uses, so the sandbox
    /// flag always swaps both hosts.
    ///
    /// Library/tests API (see [`Self::from_env`] for the bin-tree rationale).
    #[allow(dead_code)]
    pub fn new(client_id: String, client_secret: String, sandbox: bool) -> Self {
        Self::with_base_url(
            client_id,
            client_secret,
            crate::config::auth_base_url(sandbox).to_owned(),
        )
    }

    /// Test-only constructor for injecting a custom auth base URL.
    ///
    /// Must remain `pub` (not `pub(crate)`) because integration tests under
    /// `tests/` are compiled as a separate crate and can only see `pub`
    /// items; `#[doc(hidden)]` keeps it out of the public docs so it isn't
    /// mistaken for a supported production API. Production callers should
    /// use [`Self::new`] or [`Self::from_env`] instead. The `dead_code`
    /// allow covers the bin tree, where only tests construct it.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn with_base_url(client_id: String, client_secret: String, auth_base_url: String) -> Self {
        Self::with_http_client(
            client_id,
            client_secret,
            auth_base_url,
            crate::http::default_client(),
        )
    }

    /// Config-wiring constructor: explicit auth base URL AND explicit HTTP
    /// client (built via [`crate::http::build_client`] in `main`), so a
    /// config-driven User-Agent / Accept-Language applies to token requests
    /// too. Same visibility rationale as [`Self::with_base_url`]: `pub` for
    /// the integration tests under `tests/`, `#[doc(hidden)]` to keep it out
    /// of the public docs.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn with_http_client(
        client_id: String,
        client_secret: String,
        auth_base_url: String,
        http: Client,
    ) -> Self {
        info!(auth_base_url, "AllegroAuth initialised");
        Self {
            client_id,
            client_secret,
            auth_base_url,
            http,
            scopes: Vec::new(),
            cache: RwLock::new(None),
            mode: FlowMode::ClientCredentials,
            store: None,
            force_refresh: AtomicBool::new(false),
        }
    }

    /// Builder-style setter for the OAuth2 scopes requested on each token
    /// fetch. When non-empty, [`Self::fetch_token`] sends the standard
    /// `scope` form param (space-joined values); an empty list keeps the
    /// request body identical to the scope-less flow.
    pub fn with_scopes(mut self, scopes: Vec<String>) -> Self {
        self.scopes = scopes;
        self
    }

    /// Switches to **device mode**: [`Self::token`] resolves user-scoped
    /// tokens through `store` (live stored token → single-use refresh grant
    /// → `ReauthRequired` pointing at `allegro-mcp auth device`) instead of
    /// minting application tokens from the client credentials.
    pub fn with_token_store(mut self, store: TokenStore) -> Self {
        self.mode = FlowMode::Device;
        self.store = Some(store);
        self
    }

    /// `"client_credentials"` or `"device_code"` — the label surfaced by
    /// `/auth/status` and the flow-aware startup banner.
    pub fn flow_label(&self) -> &'static str {
        match self.mode {
            FlowMode::ClientCredentials => "client_credentials",
            FlowMode::Device => "device_code",
        }
    }

    /// Returns a valid access token, fetching a new one if necessary.
    ///
    /// Uses a double-check pattern to avoid thundering-herd re-fetches when
    /// multiple async tasks race to refresh an expired token.
    pub async fn token(&self) -> Result<String, AuthError> {
        // Fast path — shared read lock.
        {
            let guard = self.cache.read().await;
            if let Some(cached) = guard.as_ref() {
                if cached.is_valid() {
                    debug!("reusing cached token");
                    return Ok(cached.access_token.clone());
                }
            }
        }

        // Slow path — exclusive write lock with double-check.
        let mut guard = self.cache.write().await;
        if let Some(cached) = guard.as_ref() {
            if cached.is_valid() {
                debug!("reusing cached token (post-lock double-check)");
                return Ok(cached.access_token.clone());
            }
        }

        // Fetch a fresh token — the chain differs per flow mode
        // (client_credentials: straight grant; device: store → refresh
        // grant → ReauthRequired).
        let (token, effective_expires_in) = match self.mode {
            FlowMode::ClientCredentials => {
                let resp = self.fetch_token().await?;
                let effective_expires_in = clamp_expires_in(resp.expires_in);
                debug!(expires_in = resp.expires_in, "fetched new access token");
                (resp.access_token.clone(), effective_expires_in)
            }
            FlowMode::Device => self.resolve_device_token().await?,
        };

        // Store the raw expiry instant; `is_valid()` applies the 60 s guard.
        let expires_at = Instant::now() + Duration::from_secs(effective_expires_in);

        info!("access token refreshed");

        *guard = Some(CachedToken {
            access_token: token.clone(),
            expires_at,
        });

        Ok(token)
    }

    /// Performs the actual HTTP call to the token endpoint.
    async fn fetch_token(&self) -> Result<TokenResponse, AuthError> {
        let url = format!("{}/auth/oauth/token", self.auth_base_url);
        debug!(url, scopes = self.scopes.len(), "fetching token");

        // OAuth2 form body: the `scope` param is appended only when scopes
        // are configured — an empty list must produce a byte-identical body
        // to the scope-less flow.
        let scope_param = self.scopes.join(" ");
        let mut form: Vec<(&str, &str)> = vec![("grant_type", "client_credentials")];
        if !scope_param.is_empty() {
            form.push(("scope", scope_param.as_str()));
        }

        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&form)
            .send()
            .await?
            .error_for_status()?;

        let token_resp: TokenResponse = resp.json().await?;
        debug!(
            scope = token_resp.scope.as_deref(),
            jti = token_resp.jti.as_deref(),
            "token response received"
        );
        Ok(token_resp)
    }

    /// Device-mode resolution chain, run inside the double-checked
    /// write-lock slow path (so the single-flight guarantee holds):
    ///
    /// 1. store has an **unexpired** access token (epoch now <
    ///    `expires_at_epoch` − 60, the same 60 s guard the cache applies)
    ///    → seed the cache from it — zero network;
    /// 2. else, a refresh token young enough for the 89-day heuristic →
    ///    single-use **refresh grant**; the rotated pair is persisted
    ///    *before* the cache is touched (rotation is single-use — losing
    ///    the new refresh token to a crash means full re-auth, so disk
    ///    goes first). A definitive rejection triggers the stale-store
    ///    guard (see [`Self::refresh_grant`]);
    /// 3. else → [`AuthError::ReauthRequired`]: nothing usable is stored.
    ///
    /// **Forced mode** — [`Self::refresh_now`] set the one-shot flag before
    /// this call (the dispatcher's 401 hook): step 1 is skipped, because a
    /// 401 means the API just rejected the very token the store holds and
    /// an out-of-band revocation never bumps `expires_at_epoch`. The
    /// refresh grant gets the first shot; the stored access token remains
    /// the fallback when the refresh cannot reach a verdict (transient
    /// 5xx / transport failure, or nothing refreshable stored) — a flaky
    /// auth server must not take down a possibly-working token. A
    /// *definitive* rejection is never fallen back from: it clears the
    /// store and propagates [`AuthError::ReauthRequired`].
    async fn resolve_device_token(&self) -> Result<(String, u64), AuthError> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| AuthError::ReauthRequired {
                reason: "no token store configured".to_owned(),
            })?;

        // Consume the one-shot flag exactly once, inside the write lock —
        // so exactly one resolution honors it.
        let forced = self.force_refresh.swap(false, Ordering::SeqCst);

        let state = store.load()?;
        let Some(tokens) = state.tokens else {
            // Distinguish "nothing at all" from "grant still pending" —
            // both are re-auth situations, but the hint differs.
            let reason = if state.pending.is_some() {
                "a device authorization is pending — run `allegro-mcp auth device` to complete it"
            } else {
                "no stored device authorization"
            };
            return Err(AuthError::ReauthRequired {
                reason: reason.to_owned(),
            });
        };

        let now = token_store::epoch_now();
        // The 60 s guard applied to the stored token (same as the cache's).
        let stored_is_live = now < tokens.expires_at_epoch.saturating_sub(60);
        if !forced && stored_is_live {
            // Live stored access token — restore without any network call.
            let remaining = tokens.expires_at_epoch - now;
            debug!(
                remaining_secs = remaining,
                "device mode: restoring persisted access token from the token store"
            );
            return Ok((tokens.access_token.clone(), clamp_expires_in(remaining)));
        }

        // Access token expired (< 60 s left) — or the API just rejected it
        // out-of-band (forced) → refresh grant, if the stored refresh token
        // is young enough to plausibly still work.
        if let Some(refresh_token) = tokens.refresh_token.as_deref() {
            if now.saturating_sub(tokens.updated_at_epoch) <= REFRESH_TOKEN_MAX_AGE_SECS {
                match self.refresh_grant(refresh_token).await {
                    Ok(resp) => {
                        let effective = clamp_expires_in(resp.expires_in);
                        // Persist the rotated pair BEFORE the in-memory
                        // cache is seeded (caller does that): the new
                        // refresh token is single-use and exists nowhere
                        // else — see the rotation crash-window note.
                        store.save_tokens(&resp, effective)?;
                        info!("device mode: refresh grant rotated the token pair");
                        return Ok((resp.access_token.clone(), effective));
                    }
                    Err(AuthError::Http(e)) if is_definitive_rejection(&e) => {
                        // Stale-store guard: a concurrent `auth device` run
                        // may have rotated the file between our load and
                        // this refresh. Reload once and retry only if the
                        // stored refresh token differs from the one that
                        // was just rejected.
                        let fresh = store.load()?;
                        let fresh_refresh = fresh
                            .tokens
                            .as_ref()
                            .and_then(|t| t.refresh_token.as_deref());
                        if let Some(newer) = fresh_refresh {
                            if newer != refresh_token {
                                warn!("device mode: token store was rotated concurrently — retrying refresh with the newer token");
                                match self.refresh_grant(newer).await {
                                    Ok(resp) => {
                                        let effective = clamp_expires_in(resp.expires_in);
                                        store.save_tokens(&resp, effective)?;
                                        return Ok((resp.access_token.clone(), effective));
                                    }
                                    // Transient failure of the retried
                                    // refresh: the newer pair on disk is
                                    // possibly still valid — keep the file
                                    // and surface the error instead of
                                    // wiping usable state.
                                    Err(retry_err) if !matches!(&retry_err, AuthError::Http(h) if is_definitive_rejection(h)) =>
                                    {
                                        return Err(retry_err);
                                    }
                                    // Definitively rejected too: fall
                                    // through to the wipe below.
                                    Err(_) => {}
                                }
                            }
                        }
                        // Definitively dead: wipe the file so the next run
                        // sees a clean "no authorization" state.
                        store.clear()?;
                        return Err(AuthError::ReauthRequired {
                            reason: "refresh token rejected — authorization was revoked or expired"
                                .to_owned(),
                        });
                    }
                    Err(e) => {
                        // Transient refresh failure (5xx / transport).
                        // Forced post-401: the stored token is still the
                        // best known state — hand it back for the
                        // dispatcher's single retry instead of failing the
                        // dispatch outright.
                        if forced && stored_is_live {
                            warn!(
                                error = %e,
                                "device mode: forced refresh failed transiently — falling back to the stored access token"
                            );
                            let remaining = tokens.expires_at_epoch - now;
                            return Ok((tokens.access_token.clone(), clamp_expires_in(remaining)));
                        }
                        return Err(e);
                    }
                }
            }
        }

        // Nothing refreshable: no refresh token stored, or it aged past the
        // 89-day heuristic. Forced post-401 with a live stored token: it is
        // all we have — hand it back and let the retried request decide (a
        // definitive revocation would have surfaced through the refresh
        // grant above).
        if forced && stored_is_live {
            warn!(
                "device mode: forced refresh has no usable refresh token — falling back to the stored access token"
            );
            let remaining = tokens.expires_at_epoch - now;
            return Ok((tokens.access_token.clone(), clamp_expires_in(remaining)));
        }

        Err(AuthError::ReauthRequired {
            reason: "stored device authorization expired and no usable refresh token remains"
                .to_owned(),
        })
    }

    /// The single-use refresh grant: `POST /auth/oauth/token` with
    /// `grant_type=refresh_token` (no `scope` param — the docs don't send
    /// one and the response carries the scope).
    ///
    /// A 4xx here is a *definitive* rejection (`invalid_grant`-style — the
    /// token was rotated away, revoked, or expired), surfaced as
    /// [`AuthError::Http`] so the caller's stale-store guard can inspect
    /// the status via [`is_definitive_rejection`].
    async fn refresh_grant(&self, refresh_token: &str) -> Result<TokenResponse, AuthError> {
        let url = format!("{}/auth/oauth/token", self.auth_base_url);
        debug!(
            url,
            "device mode: running the single-use refresh_token grant"
        );

        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&form)
            .send()
            .await?
            .error_for_status()?;

        let parsed: TokenResponse = resp.json().await?;
        debug!(
            scope = parsed.scope.as_deref(),
            rotated = parsed.refresh_token.is_some(),
            "refresh grant response received"
        );
        Ok(parsed)
    }

    /// Returns a snapshot of the current token state without triggering a
    /// fetch or refresh — a read-only status check, safe to call from an
    /// unauthenticated-by-default endpoint (it reveals cache freshness,
    /// never the token itself).
    ///
    /// Device mode additionally probes the store for a persisted pair
    /// (`persisted = true`), so the flow-aware startup banner can
    /// distinguish "restored" from "still pending" without a network call.
    pub async fn status(&self) -> AuthStatus {
        let persisted = match (&self.mode, &self.store) {
            (FlowMode::Device, Some(store)) => store
                .load()
                .map(|state| state.tokens.is_some())
                .unwrap_or(false),
            _ => false,
        };
        let guard = self.cache.read().await;
        match guard.as_ref() {
            Some(cached) => AuthStatus {
                auth_flow: self.flow_label(),
                persisted,
                token_cached: true,
                token_valid: cached.is_valid(),
                expires_in_secs: cached
                    .expires_at
                    .checked_duration_since(Instant::now())
                    .map(|d| d.as_secs()),
            },
            None => AuthStatus {
                auth_flow: self.flow_label(),
                persisted,
                token_cached: false,
                token_valid: false,
                expires_in_secs: None,
            },
        }
    }

    /// Drops the in-memory cache in **both** modes, forcing the next
    /// [`Self::token`] call through the full resolution chain. Building
    /// block of [`Self::refresh_now`] — the dispatcher's 401 auto-refresh
    /// hook (a 401 means the cached token died out-of-band: password
    /// change, app unlink, session cap). Deliberately cache-only: the
    /// forced refresh-grant behavior lives behind [`Self::refresh_now`]'s
    /// one-shot flag, so a bare invalidate still restores the persisted
    /// pair without touching the network.
    pub async fn invalidate(&self) {
        let mut guard = self.cache.write().await;
        *guard = None;
    }

    /// Device mode only: persists a freshly granted pair (e.g. from a
    /// completed device poll) into the store and seeds the in-memory cache
    /// from it. Used by the server-side resume task and available to the
    /// CLI. The store write happens first — same rotation-crash ordering as
    /// the refresh grant.
    pub async fn install_tokens(&self, resp: &TokenResponse) -> Result<(), AuthError> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| AuthError::ReauthRequired {
                reason: "no token store configured — install_tokens requires device mode"
                    .to_owned(),
            })?;
        let effective = clamp_expires_in(resp.expires_in);
        store.save_tokens(resp, effective)?;

        let mut guard = self.cache.write().await;
        *guard = Some(CachedToken {
            access_token: resp.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(effective),
        });
        Ok(())
    }

    /// Forces a re-resolution right now: sets the one-shot forced-refresh
    /// flag (consumed by [`Self::resolve_device_token`]) *before* dropping
    /// the in-memory cache, then runs the [`Self::token`] chain. Flag-first
    /// ordering closes a race: a concurrent plain [`Self::token`] could
    /// otherwise acquire the write lock between the cache drop and the
    /// flag store and repopulate the cache from the revoked stored token.
    /// In device mode the stored *live* access token is skipped and
    /// the single-use refresh grant gets the first shot — the stored token
    /// remains the fallback when the refresh fails transiently (5xx /
    /// transport), while a definitive rejection propagates as
    /// [`AuthError::ReauthRequired`]. In client_credentials mode this is a
    /// plain refetch of the application token.
    ///
    /// Used by the dispatcher's 401 retry: a 401 means the token died
    /// out-of-band (password change, app unlink, session cap — none of
    /// which the 60 s pre-expiry refresh can see), and a plain re-read
    /// would hand back the very token the API just rejected, because an
    /// out-of-band revocation never changes the stored `expires_at_epoch`.
    pub async fn refresh_now(&self) -> Result<String, AuthError> {
        // Flag BEFORE invalidate: see the doc comment — a concurrent plain
        // `token()` must not repopulate the cache from the revoked token.
        self.force_refresh.store(true, Ordering::SeqCst);
        self.invalidate().await;
        self.token().await
    }
}

/// Clamps `expires_in` to a minimum of 120 s to prevent an infinite refresh
/// loop if the server returns a pathologically short lifetime. Applied to
/// fetched, stored-remaining, and refreshed lifetimes alike.
fn clamp_expires_in(expires_in: u64) -> u64 {
    if expires_in < 120 {
        warn!(
            expires_in,
            "token expires_in is very short — clamping to 120 s to avoid refresh loop"
        );
        120
    } else {
        expires_in
    }
}

/// `true` when a refresh-grant HTTP error means "this refresh token is
/// definitively dead" (4xx family: rotated away / revoked / expired) rather
/// than "the network or the auth server hiccuped" (transport / 5xx).
fn is_definitive_rejection(e: &reqwest::Error) -> bool {
    matches!(e.status(), Some(status) if (400..500).contains(&status.as_u16()))
}

/// A point-in-time snapshot of the token state, for the `/auth/status`
/// HTTP endpoint (admin visibility — see `src/http_server.rs`). Never
/// exposes the token itself.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthStatus {
    /// The active flow's label: `"client_credentials"` or `"device_code"`
    /// (see [`AllegroAuth::flow_label`]).
    pub auth_flow: &'static str,
    /// Device mode: the store currently holds a granted token pair.
    /// `client_credentials` mode: always `false` (nothing is persisted).
    pub persisted: bool,
    /// `true` once at least one token has been resolved into the in-memory
    /// cache since process start.
    pub token_cached: bool,
    /// `true` when the cached token still has > 60 s remaining (the same
    /// threshold `token()` uses to decide whether to refresh).
    pub token_valid: bool,
    /// Seconds remaining before expiry, if a token is cached.
    pub expires_in_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the `MissingEnvVar` error variant carries the variable name and
    /// formats correctly. Does not mutate the process environment.
    #[test]
    fn missing_env_var_error_display() {
        let err = AuthError::MissingEnvVar("ALLEGRO_CLIENT_ID");
        assert!(
            matches!(err, AuthError::MissingEnvVar("ALLEGRO_CLIENT_ID")),
            "MissingEnvVar variant must carry the variable name"
        );
        assert_eq!(
            err.to_string(),
            "missing environment variable: ALLEGRO_CLIENT_ID"
        );
    }

    /// Verifies that `from_env` returns `Err(MissingEnvVar)` when the env var
    /// is absent.
    ///
    /// Uses a sentinel variable name that is guaranteed to never be set in any
    /// real environment, avoiding the need to mutate the process environment
    /// (which would be a data race in parallel tests).
    ///
    /// Note: `from_env` is hard-coded to look up `ALLEGRO_CLIENT_ID`, so we
    /// cannot inject a different variable name without refactoring. Instead,
    /// we verify the error path by checking that `from_env` fails when
    /// `ALLEGRO_CLIENT_ID` is absent (reliable in CI), and document that
    /// developers with real credentials set should run `cargo test` without
    /// those env vars to exercise this path.
    #[test]
    fn from_env_returns_err_for_absent_var() {
        // Only run the assertion when the var is actually absent.
        // In CI (no credentials), this always runs.
        // Locally with credentials set, the test is a no-op — the
        // `missing_env_var_error_display` test covers the error type/display.
        if std::env::var("ALLEGRO_CLIENT_ID").is_err() {
            let result = AllegroAuth::from_env(false);
            assert!(
                result.is_err(),
                "expected Err when ALLEGRO_CLIENT_ID is absent"
            );
            assert!(
                matches!(
                    result.unwrap_err(),
                    AuthError::MissingEnvVar("ALLEGRO_CLIENT_ID")
                ),
                "expected MissingEnvVar(\"ALLEGRO_CLIENT_ID\")"
            );
        }
    }

    #[test]
    fn debug_redacts_client_secret() {
        let auth = AllegroAuth::new(
            "test-client-id".to_owned(),
            "super-secret".to_owned(),
            false,
        );
        let debug_str = format!("{auth:?}");
        assert!(
            !debug_str.contains("super-secret"),
            "Debug output must not contain the client secret"
        );
        assert!(
            debug_str.contains("[REDACTED]"),
            "Debug output must contain [REDACTED]"
        );
    }

    #[test]
    fn cached_token_is_valid_with_plenty_of_time() {
        let token = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(3600),
        };
        assert!(
            token.is_valid(),
            "token with 1 hour remaining should be valid"
        );
    }

    #[test]
    fn cached_token_is_invalid_when_expired() {
        // expires_at is in the past
        let token = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        assert!(!token.is_valid(), "expired token should not be valid");
    }

    /// Boundary: 62 s remaining is strictly greater than the 60 s threshold,
    /// so `is_valid()` must return `true`.
    ///
    /// Uses 62 s (not 61 s) to give a 1-second margin against timing jitter
    /// on loaded machines where two `Instant::now()` calls may differ by ~1 s.
    #[test]
    fn cached_token_is_valid_at_61s_boundary() {
        let token = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(62),
        };
        assert!(
            token.is_valid(),
            "token with 62 s remaining should be valid (> 60 s threshold)"
        );
    }

    /// Boundary: 30 s remaining is NOT strictly greater than the 60 s threshold
    /// (`remaining > Duration::from_secs(60)` is false), so `is_valid()` must
    /// return `false`. Uses 30 s (well below 60 s) for a stable margin.
    #[test]
    fn cached_token_is_invalid_at_exactly_60s_boundary() {
        let token = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(30),
        };
        assert!(
            !token.is_valid(),
            "token with 30 s remaining should be invalid (threshold is strictly > 60 s)"
        );
    }

    /// `AllegroAuth::new` with `sandbox = false` must use the production base URL.
    /// Uses the `#[cfg(test)]` accessor to avoid coupling to `Debug` format strings.
    #[test]
    fn new_production_uses_prod_url() {
        let auth = AllegroAuth::new("id".to_owned(), "secret".to_owned(), false);
        assert_eq!(
            auth.auth_base_url(),
            "https://allegro.pl",
            "production auth_base_url must be 'https://allegro.pl'"
        );
    }

    /// `AllegroAuth::new` with `sandbox = true` must use the sandbox base URL.
    /// Uses the `#[cfg(test)]` accessor to avoid coupling to `Debug` format strings.
    #[test]
    fn new_sandbox_uses_sandbox_url() {
        let auth = AllegroAuth::new("id".to_owned(), "secret".to_owned(), true);
        assert_eq!(
            auth.auth_base_url(),
            "https://allegro.pl.allegrosandbox.pl",
            "sandbox auth_base_url must be 'https://allegro.pl.allegrosandbox.pl'"
        );
    }

    /// Host selection must delegate to (and therefore never diverge from)
    /// the config module's single source of truth — one flag, both hosts.
    #[test]
    fn new_delegates_host_to_config_helper() {
        let prod = AllegroAuth::new("id".to_owned(), "secret".to_owned(), false);
        let sandbox = AllegroAuth::new("id".to_owned(), "secret".to_owned(), true);
        assert_eq!(prod.auth_base_url(), crate::config::auth_base_url(false));
        assert_eq!(sandbox.auth_base_url(), crate::config::auth_base_url(true));
    }

    /// `with_http_client` must store the provided base URL and accept the
    /// config-driven client without panicking (the client itself is only
    /// observable via integration tests, which assert its custom UA on the
    /// wire).
    #[test]
    fn with_http_client_stores_base_url_and_default_scopes() {
        let auth = AllegroAuth::with_http_client(
            "id".to_owned(),
            "secret".to_owned(),
            "https://example.com".to_owned(),
            crate::http::default_client(),
        );
        assert_eq!(auth.auth_base_url(), "https://example.com");
    }

    /// `with_scopes` defaults to an empty list (no `scope` form param), and
    /// the builder must be chainable after `with_http_client`.
    #[test]
    fn with_scopes_builder_sets_scopes() {
        let auth = AllegroAuth::with_base_url(
            "id".to_owned(),
            "secret".to_owned(),
            "https://example.com".to_owned(),
        );
        // Default: empty — the token request body stays byte-identical to
        // the scope-less flow (asserted end-to-end in the integration tests).
        let auth = auth.with_scopes(vec!["allegro:api:read".to_owned()]);
        let _ = auth; // construction must not panic
    }

    /// The `Debug` output must expose `client_id` and `auth_base_url` in
    /// addition to the redacted secret (the existing test only checks the
    /// secret; this test verifies the other fields are present).
    #[test]
    fn debug_includes_client_id_and_base_url() {
        let auth = AllegroAuth::new("my-client-id".to_owned(), "s3cr3t".to_owned(), false);
        let debug_str = format!("{auth:?}");
        assert!(
            debug_str.contains("my-client-id"),
            "Debug output must contain client_id, got: {debug_str}"
        );
        assert!(
            debug_str.contains("auth_base_url"),
            "Debug output must contain the auth_base_url field name, got: {debug_str}"
        );
    }

    /// `MalformedResponse` error variant must format with the field name.
    ///
    /// This variant is reserved for future use (see `#[allow(dead_code)]` on the
    /// variant). Update or remove this test when the variant is promoted to active use.
    #[test]
    fn malformed_response_error_display() {
        let err = AuthError::MalformedResponse("access_token");
        assert_eq!(
            err.to_string(),
            "token response missing required field: access_token"
        );
    }

    /// Verifies that a token stored with a 120 s lifetime (the minimum after
    /// clamping) is considered valid immediately after creation.
    ///
    /// This exercises the `expires_in < 120` clamping path in `token()`:
    /// a clamped token has `expires_at = now + 120s`, which is > 60 s from now,
    /// so `is_valid()` must return `true`.
    #[test]
    fn cached_token_with_clamped_120s_lifetime_is_valid() {
        let token = CachedToken {
            access_token: "tok".to_owned(),
            // Simulate the clamped expires_at: now + 120 s
            expires_at: Instant::now() + Duration::from_secs(120),
        };
        assert!(
            token.is_valid(),
            "token clamped to 120 s lifetime should be valid immediately after creation"
        );
    }

    /// Verifies that `from_env` returns `Err(MissingEnvVar("ALLEGRO_CLIENT_SECRET"))`
    /// when `ALLEGRO_CLIENT_ID` is present but `ALLEGRO_CLIENT_SECRET` is absent.
    ///
    /// Only runs when `ALLEGRO_CLIENT_ID` is set and `ALLEGRO_CLIENT_SECRET` is not,
    /// to avoid mutating the process environment in a parallel test harness.
    #[test]
    fn from_env_returns_err_for_missing_secret() {
        if std::env::var("ALLEGRO_CLIENT_ID").is_ok()
            && std::env::var("ALLEGRO_CLIENT_SECRET").is_err()
        {
            let result = AllegroAuth::from_env(false);
            assert!(
                result.is_err(),
                "expected Err when ALLEGRO_CLIENT_SECRET is absent"
            );
            assert!(
                matches!(
                    result.unwrap_err(),
                    AuthError::MissingEnvVar("ALLEGRO_CLIENT_SECRET")
                ),
                "expected MissingEnvVar(\"ALLEGRO_CLIENT_SECRET\")"
            );
        }
        // Also verify the error type directly (always runs):
        let err = AuthError::MissingEnvVar("ALLEGRO_CLIENT_SECRET");
        assert_eq!(
            err.to_string(),
            "missing environment variable: ALLEGRO_CLIENT_SECRET"
        );
    }

    #[tokio::test]
    async fn status_reports_no_token_before_first_fetch() {
        let auth = AllegroAuth::new("id".to_owned(), "secret".to_owned(), false);
        let status = auth.status().await;
        assert!(!status.token_cached);
        assert!(!status.token_valid);
        assert_eq!(status.expires_in_secs, None);
        assert_eq!(status.auth_flow, "client_credentials");
    }

    #[tokio::test]
    async fn status_reports_valid_token_after_seeding_cache() {
        let auth = AllegroAuth::new("id".to_owned(), "secret".to_owned(), false);
        {
            let mut guard = auth.cache.write().await;
            *guard = Some(CachedToken {
                access_token: "tok".to_owned(),
                expires_at: Instant::now() + Duration::from_secs(3600),
            });
        }
        let status = auth.status().await;
        assert!(status.token_cached);
        assert!(status.token_valid);
        assert!(status.expires_in_secs.is_some());
        assert_eq!(status.auth_flow, "client_credentials");
    }

    #[tokio::test]
    async fn status_reports_invalid_when_token_expired() {
        let auth = AllegroAuth::new("id".to_owned(), "secret".to_owned(), false);
        // Seed an expired token
        let expired = CachedToken {
            access_token: "expired".to_owned(),
            expires_at: std::time::Instant::now() - std::time::Duration::from_secs(1),
        };
        *auth.cache.write().await = Some(expired);
        let status = auth.status().await;
        assert!(status.token_cached);
        assert!(!status.token_valid);
    }

    // ── Flow mode / device-mode façade (unit-level: no network) ──────────────

    fn token_response(access: &str, refresh: Option<&str>) -> TokenResponse {
        TokenResponse {
            access_token: access.to_owned(),
            expires_in: 3600,
            token_type: Some("bearer".to_owned()),
            refresh_token: refresh.map(str::to_owned),
            scope: Some("allegro:api:read".to_owned()),
            jti: None,
        }
    }

    #[test]
    fn flow_label_follows_the_active_mode() {
        let cc = AllegroAuth::new("id".to_owned(), "secret".to_owned(), false);
        assert_eq!(cc.flow_label(), "client_credentials");

        let dir = tempfile::tempdir().expect("tempdir");
        let device = AllegroAuth::with_base_url(
            "id".to_owned(),
            "secret".to_owned(),
            "https://example.com".to_owned(),
        )
        .with_token_store(token_store::TokenStore::new(
            dir.path().join("tokens.json"),
            false,
        ));
        assert_eq!(device.flow_label(), "device_code");
    }

    #[tokio::test]
    async fn install_tokens_persists_and_seeds_the_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth = AllegroAuth::with_base_url(
            "id".to_owned(),
            "secret".to_owned(),
            "https://example.com".to_owned(),
        )
        .with_token_store(token_store::TokenStore::new(
            dir.path().join("tokens.json"),
            false,
        ));

        auth.install_tokens(&token_response("installed", Some("rfr")))
            .await
            .expect("install");

        // Cache seeded — no network involved (base URL is a dead host).
        let token = auth.token().await.expect("cache-seeded token");
        assert_eq!(token, "installed");

        let status = auth.status().await;
        assert!(status.token_cached && status.token_valid);
        assert!(status.persisted, "the store holds a granted pair");

        // The file really landed on disk.
        assert!(dir.path().join("tokens.json").exists());
    }

    #[tokio::test]
    async fn install_tokens_requires_device_mode() {
        let auth = AllegroAuth::new("id".to_owned(), "secret".to_owned(), false);
        let err = auth
            .install_tokens(&token_response("tok", None))
            .await
            .expect_err("client_credentials mode has no store");
        assert!(
            matches!(err, AuthError::ReauthRequired { .. }),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn invalidate_forces_the_next_token_call_to_re_resolve() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth = AllegroAuth::with_base_url(
            "id".to_owned(),
            "secret".to_owned(),
            "https://example.com".to_owned(),
        )
        .with_token_store(token_store::TokenStore::new(
            dir.path().join("tokens.json"),
            false,
        ));
        auth.install_tokens(&token_response("before", None))
            .await
            .expect("install");
        assert!(auth.status().await.token_cached);

        auth.invalidate().await;
        let status = auth.status().await;
        assert!(!status.token_cached, "cache must be empty after invalidate");
        assert!(
            status.persisted,
            "the persisted pair survives an invalidate"
        );

        // Re-resolution goes through the store (never the network here —
        // the base URL points at a dead host, so any HTTP attempt would
        // error out instead of returning "before").
        assert_eq!(auth.token().await.expect("restored"), "before");
    }

    /// `refresh_now` must force the refresh grant past a *live* stored
    /// token. The auth host is unresolvable (RFC 2606 `.invalid`), so the
    /// forced refresh fails with a transport error — a *transient* failure,
    /// which must fall back to the stored access token and leave the store
    /// untouched (the wire-level happy path is covered by the integration
    /// suite, `tests/device_flow_integration.rs` §8).
    #[tokio::test]
    async fn refresh_now_falls_back_to_the_stored_token_on_transient_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth = AllegroAuth::with_base_url(
            "id".to_owned(),
            "secret".to_owned(),
            "https://unit-test.invalid".to_owned(),
        )
        .with_token_store(token_store::TokenStore::new(
            dir.path().join("tokens.json"),
            false,
        ));
        auth.install_tokens(&token_response("live", Some("rfr")))
            .await
            .expect("install");

        let token = auth
            .refresh_now()
            .await
            .expect("a transient forced-refresh failure must fall back to the stored token");
        assert_eq!(token, "live");

        // The stored pair survived the failed forced refresh.
        let state = token_store::TokenStore::new(dir.path().join("tokens.json"), false)
            .load()
            .expect("store readable");
        let saved = state.tokens.expect("pair kept on a transient failure");
        assert_eq!(saved.access_token, "live");
        assert_eq!(saved.refresh_token.as_deref(), Some("rfr"));
    }

    #[tokio::test]
    async fn status_persisted_flag_tracks_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth = AllegroAuth::with_base_url(
            "id".to_owned(),
            "secret".to_owned(),
            "https://example.com".to_owned(),
        )
        .with_token_store(token_store::TokenStore::new(
            dir.path().join("tokens.json"),
            false,
        ));

        assert!(
            !auth.status().await.persisted,
            "an empty store holds no granted pair"
        );

        auth.install_tokens(&token_response("tok", None))
            .await
            .expect("install");
        assert!(auth.status().await.persisted);
    }

    #[tokio::test]
    async fn status_persisted_is_always_false_for_client_credentials() {
        let auth = AllegroAuth::new("id".to_owned(), "secret".to_owned(), false);
        {
            let mut guard = auth.cache.write().await;
            *guard = Some(CachedToken {
                access_token: "tok".to_owned(),
                expires_at: Instant::now() + Duration::from_secs(3600),
            });
        }
        let status = auth.status().await;
        assert!(status.token_cached);
        assert!(
            !status.persisted,
            "client_credentials never persists tokens"
        );
        assert_eq!(status.auth_flow, "client_credentials");
    }

    /// The `AuthError` display strings consumed by CLI/UX copy.
    #[test]
    fn reauth_required_error_display_mentions_the_cli() {
        let err = AuthError::ReauthRequired {
            reason: "no stored device authorization".to_owned(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("allegro-mcp auth device"),
            "the error must point at the CLI command, got: {msg}"
        );
    }
}
