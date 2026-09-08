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
