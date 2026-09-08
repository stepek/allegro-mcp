# Plan: gh-3 — Phase 3 Tool Registry

## Status: PENDING

---

## Overview

**What**: Implement the tool registry — the heart of the configurable engine. Every `(method, path)` pair in the loaded OpenAPI document becomes a dynamic MCP tool definition (`ToolDef`). The registry exposes a stable API (`list_tools`, `get_tool`, `filter_tools`) consumed by the MCP server layer in later phases.

**Root cause / scope**: No tool registry exists yet. The codebase has a working schema pipeline (`src/schema/`) that loads and parses `openapiv3::OpenAPI`. This phase adds a new top-level module `src/tool_registry/` that consumes `OpenAPI` and produces `Vec<ToolDef>`.

**Key design decisions**:
- `ToolDef` is a plain data struct (no async, no I/O) — pure transformation of `openapiv3` types into `serde_json::Value` JSON Schema objects.
- `$ref` resolution is done eagerly at registry build time, not lazily, so `get_tool` is O(1) and allocation-free.
- Cycle detection uses a `HashSet<String>` of ref strings accumulated on the recursive call stack (not a global set), so it is re-entrant and correct for diamond-shaped schemas.
- No new Cargo dependencies — `serde_json`, `openapiv3`, `thiserror`, `tracing` are already present.

---

## Files to Create / Modify

### New files (create)
```
src/tool_registry/
├── mod.rs           — public API: ToolRegistry, ToolDef, ToolRegistryError
├── builder.rs       — OpenAPI → Vec<ToolDef> mapping logic
├── ref_resolver.rs  — $ref chain resolution with cycle detection
└── schema_builder.rs — parameter + requestBody → JSON Schema object

tests/
└── tool_registry_integration.rs  — integration tests using fixture files

fixtures/
└── allegro_sample.yaml  — realistic Allegro-like fixture with $ref, requestBody, x- extensions
```

### Existing files (modify)
```
src/lib.rs    — add: pub mod tool_registry;
src/main.rs   — add: tools list subcommand
```

---

## Phases

### Phase 1: Core data types and error type in `src/tool_registry/mod.rs`

**Status**: PENDING

**Agent**: implementer

**File**: `src/tool_registry/mod.rs`

Declare the public surface of the module. No logic here — only type definitions, re-exports, and the registry struct.

#### Structs and types to define

**`ToolDef`** — the MCP tool definition:
```
pub struct ToolDef {
    pub id: String,           // sanitized operationId or derived name (no prefix)
    pub name: String,         // "allegro_" + id
    pub description: String,  // from operation.summary, fallback to operation.description, fallback to ""
    pub input_schema: serde_json::Value,  // merged JSON Schema object {"type":"object","properties":{...},"required":[...]}
}
```

**`ToolRegistryError`** — error enum via `thiserror`:
```
pub enum ToolRegistryError {
    BuildError(String),   // catch-all for builder failures; carries human-readable context
}
```
Display: `"tool registry build failed: {0}"`

**`ToolRegistry`** — the registry struct:
```
pub struct ToolRegistry {
    tools: Vec<ToolDef>,
    index: std::collections::HashMap<String, usize>,  // maps tool.id → index in tools vec for O(1) get_tool
}
```

#### Public API methods on `ToolRegistry`

```rust
// Construct from a parsed OpenAPI document. Calls builder::build_tools internally.
pub fn from_openapi(api: &openapiv3::OpenAPI) -> Result<ToolRegistry, ToolRegistryError>

// Return all tools in insertion order (path iteration order from openapiv3).
pub fn list_tools(&self) -> &[ToolDef]

// Look up a single tool by its id (not name — id has no prefix).
// Returns None if not found.
pub fn get_tool(&self, id: &str) -> Option<&ToolDef>

// Return all tools matching the predicate. Used by Phase 10 filtering hooks.
pub fn filter_tools(&self, predicate: impl Fn(&ToolDef) -> bool) -> Vec<&ToolDef>
```

#### `from_openapi` implementation detail
```
let tools = builder::build_tools(api)?;
let index = tools.iter().enumerate()
    .map(|(i, t)| (t.id.clone(), i))
    .collect();
Ok(ToolRegistry { tools, index })
```

#### `get_tool` implementation detail
```
fn get_tool(&self, id: &str) -> Option<&ToolDef> {
    self.index.get(id).map(|&i| &self.tools[i])
}
```

#### Module declarations
```rust
pub mod builder;
pub mod ref_resolver;
pub mod schema_builder;
```

#### Unit tests in `mod.rs`
- `empty_openapi_yields_empty_registry` — `paths: {}` → `list_tools()` returns empty slice
- `get_tool_returns_none_for_unknown_id` — registry with one tool, look up wrong id → `None`
- `filter_tools_returns_matching_subset` — registry with 3 tools, filter by name prefix → correct subset

---

### Phase 2: Name sanitization and operation ID derivation in `src/tool_registry/builder.rs`

**Status**: PENDING

**Agent**: implementer

