use crate::tool_registry::ref_resolver;
use openapiv3::{OpenAPI, Parameter, ParameterData, ReferenceOr, RequestBody, Responses};
use serde_json::{json, Value};
use std::collections::HashSet;

/// Build the merged input JSON Schema for one operation.
pub fn build_input_schema(
    api: &OpenAPI,
    parameters: &[ReferenceOr<Parameter>],
    request_body: Option<&ReferenceOr<RequestBody>>,
) -> Result<Value, crate::tool_registry::ToolRegistryError> {
    let mut properties: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();

    for param_ref in parameters {
        let param = match param_ref {
            ReferenceOr::Item(p) => p,
            ReferenceOr::Reference { reference } => match resolve_parameter_ref(api, reference) {
                Some(p) => p,
                None => continue,
            },
        };

        let (name, data, is_required) = extract_parameter_parts(param);

        // In openapiv3 2.2.0, ParameterData has `format: ParameterSchemaOrContent`
        // NOT a `.schema` field. Extract the schema from the format enum.
        let schema_val = match &data.format {
            openapiv3::ParameterSchemaOrContent::Schema(schema_ref) => {
                let mut visited = HashSet::new();
                ref_resolver::resolve_schema(api, schema_ref, &mut visited)
            }
            openapiv3::ParameterSchemaOrContent::Content(_) => json!({"type": "object"}),
        };

        properties.insert(name.clone(), schema_val);
        if is_required {
            required.push(Value::String(name));
        }
    }

    // requestBody: add as "body" property
    if let Some(body_ref) = request_body {
        let body_schema = extract_request_body_schema(api, body_ref);
        properties.insert("body".to_string(), body_schema);
        // requestBody.required defaults to false per OAS spec
        if is_request_body_required(api, body_ref) {
            required.push(Value::String("body".to_string()));
        }
    }

    let mut schema = json!({
        "type": "object",
        "properties": Value::Object(properties),
    });

    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }

    Ok(schema)
}

/// Look up a `#/components/parameters/<Name>` reference in the OpenAPI document.
/// Only resolves a single level — chained refs are skipped with a trace log.
fn resolve_parameter_ref<'a>(api: &'a OpenAPI, reference: &str) -> Option<&'a Parameter> {
    let name = reference.strip_prefix("#/components/parameters/")?;
    let components = api.components.as_ref()?;
    match components.parameters.get(name)? {
        ReferenceOr::Item(p) => Some(p),
        ReferenceOr::Reference {
            reference: inner_ref,
        } => {
            tracing::trace!("skipping chained $ref parameter: {}", inner_ref);
            None
        }
    }
}

/// Extract (name, ParameterData, is_required) from a Parameter enum.
fn extract_parameter_parts(param: &Parameter) -> (String, &ParameterData, bool) {
    match param {
        Parameter::Path { parameter_data, .. } => {
            (parameter_data.name.clone(), parameter_data, true) // path params always required
        }
        Parameter::Query { parameter_data, .. } => (
            parameter_data.name.clone(),
            parameter_data,
            parameter_data.required,
        ),
        Parameter::Header { parameter_data, .. } => (
            parameter_data.name.clone(),
            parameter_data,
            parameter_data.required,
        ),
        Parameter::Cookie { parameter_data, .. } => (
            parameter_data.name.clone(),
            parameter_data,
            parameter_data.required,
        ),
    }
}

/// Extract the JSON Schema from a requestBody (prefers application/json).
fn extract_request_body_schema(api: &OpenAPI, body_ref: &ReferenceOr<RequestBody>) -> Value {
    let body = match body_ref {
        ReferenceOr::Item(b) => b,
        ReferenceOr::Reference { reference } => match resolve_request_body_ref(api, reference) {
            Some(b) => b,
            None => {
                tracing::warn!("unresolvable $ref requestBody: {}", reference);
                return json!({"type": "object"});
            }
        },
    };

    // Prefer application/json, fall back to first available
    let media = body
        .content
        .get("application/json")
        .or_else(|| body.content.values().next());

    match media.and_then(|m| m.schema.as_ref()) {
        Some(schema_ref) => {
            let mut visited = HashSet::new();
            ref_resolver::resolve_schema(api, schema_ref, &mut visited)
        }
        None => json!({"type": "object"}),
    }
}

