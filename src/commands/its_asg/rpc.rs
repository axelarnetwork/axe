use std::future::Future;

use eyre::{Result, WrapErr, ensure};

use crate::retry;

fn unavailable(error: &eyre::Report) -> bool {
    let message = format!("{error:#}").to_lowercase();
    message.contains("historical state") && message.contains("not available")
}

pub(super) fn url(configured: Option<&str>, selected: Option<&str>) -> Result<String> {
    let value = selected
        .or(configured)
        .ok_or_else(|| eyre::eyre!("missing EVM rpc"))?;
    let parsed = reqwest::Url::parse(value).map_err(|_| eyre::eyre!("invalid ASG RPC URL"))?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some(),
        "ASG RPC URL must use HTTP or HTTPS"
    );
    Ok(value.to_owned())
}

pub(super) async fn finalized_read<F, Fut, T>(label: &str, operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let result = retry::retry_async(label, 3, unavailable, operation).await;
    match result {
        Err(error) if unavailable(&error) => Err(error).wrap_err(
            "RPC cannot serve finalized historical state after retries. Use --rpc-url or ASG_RPC_URL with a provider that serves finalized state. Finality checks have not been relaxed",
        ),
        other => other,
    }
}