**File**: `src/tool_registry/builder.rs`

This is the core mapping logic. Iterates `api.paths.paths`, extracts all operations, and produces one `ToolDef` per operation.

#### Function signatures

```rust
// Entry point called by ToolRegistry::from_openapi
pub fn build_tools(api: &openapiv3::OpenAPI) -> Result<Vec<ToolDef>, ToolRegistryError>

// Sanitize an operationId or derived name into a valid tool id.
// Rules:
//   1. Replace every char that is not [a-zA-Z0-9] with '_'
//   2. Collapse consecutive underscores into one '_'
//   3. Strip leading and trailing underscores
//   4. Lowercase the result
//   5. If result is empty after sanitization, return Err(ToolRegistryError::BuildError(...))
pub(crate) fn sanitize_name(raw: &str) -> Result<String, ToolRegistryError>

// Derive a tool id from HTTP method + path when operationId is absent.
// Example: GET /sale/offers/{offerId} → "get_sale_offers_offer_id"
// Rules:
//   1. Start with lowercase method name (get, post, put, delete, patch)
//   2. Split path on '/' — skip empty segments
//   3. For each segment: if it starts with '{', strip braces, snake_case the content
//      (e.g. "{offerId}" → "offer_id")
//   4. Join all parts with '_'
//   5. Apply sanitize_name to the result
pub(crate) fn derive_name_from_path(method: &str, path: &str) -> Result<String, ToolRegistryError>
```

#### Snake-casing path parameter names
When a path segment is `{offerId}`, strip the braces to get `offerId`, then convert camelCase to snake_case:
- Insert `_` before each uppercase letter that follows a lowercase letter
- Lowercase the whole string
- Example: `offerId` → `offer_id`, `categoryId` → `category_id`, `id` → `id`

This conversion is done inline in `derive_name_from_path` without any new dependency — iterate chars, detect uppercase-after-lowercase transitions.

#### Iteration logic in `build_tools`

```
let mut tools: Vec<ToolDef> = Vec::new();
let mut seen_ids: std::collections::HashMap<String, usize> = HashMap::new();

for (path_str, path_item_ref) in &api.paths.paths:
    match path_item_ref:
        ReferenceOr::Reference { reference } =>
            tracing::trace!("skipping $ref path item at {path_str}: {reference}")
            continue
        ReferenceOr::Item(path_item) =>
            for (method_str, operation) in path_item.iter():
                // Log vendor extensions at TRACE
                if !operation.extensions.is_empty():
                    tracing::trace!(
                        "operation at {method_str} {path_str} has {} vendor extension(s) — skipping extensions",
                        operation.extensions.len()
                    )

                let raw_id = operation.operation_id.as_deref().unwrap_or("");
                let base_id = if raw_id.is_empty():
                    derive_name_from_path(method_str, path_str)?
                else:
                    sanitize_name(raw_id)?

                // Deduplication: if base_id already seen, append _2, _3, ...
                let id = deduplicate_id(base_id, &mut seen_ids);

                let name = format!("allegro_{id}");
                let description = operation.summary.clone()
                    .or_else(|| operation.description.clone())
                    .unwrap_or_default();
                if description.is_empty():
                    tracing::trace!("operation {id} has no summary or description")

                let input_schema = schema_builder::build_input_schema(
                    &operation.parameters,
                    operation.request_body.as_ref(),
                    api,
                )?;

                tools.push(ToolDef { id, name, description, input_schema });

Ok(tools)
```

#### `deduplicate_id` helper (private)
```rust
fn deduplicate_id(base: String, seen: &mut HashMap<String, usize>) -> String {
    let count = seen.entry(base.clone()).or_insert(0);
    *count += 1;
    if *count == 1 {
        base
    } else {
        let new_id = format!("{}_{}", base, count);
        tracing::warn!("duplicate tool id '{base}' after sanitization — renamed to '{new_id}'");
        new_id
    }
}
```

#### Unit tests in `builder.rs`
- `sanitize_name_lowercase_only` — `"getOfferById"` → `"getofferbyid"` (no non-alnum chars, just lowercased)
- `sanitize_name_replaces_hyphens` — `"get-offer-by-id"` → `"get_offer_by_id"`
- `sanitize_name_replaces_spaces` — `"get offer"` → `"get_offer"`
- `sanitize_name_collapses_consecutive_underscores` — `"get__offer___id"` → `"get_offer_id"`
- `sanitize_name_strips_leading_trailing_underscores` — `"_get_offer_"` → `"get_offer"`
- `sanitize_name_empty_after_sanitize_returns_err` — `"___"` → `Err(ToolRegistryError::BuildError(...))`
- `sanitize_name_empty_input_returns_err` — `""` → `Err(ToolRegistryError::BuildError(...))`
- `derive_name_simple_path` — `("get", "/items")` → `"get_items"`
- `derive_name_with_path_param` — `("get", "/items/{id}")` → `"get_items_id"`
- `derive_name_with_camel_case_param` — `("get", "/sale/offers/{offerId}")` → `"get_sale_offers_offer_id"`
- `derive_name_nested_path` — `("delete", "/sale/offers/{offerId}/images/{imageId}")` → `"delete_sale_offers_offer_id_images_image_id"`
- `build_tools_uses_operation_id_when_present` — operation with `operationId: "listOffers"` → tool id `"listoffers"`, name `"allegro_listoffers"`
- `build_tools_derives_name_when_no_operation_id` — operation without operationId on `GET /items` → id `"get_items"`
- `build_tools_skips_ref_path_items` — path item that is a `$ref` → not counted in output
- `build_tools_description_prefers_summary` — has both summary and description → uses summary
- `build_tools_description_falls_back_to_description_field` — no summary, has description → uses description
- `build_tools_description_empty_when_neither_present` — no summary, no description → `description == ""`
- `build_tools_deduplicates_colliding_ids` — two operations sanitize to same id → second gets `_2` suffix

