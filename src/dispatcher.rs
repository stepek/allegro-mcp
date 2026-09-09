//! HTTP dispatch layer — turns a [`crate::tool_registry::ToolDef`] and a set
//! of MCP tool arguments into an authenticated HTTP request against the
//! Allegro REST API, and returns the response body as text.

use serde_json::Value;

/// Maximum response body size returned to the MCP client (100 KB).
const MAX_BODY_BYTES: usize = 102_400;

/// Returns the Allegro API base URL for the given environment.
fn allegro_api_base(sandbox: bool) -> &'static str {
    if sandbox {
        "https://api.allegrosandbox.pl"
    } else {
        "https://api.allegro.pl"
    }
}

/// Substitutes `{paramName}` segments in `path` with values from `arguments`.
///
/// Returns the substituted path and the list of argument names that were
/// consumed. Path params missing from `arguments` are left as literals.
fn substitute_path_params(
    path: &str,
    arguments: &serde_json::Map<String, Value>,
) -> (String, Vec<String>) {
    let mut result = String::with_capacity(path.len());
    let mut consumed = Vec::new();
    let bytes = path.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = path[i..].find('}') {
                let end = i + end;
                let name = &path[i + 1..end];
                if let Some(value) = arguments.get(name) {
                    let encoded = match value {
                        Value::String(s) => percent_encode(s),
                        other => percent_encode(&other.to_string()),
                    };
                    result.push_str(&encoded);
                    consumed.push(name.to_string());
                } else {
                    result.push_str(&path[i..=end]);
                }
                i = end + 1;
                continue;
            }
        }
        // Advance by one char (not byte) to stay on UTF-8 boundaries.
        let ch = path[i..].chars().next().unwrap();
        result.push(ch);
        i += ch.len_utf8();
    }
    (result, consumed)
}

/// Percent-encodes a string for use in a URL path segment.
///
/// Encodes everything except unreserved characters (RFC 3986):
/// ALPHA / DIGIT / "-" / "." / "_" / "~".
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

/// Truncates `body` to at most [`MAX_BODY_BYTES`] bytes at a UTF-8 char
/// boundary, appending `"\n[truncated]"` if truncation occurred.
fn truncate_body(body: String) -> String {
    if body.len() <= MAX_BODY_BYTES {
        return body;
    }
    let mut pos = MAX_BODY_BYTES;
    while pos > 0 && !body.is_char_boundary(pos) {
        pos -= 1;
    }
    let mut truncated = body[..pos].to_string();
    truncated.push_str("\n[truncated]");
    truncated
}

