//! Versioned on-disk token store for the device flow (`auth_flow =
//! "device_code"`).
//!
//! One JSON file holds both the *granted tokens* and the *in-flight device
//! grant* ([`PendingDeviceGrant`]). Persisting the pending grant is what
//! makes "kill mid-poll → restart → resume" work: the restarted process
//! polls the same single-use `device_code` instead of demanding a fresh
//! authorization.
//!
//! Durability + secrecy properties:
//! - **Wall-clock epoch seconds** (`expires_at_epoch`, `updated_at_epoch`) —
//!   tokens must survive process restarts and sleep/suspend, which
//!   `tokio::time::Instant` cannot represent on disk.
//! - **Atomic writes** ([`write_atomic`]): temp file in the same directory +
//!   `rename`, so a crash never leaves a torn file behind.
//! - **`0600` file / `0700` created dir** on unix — the file holds bearer
//!   tokens and a single-use refresh token.
//! - **Environment label** (`"production"` / `"sandbox"`): tokens are NOT
//!   interchangeable between environments (see `crate::config`), so a
//!   mismatch is a hard error, never a silent swap.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::AuthError;

/// On-disk format version. Bump on any breaking change to the envelope;
/// [`TokenStore::load`] refuses files written by a newer format instead of
/// misparsing them.
pub const STORE_FORMAT_VERSION: u64 = 1;

// ── Stored types ──────────────────────────────────────────────────────────────

/// A granted token pair with wall-clock expiry bookkeeping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    /// Single-use refresh token — Allegro rotates it on every refresh, so
    /// whatever is on disk is the *only* copy (see `mod.rs`'s refresh grant).
    pub refresh_token: Option<String>,
    /// Access-token expiry as UNIX epoch seconds.
    pub expires_at_epoch: u64,
    /// Scope carried by the response (the refresh grant echoes it).
    pub scope: Option<String>,
    /// When this pair was written — drives the client-side refresh-token age
    /// heuristic (Allegro never returns the refresh token's own expiry;
    /// docs give it ~3 months, see `super::REFRESH_TOKEN_MAX_AGE_SECS`).
    pub updated_at_epoch: u64,
}

/// An in-flight device authorization (RFC 8628 §3.2 response subset) kept on
/// disk so a killed process can resume polling the same `device_code`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingDeviceGrant {
    /// Polling handle — single-use and **never** logged or displayed.
    pub device_code: String,
    /// Short code the user types into the verification page.
    pub user_code: String,
    pub verification_uri: String,
    /// Verification URL with the user code pre-filled (optional per spec).
    pub verification_uri_complete: Option<String>,
    /// Server-required minimum seconds between polls.
    pub interval_secs: u64,
    /// When both codes die (UNIX epoch seconds).
    pub expires_at_epoch: u64,
}

/// What [`TokenStore::load`] returns: at most one granted pair and at most
/// one pending grant (an authorized pair and a fresh pending grant can
/// coexist briefly while a re-authorization is in progress).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StoredState {
    pub tokens: Option<StoredTokens>,
    pub pending: Option<PendingDeviceGrant>,
}

/// The JSON envelope written to disk.
///
/// `#[serde(default)]` (no `deny_unknown_fields`): the file is
/// machine-written and forward compatibility is handled by `version`, so
/// unknown/future keys must not break older readers.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct StoreEnvelope {
    version: u64,
    /// `"production"` | `"sandbox"` — see [`env_label`].
    env: String,
    tokens: Option<StoredTokens>,
    #[serde(rename = "pending_device_grant")]
    pending: Option<PendingDeviceGrant>,
}

/// The store's environment label for the given sandbox flag — the single
/// source of truth shared by the writer and the loader's mismatch check.
pub(crate) fn env_label(sandbox: bool) -> &'static str {
    if sandbox {
        "sandbox"
    } else {
        "production"
    }
}

/// Current UNIX time in seconds — the wall clock the on-disk format speaks.
pub(crate) fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        // Clock before the epoch is unrecoverable; 0 fails every expiry
        // check ("expired"), which is the safe direction.
        .unwrap_or(0)
}

