# Plan: gh-3 — Phase 3 Tool Registry: OpenAPI paths → MCP tool definitions

## Status: PENDING

## Context

This is Phase 3 of the allegro-mcp project. The goal is to implement the "tool registry" — the heart of the configurable engine — where every `(method, path)` pair in the Allegro OpenAPI schema becomes a dynamic MCP tool definition.

### Existing codebase
- `src/schema/` — loads, caches, and parses the OpenAPI YAML (Phases 1–2)
- `src/auth/` — OAuth2 client_credentials flow (Phase 4)
- `src/lib.rs` — exposes `pub mod schema;`
- `src/main.rs` — CLI with `schema stats` subcommand
- `Cargo.toml` — deps: `openapiv3 2.2.0`, `serde_json`, `serde`, `thiserror`, `tracing`; no new deps needed

---

## Phases

### Phase 1: Core types — `src/tool_registry/mod.rs`
**Status**: PENDING

Create `src/tool_registry/mod.rs` with:

```rust
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
    pub fn get_tool(&self, id: &str) -> Option<&ToolDef> {
        self.tools.iter().find(|t| t.id == id)
    }

    /// Filter tools by a predicate (for Phase 10 filtering hooks).
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
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
```

---

### Phase 2: Builder — `src/tool_registry/builder.rs`
**Status**: PENDING

Core mapping logic: `openapiv3::OpenAPI` → `Vec<ToolDef>`.

```rust
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
    method: &'static str,
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
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    // Collapse consecutive underscores
    let mut result = String::new();
    let mut prev_underscore = false;
    for c in s.chars() {
        if c == '_' {
            if !prev_underscore { result.push(c); }
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
        .map(|s| sanitize_name(s))
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
```

---

### Phase 3: $ref resolver — `src/tool_registry/ref_resolver.rs`
**Status**: PENDING

Resolves `$ref` chains in JSON Schema with cycle detection.

```rust
use openapiv3::{OpenAPI, ReferenceOr, Schema, SchemaKind};
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
        ReferenceOr::Reference { reference: inner_ref } => {
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
                let schema_ref = ReferenceOr::Reference { reference: ref_str.clone() };
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
```

---

### Phase 4: Schema builder — `src/tool_registry/schema_builder.rs`
**Status**: PENDING

Merges path/query/header parameters + requestBody into one JSON Schema object.

```rust
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
        Parameter::Query { parameter_data, .. } => {
            (parameter_data.name.clone(), parameter_data, parameter_data.required)
        }
        Parameter::Header { parameter_data, .. } => {
            (parameter_data.name.clone(), parameter_data, parameter_data.required)
        }
        Parameter::Cookie { parameter_data, .. } => {
            (parameter_data.name.clone(), parameter_data, parameter_data.required)
        }
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
```

---

### Phase 5: Fixture — `fixtures/allegro_sample.yaml`
**Status**: PENDING

Create a realistic Allegro-like fixture with:
- 5 operations (GET list, GET by id, POST create, PUT update, DELETE)
- `$ref` to components/schemas
- A self-referential (cyclic) schema
- Vendor extensions (`x-allegro-*`)
- requestBody on POST/PUT

```yaml
openapi: "3.0.3"
info:
  title: Allegro Sample API
  version: "1.0.0"
paths:
  /sale/offers:
    get:
      operationId: getListingOffers
      summary: List offers
      x-allegro-scope: sale
      parameters:
        - name: limit
          in: query
          schema:
            type: integer
        - name: offset
          in: query
          schema:
            type: integer
      responses:
        "200":
          description: OK
    post:
      operationId: createOffer
      summary: Create an offer
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: "#/components/schemas/OfferRequest"
      responses:
        "201":
          description: Created
  /sale/offers/{offerId}:
    get:
      operationId: getOffer
      summary: Get offer by ID
      parameters:
        - name: offerId
          in: path
          required: true
          schema:
            type: string
      responses:
        "200":
          description: OK
    put:
      operationId: updateOffer
      summary: Update an offer
      parameters:
        - name: offerId
          in: path
          required: true
          schema:
            type: string
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: "#/components/schemas/OfferRequest"
      responses:
        "200":
          description: OK
    delete:
      operationId: deleteOffer
      summary: Delete an offer
      parameters:
        - name: offerId
          in: path
          required: true
          schema:
            type: string
      responses:
        "204":
          description: No Content
components:
  schemas:
    OfferRequest:
      type: object
      required:
        - name
        - price
      properties:
        name:
          type: string
        price:
          $ref: "#/components/schemas/Price"
        category:
          $ref: "#/components/schemas/Category"
    Price:
      type: object
      properties:
        amount:
          type: string
        currency:
          type: string
    Category:
      type: object
      properties:
        id:
          type: string
        parent:
          $ref: "#/components/schemas/Category"  # self-referential cycle
```

---

### Phase 6: Integration tests — `tests/tool_registry_integration.rs`
**Status**: PENDING