---

### Phase 3: `$ref` resolution with cycle detection in `src/tool_registry/ref_resolver.rs`

**Status**: PENDING

**Agent**: implementer

**File**: `src/tool_registry/ref_resolver.rs`

Resolves `openapiv3::ReferenceOr<openapiv3::Schema>` into a flat `serde_json::Value` JSON Schema object. The `openapiv3` crate represents `$ref` as `ReferenceOr::Reference { reference: String }`.

#### Key types from `openapiv3` used here
- `openapiv3::ReferenceOr<T>` — either `Reference { reference: String }` or `Item(T)`
- `openapiv3::Schema` — has `schema_kind: SchemaKind` and `schema_data: SchemaData`
- `openapiv3::SchemaKind` — `Type(Type)`, `OneOf { one_of }`, `AnyOf { any_of }`, `AllOf { all_of }`, `Not { not }`, `Any(AnySchema)`
- `openapiv3::Type` — `String(StringType)`, `Number(NumberType)`, `Integer(IntegerType)`, `Boolean(BooleanType)`, `Array(ArrayType)`, `Object(ObjectType)`
- `openapiv3::Components` — has `schemas: IndexMap<String, ReferenceOr<Schema>>`

#### Ref string format
Allegro uses JSON Pointer refs: `"#/components/schemas/SomeName"`. The resolver handles only this format. Other formats (external refs, relative refs) are treated as unresolvable and emit the sentinel.

#### Function signatures

```rust
// Resolve a ReferenceOr<Schema> to a serde_json::Value JSON Schema.
// visited: set of ref strings currently on the call stack (cycle detection).
//          Pass &mut HashSet::new() at the top-level call site.
// api: the root OpenAPI document (needed to look up #/components/schemas/...).
pub fn resolve_schema(
    schema_ref: &openapiv3::ReferenceOr<openapiv3::Schema>,
    api: &openapiv3::OpenAPI,
    visited: &mut std::collections::HashSet<String>,
) -> serde_json::Value

// Convert an inline openapiv3::Schema to serde_json::Value.
// Calls resolve_schema recursively for nested schemas.
pub(crate) fn schema_to_value(
    schema: &openapiv3::Schema,
    api: &openapiv3::OpenAPI,
    visited: &mut std::collections::HashSet<String>,
) -> serde_json::Value

// Look up a $ref string in api.components.schemas.
// Returns None if the ref is not in "#/components/schemas/{name}" format
// or if the name is not found in components.
pub(crate) fn lookup_ref<'a>(
    ref_str: &str,
    api: &'a openapiv3::OpenAPI,
) -> Option<&'a openapiv3::ReferenceOr<openapiv3::Schema>>
```

#### `resolve_schema` algorithm

```
fn resolve_schema(schema_ref, api, visited) -> Value:
    match schema_ref:
        ReferenceOr::Item(schema) =>
            schema_to_value(schema, api, visited)
        ReferenceOr::Reference { reference } =>
            if visited.contains(reference):
                tracing::trace!("cycle detected at ref '{reference}', emitting sentinel")
                return json!({"type": "object", "description": "circular reference"})
            visited.insert(reference.clone())
            let result = match lookup_ref(reference, api):
                None =>
                    tracing::trace!("unresolvable ref '{reference}', emitting sentinel")
                    json!({"type": "object", "description": "unresolvable $ref"})
                Some(target) =>
                    resolve_schema(target, api, visited)  // recurse
            visited.remove(reference)  // CRITICAL: remove after recursion for diamond schemas
            result
```

#### `schema_to_value` algorithm

Convert `openapiv3::Schema` to a `serde_json::Value` JSON Schema object.

**SchemaKind::Type(Type::Object(obj))**:
- Build `properties` map: for each `(name, schema_ref)` in `obj.properties`, call `resolve_schema` recursively
- `required` list: `obj.required.clone()`
- Output:
```json
{
  "type": "object",
  "properties": { ... },
  "required": [ ... ]
}
```
Omit `"required"` key if `obj.required` is empty.
If `schema_data.description` is `Some`, add `"description"` key.

