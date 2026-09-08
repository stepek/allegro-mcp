use allegro_mcp::schema::{self, SchemaSource};
use allegro_mcp::tool_registry::ToolRegistry;
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
        assert!(
            tool.id.starts_with("allegro_"),
            "id '{}' must start with allegro_",
            tool.id
        );
    }
}

#[tokio::test]
async fn minimal_fixture_input_schemas_are_objects() {
    let source = SchemaSource::File(fixture_path("minimal_oas3.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    for tool in registry.list_tools() {
        assert_eq!(
            tool.input_schema["type"], "object",
            "tool '{}' input_schema must have type=object",
            tool.id
        );
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
    assert_eq!(get_tools.len(), 2); // minimal_oas3.yaml has 2 GET operations (no operationId, names derived from path)
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
    let ids: Vec<&str> = registry
        .list_tools()
        .iter()
        .map(|t| t.id.as_str())
        .collect();
    assert!(
        ids.contains(&"allegro_getlistingoffers"),
        "expected allegro_getlistingoffers, got: {:?}",
        ids
    );
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
    assert!(
        create.input_schema["properties"]["body"].is_object(),
        "POST tool must have a 'body' property in input_schema"
    );
}

#[tokio::test]
async fn sample_fixture_path_params_are_required() {
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    let get_offer = registry.get_tool("allegro_getoffer").unwrap();
    let required = get_offer.input_schema["required"].as_array().unwrap();
    let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        required_names.contains(&"offerId"),
        "path param offerId must be required, got: {:?}",
        required_names
    );
}

#[tokio::test]
async fn sample_fixture_cyclic_ref_does_not_panic() {
    // Category has a self-referential $ref — must not stack overflow or panic
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    // Just verify it completes without panic
    assert_eq!(registry.len(), 5);

    // Verify the cyclic Category.parent is replaced with the sentinel
    let create = registry.get_tool("allegro_createoffer").unwrap();
    let body = &create.input_schema["properties"]["body"];
    // The body schema is the resolved OfferRequest — category.parent should be the sentinel
    // (exact path depends on how deep the resolution goes, but it must not be a $ref)
    let category_parent = &body["properties"]["category"]["properties"]["parent"];
    assert!(
        category_parent.get("$ref").is_none(),
        "cyclic $ref must be replaced with sentinel, not left as $ref: {:?}",
        category_parent
    );
}

#[tokio::test]
async fn sample_fixture_descriptions_are_non_empty() {
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    for tool in registry.list_tools() {
        assert!(
            !tool.description.is_empty(),
            "tool '{}' must have a non-empty description",
            tool.id
        );
    }
}

// ── name sanitization (unit-style via builder) ────────────────────────────────

#[test]
fn sanitize_name_lowercases_and_replaces_special_chars() {
    use allegro_mcp::tool_registry::builder::sanitize_name;
    assert_eq!(sanitize_name("getListingOffers"), "getlistingoffers");
    assert_eq!(sanitize_name("get-listing-offers"), "get_listing_offers");
    assert_eq!(sanitize_name("GET_OFFERS_V2"), "get_offers_v2");
    assert_eq!(sanitize_name("__leading__"), "leading");
}

#[test]
fn derive_name_from_path_produces_valid_name() {
    use allegro_mcp::tool_registry::builder::derive_name_from_path;
    assert_eq!(
        derive_name_from_path("get", "/sale/offers"),
        "get_sale_offers"
    );
    assert_eq!(
        derive_name_from_path("get", "/sale/offers/{offerId}"),
        "get_sale_offers_offerid"
    );
    assert_eq!(derive_name_from_path("post", "/"), "post");
}

// ── empty OpenAPI document ────────────────────────────────────────────────────

#[test]
fn empty_openapi_yields_empty_registry() {
    use openapiv3::OpenAPI;
    let api: OpenAPI =
        serde_yaml::from_str("openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths: {}\n")
            .unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    assert_eq!(registry.len(), 0);
    assert!(registry.is_empty());
}

// ── minimal_oas3.yaml: method and path fields ─────────────────────────────────

#[tokio::test]
async fn minimal_fixture_tools_have_correct_method_and_path() {
    let source = SchemaSource::File(fixture_path("minimal_oas3.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();

    // Both tools must have method="get" (minimal_oas3.yaml has only GET ops)
    for tool in registry.list_tools() {
        assert_eq!(
            tool.method, "get",
            "tool '{}' must have method='get'",
            tool.id
        );
    }

    // Paths must be the raw OAS path strings
    let paths: Vec<&str> = registry.list_tools().iter().map(|t| t.path.as_str()).collect();
    assert!(
        paths.contains(&"/items"),
        "expected /items path, got: {paths:?}"
    );
    assert!(
        paths.contains(&"/items/{id}"),
        "expected /items/{{id}} path, got: {paths:?}"
    );
}

// ── allegro_sample.yaml: DELETE tool has no body property ────────────────────

#[tokio::test]
async fn sample_fixture_delete_tool_has_no_body_property() {
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    let delete = registry.get_tool("allegro_deleteoffer").unwrap();
    assert!(
        delete.input_schema["properties"].get("body").is_none(),
        "DELETE tool must not have a 'body' property (no requestBody), got: {:?}",
        delete.input_schema["properties"]
    );
}

// ── allegro_sample.yaml: PUT tool has offerId AND body in required ────────────

#[tokio::test]
async fn sample_fixture_put_tool_has_offer_id_and_body_required() {
    let source = SchemaSource::File(fixture_path("allegro_sample.yaml"));
    let (api, _) = schema::load(&source).await.unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    let update = registry.get_tool("allegro_updateoffer").unwrap();
    let required: Vec<&str> = update.input_schema["required"]
        .as_array()
        .expect("updateOffer must have a required array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        required.contains(&"offerId"),
        "'offerId' must be in required for PUT tool, got: {required:?}"
    );
    assert!(
        required.contains(&"body"),
        "'body' must be in required for PUT tool (required requestBody), got: {required:?}"
    );
}

// ── deduplication: two ops with the same sanitized name get _2 suffix ─────────

#[test]
fn duplicate_operation_ids_get_deduplicated_suffix() {
    use openapiv3::OpenAPI;
    // Two operations whose operationIds sanitize to the same string.
    // "getOffer" and "GetOffer" both → "getoffer" → second gets "allegro_getoffer_2"
    let api: OpenAPI = serde_yaml::from_str(concat!(
        "openapi: \"3.0.3\"\n",
        "info:\n  title: t\n  version: v\n",
        "paths:\n",
        "  /a:\n",
        "    get:\n",
        "      operationId: getOffer\n",
        "      summary: First\n",
        "      responses:\n",
        "        \"200\":\n",
        "          description: OK\n",
        "  /b:\n",
        "    get:\n",
        "      operationId: GetOffer\n",
        "      summary: Second\n",
        "      responses:\n",
        "        \"200\":\n",
        "          description: OK\n",
    ))
    .unwrap();
    let registry = ToolRegistry::from_openapi(&api).unwrap();
    assert_eq!(registry.len(), 2);
    let ids: Vec<&str> = registry
        .list_tools()
        .iter()
        .map(|t| t.id.as_str())
        .collect();
    assert!(
        ids.contains(&"allegro_getoffer"),
        "first tool must get the base id, got: {ids:?}"
    );
    assert!(
        ids.contains(&"allegro_getoffer_2"),
        "second tool must get the _2 suffix, got: {ids:?}"
    );
}
