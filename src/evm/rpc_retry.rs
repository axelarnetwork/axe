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
    build_client(url, false)
}

/// Also retry an unavailable RPC block, without changing the requested block.
/// Opt-in for governance. Existing deployment clients are unchanged.
pub fn client_with_block_retries(url: &str) -> Result<RpcClient> {
    build_client(url, true)
}

fn unavailable_block(error: &TransportError) -> bool {
    error.as_error_resp().is_some_and(|error| {
        error.code == -32000
            && error.data.is_none()
            && (error.message.eq_ignore_ascii_case("unknown block number")
                || error.message.ends_with(
                    "Error occurred during transaction simulation: Unknown block number",
                ))
    })
}

fn build_client(url: &str, retry_blocks: bool) -> Result<RpcClient> {
    let policy = RateLimitRetryPolicy::default().or(move |error| {
        let retry = retryable(error) || (retry_blocks && unavailable_block(error));
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