// ── TokenStore ────────────────────────────────────────────────────────────────

/// Reads/writes the versioned token file: atomic, `0600`, env-labelled.
/// Cheap to clone (`PathBuf` + `bool`) so the façade, the CLI, and the
/// startup policy can each hold a handle to the same path.
#[derive(Debug, Clone)]
pub struct TokenStore {
    path: PathBuf,
    sandbox: bool,
}

impl TokenStore {
    /// The platform default: `dirs::config_dir()/allegro-mcp/tokens.json`.
    pub fn default_path() -> Result<PathBuf, AuthError> {
        dirs::config_dir()
            .map(|d| d.join("allegro-mcp").join("tokens.json"))
            .ok_or_else(|| {
                AuthError::StoreIo(
                    "cannot determine the user config directory (dirs::config_dir() returned None)"
                        .to_owned(),
                )
            })
    }

    /// Constructs a store bound to an explicit path. `sandbox` selects the
    /// environment label written to (and required by) the file.
    pub fn new(path: PathBuf, sandbox: bool) -> Self {
        Self { path, sandbox }
    }

    /// The file this store reads/writes (exposed for logging and tests).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads the stored state. A missing file is an empty state (first run),
    /// never an error. A corrupt or foreign file is surfaced, **never
    /// auto-deleted** — `auth device` overwrites on success anyway, and
    /// silently wiping a readable token file would be data loss.
    pub fn load(&self) -> Result<StoredState, AuthError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(StoredState::default()),
            Err(e) => return Err(AuthError::StoreIo(format!("{}: {e}", self.path.display()))),
        };

        let envelope: StoreEnvelope = serde_json::from_slice(&bytes).map_err(|e| {
            AuthError::StoreIo(format!(
                "{}: unparsable token store: {e}",
                self.path.display()
            ))
        })?;

        if envelope.version != STORE_FORMAT_VERSION {
            return Err(AuthError::StoreVersion {
                found: envelope.version,
            });
        }

        // Validate the env label against the two known values so the
        // `&'static str` in `EnvMismatch` is never synthesized from file
        // contents (and an unknown label is a loud error, not a mismatch).
        let stored: &'static str = match envelope.env.as_str() {
            "production" => "production",
            "sandbox" => "sandbox",
            other => {
                return Err(AuthError::StoreIo(format!(
                    "{}: unknown environment label {other:?} in token store",
                    self.path.display()
                )))
            }
        };
        let current = env_label(self.sandbox);
        if stored != current {
            return Err(AuthError::EnvMismatch { stored, current });
        }

        Ok(StoredState {
            tokens: envelope.tokens,
            pending: envelope.pending,
        })
    }

    /// Persists the granted pair as the *entire* envelope, dropping any
    /// stale pending grant in the same atomic write: grant completion and
    /// pending-clearance are one operation, so no torn state (tokens + dead
    /// pending) can ever be observed.
    pub fn save_tokens(
        &self,
        pair: &super::TokenResponse,
        effective_expires_in: u64,
    ) -> Result<(), AuthError> {
        let now = epoch_now();
        let envelope = StoreEnvelope {
            version: STORE_FORMAT_VERSION,
            env: env_label(self.sandbox).to_owned(),
            tokens: Some(StoredTokens {
                access_token: pair.access_token.clone(),
                refresh_token: pair.refresh_token.clone(),
                expires_at_epoch: now.saturating_add(effective_expires_in),
                scope: pair.scope.clone(),
                updated_at_epoch: now,
            }),
            pending: None,
        };
        self.write_envelope(&envelope)
    }

    /// Persists an in-flight device grant, preserving any already-granted
    /// tokens (a re-authorization must not destroy working tokens if the
    /// user later denies the new request).
    pub fn save_pending(
        &self,
        resp: &super::device::DeviceAuthorizationResponse,
    ) -> Result<(), AuthError> {
        let existing = self.load()?;
        let envelope = StoreEnvelope {
            version: STORE_FORMAT_VERSION,
            env: env_label(self.sandbox).to_owned(),
            tokens: existing.tokens,
            pending: Some(PendingDeviceGrant {
                device_code: resp.device_code.clone(),
                user_code: resp.user_code.clone(),
                verification_uri: resp.verification_uri.clone(),
                verification_uri_complete: resp.verification_uri_complete.clone(),
                interval_secs: resp.interval,
                expires_at_epoch: epoch_now().saturating_add(resp.expires_in),
            }),
        };
        self.write_envelope(&envelope)
    }

    /// Drops the pending grant while preserving granted tokens (e.g. the
    /// server-side resume task clearing a grant it just completed — though
    /// `save_tokens` already covers that path in one write).
    /// Explicitly discards a stored pending grant (tokens, if any, are
    /// preserved). The normal flows never need this — `save_tokens` drops
    /// the grant on completion and an expired grant is simply overwritten —
    /// but it keeps the store API complete for tooling.
    // Unused inside the bin crate (which privately re-declares this
    // module); kept as deliberate public API.
    #[allow(dead_code)]
    pub fn clear_pending(&self) -> Result<(), AuthError> {
        let existing = self.load()?;
        let envelope = StoreEnvelope {
            version: STORE_FORMAT_VERSION,
            env: env_label(self.sandbox).to_owned(),
            tokens: existing.tokens,
            pending: None,
        };
        self.write_envelope(&envelope)
    }

    /// Removes the file entirely (logout / rejected refresh). Missing file
    /// ⇒ already cleared ⇒ success.
    pub fn clear(&self) -> Result<(), AuthError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AuthError::StoreIo(format!("{}: {e}", self.path.display()))),
        }
    }

    /// Serializes + atomically writes an envelope.
    fn write_envelope(&self, envelope: &StoreEnvelope) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec_pretty(envelope)
            .map_err(|e| AuthError::StoreIo(format!("serialize token store: {e}")))?;
        write_atomic(&self.path, &bytes)
            .map_err(|e| AuthError::StoreIo(format!("{}: {e}", self.path.display())))
    }
}

