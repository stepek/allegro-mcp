pub mod builder;
pub mod ref_resolver;
pub mod schema_builder;

use serde_json::Value;
use thiserror::Error;

/// A single MCP tool definition derived from one OpenAPI operation.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    /// Sanitized, prefixed tool ID: `allegro_<sanitized_operation_id>`
    pub id: String,
    /// Human-readable name (same as id for now)
    pub name: String,
    /// From operation `summary` or `description`, falling back to the id
    pub description: String,
    /// Merged JSON Schema object for all inputs (path/query/header params + requestBody)
    pub input_schema: Value,
    /// HTTP method (lowercase): "get", "post", "put", "delete", "patch"
    pub method: String,
    /// Raw path string: "/sale/offers/{offerId}"
    pub path: String,
}

/// Errors from the tool registry.
#[derive(Debug, Error)]
pub enum ToolRegistryError {
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    /// Reserved for future schema-building failure modes.
    #[allow(dead_code)]
    #[error("Schema build error: {0}")]
    Build(String),
}

/// The tool registry: holds all tools derived from an OpenAPI document.
pub struct ToolRegistry {
    tools: Vec<ToolDef>,
}

impl ToolRegistry {
    /// Build a registry from a parsed OpenAPI document.
    pub fn from_openapi(api: &openapiv3::OpenAPI) -> Result<Self, ToolRegistryError> {
        let tools = builder::build_tools(api)?;
        Ok(Self { tools })
    }

    /// Return all tools.
    pub fn list_tools(&self) -> &[ToolDef] {
        &self.tools
    }

    /// Look up a tool by its id.
    #[allow(dead_code)]
    pub fn get_tool(&self, id: &str) -> Option<&ToolDef> {
        self.tools.iter().find(|t| t.id == id)
    }

    /// Look up a tool by its `name` field (as exposed to MCP clients via
    /// `tools/list` and referenced in `tools/call` requests).
    ///
    /// Note: `builder.rs` currently sets `ToolDef::name == ToolDef::id` for
    /// every tool, so this and [`Self::get_tool`] are equivalent today. This
    /// method exists so callers that are logically looking up "the tool the
    /// MCP client asked for by name" (e.g. `AllegroServer::call_tool`) don't
    /// depend on that invariant holding forever.
    pub fn get_tool_by_name(&self, name: &str) -> Option<&ToolDef> {
        self.tools.iter().find(|t| t.name == name)
    }

    /// Filter tools by a predicate (for Phase 10 filtering hooks).
    #[allow(dead_code)]
    pub fn filter_tools<F>(&self, predicate: F) -> Vec<&ToolDef>
    where
        F: Fn(&ToolDef) -> bool,
    {
        self.tools.iter().filter(|t| predicate(t)).collect()
    }

    /// Return the number of tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Return true if the registry is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_registry() -> ToolRegistry {
        let api: openapiv3::OpenAPI = serde_yaml::from_str(
            "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
        )
        .unwrap();
        ToolRegistry::from_openapi(&api).unwrap()
    }

    fn two_tool_registry() -> ToolRegistry {
        let api: openapiv3::OpenAPI = serde_yaml::from_str(concat!(
            "openapi: \"3.0.3\"\n",
            "info:\n  title: t\n  version: v\n",
            "paths:\n",
            "  /a:\n",
            "    get:\n",
            "      summary: A\n",
            "      responses:\n",
            "        \"200\":\n",
            "          description: OK\n",
            "  /b:\n",
            "    post:\n",
            "      summary: B\n",
            "      responses:\n",
            "        \"201\":\n",
            "          description: Created\n",
        ))
        .unwrap();
        ToolRegistry::from_openapi(&api).unwrap()
    }

    // ── is_empty ──────────────────────────────────────────────────────────────

    #[test]
    fn is_empty_returns_true_for_empty_registry() {
        let registry = empty_registry();
        assert!(
            registry.is_empty(),
            "empty registry must report is_empty=true"
        );
    }

    #[test]
    fn is_empty_returns_false_for_non_empty_registry() {
        let registry = two_tool_registry();
        assert!(
            !registry.is_empty(),
            "non-empty registry must report is_empty=false"
        );
    }

    // ── filter_tools ──────────────────────────────────────────────────────────

    #[test]
    fn filter_tools_always_true_returns_all_tools() {
        let registry = two_tool_registry();
        let all = registry.filter_tools(|_| true);
        assert_eq!(
            all.len(),
            registry.len(),
            "always-true predicate must return all tools"
        );
    }

    #[test]
    fn filter_tools_always_false_returns_empty_vec() {
        let registry = two_tool_registry();
        let none = registry.filter_tools(|_| false);
        assert!(
            none.is_empty(),
            "always-false predicate must return empty vec"
        );
    }
}
