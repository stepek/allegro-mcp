use openapiv3::{OpenAPI, ReferenceOr, Schema};
use serde_json::{json, Value};
use std::collections::HashSet;

/// Resolve a `ReferenceOr<Schema>` to a JSON Schema `Value`.
/// `visited` tracks the current resolution stack to detect cycles.
pub fn resolve_schema(
    api: &OpenAPI,
    schema_ref: &ReferenceOr<Schema>,
    visited: &mut HashSet<String>,
) -> Value {
    match schema_ref {
        ReferenceOr::Item(schema) => schema_to_value(api, schema, visited),
        ReferenceOr::Reference { reference } => {
            if visited.contains(reference.as_str()) {
                tracing::trace!("cycle detected at {}", reference);
                return json!({"type": "object", "description": "circular reference"});
            }
            visited.insert(reference.clone());
            let resolved = resolve_ref_string(api, reference, visited);
            let result = match resolved {
                Some(schema) => schema_to_value(api, schema, visited),
                None => {
                    tracing::trace!("unresolvable $ref: {}", reference);
                    json!({"type": "object", "description": format!("unresolved $ref: {}", reference)})
                }
            };
            visited.remove(reference.as_str());
            result
        }
    }
}

/// Look up a `#/components/schemas/<Name>` reference in the OpenAPI document.
/// Follows chained `$ref` within components (e.g. a schema that is itself a `$ref`).
/// The `visited` set prevents infinite loops on chained cycles.
fn resolve_ref_string<'a>(
    api: &'a OpenAPI,
    reference: &str,
    visited: &mut HashSet<String>,
) -> Option<&'a Schema> {
    // Only handle local #/components/schemas/ refs
    let name = reference.strip_prefix("#/components/schemas/")?;
    let components = api.components.as_ref()?;
    match components.schemas.get(name)? {
        ReferenceOr::Item(s) => Some(s),
        ReferenceOr::Reference {
            reference: inner_ref,
        } => {
            // Follow chained $ref — cycle guard is already in resolve_schema caller
            if visited.contains(inner_ref.as_str()) {
                return None; // cycle — caller will emit sentinel
            }
            resolve_ref_string(api, inner_ref, visited)
        }
    }
}

/// Convert an inline `openapiv3::Schema` to a `serde_json::Value`.
pub fn schema_to_value(api: &OpenAPI, schema: &Schema, visited: &mut HashSet<String>) -> Value {
    // Start with the schema's JSON representation via serde_json
    // Then recursively resolve any nested $refs
    let mut obj = serde_json::to_value(schema).unwrap_or_else(|_| json!({"type": "object"}));
    resolve_refs_in_value(api, &mut obj, visited);
    obj
}

/// Walk a `serde_json::Value` tree and resolve any `{"$ref": "..."}` nodes in-place.
fn resolve_refs_in_value(api: &OpenAPI, value: &mut Value, visited: &mut HashSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(ref_str)) = map.get("$ref").cloned() {
                // Replace the entire object with the resolved schema
                let schema_ref = ReferenceOr::Reference {
                    reference: ref_str.clone(),
                };
                *value = resolve_schema(api, &schema_ref, visited);
                return;
            }
            for v in map.values_mut() {
                resolve_refs_in_value(api, v, visited);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                resolve_refs_in_value(api, v, visited);
            }
        }
        _ => {}
    }
}
