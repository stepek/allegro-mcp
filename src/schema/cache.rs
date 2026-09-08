use crate::schema::SchemaError;
use std::path::PathBuf;

/// Returns the cache directory path.
/// Respects `ALLEGRO_MCP_CACHE_DIR` env var (for tests/containers).
/// Falls back to `dirs::cache_dir()/allegro-mcp/`.
pub fn cache_dir() -> Result<PathBuf, SchemaError> {
    if let Ok(dir) = std::env::var("ALLEGRO_MCP_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    dirs::cache_dir()
        .map(|d| d.join("allegro-mcp"))
        .ok_or(SchemaError::NoCacheDir)
}

/// Returns the path to the cached swagger.yaml.
pub fn cache_path() -> Result<PathBuf, SchemaError> {
    Ok(cache_dir()?.join("swagger.yaml"))
}

/// Returns the path to the sha256 file.
pub fn sha256_path() -> Result<PathBuf, SchemaError> {
    Ok(cache_dir()?.join("swagger.yaml.sha256"))
}

/// Write bytes to the cache file, creating directories as needed.
/// Also writes the sha256 of the bytes to a sidecar file.
pub fn write_cache(bytes: &[u8]) -> Result<(), SchemaError> {
    use sha2::{Digest, Sha256};
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(cache_path()?, bytes)?;
    let hash = format!("{:x}", Sha256::digest(bytes));
    std::fs::write(sha256_path()?, hash)?;
    Ok(())
}

/// Read bytes from the cache file.
pub fn read_cache() -> Result<Vec<u8>, SchemaError> {
    Ok(std::fs::read(cache_path()?)?)
}

/// Read the stored sha256 of the last good schema (empty string if absent).
pub fn read_cached_sha256() -> String {
    sha256_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

/// Compute sha256 of bytes and compare to cached value.
/// Returns `true` if the schema has changed since last cache write.
pub fn detect_drift(bytes: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let current = format!("{:x}", Sha256::digest(bytes));
    let cached = read_cached_sha256();
    !cached.is_empty() && current != cached
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Run `f` with `ALLEGRO_MCP_CACHE_DIR` pointing at a fresh temp dir,
    /// then clean up the env var regardless of outcome.
    fn with_temp_cache<F: FnOnce(&std::path::Path)>(f: F) {
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: tests using this helper are marked #[serial] so only one
        // thread mutates the env var at a time.
        unsafe { std::env::set_var("ALLEGRO_MCP_CACHE_DIR", dir.path()) };
        f(dir.path());
        unsafe { std::env::remove_var("ALLEGRO_MCP_CACHE_DIR") };
    }

    // ── cache_dir / cache_path / sha256_path ─────────────────────────────

    #[test]
    #[serial]
    fn cache_dir_uses_env_var() {
        with_temp_cache(|tmp| {
            let dir = cache_dir().expect("cache_dir should succeed");
            assert_eq!(dir, tmp);
        });
    }

    #[test]
    #[serial]
    fn cache_path_is_swagger_yaml_inside_cache_dir() {
        with_temp_cache(|tmp| {
            let path = cache_path().expect("cache_path should succeed");
            assert_eq!(path, tmp.join("swagger.yaml"));
        });
    }

    #[test]
    #[serial]
    fn sha256_path_is_sidecar_inside_cache_dir() {
        with_temp_cache(|tmp| {
            let path = sha256_path().expect("sha256_path should succeed");
            assert_eq!(path, tmp.join("swagger.yaml.sha256"));
        });
    }

    // ── read_cached_sha256 ───────────────────────────────────────────────

    #[test]
    #[serial]
    fn read_cached_sha256_returns_empty_when_no_cache_exists() {
        with_temp_cache(|_tmp| {
            // No write_cache call — sidecar file does not exist.
            let sha = read_cached_sha256();
            assert_eq!(sha, "", "should return empty string when sidecar is absent");
        });
    }

    #[test]
    #[serial]
    fn read_cached_sha256_returns_written_hash() {
        with_temp_cache(|_tmp| {
            let data = b"hello world";
            write_cache(data).expect("write_cache");
            let sha = read_cached_sha256();
            assert_eq!(sha.len(), 64, "sha256 hex string should be 64 chars");
            // Verify it is the correct sha256 of the written bytes.
            use sha2::{Digest, Sha256};
            let expected = format!("{:x}", Sha256::digest(data));
            assert_eq!(sha, expected);
        });
    }

    // ── detect_drift ─────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn detect_drift_returns_false_when_no_cache_exists() {
        with_temp_cache(|_tmp| {
            // No prior write → cached sha is empty → no drift by definition.
            assert!(
                !detect_drift(b"any bytes"),
                "drift should be false when there is no cached sha"
            );
        });
    }

    #[test]
    #[serial]
    fn detect_drift_returns_false_when_bytes_match_cache() {
        with_temp_cache(|_tmp| {
            let data = b"stable schema bytes";
            write_cache(data).expect("write_cache");
            assert!(
                !detect_drift(data),
                "drift should be false when bytes are identical to cached version"
            );
        });
    }

    #[test]
    #[serial]
    fn detect_drift_returns_true_when_bytes_differ_from_cache() {
        with_temp_cache(|_tmp| {
            let original = b"original schema";
            let updated = b"updated schema - something changed";
            write_cache(original).expect("write_cache");
            assert!(
                detect_drift(updated),
                "drift should be true when bytes differ from cached version"
            );
        });
    }

    // ── write_cache / read_cache roundtrip ───────────────────────────────

    #[test]
    #[serial]
    fn write_cache_creates_parent_directories() {
        // Use a nested path that does not yet exist.
        let base = tempfile::tempdir().expect("tempdir");
        let nested = base.path().join("a").join("b").join("c");
        unsafe { std::env::set_var("ALLEGRO_MCP_CACHE_DIR", &nested) };
        let result = write_cache(b"data");
        unsafe { std::env::remove_var("ALLEGRO_MCP_CACHE_DIR") };
        result.expect("write_cache should create missing parent directories");
        assert!(nested.join("swagger.yaml").exists());
    }

    #[test]
    #[serial]
    fn read_cache_returns_error_when_no_file_exists() {
        with_temp_cache(|_tmp| {
            let err = read_cache().expect_err("read_cache should fail when cache is empty");
            assert!(
                matches!(err, SchemaError::Io(_)),
                "expected SchemaError::Io, got: {err:?}"
            );
        });
    }
}