/// Dispatches an HTTP request for the given tool and arguments, returning
/// the response body as text (truncated to 100 KB) on success, or an error
/// message on failure.
pub async fn dispatch(
    auth: &crate::auth::AllegroAuth,
    http: &reqwest::Client,
    sandbox: bool,
    tool_def: &crate::tool_registry::ToolDef,
    arguments: serde_json::Map<String, Value>,
) -> Result<String, String> {
    let token = auth.token().await.map_err(|e| format!("auth error: {e}"))?;

    let (path, consumed) = substitute_path_params(&tool_def.path, &arguments);
    let url = format!("{}{}", allegro_api_base(sandbox), path);

    let method = reqwest::Method::from_bytes(tool_def.method.to_uppercase().as_bytes())
        .map_err(|_| format!("unsupported HTTP method: {}", tool_def.method))?;

    let mut remaining = arguments;
    for name in &consumed {
        remaining.remove(name);
    }

    let mut builder = http
        .request(method.clone(), &url)
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "allegro-mcp/0.1.0")
        .header("Accept", "application/vnd.allegro.public.v1+json");

    if matches!(method, reqwest::Method::GET | reqwest::Method::HEAD) {
        let pairs: Vec<(String, String)> = remaining
            .into_iter()
            .map(|(k, v)| {
                let value_str = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                (k, value_str)
            })
            .collect();
        builder = builder.query(&pairs);
    } else if let Some(body) = remaining.remove("body") {
        // CONVENTION: "body" key is reserved for requestBody content; set by
        // schema_builder.rs. `schema_builder::build_input_schema` only adds a
        // top-level "body" property to a tool's input_schema when the OpenAPI
        // operation declares a `requestBody`, so a non-GET/HEAD tool call
        // arriving with a "body" argument is always the intended request
        // payload, never an unrelated query/form field with the same name.
        builder = builder.json(&body);
    } else {
        builder = builder.json(&remaining);
    }

    let response = builder
        .send()
        .await
        .map_err(|e| format!("HTTP error: {e}"))?;

    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        let body = response
            .text()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;
        return Err(format!("HTTP {status}: {body}"));
    }

    let body = response
        .text()
        .await
        .map_err(|e| format!("HTTP error: {e}"))?;

    Ok(truncate_body(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect()
    }

    #[test]
    fn test_substitute_path_params_replaces_single_param() {
        let args = map(&[("offerId", "123")]);
        let (path, consumed) = substitute_path_params("/sale/offers/{offerId}", &args);
        assert_eq!(path, "/sale/offers/123");
        assert_eq!(consumed, vec!["offerId".to_string()]);
    }

    #[test]
    fn test_substitute_path_params_no_params() {
        let args = map(&[]);
        let (path, consumed) = substitute_path_params("/sale/offers", &args);
        assert_eq!(path, "/sale/offers");
        assert!(consumed.is_empty());
    }

    #[test]
    fn test_substitute_path_params_missing_param_left_as_literal() {
        let args = map(&[]);
        let (path, consumed) = substitute_path_params("/sale/offers/{offerId}", &args);
        assert_eq!(path, "/sale/offers/{offerId}");
        assert!(consumed.is_empty());
    }

    #[test]
    fn test_substitute_path_params_multiple_params() {
        let args = map(&[("offerId", "123"), ("itemId", "456")]);
        let (path, consumed) =
            substitute_path_params("/sale/offers/{offerId}/items/{itemId}", &args);
        assert_eq!(path, "/sale/offers/123/items/456");
        assert_eq!(consumed.len(), 2);
        assert!(consumed.contains(&"offerId".to_string()));
        assert!(consumed.contains(&"itemId".to_string()));
    }

    #[test]
    fn test_truncate_body_at_100kb() {
        let body = "a".repeat(MAX_BODY_BYTES + 100);
        let truncated = truncate_body(body);
        assert!(truncated.ends_with("[truncated]"));
        assert!(truncated.len() <= MAX_BODY_BYTES + "\n[truncated]".len());
    }

    #[test]
    fn test_truncate_body_not_applied_under_100kb() {
        let body = "small body".to_string();
        let truncated = truncate_body(body.clone());
        assert_eq!(truncated, body);
    }

    #[test]
    fn test_truncate_body_at_utf8_boundary() {
        // Build a string where a multi-byte char straddles the truncation point.
        let mut body = "a".repeat(MAX_BODY_BYTES - 1);
        body.push('€'); // 3-byte UTF-8 char straddling the cutoff
        body.push_str(&"b".repeat(50));
        let truncated = truncate_body(body);
        assert!(truncated.ends_with("[truncated]"));
        // Must always be valid UTF-8 (String type guarantees this if constructed successfully).
        assert!(truncated.is_char_boundary(truncated.len() - "\n[truncated]".len()));
    }

    #[test]
    fn test_allegro_api_base_production() {
        assert_eq!(allegro_api_base(false), "https://api.allegro.pl");
    }

    #[test]
    fn test_allegro_api_base_sandbox() {
        assert_eq!(allegro_api_base(true), "https://api.allegrosandbox.pl");
    }

    #[tokio::test]
    async fn test_dispatch_unsupported_method_returns_error() {
        // Auth must succeed first (dispatch fetches the token before parsing
        // the method), so mock a valid token response.
        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/auth/oauth/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "access_token": "tok", "expires_in": 3600 }),
                ),
            )
            .mount(&mock_server)
            .await;

        let auth = crate::auth::AllegroAuth::with_base_url(
            "id".to_string(),
            "secret".to_string(),
            mock_server.uri(),
        );
        let http = reqwest::Client::new();
        let tool_def = crate::tool_registry::ToolDef {
            id: "allegro_bad_method".to_string(),
            name: "allegro_bad_method".to_string(),
            description: "d".to_string(),
            input_schema: serde_json::json!({}),
            // A space is not a valid HTTP token character, so this is
            // guaranteed to be rejected by `Method::from_bytes`.
            method: "get post".to_string(),
            path: "/x".to_string(),
        };

        let result = dispatch(&auth, &http, false, &tool_def, serde_json::Map::new()).await;
        let err = result.expect_err("invalid method must be rejected");
        assert!(
            err.contains("unsupported HTTP method"),
            "expected 'unsupported HTTP method' error, got: {err}"
        );
    }
}
