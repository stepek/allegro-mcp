use crate::schema::SchemaError;
use reqwest::Client;
use std::time::Duration;

const TIMEOUT_SECS: u64 = 10;
const MAX_RETRIES: u32 = 1;

/// Fetch raw bytes from a URL with a 10s timeout and 1 retry.
/// Only transport/timeout errors trigger a retry; HTTP 4xx/5xx do not.
pub async fn fetch_bytes(url: &str) -> Result<Vec<u8>, SchemaError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()?;

    let mut last_err = None;
    for attempt in 0..=MAX_RETRIES {
        match client.get(url).send().await {
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
