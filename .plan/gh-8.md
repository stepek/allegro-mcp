# gh-8 — Phase 8: MCP wiring: rmcp server over stdio

## Status: PENDING

---

## Context

The repo already has:
- `src/auth/mod.rs` — `AllegroAuth` with `client_credentials` OAuth2 flow, in-memory token cache
- `src/schema/` — schema fetch/cache/parse pipeline
- `src/tool_registry/` — `ToolRegistry` with `ToolDef` (id, name, description, input_schema, method, path)
- `src/main.rs` — CLI with `schema stats` and `tools list` subcommands; `None` branch says "MCP server mode not yet implemented"
- `src/lib.rs` — exposes `schema` and `tool_registry` as public modules
- `Cargo.toml` — already has `rmcp = { version = "3.2.0", features = ["server", "transport-io"] }`

The `None` branch in `main.rs` is the entry point for MCP server mode. The `auth` module is declared with `#[allow(dead_code)]` because it is not yet wired into any live path.

**rmcp 3.2.0 API summary** (from source inspection):
- `ServerHandler` trait — implement `get_info()`, `list_tools()`, `call_tool()`. All other methods have default no-op implementations.
- `ServerInfo` is a type alias for `InitializeResult` which holds `protocol_version`, `capabilities: ServerCapabilities`, `server_info: Implementation`, and optional `instructions`.
- `ServerCapabilities::builder().enable_tools().build()` produces a capabilities object advertising only tools.
- `serve_server(handler, transport)` — async fn that performs the `initialize` handshake then enters the serve loop; returns `RunningService`.
- `rmcp::transport::io::stdio()` — returns `(tokio::io::Stdin, tokio::io::Stdout)` tuple; the `(R, W)` pair implements `IntoTransport` via `AsyncRwTransport`.
- `RunningService::waiting()` — awaits the serve loop until stdin EOF or cancellation.
- `list_tools()` receives `Option<PaginatedRequestParams>` and must return `ListToolsResult { tools: Vec<Tool>, next_cursor: Option<String> }`.
- `call_tool()` receives `CallToolRequestParams { name, arguments: Option<JsonObject>, .. }` and returns `Result<CallToolResponse, McpError>`.
- `CallToolResponse::Complete(CallToolResult)` is the normal return path.
- `CallToolResult::success(vec![ContentBlock::text(...)])` — successful result.
- `CallToolResult::error(vec![ContentBlock::text(...)])` — tool-level error (session survives).
- `Tool::new(name, description, input_schema)` then `.with_annotations(ToolAnnotations::new().read_only(true))` for GET/HEAD.
- `Tool.input_schema` is `Arc<JsonObject>` — a `serde_json::Map<String, Value>`.
- Logs must go to **stderr only** — `tracing_subscriber` is already configured with `.with_writer(std::io::stderr)`.

---

## Approach

### Design decisions

1. **`AllegroServer` struct** — holds `Arc<ToolRegistry>`, `Arc<AllegroAuth>`, `reqwest::Client`, and `sandbox: bool`. It is `Send + Sync + 'static` (required by `ServerHandler`). The registry is built once at startup and shared immutably.

2. **`ToolDef` → `rmcp::Tool` conversion** — a pure function `tool_def_to_rmcp(def: &ToolDef) -> Tool` that:
   - Converts `def.input_schema` (`serde_json::Value::Object`) to `Arc<JsonObject>` (unwrap the object map).
   - Sets `readOnlyHint = true` for `method == "get"` or `method == "head"`.
   - Sets `readOnlyHint = false` for all other methods (POST, PUT, PATCH, DELETE).

3. **HTTP dispatcher** — a standalone async function `dispatch(auth, http, sandbox, tool_def, arguments) -> Result<String, String>` that:
   - Substitutes `{paramName}` path placeholders from `arguments`.
   - Separates remaining arguments into query params (for GET/HEAD) or JSON body (for POST/PUT/PATCH).
   - Injects `Authorization: Bearer <token>` from `AllegroAuth::token()`.
   - Injects `User-Agent: allegro-mcp/0.1.0`.
   - Injects `Accept: application/vnd.allegro.public.v1+json` (Allegro API requirement).
   - Returns the response body as a `String`, truncated to 100 KB.

