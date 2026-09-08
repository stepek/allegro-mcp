use crate::tool_registry::ref_resolver;
use openapiv3::{OpenAPI, Parameter, ParameterData, ReferenceOr, RequestBody};
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a minimal OpenAPI YAML string into an OpenAPI struct.
    fn api_from_yaml(yaml: &str) -> OpenAPI {
        serde_yaml::from_str(yaml).expect("invalid test YAML")
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
        let api = api_from_yaml(
            "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
        );
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
}
