use crate::schema::SchemaError;
use reqwest::Client;
use std::sync::LazyLock;
use std::time::Duration;

const TIMEOUT_SECS: u64 = 10;
const MAX_RETRIES: u32 = 1;

static HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .expect("failed to build HTTP client")
});

/// Fetch raw bytes from a URL with a 10s timeout and 1 retry.
/// Only transport/timeout errors trigger a retry; HTTP 4xx/5xx do not.
pub async fn fetch_bytes(url: &str) -> Result<Vec<u8>, SchemaError> {
    let mut last_err = None;
    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match HTTP_CLIENT.get(url).send().await {
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
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fetch_bytes_success_returns_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
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
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let result = fetch_bytes(&server.uri()).await;
        assert!(matches!(result, Err(SchemaError::Fetch(_))));
        // 500 should NOT retry — verify only 1 request was made
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
