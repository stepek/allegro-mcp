use allegro_mcp::schema::{self, SchemaError, SchemaSource};
use std::path::PathBuf;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("minimal_oas3.yaml")
}

#[tokio::test]
async fn load_from_file_returns_parsed_api() {
    let source = SchemaSource::File(fixture_path());
    let (api, raw) = schema::load(&source).await.expect("load failed");
    assert!(api.openapi.starts_with("3."));
    assert!(!raw.is_empty());
}

#[tokio::test]
async fn compute_stats_counts_correctly() {
    let source = SchemaSource::File(fixture_path());
    let (api, raw) = schema::load(&source).await.expect("load failed");
    let stats = schema::compute_stats(&api, &raw);
    assert_eq!(stats.path_count, 2);
    assert_eq!(stats.operation_count, 2);
    assert_eq!(stats.parameter_count, 2); // /items GET has 1 param (limit), /items/{id} GET has 1 param (id) = 2 total
    assert_eq!(stats.sha256.len(), 64); // hex sha256
}

#[tokio::test]
async fn not_oas3_returns_error() {
    // A doc with openapi: "2.0" triggers NotOas3 (the openapi field is present but non-3.x)
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oas2.yaml");
    std::fs::write(
        &path,
        b"openapi: \"2.0\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
    )
    .unwrap();
    let source = SchemaSource::File(path);
    let result = schema::load(&source).await;
    assert!(matches!(result, Err(SchemaError::NotOas3 { .. })));
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn cache_write_and_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: current_thread + serial ensures single-threaded, no concurrent env access
    unsafe {
        std::env::set_var("ALLEGRO_MCP_CACHE_DIR", dir.path());
    }
    let bytes = std::fs::read(fixture_path()).unwrap();
    schema::cache::write_cache(&bytes).expect("write_cache failed");
    let read_back = schema::cache::read_cache().expect("read_cache failed");
    assert_eq!(bytes, read_back);
    let sha256_path = dir.path().join("swagger.yaml.sha256");
    assert!(sha256_path.exists(), "sha256 sidecar file should exist");
    let sha256_content = std::fs::read_to_string(&sha256_path).unwrap();
    assert_eq!(sha256_content.len(), 64, "sha256 should be 64 hex chars");
    unsafe {
        std::env::remove_var("ALLEGRO_MCP_CACHE_DIR");
    }
}

// ── SchemaSource ─────────────────────────────────────────────────────────────

#[test]
fn schema_source_default_is_allegro_url() {
    let source = SchemaSource::default();
    match source {
        SchemaSource::Url(url) => {
            assert_eq!(
                url, "https://developer.allegro.pl/swagger.yaml",
                "default URL must point to the official Allegro swagger endpoint"
            );
        }
        other => panic!("expected SchemaSource::Url, got: {other:?}"),
    }
}

// ── load() error paths ───────────────────────────────────────────────────────

#[tokio::test]
async fn load_non_existent_file_returns_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does_not_exist.yaml");
    // Verify the path doesn't exist
    assert!(!path.exists());
    let source = SchemaSource::File(path);
    let result = schema::load(&source).await;
    assert!(matches!(result, Err(SchemaError::Io(_))));
}

// ── compute_stats edge cases ─────────────────────────────────────────────────

#[tokio::test]
async fn compute_stats_zero_paths() {
    // Write a minimal OAS 3.x doc with no paths to a temp file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty_paths.yaml");
    std::fs::write(
        &path,
        b"openapi: \"3.0.3\"\ninfo:\n  title: Empty\n  version: \"0.1.0\"\npaths: {}\n",
    )
    .unwrap();

    let source = SchemaSource::File(path);
    let (api, raw) = schema::load(&source).await.expect("load failed");
    let stats = schema::compute_stats(&api, &raw);

    assert_eq!(
        stats.path_count, 0,
        "path_count should be 0 for empty paths"
    );
    assert_eq!(
        stats.operation_count, 0,
        "operation_count should be 0 for empty paths"
    );
    assert_eq!(
        stats.parameter_count, 0,
        "parameter_count should be 0 for empty paths"
    );
    assert_eq!(
        stats.sha256.len(),
        64,
        "sha256 should always be 64 hex chars"
    );
}

#[tokio::test]
async fn compute_stats_sha256_is_deterministic() {
    // Same bytes → same sha256 on every call.
    let source = SchemaSource::File(fixture_path());
    let (api, raw) = schema::load(&source).await.expect("load failed");
    let stats1 = schema::compute_stats(&api, &raw);
    let stats2 = schema::compute_stats(&api, &raw);
    assert_eq!(
        stats1.sha256, stats2.sha256,
        "sha256 must be deterministic for identical input"
    );
}

