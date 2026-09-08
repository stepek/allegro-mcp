use allegro_mcp::schema::{self, SchemaError, SchemaSource};
use std::path::PathBuf;

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
    // Write a fake OAS 2.x doc to a temp file
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swagger2.yaml");
    std::fs::write(
        &path,
        b"swagger: \"2.0\"\ninfo:\n  title: t\n  version: v\npaths: {}\n",
    )
    .unwrap();
    let source = SchemaSource::File(path);
    let result = schema::load(&source).await;
    assert!(result.is_err());
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
    let source = SchemaSource::File(PathBuf::from("/tmp/allegro_mcp_does_not_exist_xyz.yaml"));
    let err = schema::load(&source)
        .await
        .expect_err("loading a missing file should fail");
    assert!(
        matches!(err, SchemaError::Io(_)),
        "expected SchemaError::Io for missing file, got: {err:?}"
    );
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

    assert_eq!(stats.path_count, 0, "path_count should be 0 for empty paths");
    assert_eq!(
        stats.operation_count, 0,
        "operation_count should be 0 for empty paths"
    );
    assert_eq!(
        stats.parameter_count, 0,
        "parameter_count should be 0 for empty paths"
    );
    assert_eq!(stats.sha256.len(), 64, "sha256 should always be 64 hex chars");
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
