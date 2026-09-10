//! HTTP dispatch layer — turns a [`crate::tool_registry::ToolDef`] and a set
//! of MCP tool arguments into an authenticated HTTP request against the
//! Allegro REST API, and returns the response body as text.

use serde_json::Value;

/// Maximum response body size returned to the MCP client (100 KB).
const MAX_BODY_BYTES: usize = 102_400;

/// Returns the Allegro API base URL for the given environment.
///
/// Delegates to [`crate::config::api_base_url`] — the single source of truth
/// shared with the auth-host selection, so the sandbox flag can never swap
/// one host but not the other.
fn allegro_api_base(sandbox: bool) -> &'static str {
    crate::config::api_base_url(sandbox)
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

/// Sets the versioned `Content-Type` when the schema declares one, before
/// `.json()` runs.
///
/// Ordering rule: reqwest's `.header()` appends and `.json()` only fills
/// Content-Type when it is absent, so exactly one Content-Type header is
/// sent. Only `vnd.allegro` types (the only ones extraction can produce) are
/// set explicitly; otherwise `.json()` applies `application/json` as today.
fn with_versioned_content_type(
    builder: reqwest::RequestBuilder,
    tool_def: &crate::tool_registry::ToolDef,
) -> reqwest::RequestBuilder {
    match tool_def.accept_media_type.as_deref() {
        Some(media_type) => builder.header(reqwest::header::CONTENT_TYPE, media_type),
        None => builder,
    }
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
    dispatch_with_base(auth, http, allegro_api_base(sandbox), tool_def, arguments).await
}

