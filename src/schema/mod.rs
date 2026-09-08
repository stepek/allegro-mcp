pub mod cache;
pub mod fetch;
pub mod parse;

use openapiv3::OpenAPI;
use thiserror::Error;

/// Where to load the schema from.
#[derive(Debug, Clone)]
pub enum SchemaSource {
    /// Fetch from a URL (default: https://developer.allegro.pl/swagger.yaml)
    Url(String),
    /// Load from a local file path (--schema-file flag)
    File(std::path::PathBuf),
}

impl Default for SchemaSource {
    fn default() -> Self {
        Self::Url("https://developer.allegro.pl/swagger.yaml".to_string())
    }
}

/// All errors that can occur in the schema pipeline.
#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("HTTP fetch failed: {0}")]
    Fetch(#[from] reqwest::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("Not an OpenAPI 3.x document (found openapi field: {found:?})")]
    NotOas3 { found: String },

    #[error("Cache directory unavailable")]
    NoCacheDir,
}

/// Statistics about the loaded schema.
#[derive(Debug)]
pub struct SchemaStats {
    pub path_count: usize,
    pub operation_count: usize,
    pub parameter_count: usize,
    pub sha256: String,
}

/// Load the schema from the given source.
/// Returns the parsed OpenAPI document and the raw YAML bytes.
/// On URL fetch failure, falls back to the cached copy if available.
pub async fn load(source: &SchemaSource) -> Result<(OpenAPI, Vec<u8>), SchemaError> {
    let bytes = match source {
        SchemaSource::File(path) => std::fs::read(path)?,
        SchemaSource::Url(url) => match fetch::fetch_bytes(url).await {
            Ok(b) => {
                let _ = cache::write_cache(&b);
                b
            }
            Err(e) => {
                tracing::warn!("fetch failed ({}), trying cache", e);
                cache::read_cache()?
            }
        },
    };
    let api = parse::parse_bytes(&bytes)?;
    Ok((api, bytes))
}

/// Compute statistics from a parsed OpenAPI document and its raw bytes.
pub fn compute_stats(api: &OpenAPI, raw: &[u8]) -> SchemaStats {
    use sha2::{Digest, Sha256};
    let mut op_count = 0usize;
    let mut param_count = 0usize;
    for (_path, item) in &api.paths.paths {
        if let openapiv3::ReferenceOr::Item(item) = item {
            // PathItem::iter() yields (&str, &Operation) for all present operations
            for (_method, op) in item.iter() {
                op_count += 1;
                param_count += op.parameters.len();
            }
        }
    }
    SchemaStats {
        path_count: api.paths.paths.len(),
        operation_count: op_count,
        parameter_count: param_count,
        sha256: format!("{:x}", Sha256::digest(raw)),
    }
}
