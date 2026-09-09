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
    /// Override for the Allegro API base URL. `None` means use the default
    /// derived from `sandbox`. Set via [`Self::with_api_base_url`] in tests.
    api_base_url: Option<String>,
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
            api_base_url: None,
        }
    }

    /// Override the Allegro API base URL used for dispatching tool calls.
    ///
    /// Intended for testing only — allows injecting a wiremock server URL so
    /// that integration tests can intercept API-level HTTP calls without real
    /// network access. Production callers should use [`Self::new`].
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn with_api_base_url(mut self, api_base_url: String) -> Self {
        self.api_base_url = Some(api_base_url);
        self
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

        let dispatch_result = if let Some(ref base) = self.api_base_url {
            crate::dispatcher::dispatch_with_base(&self.auth, &self.http, base, tool_def, arguments)
                .await
        } else {
            crate::dispatcher::dispatch(&self.auth, &self.http, self.sandbox, tool_def, arguments)
                .await
        };

        match dispatch_result {
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

    // ── Additional method coverage ─────────────────────────────────────────────

    #[test]
    fn test_tool_def_to_rmcp_put_no_read_only_hint() {
        let def = make_tool_def("put");
        let tool = tool_def_to_rmcp(&def);
        assert_eq!(
            tool.annotations.unwrap().read_only_hint,
            Some(false),
            "PUT must not be read-only"
        );
    }

    #[test]
    fn test_tool_def_to_rmcp_patch_no_read_only_hint() {
        let def = make_tool_def("patch");
        let tool = tool_def_to_rmcp(&def);
        assert_eq!(
            tool.annotations.unwrap().read_only_hint,
            Some(false),
            "PATCH must not be read-only"
        );
    }

    // ── input_schema with non-object value falls back to empty map ────────────

    #[test]
    fn test_tool_def_to_rmcp_non_object_input_schema_falls_back_to_empty() {
        // If input_schema is not a JSON object (e.g. null), tool_def_to_rmcp
        // must fall back to an empty map rather than panicking.
        let def = ToolDef {
            id: "allegro_null_schema".to_string(),
            name: "allegro_null_schema".to_string(),
            description: "Tool with null schema".to_string(),
            input_schema: serde_json::Value::Null,
            method: "get".to_string(),
            path: "/test".to_string(),
        };
        let tool = tool_def_to_rmcp(&def);
        // Must not panic; input_schema must be an empty map.
        assert!(
            tool.input_schema.is_empty(),
            "non-object input_schema must fall back to empty map"
        );
    }

    // ── get_info returns expected server name and capabilities ─────────────────

    #[test]
    fn test_get_info_returns_allegro_mcp_server_name() {
        use rmcp::ServerHandler;

        let registry = crate::tool_registry::ToolRegistry::from_openapi(
            &serde_yaml::from_str::<openapiv3::OpenAPI>(
                "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
            )
            .unwrap(),
        )
        .unwrap();
        let auth = crate::auth::AllegroAuth::new("id".to_string(), "secret".to_string(), false);
        let server = AllegroServer::new(registry, auth, false);

        let info = server.get_info();
        // The server info must identify itself as "allegro-mcp".
        assert_eq!(
            info.server_info.name.as_str(),
            "allegro-mcp",
            "server name must be 'allegro-mcp'"
        );
    }

    #[test]
    fn test_get_info_has_tools_capability() {
        use rmcp::ServerHandler;

        let registry = crate::tool_registry::ToolRegistry::from_openapi(
            &serde_yaml::from_str::<openapiv3::OpenAPI>(
                "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
            )
            .unwrap(),
        )
        .unwrap();
        let auth = crate::auth::AllegroAuth::new("id".to_string(), "secret".to_string(), false);
        let server = AllegroServer::new(registry, auth, false);

        let info = server.get_info();
        // The server must advertise tools capability.
        assert!(
            info.capabilities.tools.is_some(),
            "server must advertise tools capability"
        );
    }

    #[test]
    fn test_get_info_instructions_mention_allegro() {
        use rmcp::ServerHandler;

        let registry = crate::tool_registry::ToolRegistry::from_openapi(
            &serde_yaml::from_str::<openapiv3::OpenAPI>(
                "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
            )
            .unwrap(),
        )
        .unwrap();
        let auth = crate::auth::AllegroAuth::new("id".to_string(), "secret".to_string(), false);
        let server = AllegroServer::new(registry, auth, false);

        let info = server.get_info();
        let instructions = info.instructions.as_deref().unwrap_or("");
        assert!(
            instructions.contains("Allegro"),
            "instructions must mention Allegro, got: {instructions}"
        );
    }

    // ── AllegroServer::new sandbox flag ───────────────────────────────────────

    #[test]
    fn test_allegro_server_new_stores_sandbox_flag() {
        // We can't directly inspect `sandbox` on AllegroServer (it's private),
        // but we can verify construction doesn't panic for both values.
        let make_server = |sandbox: bool| {
            let registry = crate::tool_registry::ToolRegistry::from_openapi(
                &serde_yaml::from_str::<openapiv3::OpenAPI>(
                    "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
                )
                .unwrap(),
            )
            .unwrap();
            let auth =
                crate::auth::AllegroAuth::new("id".to_string(), "secret".to_string(), sandbox);
            AllegroServer::new(registry, auth, sandbox)
        };

        // Must not panic for either value.
        let _prod = make_server(false);
        let _sandbox = make_server(true);
    }
}
