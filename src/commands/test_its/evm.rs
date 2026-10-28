mod messages;
mod preflight;
mod types;

#[cfg(test)]
mod tests;

use std::time::Instant;

use alloy::primitives::{Bytes, U256};
use eyre::Result;

use super::{
    ConfigArgs, ConfigRoute, extract_token_deployed_event, generate_salt, relay::confirmed_block,
};
use crate::config::ChainsConfig;
use crate::evm::{
    ERC20, InterchainToken, InterchainTokenFactory, InterchainTokenService, broadcast_and_log,
};
use crate::ui;
use types::{Context, Token};

pub(super) async fn run(cfg: &ChainsConfig, route: ConfigRoute, args: ConfigArgs) -> Result<()> {
    let start = Instant::now();
    let ctx = preflight::prepare(cfg, route, args).await?;
    let token = deploy_token(&ctx).await?;
    transfer(&ctx, &token).await?;
    ui::success(&format!(
        "ITS test passed: {} → {} ({}). Axe relayed the token deployment and transfer.",
        ctx.route.source_axelar_id,
        ctx.route.destination_axelar_id,
        ui::format_elapsed(start)
    ));
    Ok(())
}

async fn deploy_token(ctx: &Context) -> Result<Token> {
    ui::section("Phase A: deploy test token locally and remotely");
    let spec = crate::types::EVM_LEGACY_SPEC;
    let salt = generate_salt();
    let factory = InterchainTokenFactory::new(ctx.factory, &ctx.source);
    let receipt = broadcast_and_log(
        factory
            .deployInterchainToken(
                salt,
                spec.name.into(),
                spec.symbol.into(),
                spec.decimals,
                crate::types::whole_tokens(1000, spec.decimals),
                ctx.signer,
            )
            .send()
            .await?,
        "local token tx",
    )
    .await?;
    let block = confirmed_block(&receipt)?;
    let (id, local) = extract_token_deployed_event(&receipt)?;
    eyre::ensure!(
        InterchainToken::new(local, &ctx.source)
            .interchainTokenId()
            .block(block.into())
            .call()
            .await?
            == id,
        "local token ID does not match deployment event"
    );
    ui::kv("token ID", &id.to_string());
    ui::address("source token", &local.to_string());
    let remote_receipt = broadcast_and_log(
        factory
            .deployRemoteInterchainToken(
                salt,
                ctx.route.destination_axelar_id.to_string(),
                ctx.gas_value,
            )
            .value(ctx.gas_value)
            .send()
            .await?,
        "remote token request",
    )
    .await?;
    let block = messages::relay(ctx, &remote_receipt).await?;
    let remote = InterchainTokenService::new(ctx.route.destination_its, &ctx.destination)
        .registeredTokenAddress(id)
        .block(block.into())
        .call()
        .await?;
    eyre::ensure!(
        InterchainToken::new(remote, &ctx.destination)
            .interchainTokenId()
            .block(block.into())
            .call()
            .await?
            == id,
        "destination token ID does not match source token"
    );
    ui::address("destination token", &remote.to_string());
    Ok(Token { id, local, remote })
}

async fn transfer(ctx: &Context, token: &Token) -> Result<()> {
    ui::section("Phase B: transfer test tokens and relay both legs");
    ui::kv("token ID", &token.id.to_string());
    let before = ERC20::new(token.remote, &ctx.destination)
        .balanceOf(ctx.signer)
        .call()
        .await?;
    let receipt = broadcast_and_log(
        InterchainToken::new(token.local, &ctx.source)
            .interchainTransfer(
                ctx.route.destination_axelar_id.to_string(),
                Bytes::copy_from_slice(ctx.signer.as_slice()),
                ctx.amount,
                Bytes::new(),
            )
            .value(ctx.gas_value)
            .send()
            .await?,
        "transfer tx",
    )
    .await?;
    let block = messages::relay(ctx, &receipt).await?;
    let after = ERC20::new(token.remote, &ctx.destination)
        .balanceOf(ctx.signer)
        .block(block.into())
        .call()
        .await?;
    verify_balance(before, after, ctx.amount)?;
    ui::success(&format!(
        "Destination balance increased by exactly {} token base units",
        ctx.amount
    ));
    Ok(())
}

fn verify_balance(before: U256, after: U256, amount: U256) -> Result<()> {
    let expected = before
        .checked_add(amount)
        .ok_or_else(|| eyre::eyre!("balance overflow"))?;
    eyre::ensure!(
        after == expected,
        "destination balance {after} does not match expected {expected} (before {before}, amount {amount})"
    );
    Ok(())
}