**SchemaKind::Type(Type::Array(arr))**:
- `items`: if `arr.items` is `Some(schema_ref)`, call `resolve_schema`; else `json!({})`
- Output: `{"type": "array", "items": {...}}`
- Add `"description"` if present.

**SchemaKind::Type(Type::String(s))**:
- Output: `{"type": "string"}`
- If `s.enumeration` is non-empty: add `"enum": [...]`
  - Note: `s.enumeration` is `Vec<Option<String>>`; filter out `None` entries
- Add `"description"` if present.

**SchemaKind::Type(Type::Integer(_))**:
- Output: `{"type": "integer"}`
- Add `"description"` if present.

**SchemaKind::Type(Type::Number(_))**:
- Output: `{"type": "number"}`
- Add `"description"` if present.

**SchemaKind::Type(Type::Boolean(_))**:
- Output: `{"type": "boolean"}`
- Add `"description"` if present.

**SchemaKind::OneOf { one_of }**:
- Output: `{"oneOf": [resolve each element]}`

**SchemaKind::AnyOf { any_of }**:
- Output: `{"anyOf": [resolve each element]}`

**SchemaKind::AllOf { all_of }**:
- Output: `{"allOf": [resolve each element]}`

**SchemaKind::Not { not }**:
- Output: `{"not": resolve(not)}`

**SchemaKind::Any(_)**:
- Output: `{}` (empty schema — accepts any value)

**Nullable handling**: After building the base value, if `schema_data.nullable` is `true`, add `"nullable": true` to the output object. This is the JSON Schema draft-07 / OpenAPI 3.0 convention used by MCP.

#### `lookup_ref` algorithm
```
fn lookup_ref(ref_str, api) -> Option<&ReferenceOr<Schema>>:
    let name = ref_str.strip_prefix("#/components/schemas/")?
    api.components.as_ref()?.schemas.get(name)
```

#### Unit tests in `ref_resolver.rs`
- `resolve_inline_string_schema` — inline `{type: string}` → `{"type":"string"}`
- `resolve_inline_integer_schema` — inline `{type: integer}` → `{"type":"integer"}`
- `resolve_inline_boolean_schema` — inline `{type: boolean}` → `{"type":"boolean"}`
- `resolve_inline_object_schema_with_properties` — object with `name: string`, `age: integer` → correct JSON Schema with properties
- `resolve_inline_object_required_fields` — object with required `["id"]` → `"required": ["id"]` in output
- `resolve_inline_array_schema` — array with string items → `{"type":"array","items":{"type":"string"}}`
- `resolve_inline_string_enum` — string with enumeration `["A","B","C"]` → `{"type":"string","enum":["A","B","C"]}`
- `resolve_ref_to_component` — `$ref: "#/components/schemas/Foo"` with Foo defined as `{type: string}` → `{"type":"string"}`
- `resolve_unknown_ref_emits_sentinel` — `$ref: "#/components/schemas/DoesNotExist"` → `{"type":"object","description":"unresolvable $ref"}`
- `resolve_external_ref_emits_sentinel` — `$ref: "./other.yaml#/Foo"` → `{"type":"object","description":"unresolvable $ref"}`
- `resolve_cycle_emits_sentinel_no_stack_overflow` — schema A has property of type `$ref: "#/components/schemas/A"` → sentinel emitted, no panic
- `resolve_diamond_schema_no_false_cycle` — A refs B and C, both B and C ref D → D resolved twice without false cycle detection
- `resolve_array_with_ref_items` — array schema with `$ref` items → items resolved to correct schema
- `resolve_one_of_two_schemas` — oneOf with string and integer → `{"oneOf":[{"type":"string"},{"type":"integer"}]}`
- `resolve_nullable_string` — `{type: string, nullable: true}` → `{"type":"string","nullable":true}`
- `resolve_any_schema_yields_empty_object` — `SchemaKind::Any` → `{}`
- `lookup_ref_returns_none_for_wrong_prefix` — `"#/components/parameters/Foo"` → `None`
- `lookup_ref_returns_none_when_components_absent` — API with no components → `None`

---

### Phase 4: Parameter and requestBody merging in `src/tool_registry/schema_builder.rs`

**Status**: PENDING

**Agent**: implementer

**File**: `src/tool_registry/schema_builder.rs`

Merges path parameters, query parameters, header parameters, and requestBody into a single JSON Schema object that becomes `ToolDef.input_schema`.

#### Function signatures

