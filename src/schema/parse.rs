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

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_OAS3: &[u8] = br#"
openapi: "3.0.3"
info:
  title: Test API
  version: "1.0.0"
paths: {}
"#;

    // A Swagger 2.x doc uses `swagger:` instead of `openapi:`.
    // The openapiv3 deserialiser requires the `openapi` field, so this
    // fails at the serde step (missing field) before the version guard.
    const SWAGGER2_DOC: &[u8] = br#"
swagger: "2.0"
info:
  title: Old API
  version: "1.0"
paths: {}
"#;

    // A doc that has the `openapi` key but with a non-3.x value triggers
    // the explicit version guard inside parse_bytes.
    const FAKE_OAS2_WITH_OPENAPI_KEY: &[u8] = br#"
openapi: "2.0"
info:
  title: Fake
  version: "1.0"
paths: {}
"#;

    const MALFORMED_YAML: &[u8] = b"openapi: [unclosed bracket\ninfo: {bad: yaml: here";

    #[test]
    fn parse_valid_oas3_succeeds() {
        let api = parse_bytes(MINIMAL_OAS3).expect("should parse valid OAS 3.x");
        assert!(
            api.openapi.starts_with("3."),
            "openapi field should start with '3.', got: {}",
            api.openapi
        );
        assert_eq!(api.info.title, "Test API");
    }

    #[test]
    fn parse_oas3_version_field_preserved() {
        let api = parse_bytes(MINIMAL_OAS3).unwrap();
        assert_eq!(api.openapi, "3.0.3");
    }

    #[test]
    fn parse_swagger2_doc_returns_yaml_error() {
        // Swagger 2.x uses `swagger:` not `openapi:` — the openapiv3 deserialiser
        // rejects it with a missing-field error before the version guard fires.
        let err = parse_bytes(SWAGGER2_DOC).expect_err("Swagger 2.x should be rejected");
        assert!(
            matches!(err, SchemaError::Yaml(_)),
            "expected SchemaError::Yaml (missing `openapi` field), got: {err:?}"
        );
    }

    #[test]
    fn parse_non_3x_openapi_field_returns_not_oas3_error() {
        // A doc that has `openapi: "2.0"` (not a real format, but exercises the
        // version guard branch inside parse_bytes).
        let err = parse_bytes(FAKE_OAS2_WITH_OPENAPI_KEY)
            .expect_err("non-3.x openapi field should be rejected");
        match err {
            SchemaError::NotOas3 { found } => {
                assert_eq!(found, "2.0", "error should carry the found version string");
            }
            other => panic!("expected NotOas3, got: {other:?}"),
        }
    }

    #[test]
    fn parse_malformed_yaml_returns_yaml_error() {
        let err = parse_bytes(MALFORMED_YAML).expect_err("malformed YAML should fail");
        assert!(
            matches!(err, SchemaError::Yaml(_)),
            "expected SchemaError::Yaml, got: {err:?}"
        );
    }

    #[test]
    fn parse_empty_bytes_returns_yaml_error() {
        let err = parse_bytes(b"").expect_err("empty input should fail");
        assert!(
            matches!(err, SchemaError::Yaml(_)),
            "expected SchemaError::Yaml for empty input, got: {err:?}"
        );
    }
}