/// Look up a `#/components/requestBodies/<Name>` reference in the OpenAPI document.
/// Only resolves a single level — chained refs are treated as unresolvable.
fn resolve_request_body_ref<'a>(api: &'a OpenAPI, reference: &str) -> Option<&'a RequestBody> {
    let name = reference.strip_prefix("#/components/requestBodies/")?;
    let components = api.components.as_ref()?;
    match components.request_bodies.get(name)? {
        ReferenceOr::Item(b) => Some(b),
        ReferenceOr::Reference {
            reference: inner_ref,
        } => {
            tracing::trace!("skipping chained $ref requestBody: {}", inner_ref);
            None
        }
    }
}

/// Check if a requestBody is required.
fn is_request_body_required(api: &OpenAPI, body_ref: &ReferenceOr<RequestBody>) -> bool {
    match body_ref {
        ReferenceOr::Item(b) => b.required,
        ReferenceOr::Reference { reference } => resolve_request_body_ref(api, reference)
            .map(|b| b.required)
            .unwrap_or(false),
    }
}

// ── Per-operation Accept media type ───────────────────────────────────────────

/// `application/vnd.allegro.`-prefixed, `+json`-suffixed media type prefix.
const VND_PREFIX: &str = "application/vnd.allegro.";
/// Suffix required on versioned Allegro media types.
const JSON_SUFFIX: &str = "+json";

/// Returns `true` for keys like `application/vnd.allegro.public.v1+json`.
///
/// Plain `application/json` keys (used by the local test fixture) do NOT
/// match — matching them would send `Accept: application/json` to Allegro
/// (406 risk).
fn is_allegro_versioned_media_type(key: &str) -> bool {
    key.starts_with(VND_PREFIX) && key.ends_with(JSON_SUFFIX)
}

/// Extracts the versioned `Accept` media type for one operation from its
/// OpenAPI `content` map keys (the live swagger.yaml carries
/// `application/vnd.allegro.public.v1+json` etc. verbatim as keys).
///
/// Scan order (first match wins):
/// 1. the `requestBody` (inline or resolved `$ref`) — it pins the request
///    media type unambiguously;
/// 2. `responses` — 2xx status codes in ascending numeric order, then the
///    remaining entries in declaration order (`StatusCode::Range`/`All`
///    included), then `responses.default` last.
///
/// If an operation yields several **distinct** matches, the first one wins
/// and a `tracing::warn!` names both values. Ops with no `vnd.allegro` keys
/// (e.g. DELETE — unversioned per Allegro docs — or fixture docs using plain
/// `application/json`) return `None` → the dispatcher sends its default
/// `Accept`.
pub fn extract_accept_media_type(
    api: &OpenAPI,
    request_body: Option<&ReferenceOr<RequestBody>>,
    responses: &Responses,
) -> Option<String> {
    // All matches in scan order (see doc comment); the distinct-values check
    // at the end decides whether to warn.
    let mut candidates: Vec<String> = Vec::new();

    // (a) requestBody first — it wins over any response media type.
    if let Some(body_ref) = request_body {
        let body = match body_ref {
            ReferenceOr::Item(b) => Some(b),
            ReferenceOr::Reference { reference } => resolve_request_body_ref(api, reference),
        };
        if let Some(body) = body {
            for key in body.content.keys() {
                if is_allegro_versioned_media_type(key) {
                    candidates.push(key.clone());
                }
            }
        }
    }

    // (b) responses: 2xx codes ascending, then remaining entries in map
    // order (Range/All keys included), then `default` last. A stable sort by
    // (tier, code) preserves the IndexMap declaration order within each tier.
    if candidates.is_empty() {
        let mut entries: Vec<(Option<u16>, &ReferenceOr<openapiv3::Response>)> = responses
            .responses
            .iter()
            .map(|(status, resp_ref)| {
                let code = match status {
                    openapiv3::StatusCode::Code(n) => Some(*n),
                    openapiv3::StatusCode::Range(_) => None,
                };
                (code, resp_ref)
            })
            .collect();
        entries.sort_by_key(|(code, _)| match code {
            Some(n) if (200..300).contains(n) => (0, *n),
            _ => (1, 0),
        });
        if let Some(default) = &responses.default {
            entries.push((None, default));
        }

        for (_, resp_ref) in entries {
            let response = match resp_ref {
                ReferenceOr::Item(r) => Some(r),
                ReferenceOr::Reference { reference } => resolve_response_ref(api, reference),
            };
            let Some(response) = response else {
                tracing::trace!(
                    "skipping unresolvable $ref response: {}",
                    match resp_ref {
                        ReferenceOr::Item(_) => "<inline>",
                        ReferenceOr::Reference { reference } => reference,
                    }
                );
                continue;
            };
            // First matching key per response entry.
            for key in response.content.keys() {
                if is_allegro_versioned_media_type(key) {
                    candidates.push(key.clone());
                    break;
                }
            }
        }
    }

    let first = candidates.first()?.clone();
    // Several DISTINCT matches are ambiguous — warn but stay deterministic
    // (first in scan order wins).
    if let Some(second) = candidates.iter().find(|c| *c != &first) {
        tracing::warn!(
            first = %first,
            second = %second,
            "operation declares multiple versioned media types; using the first"
        );
    }
    Some(first)
}