4. **Stdio transport** — `rmcp::transport::io::stdio()` returns `(Stdin, Stdout)`. Pass directly to `serve_server`. The rmcp serve loop exits when stdin EOF is detected (`QuitReason::Closed`), which is the graceful shutdown path.

5. **Error handling** — tool errors (HTTP 4xx/5xx, auth failure, network error) are returned as `Ok(CallToolResponse::Complete(CallToolResult::error(...)))` so the session survives. Only unknown tool name returns `Err(McpError::method_not_found(...))`.

6. **`main.rs` changes** — the `None` branch becomes the MCP server mode. It builds `AllegroAuth`, loads the schema, builds `ToolRegistry`, constructs `AllegroServer`, calls `serve_server`, and awaits `waiting()`.

7. **No new Cargo dependencies** — all required types are already available via `rmcp`, `reqwest`, `serde_json`, `tokio`, `tracing`, `anyhow`.

---

## Files to Create/Modify

| File | Action | Purpose |
|---|---|---|
| `src/server.rs` | **Create** | `AllegroServer` struct implementing `rmcp::ServerHandler` |
| `src/dispatcher.rs` | **Create** | HTTP request builder/dispatcher; path/query param substitution |
| `src/main.rs` | **Modify** | Wire `None` branch to MCP server mode; expose `auth` module |
| `src/lib.rs` | **Modify** | Expose `server` and `dispatcher` modules for integration tests |

---

## Implementation Steps

### Phase 1: PENDING — `src/dispatcher.rs` (HTTP dispatch layer)

This module is pure logic with no rmcp dependency. It can be written and unit-tested independently.

**Step 1.1 — Define the base URL helper**

```
fn allegro_api_base(sandbox: bool) -> &'static str
```
- Returns `"https://api.allegro.pl"` for production.
- Returns `"https://api.allegrosandbox.pl"` for sandbox.

**Step 1.2 — Define path substitution**

```
fn substitute_path_params(
    path: &str,
    arguments: &serde_json::Map<String, Value>,
) -> (String, Vec<String>)
```
- Scans `path` for `{paramName}` segments using a simple character scan (no regex needed).
- For each `{paramName}` found, looks up `paramName` in `arguments`.
  - If found: replaces `{paramName}` with `percent_encode(value.as_str().unwrap_or(&value.to_string()))`.
  - Collects the names of all substituted params into a `Vec<String>` (consumed_params).
- Returns `(substituted_path, consumed_params)`.
- Path params that are missing from arguments are left as `{paramName}` literals (the HTTP call will fail with a 404, which is surfaced as a tool error — not a crash).

**Step 1.3 — Define the dispatch function**

```
pub async fn dispatch(
    auth: &crate::auth::AllegroAuth,
    http: &reqwest::Client,
    sandbox: bool,
    tool_def: &crate::tool_registry::ToolDef,
    arguments: serde_json::Map<String, Value>,
) -> Result<String, String>
```

Logic:
1. Call `auth.token().await` — on error, return `Err(format!("auth error: {e}"))`.
2. Call `substitute_path_params(&tool_def.path, &arguments)` → `(path, consumed)`.
3. Build `url = format!("{}{}", allegro_api_base(sandbox), path)`.
4. Build `reqwest::RequestBuilder`:
   - `.method(reqwest::Method::from_bytes(tool_def.method.to_uppercase().as_bytes()).unwrap_or(reqwest::Method::GET))`
   - `.url(url)`
   - `.header("Authorization", format!("Bearer {token}"))`
   - `.header("User-Agent", "allegro-mcp/0.1.0")`
   - `.header("Accept", "application/vnd.allegro.public.v1+json")`
5. Remove consumed path params from `arguments` to get `remaining`.
6. For GET/HEAD methods: add remaining params as query string via `.query(&pairs)` where `pairs: Vec<(&str, String)>` is built from `remaining` (values serialized to string).
7. For POST/PUT/PATCH/DELETE: if `remaining` contains key `"body"`, extract it and send as `.json(&body_value)`. Otherwise send remaining as `.json(&remaining)`.
8. Call `.send().await` — on error, return `Err(format!("HTTP error: {e}"))`.
9. Check `response.status()`:
   - On 4xx/5xx: read body text, return `Err(format!("HTTP {status}: {body}"))`.