/// Like [`dispatch`] but accepts an explicit API base URL instead of deriving
/// it from the `sandbox` flag.
///
/// Exposed as `pub` (with `#[doc(hidden)]`) so that integration tests in
/// `tests/` — which are compiled as a separate crate — can inject a wiremock
/// base URL without changing the production public API. Production callers
/// should use [`dispatch`] instead.
#[doc(hidden)]
pub async fn dispatch_with_base(
    auth: &crate::auth::AllegroAuth,
    http: &reqwest::Client,
    api_base: &str,
    tool_def: &crate::tool_registry::ToolDef,
    arguments: serde_json::Map<String, Value>,
) -> Result<String, String> {
    let token = auth.token().await.map_err(|e| format!("auth error: {e}"))?;

    let (path, consumed) = substitute_path_params(&tool_def.path, &arguments);
    let url = format!("{}{}", api_base, path);

    let method = reqwest::Method::from_bytes(tool_def.method.to_uppercase().as_bytes())
        .map_err(|_| format!("unsupported HTTP method: {}", tool_def.method))?;

    let mut remaining = arguments;
    for name in &consumed {
        remaining.remove(name);
    }

    // Per-op versioned Accept when the schema declares one (see
    // `ToolDef::accept_media_type`), the dispatcher default otherwise.
    // `User-Agent` + `Accept-Language` come from the shared client built by
    // `crate::http` — never set here, so every request carries them.
    let accept = tool_def
        .accept_media_type
        .as_deref()
        .unwrap_or(crate::http::DEFAULT_ACCEPT);

    // The request "plan" is computed once so the 401 retry below can rebuild
    // the request from identical inputs (`remaining` is consumed while
    // shaping the body; the plan freezes the result).
    enum Plan {
        Query(Vec<(String, String)>),
        ReservedBody(Value),
        JsonMap(serde_json::Map<String, Value>),
    }
    let plan = if matches!(method, reqwest::Method::GET | reqwest::Method::HEAD) {
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
        Plan::Query(pairs)
    } else if let Some(body) = remaining.remove("body") {
        // CONVENTION: "body" key is reserved for requestBody content; set by
        // schema_builder.rs. `schema_builder::build_input_schema` only adds a
        // top-level "body" property to a tool's input_schema when the OpenAPI
        // operation declares a `requestBody`, so a non-GET/HEAD tool call
        // arriving with a "body" argument is always the intended request
        // payload, never an unrelated query/form field with the same name.
        Plan::ReservedBody(body)
    } else {
        Plan::JsonMap(remaining)
    };

    let build = |token: &str| -> reqwest::RequestBuilder {
        let builder = http
            .request(method.clone(), &url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", accept);
        match &plan {
            Plan::Query(pairs) => builder.query(pairs),
            Plan::ReservedBody(body) => with_versioned_content_type(builder, tool_def).json(body),
            Plan::JsonMap(map) => with_versioned_content_type(builder, tool_def).json(map),
        }
    };

    let response = build(&token)
        .send()
        .await
        .map_err(|e| format!("HTTP error: {e}"))?;

    // Single 401 retry with token re-resolution. Device tokens are
    // user-scoped and die out-of-band (password change, app unlink, the
    // 20-active-sessions cap — none of which the 60 s pre-expiry refresh
    // can see), so this hook is the only recovery path short of a full
    // `allegro-mcp auth device` re-run. Exactly one retry, bounded: if the
    // retry also fails, its error is returned (the post-refresh body is
    // more diagnostic than the original 401's).
    let response = if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        auth.invalidate().await;
        let fresh = auth.token().await.map_err(|e| format!("auth error: {e}"))?;
        build(&fresh)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
    } else {
        response
    };

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

    /// The dispatcher's host selection must delegate to (and therefore never
    /// diverge from) the config module's single source of truth.
    #[test]
    fn test_allegro_api_base_delegates_to_config_helper() {
        assert_eq!(allegro_api_base(false), crate::config::api_base_url(false));
        assert_eq!(allegro_api_base(true), crate::config::api_base_url(true));
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
            accept_media_type: None,
        };

        let result = dispatch(&auth, &http, false, &tool_def, serde_json::Map::new()).await;
        let err = result.expect_err("invalid method must be rejected");
        assert!(
            err.contains("unsupported HTTP method"),
            "expected 'unsupported HTTP method' error, got: {err}"
        );
    }

    // ── percent_encode ────────────────────────────────────────────────────────

    #[test]
    fn test_percent_encode_unreserved_chars_unchanged() {
        // RFC 3986 unreserved: ALPHA / DIGIT / "-" / "." / "_" / "~"
        let input = "abcXYZ0129-._~";
        assert_eq!(percent_encode(input), input);
    }

    #[test]
    fn test_percent_encode_space_becomes_percent_20() {
        assert_eq!(percent_encode("hello world"), "hello%20world");
    }

    #[test]
    fn test_percent_encode_slash_becomes_percent_2f() {
        assert_eq!(percent_encode("a/b"), "a%2Fb");
    }

    #[test]
    fn test_percent_encode_empty_string() {
        assert_eq!(percent_encode(""), "");
    }

    #[test]
    fn test_percent_encode_special_chars() {
        // Curly braces, colons, and at-signs must be encoded.
        let encoded = percent_encode("{id}");
        assert_eq!(encoded, "%7Bid%7D");
    }

    // ── substitute_path_params with non-string JSON values ───────────────────

    #[test]
    fn test_substitute_path_params_numeric_value() {
        // Non-string JSON values (numbers) must be stringified and encoded.
        let mut args = serde_json::Map::new();
        args.insert("offerId".to_string(), serde_json::Value::Number(42.into()));
        let (path, consumed) = substitute_path_params("/sale/offers/{offerId}", &args);
        assert_eq!(path, "/sale/offers/42");
        assert_eq!(consumed, vec!["offerId".to_string()]);
    }

    #[test]
    fn test_substitute_path_params_value_with_special_chars_is_encoded() {
        // Values containing URL-unsafe characters must be percent-encoded.
        let args = map(&[("q", "hello world")]);
        let (path, consumed) = substitute_path_params("/search/{q}", &args);
        assert_eq!(path, "/search/hello%20world");
        assert_eq!(consumed, vec!["q".to_string()]);
    }

    // ── truncate_body exact boundary ─────────────────────────────────────────

    #[test]
    fn test_truncate_body_exactly_at_100kb_is_not_truncated() {
        // A body of exactly MAX_BODY_BYTES bytes must be returned unchanged.
        let body = "x".repeat(MAX_BODY_BYTES);
        let result = truncate_body(body.clone());
        assert_eq!(result, body, "body at exactly 100 KB must not be truncated");
    }

    // ── dispatch: auth failure returns Err with "auth error" prefix ──────────
    //
    // NOTE: `dispatch` hardcodes the Allegro API base URL via `allegro_api_base`,
    // so it is not possible to intercept the API-level HTTP calls with wiremock
    // in unit tests. The tests below cover the auth-failure path (which fires
    // before any API call) and the invalid-method path. Full end-to-end HTTP
    // routing (GET query params, POST body, 4xx/5xx responses) is covered by
    // the integration tests in `tests/mcp_server_integration.rs`.

    #[tokio::test]
    async fn test_dispatch_auth_failure_returns_auth_error() {
        // Mock the token endpoint to return a 500, causing auth to fail.
        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/auth/oauth/token"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let auth = crate::auth::AllegroAuth::with_base_url(
            "id".to_string(),
            "secret".to_string(),
            mock_server.uri(),
        );
        let http = reqwest::Client::new();
        let tool_def = crate::tool_registry::ToolDef {
            id: "allegro_get_offers".to_string(),
            name: "allegro_get_offers".to_string(),
            description: "List offers".to_string(),
            input_schema: serde_json::json!({}),
            method: "get".to_string(),
            path: "/sale/offers".to_string(),
            accept_media_type: None,
        };

        let result = dispatch(&auth, &http, false, &tool_def, serde_json::Map::new()).await;
        let err = result.expect_err("auth failure must return Err");
        assert!(
            err.contains("auth error"),
            "error must start with 'auth error', got: {err}"
        );
    }

    // ── dispatch: sandbox flag — auth is called before API URL is used ────────

    #[tokio::test]
    async fn test_dispatch_sandbox_auth_failure_returns_auth_error() {
        // Even with sandbox=true, auth failure must surface as "auth error".
        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/auth/oauth/token"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .mount(&mock_server)
            .await;

        let auth = crate::auth::AllegroAuth::with_base_url(
            "id".to_string(),
            "secret".to_string(),
            mock_server.uri(),
        );
        let http = reqwest::Client::new();
        let tool_def = crate::tool_registry::ToolDef {
            id: "allegro_get_offers".to_string(),
            name: "allegro_get_offers".to_string(),
            description: "List offers".to_string(),
            input_schema: serde_json::json!({}),
            method: "get".to_string(),
            path: "/sale/offers".to_string(),
            accept_media_type: None,
        };

        let result = dispatch(&auth, &http, true, &tool_def, serde_json::Map::new()).await;
        let err = result.expect_err("auth failure must return Err");
        assert!(
            err.contains("auth error"),
            "sandbox dispatch auth failure must return 'auth error', got: {err}"
        );
    }

    // ── dispatch_with_base: single 401 retry with token re-resolution ────────
    //
    // wiremock matching order: equal-priority mocks are checked in
    // registration order and an exhausted `up_to_n_times` mock is skipped —
    // so the *limited* mock is always mounted first, the fallback last.

    fn get_offers_tool() -> crate::tool_registry::ToolDef {
        crate::tool_registry::ToolDef {
            id: "allegro_get_offers".to_string(),
            name: "allegro_get_offers".to_string(),
            description: "List offers".to_string(),
            input_schema: serde_json::json!({}),
            method: "get".to_string(),
            path: "/sale/offers".to_string(),
            accept_media_type: None,
        }
    }

    async fn mount_token_ok(mock_server: &wiremock::MockServer, up_to: Option<u64>) {
        let mock = wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/auth/oauth/token"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "access_token": "tok", "token_type": "bearer", "expires_in": 43199 }),
            ));
        let mock = match up_to {
            Some(n) => mock.up_to_n_times(n),
            None => mock,
        };
        mock.mount(mock_server).await;
    }

    #[tokio::test]
    async fn test_dispatch_with_base_401_once_then_200_succeeds_with_two_api_hits() {
        let mock_server = wiremock::MockServer::start().await;
        mount_token_ok(&mock_server, None).await;

        // API: 401 exactly once, then 200 (limited mock mounted FIRST).
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/sale/offers"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("stale token"))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/sale/offers"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&mock_server)
            .await;

        let auth = crate::auth::AllegroAuth::with_base_url(
            "id".to_string(),
            "secret".to_string(),
            mock_server.uri(),
        );
        let tool_def = get_offers_tool();

        let out = dispatch_with_base(
            &auth,
            &reqwest::Client::new(),
            &mock_server.uri(),
            &tool_def,
            serde_json::Map::new(),
        )
        .await
        .expect("single 401 must be retried and succeed");
        assert_eq!(out, "[]");

        let reqs = mock_server.received_requests().await.unwrap();
        let api_hits = reqs
            .iter()
            .filter(|r| r.url.path() == "/sale/offers")
            .count();
        let token_hits = reqs
            .iter()
            .filter(|r| r.url.path() == "/auth/oauth/token")
            .count();
        assert_eq!(api_hits, 2, "the API must be hit twice (401 then 200)");
        assert_eq!(
            token_hits, 2,
            "the token must be re-resolved after the 401 (invalidate + token)"
        );
    }

    #[tokio::test]
    async fn test_dispatch_with_base_persistent_401_returns_second_body() {
        let mock_server = wiremock::MockServer::start().await;
        mount_token_ok(&mock_server, None).await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/sale/offers"))
            .respond_with(
                wiremock::ResponseTemplate::new(401)
                    .set_body_string("still unauthorized after refresh"),
            )
            .mount(&mock_server)
            .await;

        let auth = crate::auth::AllegroAuth::with_base_url(
            "id".to_string(),
            "secret".to_string(),
            mock_server.uri(),
        );
        let tool_def = get_offers_tool();

        let err = dispatch_with_base(
            &auth,
            &reqwest::Client::new(),
            &mock_server.uri(),
            &tool_def,
            serde_json::Map::new(),
        )
        .await
        .expect_err("persistent 401 must fail");
        assert!(
            err.contains("still unauthorized after refresh"),
            "the retry's body must be surfaced (not the first 401's), got: {err}"
        );
        assert!(err.contains("401"), "status must be included, got: {err}");
    }

    #[tokio::test]
    async fn test_dispatch_with_base_auth_failure_on_re_resolve_is_auth_error() {
        let mock_server = wiremock::MockServer::start().await;

        // Token endpoint: 200 exactly once (the initial fetch), then 500 —
        // the post-401 re-resolve fails. Limited mock mounted FIRST.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/auth/oauth/token"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "access_token": "tok", "token_type": "bearer", "expires_in": 43199 }),
            ))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/auth/oauth/token"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        // API: always 401, so the retry path runs.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/sale/offers"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .mount(&mock_server)
            .await;

        let auth = crate::auth::AllegroAuth::with_base_url(
            "id".to_string(),
            "secret".to_string(),
            mock_server.uri(),
        );
        let tool_def = get_offers_tool();

        let err = dispatch_with_base(
            &auth,
            &reqwest::Client::new(),
            &mock_server.uri(),
            &tool_def,
            serde_json::Map::new(),
        )
        .await
        .expect_err("failed re-resolve must fail the dispatch");
        assert!(
            err.contains("auth error"),
            "re-resolve failure must surface as 'auth error', got: {err}"
        );
    }
}