```rust
use allegro_mcp::tool_registry::ToolRegistry;
use allegro_mcp::schema::{self, SchemaSource};
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

// ── minimal_oas3.yaml ─────────────────────────────────────────────────────────

#[tokio::test]
async fn minimal_fixture_yields_two_tools() {
    let source = SchemaSource::File(fixture_path("minimal_oas3.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    assert_eq!(registry.len(), 2);
}

#[tokio::test]
async fn minimal_fixture_tool_ids_are_prefixed() {
    let source = SchemaSource::File(fixture_path("minimal_oas3.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    for tool in registry.list_tools() {
        assert!(tool.id.starts_with("allegro_"), "id '{}' must start with allegro_", tool.id);
    }
}

#[tokio::test]
async fn minimal_fixture_input_schemas_are_objects() {
    let source = SchemaSource::File(fixture_path("minimal_oas3.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    for tool in registry.list_tools() {
        assert_eq!(tool.input_schema["type"], "object",
            "tool '{}' input_schema must have type=object", tool.id);
    }
}

#[tokio::test]
async fn get_tool_returns_correct_tool() {
    let source = SchemaSource::File(fixture_path("minimal_oas3.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    // Find any tool and look it up by id
    let first = registry.list_tools().first().unwrap();
    let found = registry.get_tool(&first.id).unwrap();
    assert_eq!(found.id, first.id);
}

#[tokio::test]
async fn get_tool_returns_none_for_unknown_id() {
    let source = SchemaSource::File(fixture_path("minimal_oas3.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    assert!(registry.get_tool("allegro_nonexistent_xyz").is_none());
}

#[tokio::test]
async fn filter_tools_by_method() {
    let source = SchemaSource::File(fixture_path("minimal_oas3.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    let get_tools = registry.filter_tools(|t| t.method == "get");
    assert_eq!(get_tools.len(), 2); // both operations are GET
}

// ── allegro_sample.yaml ───────────────────────────────────────────────────────

#[tokio::test]
async fn sample_fixture_yields_five_tools() {
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    assert_eq!(registry.len(), 5);
}

#[tokio::test]
async fn sample_fixture_operation_ids_are_used() {
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    let ids: Vec<&str> = registry.list_tools().iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&"allegro_getlistingoffers"), "expected allegro_getlistingoffers, got: {:?}", ids);
    assert!(ids.contains(&"allegro_createoffer"));
    assert!(ids.contains(&"allegro_getoffer"));
    assert!(ids.contains(&"allegro_updateoffer"));
    assert!(ids.contains(&"allegro_deleteoffer"));
}

#[tokio::test]
async fn sample_fixture_post_has_body_property() {
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    let create = registry.get_tool("allegro_createoffer").unwrap();
    assert!(create.input_schema["properties"]["body"].is_object(),
        "POST tool must have a 'body' property in input_schema");
}

#[tokio::test]
async fn sample_fixture_path_params_are_required() {
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    let get_offer = registry.get_tool("allegro_getoffer").unwrap();
    let required = get_offer.input_schema["required"].as_array().unwrap();
    let required_names: Vec<&str> = required.iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(required_names.contains(&"offerId"),
        "path param offerId must be required, got: {:?}", required_names);
}

#[tokio::test]
async fn sample_fixture_cyclic_ref_does_not_panic() {
    // Category has a self-referential $ref — must not stack overflow or panic
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    // Just verify it completes without panic
    assert_eq!(registry.len(), 5);
}

#[tokio::test]
async fn sample_fixture_descriptions_are_non_empty() {
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    for tool in registry.list_tools() {
        assert!(!tool.description.is_empty(),
            "tool '{}' must have a non-empty description", tool.id);
    }
}

// ── name sanitization (unit-style via builder) ────────────────────────────────

#[test]
fn sanitize_name_lowercases_and_replaces_special_chars() {
    use allegro_mcp::tool_registry::builder::{sanitize_name, derive_name_from_path};
    assert_eq!(sanitize_name("getListingOffers"), "getlistingoffers");
    assert_eq!(sanitize_name("get-listing-offers"), "get_listing_offers");
    assert_eq!(sanitize_name("GET_OFFERS_V2"), "get_offers_v2");
    assert_eq!(sanitize_name("__leading__"), "leading");
}

#[test]
fn derive_name_from_path_produces_valid_name() {
    use allegro_mcp::tool_registry::builder::derive_name_from_path;
    assert_eq!(derive_name_from_path("get", "/sale/offers"), "get_sale_offers");
    assert_eq!(derive_name_from_path("get", "/sale/offers/{offerId}"), "get_sale_offers_offerid");
    assert_eq!(derive_name_from_path("post", "/"), "post");
}

// ── empty OpenAPI document ────────────────────────────────────────────────────

#[test]
fn empty_openapi_yields_empty_registry() {
    use openapiv3::OpenAPI;
    let api: OpenAPI = serde_yaml::from_str(
        "openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n"
    ).unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    assert_eq!(registry.len(), 0);
    assert!(registry.is_empty());
}
```

---

### Phase 7: Expose in `src/lib.rs`
**Status**: PENDING

Add `pub mod tool_registry;` to `src/lib.rs`:

```rust
//! allegro-mcp library — exposes internal modules for integration testing.
pub mod schema;
pub mod tool_registry;
```

---

### Phase 8: Wire into `src/main.rs`
**Status**: PENDING

Add a `tools list` subcommand that:
1. Loads the schema (using existing `--schema-url` / `--schema-file` flags)
2. Builds the registry
3. Prints: `Tools: <count>` and the first 5 tool names

Modify the `Commands` enum and `SchemaAction` enum in `main.rs`:

```rust
#[derive(Debug, Subcommand)]
enum Commands {
    Schema {
        #[command(subcommand)]
        action: SchemaAction,
    },
    Tools {
        #[command(subcommand)]
        action: ToolsAction,
    },
}

#[derive(Debug, Subcommand)]
enum ToolsAction {
    /// List all tools derived from the schema
    List,
}
```

**Important**: The existing `main.rs` has an auth smoke-test block (lines ~82–138) that runs unconditionally before `match cli.command` and returns early if `ALLEGRO_CLIENT_ID`/`ALLEGRO_CLIENT_SECRET` env vars are missing. This means `tools list` would silently exit in dev/CI environments without credentials. **Move the auth block inside the `Schema { Stats }` arm** (or remove it entirely from the top-level flow), so `tools list` works without auth credentials.

Add `mod tool_registry;` to `main.rs` (matching the existing `mod schema;` pattern). Use `tool_registry::ToolRegistry` (not `allegro_mcp::tool_registry::ToolRegistry`).

The **complete** updated `main()` function body after the source resolution should look like:

```rust
// (auth block removed from top-level — move into Schema Stats arm if needed, or remove)

match cli.command {
    Some(Commands::Schema {
        action: SchemaAction::Stats,
    }) => {
        let (api, raw) = schema::load(&source).await?;
        let stats = schema::compute_stats(&api, &raw);
        println!("Paths:      {}", stats.path_count);
        println!("Operations: {}", stats.operation_count);
        println!("Parameters: {}", stats.parameter_count);
        println!("SHA-256:    {}", stats.sha256);
    }
    Some(Commands::Tools { action: ToolsAction::List }) => {
        let (api, _raw) = schema::load(&source).await?;
        let registry = tool_registry::ToolRegistry::from_openapi(&api)?;
        println!("Tools: {}", registry.len());
        for tool in registry.list_tools().iter().take(5) {
            println!("  - {} ({})", tool.id, tool.description);
        }
        if registry.len() > 5 {
            println!("  ... and {} more", registry.len() - 5);
        }
    }
    None => {
        tracing::warn!("No subcommand — MCP server mode not yet implemented");
    }
}
```

The auth smoke-test code (token fetch, GET /sale/categories) should be **removed** from `main()` entirely for this phase — it was Phase 4 scaffolding and is not needed for Phase 3. The `auth` module and `AllegroAuth` import can remain but the smoke-test block should be deleted to avoid the early-return problem.

---

### Phase 9: Verification
**Status**: PENDING

Run in order:
```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test --locked
```

All must pass with zero errors.

---

## Edge cases

1. **No operationId**: derive from method + path segments
2. **Duplicate tool IDs**: append `_2`, `_3`, etc.
3. **`$ref` path items**: skip with TRACE log
4. **`$ref` parameters**: skip with TRACE log
5. **Cyclic `$ref`**: emit `{"type": "object", "description": "circular reference"}` sentinel
6. **Unresolvable `$ref`**: emit `{"type": "object", "description": "unresolved $ref: ..."}` sentinel
7. **`x-` vendor extensions on operations**: skip silently (openapiv3 crate ignores them in the typed struct)
8. **`x-` vendor extensions on parameters**: skip if name starts with `x-`
9. **No parameters, no requestBody**: emit `{"type": "object", "properties": {}}`
10. **requestBody with non-JSON content type**: fall back to first available media type
11. **Path param without explicit `required: true`**: treat as required (OAS spec mandates it)
12. **`openapiv3` crate API**: `ParameterData` does NOT have a `.schema` field. Use `parameter_data.format: ParameterSchemaOrContent` — match on `ParameterSchemaOrContent::Schema(ref s)` to get the schema, or `ParameterSchemaOrContent::Content(_)` for content-typed params (emit `{"type":"object"}` for those).

## Notes for implementer

- Do NOT add new Cargo dependencies — use only what's in Cargo.toml
- The `openapiv3` crate's `Parameter` enum has variants `Path`, `Query`, `Header`, `Cookie` — each wraps a `ParameterData` struct
- `ParameterData` in openapiv3 2.2.0 has `format: ParameterSchemaOrContent` (NOT `.schema`). Match on `ParameterSchemaOrContent::Schema(ref s)` to get the schema.
- `PathItem::iter()` yields `(&str, &Operation)` — use this directly in the builder (no custom `iter_operations` helper needed; avoids clippy warning)
- The `ToolRegistry` is intentionally not `Clone` (it holds a `Vec<ToolDef>` which is fine to clone if needed later)
- `builder::sanitize_name` and `builder::derive_name_from_path` must be `pub` for integration test access