10. Read response body as text via `.text().await` — on error, return `Err(...)`.
11. Truncate to 100 KB (102_400 bytes): if `body.len() > 102_400`, truncate at a UTF-8 boundary and append `"\n[truncated]"`.
12. Return `Ok(body)`.

**Step 1.4 — Unit tests in `dispatcher.rs`**

- `test_substitute_path_params_replaces_single_param` — `"/sale/offers/{offerId}"` with `{"offerId": "123"}` → `"/sale/offers/123"`, consumed = `["offerId"]`.
- `test_substitute_path_params_no_params` — path with no braces returns unchanged, consumed empty.
- `test_substitute_path_params_missing_param` — param in path not in arguments is left as literal.
- `test_substitute_path_params_multiple_params` — two path params both substituted.
- `test_truncate_at_100kb` — string > 100 KB is truncated and ends with `"[truncated]"`.

---

### Phase 2: PENDING — `src/server.rs` (AllegroServer + ServerHandler)

**Step 2.1 — Define `AllegroServer`**

```rust
pub struct AllegroServer {
    registry: Arc<crate::tool_registry::ToolRegistry>,
    auth: Arc<crate::auth::AllegroAuth>,
    http: reqwest::Client,
    sandbox: bool,
}
```

- `impl AllegroServer`:
  ```rust
  pub fn new(
      registry: crate::tool_registry::ToolRegistry,
      auth: crate::auth::AllegroAuth,
      sandbox: bool,
  ) -> Self
  ```
  Wraps registry and auth in `Arc`, creates a new `reqwest::Client`.

**Step 2.2 — `tool_def_to_rmcp` conversion function**

```rust
fn tool_def_to_rmcp(def: &crate::tool_registry::ToolDef) -> rmcp::model::Tool
```

- Extract `input_schema` as `Arc<JsonObject>`:
  - `def.input_schema` is a `serde_json::Value`. It must be a `Value::Object`.
  - Call `def.input_schema.as_object().cloned().unwrap_or_default()` to get `JsonObject`.
  - Wrap in `Arc::new(...)`.
- Determine `read_only_hint`:
  - `true` if `def.method == "get" || def.method == "head"`.
  - `false` otherwise.
- Build annotations:
  ```rust
  let annotations = rmcp::model::ToolAnnotations::new()
      .read_only(read_only_hint);
  ```
- Build tool:
  ```rust
  rmcp::model::Tool::new(
      def.name.clone(),       // Cow<'static, str> — clone as owned String, coerced
      def.description.clone(),
      input_schema,
  )
  .with_annotations(annotations)
  ```
  Note: `Tool::new` takes `N: Into<Cow<'static, str>>` — `String` satisfies this.

**Step 2.3 — Implement `ServerHandler` for `AllegroServer`**

```rust
impl rmcp::ServerHandler for AllegroServer { ... }
```

**`get_info()`**:
```rust
fn get_info(&self) -> rmcp::model::ServerInfo {
    rmcp::model::InitializeResult::new(
        rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .build(),
    )
    .with_server_info(rmcp::model::Implementation {
        name: "allegro-mcp".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    })
    .with_instructions(
        "Allegro REST API MCP server. Tools are generated from the official \
         Allegro OpenAPI schema. Use sandbox=true for the sandbox environment."
            .to_string(),
    )
}
```

**`list_tools()`**:
```rust
async fn list_tools(
    &self,
    request: Option<rmcp::model::PaginatedRequestParams>,
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
```
Note: `ListToolsResult` is produced by the `paginated_result!` macro. Use `ListToolsResult::with_all_items(tools)` if available; otherwise construct the struct directly with `tools` and `next_cursor: None`.

**`call_tool()`**:
```rust
async fn call_tool(
    &self,
    request: rmcp::model::CallToolRequestParams,
    _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
    // 1. Look up tool
    let tool_def = match self.registry.get_tool(&request.name) {
        Some(t) => t,
        None => {
            return Err(rmcp::ErrorData::method_not_found::<
                rmcp::model::CallToolRequestMethod,
            >());
        }
    };

    // 2. Extract arguments (default to empty map)
    let arguments = request.arguments.unwrap_or_default();

    // 3. Dispatch HTTP request
    match crate::dispatcher::dispatch(
        &self.auth,
        &self.http,
        self.sandbox,
        tool_def,
        arguments,
    )
    .await
    {
        Ok(body) => Ok(rmcp::model::CallToolResponse::Complete(
            rmcp::model::CallToolResult::success(vec![
                rmcp::model::ContentBlock::text(body),
            ]),
        )),
        Err(msg) => Ok(rmcp::model::CallToolResponse::Complete(
            rmcp::model::CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(msg),
            ]),
        )),
    }
}
```