```rust
// Build the merged input schema for one operation.
// parameters: the operation's parameter list (each is ReferenceOr<Parameter>)
// request_body: the operation's optional requestBody (ReferenceOr<RequestBody>)
// api: root document for $ref resolution
pub fn build_input_schema(
    parameters: &[openapiv3::ReferenceOr<openapiv3::Parameter>],
    request_body: Option<&openapiv3::ReferenceOr<openapiv3::RequestBody>>,
    api: &openapiv3::OpenAPI,
) -> Result<serde_json::Value, ToolRegistryError>

// Resolve a ReferenceOr<Parameter> to an inline Parameter.
// Returns None if the $ref cannot be resolved (logs TRACE).
pub(crate) fn resolve_parameter<'a>(
    param_ref: &'a openapiv3::ReferenceOr<openapiv3::Parameter>,
    api: &'a openapiv3::OpenAPI,
) -> Option<&'a openapiv3::Parameter>

// Resolve a ReferenceOr<RequestBody> to an inline RequestBody.
// Returns None if the $ref cannot be resolved (logs TRACE).
pub(crate) fn resolve_request_body<'a>(
    body_ref: &'a openapiv3::ReferenceOr<openapiv3::RequestBody>,
    api: &'a openapiv3::OpenAPI,
) -> Option<&'a openapiv3::RequestBody>
```

#### `build_input_schema` algorithm

The output is always a JSON Schema object:
```json
{
  "type": "object",
  "properties": {
    "<param_name>": { /* JSON Schema for this param */ },
    "body": { /* JSON Schema for requestBody, if present */ }
  },
  "required": [ /* names of required params + "body" if requestBody.required == true */ ]
}
```

**Step 1 — Resolve and categorize parameters**:
```
let mut properties: serde_json::Map<String, Value> = Map::new();
let mut required: Vec<String> = Vec::new();

for param_ref in parameters:
    let param = match resolve_parameter(param_ref, api):
        None =>
            tracing::trace!("could not resolve parameter ref — skipping")
            continue
        Some(p) => p

    let param_data = param.parameter_data_ref()  // openapiv3::ParameterData
    let schema_value = match &param_data.format:
        ParameterSchemaOrContent::Schema(schema_ref) =>
            ref_resolver::resolve_schema(schema_ref, api, &mut HashSet::new())
        ParameterSchemaOrContent::Content(_) =>
            tracing::trace!("parameter '{}' uses content-typed schema — emitting empty schema", param_data.name)
            json!({})

    match param:
        Parameter::Path { .. } =>
            properties.insert(param_data.name.clone(), schema_value)
            required.push(param_data.name.clone())  // path params are ALWAYS required
        Parameter::Query { .. } =>
            properties.insert(param_data.name.clone(), schema_value)
            if param_data.required:
                required.push(param_data.name.clone())
        Parameter::Header { .. } =>
            properties.insert(param_data.name.clone(), schema_value)
            if param_data.required:
                required.push(param_data.name.clone())
        Parameter::Cookie { .. } =>
            tracing::trace!("skipping cookie parameter '{}'", param_data.name)
            // do not add to properties
```

**Step 2 — Resolve requestBody**:
```
if let Some(body_ref) = request_body:
    match resolve_request_body(body_ref, api):
        None =>
            tracing::trace!("could not resolve requestBody ref — skipping")
        Some(body) =>
            // Prefer application/json; fall back to first content entry
            let schema_value = match body.content.get("application/json")
                                          .or_else(|| body.content.values().next()):
                None => json!({})
                Some(media) =>
                    match &media.schema:
                        None => json!({})
                        Some(schema_ref) =>
                            ref_resolver::resolve_schema(schema_ref, api, &mut HashSet::new())
            properties.insert("body".to_string(), schema_value)
            if body.required:
                required.push("body".to_string())
```

**Step 3 — Assemble output**:
```
let mut schema = json!({
    "type": "object",
    "properties": Value::Object(properties)
});
if !required.is_empty():
    schema["required"] = json!(required);
Ok(schema)
```

#### `resolve_parameter` algorithm
```
fn resolve_parameter(param_ref, api) -> Option<&Parameter>:
    match param_ref:
        ReferenceOr::Item(p) => Some(p)
        ReferenceOr::Reference { reference } =>
            let name = reference.strip_prefix("#/components/parameters/")?
            api.components.as_ref()?.parameters.get(name)?.as_item()
```

#### `resolve_request_body` algorithm
```
fn resolve_request_body(body_ref, api) -> Option<&RequestBody>:
    match body_ref:
        ReferenceOr::Item(b) => Some(b)
        ReferenceOr::Reference { reference } =>
            let name = reference.strip_prefix("#/components/requestBodies/")?
            api.components.as_ref()?.request_bodies.get(name)?.as_item()
```

#### Unit tests in `schema_builder.rs`
- `no_params_no_body_yields_empty_object_schema` — empty params, no body → `{"type":"object","properties":{}}`
- `no_params_no_body_has_no_required_key` — empty params, no body → no `"required"` key in output
- `path_param_string_is_in_properties_and_required` — path param `id: string` → `properties.id = {"type":"string"}`, `required = ["id"]`
- `query_param_integer_is_in_properties_not_required` — query param `limit: integer` (not required) → in properties, not in required
- `query_param_required_true_is_in_required` — query param with `required: true` → in required
- `header_param_is_in_properties_not_required_by_default` — header param → in properties, not in required
- `header_param_required_true_is_in_required` — header param with `required: true` → in required
- `cookie_param_is_not_in_properties` — cookie param → not in properties
- `request_body_json_schema_added_as_body_property` — POST with `application/json` body → `properties.body` present
- `request_body_required_true_adds_body_to_required` — `requestBody.required: true` → `"body"` in required
- `request_body_required_false_does_not_add_body_to_required` — `requestBody.required: false` → `"body"` not in required
- `request_body_no_content_yields_empty_body_schema` — requestBody with no content entries → `properties.body = {}`
- `mixed_path_query_body` — path param `id` + query param `limit` + required body → all three in properties, `id` and `body` in required, `limit` not in required
- `content_typed_param_yields_empty_schema` — parameter using `content:` format → `properties.<name> = {}`