#[tokio::test]
async fn compute_stats_sha256_changes_with_different_content() {
    // Two different YAML files must produce different sha256 values.
    let dir = tempfile::tempdir().unwrap();

    let path_a = dir.path().join("a.yaml");
    std::fs::write(
        &path_a,
        b"openapi: \"3.0.3\"\ninfo:\n  title: A\n  version: \"1\"\npaths: {}\n",
    )
    .unwrap();

    let path_b = dir.path().join("b.yaml");
    std::fs::write(
        &path_b,
        b"openapi: \"3.0.3\"\ninfo:\n  title: B\n  version: \"2\"\npaths: {}\n",
    )
    .unwrap();

    let (api_a, raw_a) = schema::load(&SchemaSource::File(path_a))
        .await
        .expect("load a");
    let (api_b, raw_b) = schema::load(&SchemaSource::File(path_b))
        .await
        .expect("load b");

    let stats_a = schema::compute_stats(&api_a, &raw_a);
    let stats_b = schema::compute_stats(&api_b, &raw_b);

    assert_ne!(
        stats_a.sha256, stats_b.sha256,
        "different file contents must produce different sha256 values"
    );
}

// ── load() with SchemaSource::File ───────────────────────────────────────────

#[tokio::test]
async fn load_file_returns_raw_bytes_matching_disk() {
    // The raw bytes returned by load() must exactly match what is on disk.
    let path = fixture_path();
    let on_disk = std::fs::read(&path).unwrap();
    let source = SchemaSource::File(path);
    let (_api, raw) = schema::load(&source).await.expect("load failed");
    assert_eq!(
        raw, on_disk,
        "raw bytes from load() must match the file on disk"
    );
}

#[tokio::test]
async fn load_non_3x_openapi_field_returns_not_oas3_error() {
    // A doc with `openapi: "2.0"` (has the key, but wrong version) exercises
    // the NotOas3 guard inside parse_bytes via load().
    // Note: a real Swagger 2.x doc uses `swagger:` not `openapi:`, which causes
    // a Yaml deserialization error (missing field) rather than NotOas3.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fake_oas2.yaml");
    std::fs::write(
        &path,
        b"openapi: \"2.0\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
    )
    .unwrap();
    let err = schema::load(&SchemaSource::File(path))
        .await
        .expect_err("non-3.x openapi field should be rejected");
    assert!(
        matches!(err, SchemaError::NotOas3 { .. }),
        "expected SchemaError::NotOas3, got: {err:?}"
    );
}

// ── load() URL path ──────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn load_url_success_writes_cache() {
    let dir = tempfile::tempdir().unwrap();
    let fixture_bytes = std::fs::read(fixture_path()).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture_bytes.clone()))
        .mount(&server)
        .await;
    // SAFETY: serial ensures single-threaded env access
    unsafe {
        std::env::set_var("ALLEGRO_MCP_CACHE_DIR", dir.path());
    }
    let source = SchemaSource::Url(server.uri());
    let result = schema::load(&source).await;
    unsafe {
        std::env::remove_var("ALLEGRO_MCP_CACHE_DIR");
    }
    assert!(result.is_ok());
    // Cache file should have been written
    assert!(dir.path().join("swagger.yaml").exists());
}

#[tokio::test]
#[serial_test::serial]
async fn load_url_fetch_fails_falls_back_to_cache() {
    let dir = tempfile::tempdir().unwrap();
    let fixture_bytes = std::fs::read(fixture_path()).unwrap();
    // Pre-populate cache
    // SAFETY: serial ensures single-threaded env access
    unsafe {
        std::env::set_var("ALLEGRO_MCP_CACHE_DIR", dir.path());
    }
    schema::cache::write_cache(&fixture_bytes).unwrap();
    // Use a URL that will fail (invalid host — nothing listening on port 1)
    let source = SchemaSource::Url("http://127.0.0.1:1".to_string());
    let result = schema::load(&source).await;
    unsafe {
        std::env::remove_var("ALLEGRO_MCP_CACHE_DIR");
    }
    assert!(result.is_ok(), "should fall back to cache on fetch failure");
}

#[tokio::test]
#[serial_test::serial]
async fn load_url_fetch_fails_no_cache_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: serial ensures single-threaded env access
    unsafe {
        std::env::set_var("ALLEGRO_MCP_CACHE_DIR", dir.path());
    }
    // Use a URL that will fail (invalid host), no cache pre-populated
    let source = SchemaSource::Url("http://127.0.0.1:1".to_string());
    let result = schema::load(&source).await;
    unsafe {
        std::env::remove_var("ALLEGRO_MCP_CACHE_DIR");
    }
    assert!(
        result.is_err(),
        "should return error when fetch fails and no cache"
    );
}

// ── compute_stats $ref path items ────────────────────────────────────────────

#[tokio::test]
async fn compute_stats_skips_reference_path_items() {
    // Path items that are $refs are skipped in compute_stats (counted as paths but not operations)
    // This is intentional — we only count inline operations
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ref_paths.yaml");
    std::fs::write(
        &path,
        b"openapi: \"3.0.3\"\ninfo:\n  title: t\n  version: v\npaths:\n  /ref-path:\n    $ref: \"#/components/pathItems/myPath\"\ncomponents: {}\n",
    )
    .unwrap();
    let source = SchemaSource::File(path);
    let (api, raw) = schema::load(&source).await.expect("load failed");
    let stats = schema::compute_stats(&api, &raw);
    // The path is counted but the $ref operation is not resolved/counted
    assert_eq!(stats.path_count, 1);
    assert_eq!(stats.operation_count, 0); // $ref path items are skipped
}
