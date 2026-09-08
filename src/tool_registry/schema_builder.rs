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
            ReferenceOr::Reference { reference } => {
                tracing::trace!("skipping $ref parameter: {}", reference);
                continue;
            }
        };

        let (name, data, is_required) = extract_parameter_parts(param);

        // Skip header params that are standard HTTP headers (Authorization, etc.)
        // x- vendor extensions on parameters: skip gracefully
        if name.starts_with("x-") {
            tracing::trace!("skipping x- parameter: {}", name);
            continue;
        }

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
        if is_request_body_required(body_ref) {
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
        ReferenceOr::Reference { reference } => {
            tracing::trace!("skipping $ref requestBody: {}", reference);
            return json!({"type": "object"});
        }
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

/// Check if a requestBody is required.
fn is_request_body_required(body_ref: &ReferenceOr<RequestBody>) -> bool {
    match body_ref {
        ReferenceOr::Item(b) => b.required,
        ReferenceOr::Reference { .. } => false,
    }
}
