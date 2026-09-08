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
// Not yet wired into the bin's CLI output; part of the public cache API.
#[allow(dead_code)]
pub fn read_cached_sha256() -> String {
    sha256_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

/// Compute sha256 of bytes and compare to cached value.
/// Returns `true` if the schema has changed since last cache write.
// Not yet wired into the bin's CLI output; part of the public cache API.
#[allow(dead_code)]
pub fn detect_drift(bytes: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let current = format!("{:x}", Sha256::digest(bytes));
    let cached = read_cached_sha256();
    !cached.is_empty() && current != cached
}
