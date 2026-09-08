use allegro_mcp::schema::{self, SchemaSource};
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
