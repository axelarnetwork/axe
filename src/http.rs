//! Shared outbound HTTP client with bounded timeouts.
//!
//! Every ad-hoc `reqwest::Client::new()` / `reqwest::get(..)` ships with NO
//! timeout, so a single stalled RPC connection hangs its route until the CI
//! job timeout kills it (observed: 30 min on xrpl -> xrpl-evm in cron run
//! 32638549694). All outbound HTTP goes through this client instead, and
//! `clippy.toml` disallows the untimed constructors.

use std::sync::LazyLock;
use std::time::Duration;

/// Total per-request deadline. Generous for every JSON-RPC/REST call axe
/// makes - retries and endpoint fallback layer on top of this at call sites.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on TCP+TLS establishment, so a black-holed endpoint fails fast
/// instead of consuming the whole request deadline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        // Builder failure means the TLS backend could not initialize, and
        // nothing network-related works then, so surface it at first use.
        .unwrap_or_default()
});

/// The process-wide timed HTTP client. Cheap to clone (shared pool).
pub fn client() -> &'static reqwest::Client {
    &CLIENT
}

/// Retry read-only JSON requests after transport failures, throttling or 5xx.
/// Never use this for broadcasts: an interrupted response can hide a successful write.
pub async fn get_json<T: serde::de::DeserializeOwned>(url: reqwest::Url) -> eyre::Result<T> {
    for attempt in 0..5 {
        let result = async {
            client()
                .get(url.clone())
                .send()
                .await?
                .error_for_status()?
                .json::<T>()
                .await
        }
        .await;
        match result {
            Ok(value) => return Ok(value),
            Err(error) => {
                let retryable = error.status().is_none_or(|status| {
                    status.is_server_error() || status.as_u16() == 429 || status.as_u16() == 408
                });
                if !retryable || attempt == 4 {
                    return Err(error.without_url().into());
                }
                if attempt == 0 {
                    crate::ui::warn(
                        "read request interrupted; retrying without resubmitting transactions",
                    );
                }
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
        }
    }
    Err(eyre::eyre!("read request exhausted retries"))
}

/// Test-only probe: does `url` still resolve and answer HTTP at all?
/// A 404/410 from the host means the endpoint path was retired (the
/// publicnode-renames-a-subdomain class) - any other status proves the
/// service exists (JSON-RPC endpoints commonly answer GET with 405).
#[cfg(test)]
pub(crate) async fn probe_endpoint_alive(url: &str) -> Result<(), String> {
    match client().get(url).send().await {
        Ok(resp) if resp.status() == 404 || resp.status() == 410 => {
            Err(format!("{url}: HTTP {} (endpoint retired?)", resp.status()))
        }
        Ok(_) => Ok(()),
        Err(e) => Err(format!("{url}: {e}")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A dropped response models the connection reset that interrupted proposal 645.
    pub async fn serve(
        responses: Vec<Option<String>>,
    ) -> (reqwest::Url, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let size = stream.read(&mut chunk).await.unwrap();
                    request.extend_from_slice(&chunk[..size]);
                    if size == 0 || request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&request).to_string());
                if let Some(response) = response {
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            }
            requests
        });
        (url, task)
    }

    pub fn response(status: &str, body: &str) -> Option<String> {
        Some(format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
    }

    #[tokio::test]
    async fn retries_dropped_connection_and_unavailable_server() {
        let (url, task) = serve(vec![
            None,
            response("503 Unavailable", "{}"),
            response("200 OK", "{\"ok\":true}"),
        ])
        .await;
        let value: serde_json::Value = get_json(url).await.unwrap();
        assert_eq!(value["ok"], true);
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|request| request.starts_with("GET ")));
    }

    #[tokio::test]
    async fn terminal_http_errors_are_not_retried() {
        let (url, task) = serve(vec![response("403 Forbidden", "{}")]).await;
        let result = get_json::<serde_json::Value>(url).await;
        assert!(result.is_err());
        assert_eq!(task.await.unwrap().len(), 1);
    }
}