/// Look up a `#/components/responses/<Name>` reference in the OpenAPI
/// document. Only resolves a single level — chained refs are treated as
/// unresolvable.
fn resolve_response_ref<'a>(api: &'a OpenAPI, reference: &str) -> Option<&'a openapiv3::Response> {
    let name = reference.strip_prefix("#/components/responses/")?;
    let components = api.components.as_ref()?;
    match components.responses.get(name)? {
        ReferenceOr::Item(r) => Some(r),
        ReferenceOr::Reference {
            reference: inner_ref,
        } => {
            tracing::trace!("skipping chained $ref response: {}", inner_ref);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a minimal OpenAPI YAML string into an OpenAPI struct.
    fn api_from_yaml(yaml: &str) -> OpenAPI {
        serde_yaml::from_str(yaml).expect("invalid test YAML")
    }

    /// Runs [`extract_accept_media_type`] on the first operation of the
    /// first path in the given OpenAPI YAML document.
    fn accept_from_op_yaml(op_yaml: &str) -> Option<String> {
        let api: OpenAPI = serde_yaml::from_str(op_yaml).expect("invalid test YAML");
        let (_, path_item_ref) = api.paths.paths.iter().next().unwrap();
        let path_item = match path_item_ref {
            ReferenceOr::Item(item) => item,
            _ => panic!("expected inline path item"),
        };
        let (_, op) = path_item.iter().next().unwrap();
        extract_accept_media_type(&api, op.request_body.as_ref(), &op.responses)
    }

    // ── extract_accept_media_type ─────────────────────────────────────────────

    #[test]
    fn extract_accept_request_body_vnd_key_wins() {
        let mt = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /offers:\n",
            "    put:\n",
            "      requestBody:\n",
            "        content:\n",
            "          application/vnd.allegro.public.v1+json:\n",
            "            schema:\n              type: object\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
            "          content:\n",
            "            application/vnd.allegro.beta.v1+json:\n",
            "              schema:\n                type: object\n",
        ))
        .expect("requestBody vnd key must be extracted");
        assert_eq!(mt, "application/vnd.allegro.public.v1+json");
    }

    #[test]
    fn extract_accept_ref_request_body_resolves_media_type() {
        let mt = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /offers:\n",
            "    post:\n",
            "      requestBody:\n",
            "        $ref: \"#/components/requestBodies/OfferBody\"\n",
            "      responses:\n",
            "        \"201\":\n          description: Created\n",
            "components:\n",
            "  requestBodies:\n",
            "    OfferBody:\n",
            "      content:\n",
            "        application/vnd.allegro.public.v1+json:\n",
            "          schema:\n            type: object\n",
        ))
        .expect("$ref requestBody must resolve");
        assert_eq!(mt, "application/vnd.allegro.public.v1+json");
    }

    #[test]
    fn extract_accept_get_responses_only_vnd_key() {
        let mt = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /offers:\n",
            "    get:\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
            "          content:\n",
            "            application/vnd.allegro.public.v1+json:\n",
            "              schema:\n                type: object\n",
        ))
        .expect("response vnd key must be extracted");
        assert_eq!(mt, "application/vnd.allegro.public.v1+json");
    }

    /// 2xx is preferred over 4xx even when the 4xx entry is declared FIRST
    /// in the document (proves the scan sorts rather than trusting map order).
    #[test]
    fn extract_accept_prefers_2xx_over_4xx_regardless_of_map_order() {
        let mt = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /offers:\n",
            "    get:\n",
            "      responses:\n",
            "        \"404\":\n",
            "          description: Not Found\n",
            "          content:\n",
            "            application/vnd.allegro.beta.v1+json:\n",
            "              schema:\n                type: object\n",
            "        \"200\":\n",
            "          description: OK\n",
            "          content:\n",
            "            application/vnd.allegro.public.v1+json:\n",
            "              schema:\n                type: object\n",
        ))
        .expect("a vnd key exists");
        assert_eq!(
            mt, "application/vnd.allegro.public.v1+json",
            "2xx must be scanned before 4xx regardless of declaration order"
        );
    }

    /// Lowest 2xx wins when several 2xx codes carry keys (ascending order).
    #[test]
    fn extract_accept_lowest_2xx_ascending() {
        let mt = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /offers:\n",
            "    post:\n",
            "      responses:\n",
            "        \"201\":\n",
            "          description: Created\n",
            "          content:\n",
            "            application/vnd.allegro.beta.v1+json:\n",
            "              schema:\n                type: object\n",
            "        \"200\":\n",
            "          description: OK\n",
            "          content:\n",
            "            application/vnd.allegro.public.v1+json:\n",
            "              schema:\n                type: object\n",
        ))
        .expect("a vnd key exists");
        assert_eq!(
            mt, "application/vnd.allegro.public.v1+json",
            "200 must be scanned before 201"
        );
    }

    #[test]
    fn extract_accept_beta_v1_extracted_verbatim() {
        let mt = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /beta:\n",
            "    get:\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
            "          content:\n",
            "            application/vnd.allegro.beta.v1+json:\n",
            "              schema:\n                type: object\n",
        ))
        .expect("beta key must be extracted");
        assert_eq!(mt, "application/vnd.allegro.beta.v1+json");
    }

    /// `2XX` range entries and `default` must not crash, must be scanned
    /// after explicit 2xx codes, and `default` last.
    #[test]
    fn extract_accept_range_and_default_entries_sort_last() {
        let mt = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /offers:\n",
            "    get:\n",
            "      responses:\n",
            "        \"2XX\":\n",
            "          description: Range\n",
            "          content:\n",
            "            application/vnd.allegro.beta.v2+json:\n",
            "              schema:\n                type: object\n",
            "        \"200\":\n",
            "          description: OK\n",
            "          content:\n",
            "            application/vnd.allegro.public.v1+json:\n",
            "              schema:\n                type: object\n",
            "        default:\n",
            "          description: Fallback\n",
            "          content:\n",
            "            application/vnd.allegro.beta.v1+json:\n",
            "              schema:\n                type: object\n",
        ))
        .expect("a vnd key exists");
        assert_eq!(
            mt, "application/vnd.allegro.public.v1+json",
            "explicit 200 must beat 2XX range and default"
        );
    }

    #[test]
    fn extract_accept_default_scanned_when_nothing_else_matches() {
        let mt = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /offers:\n",
            "    get:\n",
            "      responses:\n",
            "        \"404\":\n          description: Not Found\n",
            "        default:\n",
            "          description: Fallback\n",
            "          content:\n",
            "            application/vnd.allegro.beta.v1+json:\n",
            "              schema:\n                type: object\n",
        ))
        .expect("default carries a vnd key");
        assert_eq!(mt, "application/vnd.allegro.beta.v1+json");
    }

    /// `$ref` responses resolve against `#/components/responses`.
    #[test]
    fn extract_accept_ref_response_resolves() {
        let mt = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /offers:\n",
            "    get:\n",
            "      responses:\n",
            "        \"200\":\n",
            "          $ref: \"#/components/responses/OkResponse\"\n",
            "components:\n",
            "  responses:\n",
            "    OkResponse:\n",
            "      description: OK\n",
            "      content:\n",
            "        application/vnd.allegro.public.v1+json:\n",
            "          schema:\n            type: object\n",
        ))
        .expect("$ref response must resolve");
        assert_eq!(mt, "application/vnd.allegro.public.v1+json");
    }

    /// Plain `application/json` keys only → None (fixture compatibility —
    /// sending `Accept: application/json` to Allegro is a 406 risk).
    #[test]
    fn extract_accept_plain_json_keys_yield_none() {
        let result = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /offers:\n",
            "    post:\n",
            "      requestBody:\n",
            "        content:\n",
            "          application/json:\n",
            "            schema:\n              type: object\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
            "          content:\n",
            "            application/json:\n",
            "              schema:\n                type: object\n",
        ));
        assert_eq!(result, None, "plain application/json must not match");
    }

    #[test]
    fn extract_accept_no_content_keys_yields_none() {
        let result = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /offers/{offerId}:\n",
            "    delete:\n",
            "      responses:\n",
            "        \"204\":\n          description: No Content\n",
        ));
        assert_eq!(
            result, None,
            "no content keys → None (DELETE default Accept)"
        );
    }

    #[test]
    fn extract_accept_none_when_no_responses_and_no_body() {
        let api = api_from_yaml("openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n");
        let responses = openapiv3::Responses::default();
        assert_eq!(extract_accept_media_type(&api, None, &responses), None);
    }

    /// Distinct matches in the same requestBody content map: first in map
    /// order wins (and the ambiguity is logged — asserted behaviorally by
    /// the deterministic pick).
    #[test]
    fn extract_accept_multiple_distinct_keys_first_wins() {
        let mt = accept_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /offers:\n",
            "    post:\n",
            "      requestBody:\n",
            "        content:\n",
            "          application/vnd.allegro.public.v1+json:\n",
            "            schema:\n              type: object\n",
            "          application/vnd.allegro.beta.v1+json:\n",
            "            schema:\n              type: object\n",
            "      responses:\n",
            "        \"200\":\n          description: OK\n",
        ))
        .expect("first key must be extracted");
        assert_eq!(mt, "application/vnd.allegro.public.v1+json");
    }

    #[test]
    fn is_allegro_versioned_media_type_matrix() {
        assert!(is_allegro_versioned_media_type(
            "application/vnd.allegro.public.v1+json"
        ));
        assert!(is_allegro_versioned_media_type(
            "application/vnd.allegro.beta.v2+json"
        ));
        assert!(!is_allegro_versioned_media_type("application/json"));
        assert!(!is_allegro_versioned_media_type(
            "application/vnd.allegro.public.v1"
        ));
        assert!(!is_allegro_versioned_media_type("text/plain"));
        assert!(!is_allegro_versioned_media_type(
            "application/vnd.other.public.v1+json"
        ));
    }

    /// Parse a single operation's parameters and optional requestBody from a
    /// YAML path-item snippet embedded in a full OpenAPI document.
    fn schema_from_op_yaml(op_yaml: &str) -> Value {
        let api: OpenAPI = serde_yaml::from_str(op_yaml).expect("invalid test YAML");
        // Grab the first path's first operation
        let (_, path_item_ref) = api.paths.paths.iter().next().unwrap();
        let path_item = match path_item_ref {
            ReferenceOr::Item(item) => item,
            _ => panic!("expected inline path item"),
        };
        let (_, op) = path_item.iter().next().unwrap();
        build_input_schema(&api, &op.parameters, op.request_body.as_ref()).unwrap()
    }

    // ── no parameters, no requestBody ─────────────────────────────────────────

    #[test]
    fn build_input_schema_no_params_no_body_returns_empty_object() {
        let api = api_from_yaml("openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n");
        let schema = build_input_schema(&api, &[], None).unwrap();
        assert_eq!(schema["type"], "object");
        assert!(
            schema["properties"].as_object().unwrap().is_empty(),
            "properties must be empty when there are no params or body"
        );
        assert!(
            schema.get("required").is_none(),
            "required must be absent when nothing is required"
        );
    }

    // ── only path params → all required ──────────────────────────────────────

    #[test]
    fn build_input_schema_path_params_all_required() {
        let schema = schema_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /items/{id}:\n",
            "    get:\n",
            "      parameters:\n",
            "        - name: id\n",
            "          in: path\n",
            "          required: true\n",
            "          schema:\n",
            "            type: string\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
        ));
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required must be present for path params")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            required.contains(&"id"),
            "path param 'id' must be in required, got: {required:?}"
        );
    }

    // ── only optional query params → none required ────────────────────────────

    #[test]
    fn build_input_schema_optional_query_params_not_required() {
        let schema = schema_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /items:\n",
            "    get:\n",
            "      parameters:\n",
            "        - name: limit\n",
            "          in: query\n",
            "          schema:\n",
            "            type: integer\n",
            "        - name: offset\n",
            "          in: query\n",
            "          schema:\n",
            "            type: integer\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
        ));
        assert!(
            schema.get("required").is_none(),
            "optional query params must not appear in required, got: {:?}",
            schema.get("required")
        );
        // But the properties must still be present
        assert!(schema["properties"]["limit"].is_object());
        assert!(schema["properties"]["offset"].is_object());
    }

    // ── required requestBody → "body" in required ─────────────────────────────

    #[test]
    fn build_input_schema_required_body_is_in_required() {
        let schema = schema_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /items:\n",
            "    post:\n",
            "      requestBody:\n",
            "        required: true\n",
            "        content:\n",
            "          application/json:\n",
            "            schema:\n",
            "              type: object\n",
            "      responses:\n",
            "        \"201\":\n",
            "          description: Created\n",
        ));
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required must be present when requestBody is required")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            required.contains(&"body"),
            "'body' must be in required when requestBody.required=true, got: {required:?}"
        );
    }

    // ── optional requestBody → "body" NOT in required ─────────────────────────

    #[test]
    fn build_input_schema_optional_body_not_in_required() {
        let schema = schema_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /items:\n",
            "    post:\n",
            "      requestBody:\n",
            "        required: false\n",
            "        content:\n",
            "          application/json:\n",
            "            schema:\n",
            "              type: object\n",
            "      responses:\n",
            "        \"201\":\n",
            "          description: Created\n",
        ));
        // "body" property must exist
        assert!(
            schema["properties"]["body"].is_object(),
            "body property must be present even when not required"
        );
        // But "body" must NOT be in required
        if let Some(required) = schema.get("required") {
            let required_names: Vec<&str> = required
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            assert!(
                !required_names.contains(&"body"),
                "'body' must NOT be in required when requestBody.required=false, got: {required_names:?}"
            );
        }
        // If required is absent entirely, that's also fine (nothing required)
    }

    // ── $ref parameter in components/parameters resolves into properties ───────

    #[test]
    fn build_input_schema_ref_parameter_resolves_into_properties() {
        let schema = schema_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /items:\n",
            "    get:\n",
            "      parameters:\n",
            "        - $ref: \"#/components/parameters/LimitParam\"\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
            "components:\n",
            "  parameters:\n",
            "    LimitParam:\n",
            "      name: limit\n",
            "      in: query\n",
            "      schema:\n",
            "        type: integer\n",
        ));
        assert!(
            schema["properties"]["limit"].is_object(),
            "$ref parameter 'limit' must be resolved into properties, got: {:?}",
            schema["properties"]
        );
        assert_eq!(
            schema["properties"]["limit"]["type"], "integer",
            "$ref parameter must resolve to the correct schema type, got: {:?}",
            schema["properties"]["limit"]
        );
    }

    // ── $ref requestBody in components/requestBodies resolves body property ────

    #[test]
    fn build_input_schema_ref_request_body_resolves_body_property() {
        let schema = schema_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /items:\n",
            "    post:\n",
            "      requestBody:\n",
            "        $ref: \"#/components/requestBodies/ItemBody\"\n",
            "      responses:\n",
            "        \"201\":\n",
            "          description: Created\n",
            "components:\n",
            "  requestBodies:\n",
            "    ItemBody:\n",
            "      required: true\n",
            "      content:\n",
            "        application/json:\n",
            "          schema:\n",
            "            type: object\n",
            "            properties:\n",
            "              name:\n",
            "                type: string\n",
        ));
        assert!(
            schema["properties"]["body"].is_object(),
            "$ref requestBody must produce a 'body' property, got: {:?}",
            schema["properties"]
        );
        assert!(
            schema["properties"]["body"]["properties"]["name"].is_object(),
            "$ref requestBody must resolve to the actual schema content, not just a placeholder object"
        );
    }

    // ── header parameter appears in properties and respects required flag ──────

    #[test]
    fn build_input_schema_required_header_param_in_required() {
        let schema = schema_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /items:\n",
            "    get:\n",
            "      parameters:\n",
            "        - name: X-Request-Id\n",
            "          in: header\n",
            "          required: true\n",
            "          schema:\n",
            "            type: string\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
        ));
        assert!(
            schema["properties"]["X-Request-Id"].is_object(),
            "header param 'X-Request-Id' must appear in properties, got: {:?}",
            schema["properties"]
        );
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required must be present for required header param")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            required.contains(&"X-Request-Id"),
            "required header param must be in required array, got: {required:?}"
        );
    }

    #[test]
    fn build_input_schema_optional_header_param_not_in_required() {
        let schema = schema_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /items:\n",
            "    get:\n",
            "      parameters:\n",
            "        - name: X-Trace-Id\n",
            "          in: header\n",
            "          required: false\n",
            "          schema:\n",
            "            type: string\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
        ));
        assert!(
            schema["properties"]["X-Trace-Id"].is_object(),
            "optional header param must still appear in properties, got: {:?}",
            schema["properties"]
        );
        // required array must be absent or must not contain the header name
        if let Some(required) = schema.get("required") {
            let required_names: Vec<&str> = required
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            assert!(
                !required_names.contains(&"X-Trace-Id"),
                "optional header param must NOT be in required, got: {required_names:?}"
            );
        }
    }

    // ── cookie parameter appears in properties and respects required flag ──────

    #[test]
    fn build_input_schema_required_cookie_param_in_required() {
        let schema = schema_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /items:\n",
            "    get:\n",
            "      parameters:\n",
            "        - name: session\n",
            "          in: cookie\n",
            "          required: true\n",
            "          schema:\n",
            "            type: string\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
        ));
        assert!(
            schema["properties"]["session"].is_object(),
            "cookie param 'session' must appear in properties, got: {:?}",
            schema["properties"]
        );
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required must be present for required cookie param")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            required.contains(&"session"),
            "required cookie param must be in required array, got: {required:?}"
        );
    }

    #[test]
    fn build_input_schema_optional_cookie_param_not_in_required() {
        let schema = schema_from_op_yaml(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /items:\n",
            "    get:\n",
            "      parameters:\n",
            "        - name: pref\n",
            "          in: cookie\n",
            "          required: false\n",
            "          schema:\n",
            "            type: string\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
        ));
        assert!(
            schema["properties"]["pref"].is_object(),
            "optional cookie param must still appear in properties, got: {:?}",
            schema["properties"]
        );
        // required array must be absent or must not contain the cookie name
        if let Some(required) = schema.get("required") {
            let required_names: Vec<&str> = required
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            assert!(
                !required_names.contains(&"pref"),
                "optional cookie param must NOT be in required, got: {required_names:?}"
            );
        }
    }
}