**Step 2.4 — Unit tests in `server.rs`**

- `test_tool_def_to_rmcp_get_has_read_only_hint` — GET method → `annotations.read_only_hint == Some(true)`.
- `test_tool_def_to_rmcp_post_has_no_read_only_hint` — POST method → `annotations.read_only_hint == Some(false)`.
- `test_tool_def_to_rmcp_head_has_read_only_hint` — HEAD method → `annotations.read_only_hint == Some(true)`.
- `test_tool_def_to_rmcp_delete_has_no_read_only_hint` — DELETE method → `annotations.read_only_hint == Some(false)`.
- `test_tool_def_to_rmcp_name_matches` — `tool.name == def.name`.
- `test_tool_def_to_rmcp_description_matches` — `tool.description == Some(def.description)`.
- `test_tool_def_to_rmcp_input_schema_is_object` — `tool.input_schema` is a non-empty map when `def.input_schema` has properties.

---

### Phase 3: PENDING — `src/main.rs` (wire MCP server mode)

**Step 3.1 — Remove `#[allow(dead_code)]` from `mod auth`**

The auth module is now actively used. Remove the dead_code allow.

**Step 3.2 — Add `mod server` and `mod dispatcher`**

```rust
mod auth;
mod dispatcher;
mod schema;
mod server;
mod tool_registry;
```

**Step 3.3 — Replace the `None` branch**

Current:
```rust
None => {
    tracing::warn!("No subcommand — MCP server mode not yet implemented");
}
```

Replace with:
```rust
None => {
    run_mcp_server(cli.sandbox, source).await?;
}
```

**Step 3.4 — Add `run_mcp_server` async function**

```rust
async fn run_mcp_server(sandbox: bool, source: schema::SchemaSource) -> Result<()> {
    tracing::info!(sandbox, "starting MCP server (stdio transport)");

    // 1. Build auth
    let auth = auth::AllegroAuth::from_env(sandbox)
        .map_err(|e| anyhow::anyhow!("auth init failed: {e}"))?;

    // 2. Load schema and build registry
    let (api, _raw) = schema::load(&source).await
        .map_err(|e| anyhow::anyhow!("schema load failed: {e}"))?;
    let registry = tool_registry::ToolRegistry::from_openapi(&api)
        .map_err(|e| anyhow::anyhow!("registry build failed: {e}"))?;

    tracing::info!(tool_count = registry.len(), "tool registry built");

    // 3. Build server handler
    let handler = server::AllegroServer::new(registry, auth, sandbox);

    // 4. Wire stdio transport — logs already go to stderr via tracing_subscriber
    let transport = rmcp::transport::io::stdio();

    // 5. Perform MCP initialize handshake and enter serve loop
    let running = rmcp::serve_server(handler, transport)
        .await
        .map_err(|e| anyhow::anyhow!("MCP server init failed: {e}"))?;

    tracing::info!("MCP server ready");

    // 6. Block until stdin EOF (client disconnect) or cancellation
    running.waiting().await
        .map_err(|e| anyhow::anyhow!("MCP server task error: {e}"))?;

    tracing::info!("MCP server shut down");
    Ok(())
}
```

**Step 3.5 — Add required imports**

Add to the top of `main.rs`:
```rust
use rmcp::ServiceExt as _;   // needed if using .serve() extension method
```
Or, since `serve_server` is a free function re-exported from `rmcp`, just ensure `rmcp` is in scope (it already is via `Cargo.toml`).

**Step 3.6 — Update startup log message**

Change `"allegro-mcp starting (phase 4 auth)"` to `"allegro-mcp starting"`.

---

### Phase 4: PENDING — `src/lib.rs` (expose new modules)

Add the new modules so integration tests can import them:

```rust
pub mod auth;
pub mod dispatcher;
pub mod schema;
pub mod server;
pub mod tool_registry;
```