---

### Phase 5: Fixture file `fixtures/allegro_sample.yaml`

**Status**: PENDING

**Agent**: implementer

**File**: `fixtures/allegro_sample.yaml`

A realistic Allegro-like OpenAPI 3.0 fixture. Must contain exactly **5 operations** (to make the integration test count deterministic):

1. `GET /sale/offers` — with `operationId: "getListingOffers"`, `x-allegro-scope` vendor extension, `$ref` parameter
2. `POST /sale/offers` — no operationId (exercises name derivation), `$ref` requestBody
3. `GET /sale/offers/{offerId}` — with `operationId: "getOffer"`, path param
4. `PUT /sale/offers/{offerId}` — with `operationId: "updateOffer"`, path param, inline requestBody with `$ref` schema
5. `GET /sale/categories` — no operationId, query param with dot in name (`parent.id`)

Components must include:
- `parameters.LimitParam` — query param, integer
- `requestBodies.OfferBody` — required, application/json, schema `$ref: '#/components/schemas/Offer'`
- `schemas.Offer` — object with `id: string`, `name: string`, `category: $ref Category`
- `schemas.Category` — object with `id: string`, `parent: $ref Category` (self-referential cycle)

The `Category.parent` self-reference exercises the cycle detection path.
The `x-allegro-scope` on `GET /sale/offers` exercises vendor extension handling.
The `POST /sale/offers` without operationId exercises name derivation → expected id: `"post_sale_offers"`.
The `GET /sale/categories` without operationId → expected id: `"get_sale_categories"`.

---

### Phase 6: Integration test `tests/tool_registry_integration.rs`

**Status**: PENDING

**Agent**: implementer

**File**: `tests/tool_registry_integration.rs`

Uses `allegro_mcp::tool_registry::ToolRegistry` and `allegro_mcp::schema::parse`.

#### Helper functions
```rust
fn load_fixture_api(name: &str) -> openapiv3::OpenAPI {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    let bytes = std::fs::read(&path).expect("fixture file not found");
    allegro_mcp::schema::parse::parse_bytes(&bytes).expect("fixture parse failed")
}

fn registry_from_fixture(name: &str) -> allegro_mcp::tool_registry::ToolRegistry {
    let api = load_fixture_api(name);
    allegro_mcp::tool_registry::ToolRegistry::from_openapi(&api)
        .expect("registry build failed")
}
```

#### Test cases — `fixtures/minimal_oas3.yaml` (2 operations)

- `minimal_fixture_yields_two_tools`:
  `registry_from_fixture("minimal_oas3.yaml").list_tools().len() == 2`

- `minimal_fixture_tool_count_equals_operation_count`:
  Load API, compute stats, build registry. `stats.operation_count == registry.list_tools().len()`

- `minimal_fixture_all_tool_names_have_allegro_prefix`:
  Every `tool.name` in `list_tools()` starts with `"allegro_"`

- `minimal_fixture_input_schemas_are_valid_json_schema_objects`:
  Every `tool.input_schema` is a JSON object (`Value::Object`) with a `"type"` key equal to `"string"` `"object"`

- `minimal_fixture_get_items_tool_has_limit_param`:
  Find the tool for `GET /items` (derived id: `"get_items"`). Its `input_schema["properties"]["limit"]` exists and equals `{"type":"integer"}`

- `minimal_fixture_get_items_id_tool_has_id_in_required`:
  Find the tool for `GET /items/{id}` (derived id: `"get_items_id"`). Its `input_schema["required"]` contains `"id"`

#### Test cases — `fixtures/allegro_sample.yaml` (5 operations)

- `allegro_sample_yields_five_tools`:
  `registry_from_fixture("allegro_sample.yaml").list_tools().len() == 5`

- `allegro_sample_get_listing_offers_has_correct_id`:
  `registry.get_tool("getlistingoffers")` returns `Some(tool)` where `tool.name == "allegro_getlistingoffers"`

- `allegro_sample_post_sale_offers_derives_name`:
  `registry.get_tool("post_sale_offers")` returns `Some(_)` (no operationId → derived name)

- `allegro_sample_get_sale_categories_derives_name`:
  `registry.get_tool("get_sale_categories")` returns `Some(_)`

- `allegro_sample_post_offer_has_body_property`:
  Tool for POST /sale/offers: `input_schema["properties"]["body"]` exists

