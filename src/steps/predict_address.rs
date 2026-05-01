use alloy::providers::{Provider, ProviderBuilder};
use eyre::Result;

use crate::commands::deploy::DeployContext;
use crate::evm::compute_create_address;
use crate::ui;

pub async fn run(ctx: &mut DeployContext) -> Result<()> {
    let gateway_deployer = ctx
        .state
        .gateway_deployer
        .ok_or_else(|| eyre::eyre!("no gatewayDeployer in state. Run init first"))?;

    let provider = ProviderBuilder::new().connect_http(ctx.rpc_url.parse()?);
    let nonce = provider.get_transaction_count(gateway_deployer).await?;
    let proxy_nonce = nonce + 1; // +1 for implementation tx
    let predicted = compute_create_address(gateway_deployer, proxy_nonce);
    ui::address("gateway deployer", &format!("{gateway_deployer}"));
    ui::kv("current nonce", &nonce.to_string());
    ui::kv("proxy nonce (impl+1)", &proxy_nonce.to_string());
    ui::address("predicted gateway proxy", &format!("{predicted}"));

    ctx.state.predicted_gateway_address = Some(predicted);

    Ok(())
}