Note: `auth` was previously not exposed. It must be made public for integration tests that construct `AllegroAuth` directly.

---

## Tests to Write

### Unit tests (in-module, `#[cfg(test)]`)

**`src/dispatcher.rs`**:
- `test_substitute_path_params_replaces_single_param`
- `test_substitute_path_params_no_params`
- `test_substitute_path_params_missing_param_left_as_literal`
- `test_substitute_path_params_multiple_params`
- `test_truncate_body_at_100kb`
- `test_truncate_body_not_applied_under_100kb`

**`src/server.rs`**:
- `test_tool_def_to_rmcp_get_has_read_only_hint`
- `test_tool_def_to_rmcp_post_no_read_only_hint`
- `test_tool_def_to_rmcp_head_has_read_only_hint`
- `test_tool_def_to_rmcp_delete_no_read_only_hint`
- `test_tool_def_to_rmcp_name_and_description_match`
- `test_tool_def_to_rmcp_input_schema_preserved`

### Integration tests (in `tests/`)

**`tests/mcp_server_integration.rs`** (new file):
- `test_list_tools_returns_registry_tools` — build `AllegroServer` from a minimal OpenAPI doc, call `list_tools(None, ...)` directly, assert the returned `Vec<Tool>` length matches the registry.
- `test_call_tool_unknown_name_returns_method_not_found` — call `call_tool` with a name not in the registry, assert `Err(McpError)` with `ErrorCode::METHOD_NOT_FOUND`.
- `test_call_tool_error_does_not_crash_session` — mock HTTP 500 response (wiremock), call `call_tool`, assert `Ok(CallToolResponse::Complete(result))` with `is_error == Some(true)`.
- `test_call_tool_success_returns_text_content` — mock HTTP 200 with JSON body, call `call_tool`, assert `Ok(CallToolResponse::Complete(result))` with `is_error != Some(true)` and content contains the JSON body.

---

## Acceptance Criteria

1. **MCP Inspector connects** — running `allegro-mcp` (no subcommand) starts a server that responds to the MCP `initialize` handshake over stdio.
2. **`tools/list` returns tools** — Inspector lists tools derived from the Allegro OpenAPI schema (hundreds of tools).
3. **`GET /sale/categories` succeeds end-to-end** — calling the tool `allegro_get_sale_categories` (or equivalent) with sandbox credentials returns a JSON response body as text content.
4. **Server survives tool error** — calling a tool that results in an HTTP error (e.g., missing required param → 422) returns `is_error: true` content without crashing the session; subsequent tool calls still work.
5. **Logs go to stderr only** — stdout contains only valid JSON-RPC messages; no log lines appear on stdout.
6. **Graceful shutdown on stdin EOF** — when the client closes the connection, the server exits cleanly with exit code 0.
7. **`cargo test` passes** — all unit and integration tests pass.
8. **`cargo clippy -- -D warnings` passes** — no new warnings.

---

## Edge Cases

### 1. Path param present in path but missing from arguments
**Scenario**: Tool has path `/sale/offers/{offerId}` but client sends no `offerId` argument.
**Handling**: `substitute_path_params` leaves `{offerId}` as a literal in the URL. The HTTP request is sent as-is. Allegro returns 404 or 400. The error body is returned as `CallToolResult::error(...)`. Session survives.

### 2. `arguments` is `None` (client sends no arguments)
**Scenario**: Client calls a tool with no arguments field.
**Handling**: `request.arguments.unwrap_or_default()` produces an empty `JsonObject`. Path substitution finds no matches. GET tools send no query params. POST tools send an empty JSON body `{}`. Allegro may return 422 (validation error) which is surfaced as a tool error.

### 3. Auth token fetch fails (missing env vars)
**Scenario**: `ALLEGRO_CLIENT_ID` or `ALLEGRO_CLIENT_SECRET` not set.
**Handling**: `AllegroAuth::from_env()` returns `Err(AuthError::MissingEnvVar(...))` in `run_mcp_server`. The server fails to start and exits with a non-zero code and an error message on stderr. The MCP session never begins.

### 4. Schema load fails (network unavailable, no cache)
**Scenario**: No network and no cached schema.
**Handling**: `schema::load()` returns `Err(SchemaError::Io(...))`. `run_mcp_server` propagates the error via `?`. Server exits before the MCP handshake.

