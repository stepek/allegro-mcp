use crate::schema::SchemaError;
use reqwest::Client;
use std::time::Duration;

const MAX_RETRIES: u32 = 1;

/// Fetch raw bytes from a URL using the default (ToS-compliant UA) schema
/// client — a backward-compat wrapper over [`fetch_bytes_with_client`] for
/// existing callers and tests. The 10 s timeout lives in
/// [`crate::http::build_schema_client`]; only retry semantics live here.
///
/// Only transport/timeout errors trigger a retry; HTTP 4xx/5xx do not.
#[allow(dead_code)]
pub async fn fetch_bytes(url: &str) -> Result<Vec<u8>, SchemaError> {
    fetch_bytes_with_client(&crate::http::default_schema_client(), url).await
}

/// Like [`fetch_bytes`] but takes an explicit HTTP client so production
/// paths (built in `main` via [`crate::http::build_schema_client`]) can send
/// a config-driven User-Agent on schema downloads from Allegro hosts too.
pub async fn fetch_bytes_with_client(http: &Client, url: &str) -> Result<Vec<u8>, SchemaError> {
    let mut last_err = None;
    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match http.get(url).send().await {
            Ok(resp) => {
                resp.error_for_status_ref()?;
                return Ok(resp.bytes().await?.to_vec());
            }
            Err(e) if e.is_timeout() || e.is_connect() => {
                tracing::warn!("fetch attempt {} failed: {}", attempt + 1, e);
                last_err = Some(e);
            }
            Err(e) => return Err(SchemaError::Fetch(e)),
        }
    }
    Err(SchemaError::Fetch(last_err.unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Every mock requires the default User-Agent: a UA-less schema client
    // fails these tests, which is exactly the ToS guarantee under test.
    const UA: &str = crate::http::DEFAULT_USER_AGENT;

    #[tokio::test]
    async fn fetch_bytes_success_returns_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(header("User-Agent", UA))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello world"))
            .mount(&server)
            .await;
        let result = fetch_bytes(&server.uri()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn fetch_bytes_404_returns_fetch_error_immediately() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(header("User-Agent", UA))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let result = fetch_bytes(&server.uri()).await;
        assert!(matches!(result, Err(SchemaError::Fetch(_))));
        // 404 should NOT retry — verify only 1 request was made
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fetch_bytes_500_returns_fetch_error_immediately() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(header("User-Agent", UA))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let result = fetch_bytes(&server.uri()).await;
        assert!(matches!(result, Err(SchemaError::Fetch(_))));
        // 500 should NOT retry — verify only 1 request was made
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// A custom-UA client (config-driven path) must send that User-Agent on
    /// the schema download — the mock only matches when it does.
    #[tokio::test]
    async fn fetch_bytes_with_client_sends_custom_user_agent() {
        let server = MockServer::start().await;
        let custom_ua = "MyApp/2.0.0 (+https://example.com/app)";
        Mock::given(method("GET"))
            .and(path("/swagger.yaml"))
            .and(header("User-Agent", custom_ua))
            .and(header("Accept-Language", "pl-PL"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"schema"))
            .mount(&server)
            .await;
        let client = crate::http::build_client(custom_ua, crate::http::DEFAULT_ACCEPT_LANGUAGE)
            .expect("valid client");
        let result =
            fetch_bytes_with_client(&client, &format!("{}/swagger.yaml", server.uri())).await;
        assert_eq!(
            result.expect("custom-UA fetch must match the mock"),
            b"schema"
        );
    }
}