// ── Atomic write ──────────────────────────────────────────────────────────────

/// Writes `bytes` to `path` atomically and (on unix) with tight permissions:
///
/// 1. `create_dir_all` the parent; on unix, `chmod 0700` **only** a directory
///    this call created (tracked via a before/after `metadata` probe — never
///    touch a pre-existing dir's permissions);
/// 2. create the temp file **in the same directory** as `path` (same
///    filesystem ⇒ `rename` is atomic) with `create_new` (no clobbering) and
///    mode `0600` — no world-readable window ever exists on disk;
/// 3. `write_all` + `sync_all`;
/// 4. `rename` over the destination;
/// 5. best-effort parent-dir `sync_all` (ignore errors — best effort only).
///
/// The temp file is removed on any error path. On non-unix, `.mode()` is
/// skipped (Windows ACLs govern access; the write stays atomic).
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::fs::{File, OpenOptions};
    use std::io::Write;

    // Normalize a bare-filename path to "." so `create_dir_all`/`File::open`
    // below never see an empty parent.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let existed_before = parent.metadata().is_ok();
    std::fs::create_dir_all(parent)?;
    if !existed_before {
        // We created this directory — tighten it. Best effort: the 0600 file
        // mode already protects the contents.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    let file_name = path.file_name().map_or_else(
        || "tokens.json".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let tmp = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));

    let result = (|| -> io::Result<()> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;

        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        // Best-effort cleanup; the original error (if any) is reported.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::TokenResponse;

    fn token_pair(access: &str, refresh: Option<&str>) -> TokenResponse {
        TokenResponse {
            access_token: access.to_owned(),
            expires_in: 3600,
            token_type: Some("bearer".to_owned()),
            refresh_token: refresh.map(str::to_owned),
            scope: Some("allegro:api:read".to_owned()),
            jti: None,
        }
    }

    fn grant() -> super::super::device::DeviceAuthorizationResponse {
        super::super::device::DeviceAuthorizationResponse {
            user_code: "cbt3zdu4g".to_owned(),
            device_code: "645629715".to_owned(),
            expires_in: 3600,
            interval: 5,
            verification_uri: "https://allegro.pl/skojarz-aplikacje".to_owned(),
            verification_uri_complete: None,
        }
    }

    fn store_in(dir: &Path) -> TokenStore {
        TokenStore::new(dir.join("tokens.json"), false)
    }

    // ── round-trip ────────────────────────────────────────────────────────────

    #[test]
    fn round_trip_pending_then_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path());

        store.save_pending(&grant()).expect("save pending");
        let state = store.load().expect("load");
        let pending = state.pending.expect("pending persisted");
        assert_eq!(pending.device_code, "645629715");
        assert_eq!(pending.interval_secs, 5);
        assert!(pending.expires_at_epoch > epoch_now());
        assert!(
            state.tokens.is_none(),
            "no tokens before the grant completes"
        );

        store
            .save_tokens(&token_pair("tok", Some("rfr")), 3600)
            .expect("save tokens");
        let state = store.load().expect("load");
        let tokens = state.tokens.expect("tokens persisted");
        assert_eq!(tokens.access_token, "tok");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rfr"));
        assert!(tokens.expires_at_epoch > epoch_now(), "epoch expiry stored");
        assert!(state.pending.is_none(), "grant completion clears pending");
    }

    #[test]
    fn load_missing_file_returns_default_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path());
        let state = store.load().expect("missing file must not error");
        assert_eq!(state, StoredState::default());
    }

    // ── corrupt / foreign files ───────────────────────────────────────────────

    #[test]
    fn load_corrupt_json_errors_naming_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path());
        std::fs::write(store.path(), "not json at all").expect("write garbage");

        let err = store.load().expect_err("corrupt file must error");
        let msg = err.to_string();
        assert!(
            matches!(err, AuthError::StoreIo(_)),
            "expected StoreIo, got: {err:?}"
        );
        assert!(
            msg.contains(store.path().to_string_lossy().as_ref()),
            "error must name the offending path, got: {msg}"
        );
    }

    #[test]
    fn load_newer_version_is_a_store_version_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path());
        std::fs::write(store.path(), r#"{"version": 2, "env": "production"}"#)
            .expect("write v2 file");

        let err = store.load().expect_err("newer version must be refused");
        assert!(
            matches!(err, AuthError::StoreVersion { found: 2 }),
            "expected StoreVersion {{ found: 2 }}, got: {err:?}"
        );
    }

    #[test]
    fn load_env_mismatch_is_a_hard_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Production tokens, sandbox store.
        let store = TokenStore::new(dir.path().join("tokens.json"), true);
        std::fs::write(
            store.path(),
            r#"{"version": 1, "env": "production", "tokens": {"access_token": "t", "expires_at_epoch": 1, "updated_at_epoch": 1}}"#,
        )
        .expect("write prod file");

        let err = store.load().expect_err("env mismatch must error");
        assert!(
            matches!(
                err,
                AuthError::EnvMismatch {
                    stored: "production",
                    current: "sandbox"
                }
            ),
            "expected EnvMismatch, got: {err:?}"
        );
    }

    #[test]
    fn load_unknown_env_label_is_a_store_io_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path());
        std::fs::write(store.path(), r#"{"version": 1, "env": "staging"}"#).expect("write bad env");
        assert!(
            matches!(store.load(), Err(AuthError::StoreIo(_))),
            "unknown env label must be a loud error"
        );
    }

    // ── atomicity / permissions ───────────────────────────────────────────────

    #[test]
    fn save_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path());
        store.save_pending(&grant()).expect("save pending");
        store
            .save_tokens(&token_pair("tok", None), 3600)
            .expect("save tokens");
        store.clear_pending().expect("clear pending");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no .tmp-* files may survive a save, found: {leftovers:?}"
        );
    }

    /// `#[cfg(unix)]` — file mode `0600`, created parent dir `0700`.
    #[cfg(unix)]
    #[test]
    fn saved_file_and_created_dir_have_tight_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("allegro-mcp-created");
        let store = TokenStore::new(nested.join("tokens.json"), false);

        store
            .save_tokens(&token_pair("tok", None), 3600)
            .expect("save");

        let file_mode = std::fs::metadata(store.path())
            .expect("file metadata")
            .permissions()
            .mode();
        assert_eq!(
            file_mode & 0o777,
            0o600,
            "token file must be owner-read/write only"
        );
        let dir_mode = std::fs::metadata(&nested)
            .expect("dir metadata")
            .permissions()
            .mode();
        assert_eq!(
            dir_mode & 0o777,
            0o700,
            "a dir created by the store must be owner-only"
        );
    }

    /// A pre-existing directory must never be re-chmod'd by the store.
    #[cfg(unix)]
    #[test]
    fn preexisting_dir_permissions_are_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        // 0755 pre-existing dir.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");

        let store = store_in(dir.path());
        store
            .save_tokens(&token_pair("tok", None), 3600)
            .expect("save");

        let dir_mode = std::fs::metadata(dir.path())
            .expect("dir metadata")
            .permissions()
            .mode();
        assert_eq!(
            dir_mode & 0o777,
            0o755,
            "the store must never chmod a directory it did not create"
        );
    }

    // ── envelope surgery ──────────────────────────────────────────────────────

    #[test]
    fn save_tokens_drops_a_stale_pending_grant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path());
        store.save_pending(&grant()).expect("save pending");

        store
            .save_tokens(&token_pair("tok", None), 3600)
            .expect("save tokens");
        let state = store.load().expect("load");
        assert!(state.tokens.is_some());
        assert!(state.pending.is_none(), "stale pending must not survive");
    }

    #[test]
    fn save_pending_preserves_existing_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path());
        store
            .save_tokens(&token_pair("tok", Some("rfr")), 3600)
            .expect("save");
        store.save_pending(&grant()).expect("save pending");

        let state = store.load().expect("load");
        assert_eq!(
            state.tokens.expect("tokens preserved").access_token,
            "tok",
            "a re-authorization must not destroy working tokens"
        );
        assert!(state.pending.is_some());
    }

    #[test]
    fn clear_pending_keeps_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path());
        store
            .save_tokens(&token_pair("tok", None), 3600)
            .expect("save");
        store.save_pending(&grant()).expect("save pending");

        store.clear_pending().expect("clear pending");
        let state = store.load().expect("load");
        assert!(state.tokens.is_some(), "tokens must survive clear_pending");
        assert!(state.pending.is_none());
    }

    #[test]
    fn clear_removes_the_file_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path());
        store
            .save_tokens(&token_pair("tok", None), 3600)
            .expect("save");

        store.clear().expect("clear");
        assert!(!store.path().exists(), "clear must remove the file");
        store
            .clear()
            .expect("second clear must stay Ok (NotFound ⇒ Ok)");
    }

    // ── default path / env label ──────────────────────────────────────────────

    #[test]
    fn env_label_follows_sandbox_flag() {
        assert_eq!(env_label(false), "production");
        assert_eq!(env_label(true), "sandbox");
    }

    #[test]
    fn default_path_lands_in_the_allegro_mcp_config_dir() {
        // Only the shape is asserted — `dirs::config_dir()` is environment
        // dependent (and None on some CI sandboxes, which must be an error,
        // not a panic).
        match TokenStore::default_path() {
            Ok(path) => {
                assert_eq!(path.file_name().unwrap(), "tokens.json");
                assert_eq!(
                    path.parent().and_then(|p| p.file_name()),
                    Some(std::ffi::OsStr::new("allegro-mcp"))
                );
            }
            Err(AuthError::StoreIo(msg)) => {
                assert!(msg.contains("config directory"));
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}
