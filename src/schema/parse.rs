use crate::schema::SchemaError;
use openapiv3::OpenAPI;

/// Parse raw YAML bytes into an OpenAPI document.
/// Fails fast with a clear error if the document is not OAS 3.x.
pub fn parse_bytes(bytes: &[u8]) -> Result<OpenAPI, SchemaError> {
    let api: OpenAPI = serde_yaml::from_slice(bytes)?;
    if !api.openapi.starts_with("3.") {
        return Err(SchemaError::NotOas3 {
            found: api.openapi.clone(),
        });
    }
    Ok(api)
}
