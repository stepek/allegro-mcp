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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal OpenAPI document from a YAML string for testing.
    fn api_from_yaml(yaml: &str) -> OpenAPI {
        serde_yaml::from_str(yaml).expect("invalid test YAML")
    }

    // ── schema_to_value ───────────────────────────────────────────────────────

    #[test]
    fn schema_to_value_simple_string_type() {
        let api = api_from_yaml(
            "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
        );
        let schema: Schema = serde_yaml::from_str("type: string").unwrap();
        let mut visited = HashSet::new();
        let val = schema_to_value(&api, &schema, &mut visited);
        assert_eq!(
            val["type"], "string",
            "schema_to_value must preserve type: string"
        );
    }

    // ── resolve_schema with inline schema (no $ref) ───────────────────────────

    #[test]
    fn resolve_schema_inline_returns_schema_as_json() {
        let api = api_from_yaml(
            "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
        );
        let schema: Schema = serde_yaml::from_str("type: integer").unwrap();
        let schema_ref = ReferenceOr::Item(schema);
        let mut visited = HashSet::new();
        let val = resolve_schema(&api, &schema_ref, &mut visited);
        assert_eq!(val["type"], "integer");
    }

    // ── resolve_schema with a known $ref ─────────────────────────────────────

    #[test]
    fn resolve_schema_known_ref_resolves_correctly() {
        let api = api_from_yaml(
            "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\ncomponents:\n  schemas:\n    MyType:\n      type: string\n",
        );
        let schema_ref: ReferenceOr<Schema> = ReferenceOr::Reference {
            reference: "#/components/schemas/MyType".to_string(),
        };
        let mut visited = HashSet::new();
        let val = resolve_schema(&api, &schema_ref, &mut visited);
        assert_eq!(
            val["type"], "string",
            "known $ref must resolve to the component schema"
        );
    }

    // ── resolve_schema with an unknown $ref ───────────────────────────────────

    #[test]
    fn resolve_schema_unknown_ref_returns_unresolved_sentinel() {
        let api = api_from_yaml(
            "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
        );
        let schema_ref: ReferenceOr<Schema> = ReferenceOr::Reference {
            reference: "#/components/schemas/DoesNotExist".to_string(),
        };
        let mut visited = HashSet::new();
        let val = resolve_schema(&api, &schema_ref, &mut visited);
        // Must be an object sentinel — not a raw $ref
        assert_eq!(val["type"], "object");
        let desc = val["description"].as_str().unwrap_or("");
        assert!(
            desc.contains("unresolved"),
            "sentinel description must mention 'unresolved', got: {desc}"
        );
    }

    // ── resolve_schema with a self-referential cycle ──────────────────────────

    #[test]
    fn resolve_schema_cyclic_ref_returns_sentinel_without_panic() {
        // Category.parent → Category (self-referential)
        let api = api_from_yaml(
            "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\ncomponents:\n  schemas:\n    Category:\n      type: object\n      properties:\n        parent:\n          $ref: \"#/components/schemas/Category\"\n",
        );
        let schema_ref: ReferenceOr<Schema> = ReferenceOr::Reference {
            reference: "#/components/schemas/Category".to_string(),
        };
        let mut visited = HashSet::new();
        // Must not panic or stack-overflow
        let val = resolve_schema(&api, &schema_ref, &mut visited);
        // The top-level resolves to an object
        assert_eq!(val["type"], "object");
        // The nested parent must be the circular-reference sentinel (not a raw $ref)
        let parent = &val["properties"]["parent"];
        assert!(
            parent.get("$ref").is_none(),
            "cyclic $ref must be replaced with sentinel, not left as $ref: {parent:?}"
        );
        let desc = parent["description"].as_str().unwrap_or("");
        assert!(
            desc.contains("circular"),
            "sentinel description must mention 'circular', got: {desc}"
        );
    }
}
