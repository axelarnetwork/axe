use alloy::{primitives::U256, providers::Provider, signers::local::PrivateKeySigner};
use eyre::Result;

use super::super::{ConfigArgs, ConfigRoute, prepare_legacy_relay};
use super::types::Context;
use crate::config::{ChainContract, ChainsConfig};
use crate::evm::{InterchainTokenService, connect_evm_signed};
use crate::{preflight, ui};

pub(super) async fn prepare(
    cfg: &ChainsConfig,
    route: ConfigRoute,
    args: ConfigArgs,
) -> Result<Context> {
    let mnemonic = args
        .mnemonic_override
        .or_else(|| std::env::var("MNEMONIC").ok())
        .ok_or_else(|| eyre::eyre!("MNEMONIC is required for manual Cosmos relay transactions"))?;
    let key = args
        .evm_private_key_override
        .or_else(|| std::env::var("EVM_PRIVATE_KEY").ok())
        .or_else(|| std::env::var("DEPLOYER_PRIVATE_KEY").ok())
        .ok_or_else(|| {
            eyre::eyre!(
                "Set EVM_PRIVATE_KEY or DEPLOYER_PRIVATE_KEY to a wallet funded on both EVM chains"
            )
        })?;
    let signer: PrivateKeySigner = key.parse()?;
    let signer_address = signer.address();
    let source = connect_evm_signed(std::slice::from_ref(&route.source_rpc), signer.clone())?;
    let destination = connect_evm_signed(std::slice::from_ref(&route.destination_rpc), signer)?;
    for (name, provider) in [(&route.source, &source), (&route.destination, &destination)] {
        let chain = &cfg.chains[name];
        let expected = chain
            .evm_chain_id
            .ok_or_else(|| eyre::eyre!("missing EVM chainId for {name}"))?;
        eyre::ensure!(
            provider.get_chain_id().await? == expected,
            "{name}: EVM RPC chain ID mismatch"
        );
        preflight::check_evm_balances(
            chain
                .rpc
                .as_deref()
                .ok_or_else(|| eyre::eyre!("missing RPC"))?,
            &[(name.as_str(), signer_address)],
            chain.token_symbol.as_deref().unwrap_or("native"),
        )
        .await?;
    }
    let relay = prepare_legacy_relay(cfg, &mnemonic, route.source_axelar_id.as_str())?;
    crate::cosmos::check_axelar_balance(
        &relay.lcd,
        &relay.chain_id,
        &relay.axelar_address.parse()?,
        &relay.fee_denom.parse()?,
        200_000,
    )
    .await?;
    let src = &cfg.chains[&route.source];
    let context = Context {
        factory: src
            .contract_address(ChainContract::InterchainTokenFactory, &route.source)?
            .parse()?,
        source_its: src
            .contract_address(ChainContract::InterchainTokenService, &route.source)?
            .parse()?,
        axelar_rpc: cfg
            .axelar
            .rpc
            .clone()
            .ok_or_else(|| eyre::eyre!("missing axelar.rpc for manual proof relay"))?,
        amount: U256::from(args.amount.unwrap_or(1_000_000_000_000_000_000)),
        gas_value: U256::from(args.gas_value.unwrap_or(0)),
        route,
        relay,
        source,
        destination,
        signer: signer_address,
    };
    eyre::ensure!(
        context.amount > U256::ZERO,
        "transfer amount must be positive"
    );
    check_contracts(&context).await?;
    Ok(context)
}

async fn check_contracts(ctx: &Context) -> Result<()> {
    for (provider, its_address, peer, label) in [
        (
            &ctx.source,
            ctx.source_its,
            &ctx.route.destination_axelar_id,
            &ctx.route.source,
        ),
        (
            &ctx.destination,
            ctx.route.destination_its,
            &ctx.route.source_axelar_id,
            &ctx.route.destination,
        ),
    ] {
        let its = InterchainTokenService::new(its_address, provider);
        eyre::ensure!(
            its.isTrustedChain(peer.to_string()).call().await?,
            "{label} ITS does not trust {peer}; its owner or operator must register trust before testing"
        );
        eyre::ensure!(
            its.itsHubAddress().call().await? == ctx.route.its_hub,
            "{label} ITS hub address differs from the selected Axelar config"
        );
        ui::success(&format!(
            "{label} ITS trusts {peer} and uses the configured hub"
        ));
    }
    for (provider, address) in [
        (&ctx.source, ctx.factory),
        (&ctx.destination, ctx.route.destination_gateway),
    ] {
        eyre::ensure!(
            !provider.get_code_at(address).await?.is_empty(),
            "No contract at {address}"
        );
    }
    ui::address("EVM signer / receiver", &ctx.signer.to_string());
    ui::kv(
        "Transfer amount (18-decimal base units)",
        &ctx.amount.to_string(),
    );
    ui::kv(
        "Relayer-service gas payment (wei)",
        &ctx.gas_value.to_string(),
    );
    ui::info(
        "Axe submits both relay legs itself. The signer needs EVM gas on both chains and the mnemonic needs Axelar transaction fees. Verifiers must be online.",
    );
    ui::info("This test creates a fresh token each run; it does not resume a previous test.");
    Ok(())
}
