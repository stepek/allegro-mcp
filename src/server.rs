//! MCP server handler — implements [`rmcp::ServerHandler`] over the tool
//! registry, dispatching `tools/call` requests to the Allegro REST API via
//! [`crate::dispatcher`].

use std::sync::Arc;

/// The MCP server handler for the Allegro REST API.
///
/// Holds a read-only [`crate::tool_registry::ToolRegistry`] built once at
/// startup, a shared [`crate::auth::AllegroAuth`] token manager, and a
/// reusable [`reqwest::Client`] for connection pooling across tool calls.
pub struct AllegroServer {
    registry: Arc<crate::tool_registry::ToolRegistry>,
    auth: Arc<crate::auth::AllegroAuth>,
    http: reqwest::Client,
    sandbox: bool,
}

impl AllegroServer {
    /// Constructs a new [`AllegroServer`] from an already-built registry and
    /// auth manager.
    pub fn new(
        registry: crate::tool_registry::ToolRegistry,
        auth: crate::auth::AllegroAuth,
        sandbox: bool,
    ) -> Self {
        Self {
            registry: Arc::new(registry),
            auth: Arc::new(auth),
            http: reqwest::Client::new(),
            sandbox,
        }
    }
}

/// Converts a [`crate::tool_registry::ToolDef`] into an [`rmcp::model::Tool`].
///
/// GET/HEAD operations are annotated as read-only; all other HTTP methods
/// are annotated as non-read-only (may mutate Allegro state).
fn tool_def_to_rmcp(def: &crate::tool_registry::ToolDef) -> rmcp::model::Tool {
    let input_schema = Arc::new(def.input_schema.as_object().cloned().unwrap_or_default());
    let read_only_hint = matches!(def.method.as_str(), "get" | "head");
    let annotations = rmcp::model::ToolAnnotations::new().read_only(read_only_hint);

    rmcp::model::Tool::new(def.name.clone(), def.description.clone(), input_schema)
        .with_annotations(annotations)
}

impl rmcp::ServerHandler for AllegroServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::InitializeResult::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            "allegro-mcp",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(
            "Allegro REST API MCP server. Tools are generated from the official \
             Allegro OpenAPI schema. Use sandbox=true for the sandbox environment."
                .to_string(),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let tools: Vec<rmcp::model::Tool> = self
            .registry
            .list_tools()
            .iter()
            .map(tool_def_to_rmcp)
            .collect();
        Ok(rmcp::model::ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        // MCP `tools/call` requests reference the tool by its `name` field,
        // not its `id` — look it up accordingly. (Today `ToolDef::id ==
        // ToolDef::name`, an invariant enforced by builder.rs, but
        // `get_tool_by_name` doesn't depend on that holding forever.)
        let Some(tool_def) = self.registry.get_tool_by_name(request.name.as_ref()) else {
            return Err(rmcp::ErrorData::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                format!("unknown tool: {}", request.name),
                None,
            ));
        };

        let arguments = request.arguments.unwrap_or_default();

        match crate::dispatcher::dispatch(&self.auth, &self.http, self.sandbox, tool_def, arguments)
            .await
        {
            Ok(body) => Ok(rmcp::model::CallToolResponse::Complete(
                rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(body)]),
            )),
            Err(msg) => Ok(rmcp::model::CallToolResponse::Complete(
                rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text(msg)]),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_registry::ToolDef;

    fn make_tool_def(method: &str) -> ToolDef {
        ToolDef {
            id: "allegro_test_tool".to_string(),
            name: "allegro_test_tool".to_string(),
            description: "A test tool".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "foo": { "type": "string" }
                }
            }),
            method: method.to_string(),
            path: "/test/{foo}".to_string(),
        }
    }

    #[test]
    fn test_tool_def_to_rmcp_get_has_read_only_hint() {
        let def = make_tool_def("get");
        let tool = tool_def_to_rmcp(&def);
        assert_eq!(tool.annotations.unwrap().read_only_hint, Some(true));
    }

    #[test]
    fn test_tool_def_to_rmcp_post_no_read_only_hint() {
        let def = make_tool_def("post");
        let tool = tool_def_to_rmcp(&def);
        assert_eq!(tool.annotations.unwrap().read_only_hint, Some(false));
    }

    #[test]
    fn test_tool_def_to_rmcp_head_has_read_only_hint() {
        let def = make_tool_def("head");
        let tool = tool_def_to_rmcp(&def);
        assert_eq!(tool.annotations.unwrap().read_only_hint, Some(true));
    }

    #[test]
    fn test_tool_def_to_rmcp_delete_no_read_only_hint() {
        let def = make_tool_def("delete");
        let tool = tool_def_to_rmcp(&def);
        assert_eq!(tool.annotations.unwrap().read_only_hint, Some(false));
    }

    #[test]
    fn test_tool_def_to_rmcp_name_and_description_match() {
        let def = make_tool_def("get");
        let tool = tool_def_to_rmcp(&def);
        assert_eq!(tool.name.as_ref(), def.name);
        assert_eq!(tool.description.as_deref(), Some(def.description.as_str()));
    }

    #[test]
    fn test_tool_def_to_rmcp_input_schema_preserved() {
        let def = make_tool_def("get");
        let tool = tool_def_to_rmcp(&def);
        assert!(tool.input_schema.contains_key("properties"));
        assert!(!tool.input_schema.is_empty());
    }
}
