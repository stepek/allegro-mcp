use crate::tool_registry::{ToolDef, ToolRegistryError};
use openapiv3::{OpenAPI, Operation, ReferenceOr};

/// Build all tool definitions from an OpenAPI document.
pub fn build_tools(api: &OpenAPI) -> Result<Vec<ToolDef>, ToolRegistryError> {
    let mut tools = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (path_str, path_item_ref) in &api.paths.paths {
        let path_item = match path_item_ref {
            ReferenceOr::Item(item) => item,
            ReferenceOr::Reference { .. } => {
                tracing::trace!("skipping $ref path item at {}", path_str);
                continue;
            }
        };

        // Use PathItem::iter() from the openapiv3 crate (yields (&str, &Operation))
        for (method, operation) in path_item.iter() {
            let tool = build_one_tool(api, path_str, method, operation, &mut seen_ids)?;
            tools.push(tool);
        }
    }

    Ok(tools)
}

/// Build a single ToolDef from one operation.
fn build_one_tool(
    api: &OpenAPI,
    path_str: &str,
    method: &str,
    operation: &Operation,
    seen_ids: &mut std::collections::HashSet<String>,
) -> Result<ToolDef, ToolRegistryError> {
    // 1. Determine base name from operationId or path+method
    let base = match &operation.operation_id {
        Some(id) => sanitize_name(id),
        None => derive_name_from_path(method, path_str),
    };

    // 2. Apply allegro_ prefix
    let prefixed = format!("allegro_{}", base);

    // 3. Deduplicate: if already seen, append _2, _3, etc.
    let id = deduplicate_id(prefixed, seen_ids);
    seen_ids.insert(id.clone());

    // 4. Description from summary > description > id
    let description = operation
        .summary
        .clone()
        .or_else(|| operation.description.clone())
        .unwrap_or_else(|| id.clone());

    // 5. Build input schema
    let input_schema = crate::tool_registry::schema_builder::build_input_schema(
        api,
        &operation.parameters,
        operation.request_body.as_ref(),
    )?;

    Ok(ToolDef {
        id: id.clone(),
        name: id,
        description,
        input_schema,
        method: method.to_string(),
        path: path_str.to_string(),
    })
}

/// Sanitize an operationId: lowercase, replace non-alphanumeric with `_`,
/// collapse consecutive underscores, strip leading/trailing underscores.
pub fn sanitize_name(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    // Collapse consecutive underscores
    let mut result = String::new();
    let mut prev_underscore = false;
    for c in s.chars() {
        if c == '_' {
            if !prev_underscore {
                result.push(c);
            }
            prev_underscore = true;
        } else {
            result.push(c);
            prev_underscore = false;
        }
    }
    result.trim_matches('_').to_string()
}

/// Derive a tool name from HTTP method + path when no operationId is present.
/// E.g. GET /sale/offers/{offerId} → "get_sale_offers_offerid"
pub fn derive_name_from_path(method: &str, path: &str) -> String {
    let segments: String = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(sanitize_name)
        .collect::<Vec<_>>()
        .join("_");
    if segments.is_empty() {
        sanitize_name(method)
    } else {
        format!("{}_{}", method.to_ascii_lowercase(), segments)
    }
}

/// Append _2, _3, ... until the id is unique in seen_ids.
fn deduplicate_id(base: String, seen: &std::collections::HashSet<String>) -> String {
    if !seen.contains(&base) {
        return base;
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("{}_{}", base, n);
        if !seen.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── sanitize_name edge cases ──────────────────────────────────────────────

    #[test]
    fn sanitize_name_empty_string_returns_empty() {
        assert_eq!(sanitize_name(""), "");
    }

    #[test]
    fn sanitize_name_all_special_chars_returns_empty() {
        // "---" → all replaced with '_' → collapsed to '_' → trimmed → ""
        assert_eq!(sanitize_name("---"), "");
    }

    #[test]
    fn sanitize_name_single_special_char_returns_empty() {
        assert_eq!(sanitize_name("-"), "");
        assert_eq!(sanitize_name("_"), "");
        assert_eq!(sanitize_name("!"), "");
    }

    // ── deduplicate_id ────────────────────────────────────────────────────────

    #[test]
    fn deduplicate_id_returns_base_when_not_seen() {
        let seen = std::collections::HashSet::new();
        assert_eq!(deduplicate_id("allegro_foo".to_string(), &seen), "allegro_foo");
    }

    #[test]
    fn deduplicate_id_appends_2_when_base_taken() {
        let mut seen = std::collections::HashSet::new();
        seen.insert("allegro_foo".to_string());
        assert_eq!(
            deduplicate_id("allegro_foo".to_string(), &seen),
            "allegro_foo_2"
        );
    }

    #[test]
    fn deduplicate_id_appends_3_when_base_and_2_taken() {
        let mut seen = std::collections::HashSet::new();
        seen.insert("allegro_foo".to_string());
        seen.insert("allegro_foo_2".to_string());
        assert_eq!(
            deduplicate_id("allegro_foo".to_string(), &seen),
            "allegro_foo_3"
        );
    }

    // ── build_tools with empty PathItem ──────────────────────────────────────

    #[test]
    fn build_tools_empty_path_item_yields_zero_tools() {
        // A PathItem with no operations (no get/post/put/delete/patch/etc.)
        // should contribute 0 tools.
        let api: openapiv3::OpenAPI = serde_yaml::from_str(
            "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths:\n  /empty: {}\n",
        )
        .unwrap();
        let tools = build_tools(&api).unwrap();
        assert_eq!(tools.len(), 0, "empty PathItem must yield 0 tools");
    }
}
