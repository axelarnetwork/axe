use alloy::rpc::client::RpcClient;
use alloy::transports::{
    TransportError,
    layers::{RateLimitRetryPolicy, RetryBackoffLayer, RetryPolicy},
};
use eyre::Result;

use crate::ui;

#[cfg(test)]
mod tests;

fn retryable(error: &TransportError) -> bool {
    RateLimitRetryPolicy::default().should_retry(error)
        || error
            .as_transport_err()
            .and_then(|kind| kind.as_http_error())
            .is_some_and(|error| matches!(error.status, 502 | 504))
        || error
            .as_transport_err()
            .and_then(|kind| kind.as_custom())
            .and_then(|source| source.downcast_ref::<reqwest::Error>())
            .is_some_and(|error| {
                error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
            })
}

/// Retry the same serialized RPC request, including identical signed tx bytes.
/// Never retry a contract call builder that could select a fresh nonce.
pub fn client(url: &str) -> Result<RpcClient> {
    let policy = RateLimitRetryPolicy::default().or(|error| {
        let retry = retryable(error);
        if retry {
            ui::warn(&format!(
                "temporary EVM RPC failure, retrying the same request: {}",
                ui::scrub_urls(&error.to_string())
            ));
        }
        retry
    });
    let layer = RetryBackoffLayer::new_with_policy(3, 1_000, u64::MAX, policy);
    Ok(RpcClient::builder()
        .layer(layer)
        .http_with_client(crate::http::client().clone(), url.parse()?))
}