- `allegro_sample_put_offer_body_is_required`:
  Tool for PUT /sale/offers/{offerId}: `input_schema["required"]` contains `"body"`

- `allegro_sample_put_offer_offer_id_is_required`:
  Tool for PUT /sale/offers/{offerId}: `input_schema["required"]` contains `"offerId"`

- `allegro_sample_circular_ref_does_not_panic`:
  Registry builds without panic. Tool for GET /sale/offers has `input_schema` that is a valid JSON object (Category.parent cycle was handled)

- `allegro_sample_get_tool_by_id_returns_correct_tool`:
  `registry.get_tool("getoffer")` returns `Some(tool)` where `tool.name == "allegro_getoffer"`

- `allegro_sample_get_tool_unknown_id_returns_none`:
  `registry.get_tool("does_not_exist_xyz")` returns `None`

- `allegro_sample_filter_tools_by_get_prefix`:
  `registry.filter_tools(|t| t.name.starts_with("allegro_get")).len() == 3`
  (GET /sale/offers, GET /sale/offers/{offerId}, GET /sale/categories)

- `allegro_sample_vendor_extensions_do_not_cause_error`:
  `ToolRegistry::from_openapi(&api)` returns `Ok(_)` — no error despite `x-allegro-scope`

- `allegro_sample_get_listing_offers_has_limit_param`:
  Tool `"getlistingoffers"` has `input_schema["properties"]["limit"]` (resolved from `$ref` parameter)

---

### Phase 7: Expose module in `src/lib.rs`

**Status**: PENDING

**Agent**: implementer

**File**: `src/lib.rs`

Current content:
```rust
//! allegro-mcp library — exposes internal modules for integration testing.
pub mod schema;
```

New content:
```rust
//! allegro-mcp library — exposes internal modules for integration testing.
pub mod schema;
pub mod tool_registry;
```

---

### Phase 8: Wire `tools list` subcommand into `src/main.rs`

**Status**: PENDING

**Agent**: implementer

**File**: `src/main.rs`

The existing pattern uses `Commands` enum with `SchemaAction` sub-enum. Follow the same pattern.

#### Changes required

1. Add `mod tool_registry;` at the top of `main.rs` (alongside `mod auth;` and `mod schema;`).

2. Extend `Commands` enum with a new variant:
```rust
Tools {
    #[command(subcommand)]
    action: ToolsAction,
},
```

3. Add new `ToolsAction` enum:
```rust
#[derive(Debug, Subcommand)]
enum ToolsAction {
    /// List all tools derived from the schema (prints count and first 5 names)
    List,
}
```

4. Add match arm in the `match cli.command` block:
```rust
Some(Commands::Tools {
    action: ToolsAction::List,
}) => {
    let (api, _raw) = schema::load(&source).await?;
    let registry = tool_registry::ToolRegistry::from_openapi(&api)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let tools = registry.list_tools();
    println!("Total tools: {}", tools.len());
    println!("First 5 tool names:");
    for tool in tools.iter().take(5) {
        println!("  {}", tool.name);
    }
}
```

5. The `source` variable construction (lines 140–146 in current `main.rs`) already exists before the `match cli.command` block and is reused unchanged.

6. The `--schema-url` and `--schema-file` flags are already `global = true` — no changes needed.

---

### Phase 9: Verification

**Status**: PENDING

**Agent**: implementer

Run in order and fix any issues before proceeding to the next:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

Manual smoke test:
```bash
cargo run -- --schema-file fixtures/minimal_oas3.yaml tools list
# Expected output:
# Total tools: 2
# First 5 tool names:
#   allegro_get_items
#   allegro_get_items_id
```

---

## Acceptance Criteria

| Criterion | How to verify |
|---|---|
| Every path in `minimal_oas3.yaml` yields exactly one tool | `minimal_fixture_yields_two_tools` integration test passes |
| Tool count equals operation count from `compute_stats` | `minimal_fixture_tool_count_equals_operation_count` integration test passes |
| All tool names have `allegro_` prefix | `minimal_fixture_all_tool_names_have_allegro_prefix` integration test passes |
| Input schemas are JSON Schema objects with `"type":"object"` | `minimal_fixture_input_schemas_are_valid_json_schema_objects` integration test passes |
| Path params appear in `"required"` | `minimal_fixture_get_items_id_tool_has_id_in_required` integration test passes |
| Query params are optional by default | `query_param_integer_is_in_properties_not_required` unit test passes |
| `requestBody` merges as `"body"` property | `allegro_sample_post_offer_has_body_property` integration test passes |
| Required `requestBody` appears in `"required"` | `allegro_sample_put_offer_body_is_required` integration test passes |
| Circular `$ref` does not panic or stack-overflow | `allegro_sample_circular_ref_does_not_panic` integration test passes |
| `x-` vendor extensions do not cause errors | `allegro_sample_vendor_extensions_do_not_cause_error` integration test passes |
| `get_tool(id)` returns correct tool | `allegro_sample_get_tool_by_id_returns_correct_tool` integration test passes |
| `filter_tools` returns correct subset | `allegro_sample_filter_tools_by_get_prefix` integration test passes |
| `tools list` subcommand prints correct count | `cargo run -- --schema-file fixtures/minimal_oas3.yaml tools list` prints `Total tools: 2` |
| `cargo clippy -D warnings` passes | Zero warnings |
| `cargo test --locked` passes | All tests green |