### 5. Response body > 100 KB
**Scenario**: Allegro returns a very large JSON response.
**Handling**: `dispatcher::dispatch` truncates at 100 KB (102_400 bytes) at a UTF-8 boundary and appends `"\n[truncated]"`. The truncated string is returned as text content. The client sees a partial but valid UTF-8 response.

### 6. Truncation at non-UTF-8 boundary
**Scenario**: The 100 KB cutoff falls in the middle of a multi-byte UTF-8 character.
**Handling**: Walk backwards from byte 102_400 until `body.is_char_boundary(pos)` is true, then truncate there. This ensures the returned string is always valid UTF-8.

### 7. `reqwest::Client` reuse across tool calls
**Scenario**: Multiple concurrent tool calls.
**Handling**: `reqwest::Client` is `Clone + Send + Sync` and designed for reuse. It is stored in `AllegroServer` and shared across all `call_tool` invocations. Connection pooling is handled by reqwest internally.

### 8. Token expiry mid-session
**Scenario**: A long-running session where the token expires.
**Handling**: `AllegroAuth::token()` is called on every `call_tool` invocation. The double-check cache in `AllegroAuth` automatically refreshes tokens 60 seconds before expiry. No special handling needed in the dispatcher.

### 9. `input_schema` is not a JSON object
**Scenario**: A `ToolDef` has `input_schema` that is not `Value::Object` (e.g., `Value::Null` from a bug in the schema builder).
**Handling**: `def.input_schema.as_object().cloned().unwrap_or_default()` falls back to an empty `JsonObject`. The tool is still registered with an empty schema. The client can call it with no arguments.

### 10. Tool name collision with rmcp reserved names
**Scenario**: An Allegro operation produces a tool id that conflicts with an MCP built-in method name.
**Handling**: All tool ids are prefixed with `allegro_` (enforced by `builder.rs`). No MCP built-in method starts with `allegro_`. No collision is possible.

### 11. Stdin EOF during `initialize` handshake
**Scenario**: Client disconnects before sending `initialize`.
**Handling**: `serve_server` returns `Err(ServerInitializeError::ConnectionClosed(...))`. `run_mcp_server` maps this to an `anyhow::Error` and exits. The process exits with a non-zero code. This is acceptable — the session never started.

### 12. `body` key collision in arguments
**Scenario**: A GET endpoint has a query parameter literally named `"body"`.
**Handling**: For GET/HEAD methods, all remaining arguments (including `"body"`) are sent as query params. The `"body"` key is only treated specially for non-GET methods. This is correct per the schema builder convention where `"body"` is only added for operations with a `requestBody`.

### 13. Large tool registry (hundreds of tools)
**Scenario**: The Allegro schema has ~500+ operations.
**Handling**: `list_tools` returns all tools in a single response (no pagination). rmcp supports this — `ListToolsResult` has no size limit. If pagination is needed in a future phase, `next_cursor` can be populated.

### 14. Concurrent `tools/list` and `tools/call` requests
**Scenario**: MCP Inspector sends both simultaneously.
**Handling**: `AllegroServer` is `Send + Sync`. `ToolRegistry` is read-only after construction. `AllegroAuth` uses `tokio::sync::RwLock` internally. `reqwest::Client` is designed for concurrent use. No additional synchronization is needed.

### 15. `Implementation` struct field names
**Scenario**: `rmcp::model::Implementation` struct fields may differ from what is assumed.
**Handling**: From source inspection, `Implementation` has `name: String` and `version: String`. Use `Implementation { name: "allegro-mcp".to_string(), version: env!("CARGO_PKG_VERSION").to_string() }` for direct construction, or check if a constructor exists. If the struct is `#[non_exhaustive]`, use `..Default::default()` for any additional fields.

### 16. `ListToolsResult` constructor API
**Scenario**: The `paginated_result!` macro may not generate a `with_all_items` constructor.
**Handling**: If `with_all_items` does not exist, construct directly:
```rust
rmcp::model::ListToolsResult {
    tools,
    next_cursor: None,
    result_type: None,
    meta: None,
}
```
The implementer must verify the exact field names by checking the macro expansion or the generated struct. The `paginated_result!` macro in rmcp generates `tools: Vec<Tool>` and `next_cursor: Option<String>` as the primary fields.