---

## Edge Cases

### 1. Path item is a `$ref`
**Risk**: `api.paths.paths` may contain `ReferenceOr::Reference` entries (path items that are `$ref`s).
**Handling**: In `build_tools`, match on `ReferenceOr::Reference { .. }` and skip with a TRACE log. Not counted as tools. Consistent with existing `schema::compute_stats` behaviour.

### 2. Duplicate `operationId` after sanitization
**Risk**: Two different operationIds could sanitize to the same string (e.g. `"get-offer"` and `"get_offer"` both become `"get_offer"`).
**Handling**: `deduplicate_id` helper in `builder.rs` tracks seen ids. Second occurrence gets `_2` suffix, third gets `_3`, etc. WARN log emitted.

### 3. Operation with no parameters and no requestBody
**Risk**: `build_input_schema` called with empty params and `None` body.
**Handling**: Returns `{"type":"object","properties":{}}` — valid empty JSON Schema. No `"required"` key emitted.

### 4. `requestBody` with non-JSON content type
**Risk**: Some Allegro endpoints use `multipart/form-data` or `application/x-www-form-urlencoded`.
**Handling**: `build_input_schema` prefers `"application/json"` but falls back to the first content entry via `.values().next()`. If no content entries exist, emits `{}` as the body schema.

### 5. Parameter with `content` instead of `schema`
**Risk**: OpenAPI 3.x allows parameters to use `content: { "application/json": { schema: ... } }` instead of `schema: ...` directly. `openapiv3` represents this as `ParameterSchemaOrContent::Content(...)`.
**Handling**: Emit `{}` as the parameter schema and log TRACE.

### 6. `$ref` to external file or URL
**Risk**: A `$ref` like `"./other.yaml#/Foo"` or `"https://example.com/schema.json"` cannot be resolved without I/O.
**Handling**: `lookup_ref` only handles `"#/components/schemas/{name}"` format. Any other format returns `None`, which causes `resolve_schema` to emit the `"unresolvable $ref"` sentinel. Log TRACE.

### 7. Very deep (non-cyclic) `$ref` chains
**Risk**: Allegro schemas are noted as "deep". A chain A→B→C→D→E→... could cause deep recursion.
**Handling**: The cycle detection `HashSet` prevents infinite recursion for actual cycles. For non-cyclic deep chains, Rust's default stack size handles hundreds of levels. A depth counter (max 64, emit sentinel beyond) is noted as a future improvement but not implemented in this phase.

### 8. `components` is `None`
**Risk**: `api.components` is `Option<Components>`. If `None`, any `$ref` lookup will fail.
**Handling**: `lookup_ref`, `resolve_parameter`, and `resolve_request_body` all use `api.components.as_ref()?` — they return `None` gracefully, triggering the sentinel or param-skip path.

### 9. `ToolDef.description` is empty string
**Risk**: An operation with neither `summary` nor `description` yields `description: ""`. MCP clients may display an empty tooltip.
**Handling**: Accepted. The MCP spec does not require a non-empty description. Log TRACE.

### 10. `openapiv3::PathItem::iter()` method availability
**Risk**: The `iter()` method on `PathItem` is used in the existing `schema::compute_stats` code, confirming it exists in `openapiv3 2.2.0`. It yields `(&str, &Operation)` for all HTTP methods that are `Some`.
**Handling**: Use `item.iter()` directly, matching the existing pattern in `src/schema/mod.rs` line 85.

### 11. `serde_json::Map` vs `serde_json::Value::Object`
**Risk**: `build_input_schema` builds a `serde_json::Map<String, Value>` for properties, then wraps it in `json!({...})`. The `json!` macro requires the map to be passed as a `Value::Object(map)`.
**Handling**: Use `serde_json::Value::Object(properties)` when constructing the final schema value, or use `serde_json::to_value(&properties)` to convert. The `json!` macro does not accept a `Map` directly in the `{...}` position — use `Value::Object(map)` explicitly.

### 12. `openapiv3::Parameter::parameter_data_ref()` method
**Risk**: `openapiv3::Parameter` is an enum (`Path`, `Query`, `Header`, `Cookie`). Each variant wraps a struct containing `parameter_data: ParameterData`. The `parameter_data_ref()` method (if it exists) returns `&ParameterData`. If this method does not exist in `openapiv3 2.2.0`, the implementer must match on each variant explicitly.
**Handling**: The implementer must verify whether `parameter_data_ref()` exists by checking the `openapiv3` crate docs or source. If absent, use a `match` on `Parameter` variants to extract `ParameterData` from each arm. The plan's pseudocode uses `parameter_data_ref()` as a convenience — the implementer should adapt if needed.
