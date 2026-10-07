use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use alloy::sol_types::SolValue;
use eyre::Result;

use super::{
    session, storage,
    types::{InitialSigners, Transaction},
};
use crate::commands::deploy::DeployContext;
use crate::evm::{WeightedSigner, WeightedSigners, compute_create_address};
use crate::ui;
use crate::utils::read_contract_address_by_name;

sol! {
    #[sol(rpc)]
    interface ManagedContract {
        function owner() external view returns (address);
        function implementation() external view returns (address);
        function epoch() external view returns (uint256);
        function signersHashByEpoch(uint256 epoch) external view returns (bytes32);
        function operator() external view returns (address);
    }
}

/// Pin the original two CREATE transactions to the address already used by Cosmos.
pub async fn gateway_nonce<P: Provider>(
    ctx: &DeployContext,
    provider: &P,
    sender: Address,
) -> Result<Option<u64>> {
    initial_set(ctx).await?;
    let key = session::action_key("gateway implementation")?;
    let nonce = match session::current()?.get(&key).await {
        Some(Transaction::Evm {
            sender: recorded,
            nonce,
            ..
        }) => {
            eyre::ensure!(sender == recorded, "gateway deployer differs from journal");
            nonce
        }
        Some(_) => {
            eyre::bail!("gateway implementation journal type mismatch");
        }
        None => provider.get_transaction_count(sender).await?,
    };
    let expected = ctx
        .state
        .predicted_gateway_address
        .ok_or_else(|| eyre::eyre!("missing pinned gateway address"))?;
    validate_gateway_prediction(sender, nonce, expected)?;
    Ok(Some(nonce))
}

pub(super) fn validate_gateway_prediction(
    sender: Address,
    nonce: u64,
    expected: Address,
) -> Result<()> {
    let proxy_nonce = nonce
        .checked_add(1)
        .ok_or_else(|| eyre::eyre!("gateway nonce overflow"))?;
    eyre::ensure!(
        compute_create_address(sender, proxy_nonce) == expected,
        "gateway deployer nonce changed since address prediction; stop and reconcile the Cosmos configuration before sending anything"
    );
    Ok(())
}

pub async fn gateway(ctx: &DeployContext) -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(ctx.rpc_url.parse()?);
    let address =
        read_contract_address_by_name(&ctx.target_json, &ctx.axelar_id, "AxelarGateway").await?;
    eyre::ensure!(
        Some(address) == ctx.state.predicted_gateway_address,
        "gateway address mismatch"
    );
    let block = super::confirmations::observation_block(&provider).await?;
    let block_id = alloy::eips::BlockId::hash_canonical(block.header.hash);
    let gateway = ManagedContract::new(address, &provider);
    let initial = initial_set(ctx).await?;
    let epoch = gateway.epoch().block(block_id).call().await?;
    eyre::ensure!(
        epoch >= U256::from(1)
            && gateway
                .signersHashByEpoch(U256::from(1))
                .block(block_id)
                .call()
                .await?
                == initial.hash,
        "gateway initial signer hash differs from the recorded deployment"
    );
    if epoch > U256::from(1) {
        let current_hash = gateway
            .signersHashByEpoch(epoch)
            .block(block_id)
            .call()
            .await?;
        let session = session::current()?;
        let saved = session.journal.lock().await.rotations.clone();
        if let Some((previous_epoch, previous_hash)) = saved.last_key_value() {
            eyre::ensure!(
                epoch >= U256::from(*previous_epoch)
                    && gateway
                        .signersHashByEpoch(U256::from(*previous_epoch))
                        .block(block_id)
                        .call()
                        .await?
                        == *previous_hash,
                "gateway rotation history changed"
            );
        }
        let epoch: u64 = epoch.try_into()?;
        if saved.get(&epoch) != Some(&current_hash) {
            ui::kv("gateway rotated to epoch", &epoch.to_string());
            ui::kv("current signer hash", &current_hash.to_string());
            ui::info(
                "The gateway retains the verified initial signer hash. Review this on-chain rotation before continuing deployment.",
            );
            if !ui::confirm("Accept this gateway rotation for subsequent deployment checks?").await
            {
                return Err(session::pause("gateway rotation not approved"));
            }
            let mut journal = session.journal.lock().await;
            journal.rotations.insert(epoch, current_hash);
            storage::atomic_write(&session.path, &serde_json::to_vec_pretty(&*journal)?)?;
        }
    }
    eyre::ensure!(
        gateway.operator().block(block_id).call().await?
            == session::current()?.plan.gateway_operator,
        "gateway operator mismatch"
    );
    Ok(())
}

pub async fn completed(ctx: &DeployContext) -> Result<()> {
    gateway(ctx).await?;
    let plan = session::current()?.plan.clone();
    let provider = ProviderBuilder::new().connect_http(ctx.rpc_url.parse()?);
    for (name, owner) in [
        ("AxelarGateway", plan.gateway_owner),
        ("Operators", plan.operators_owner),
        ("AxelarGasService", plan.gas_service_owner),
        ("InterchainTokenService", plan.its_owner),
        ("InterchainTokenFactory", plan.factory_owner),
    ] {
        let address = read_contract_address_by_name(&ctx.target_json, &ctx.axelar_id, name).await?;
        eyre::ensure!(
            ManagedContract::new(address, &provider)
                .owner()
                .call()
                .await?
                == owner,
            "{name}: final owner mismatch"
        );
    }
    Ok(())
}

pub async fn initial_set(_ctx: &DeployContext) -> Result<InitialSigners> {
    let session = session::current()?;
    if let Some(initial) = session.journal.lock().await.initial_signers.clone() {
        eyre::ensure!(
            initial.hash == signer_hash(&initial.signers, initial.threshold, initial.nonce),
            "initial signer snapshot hash mismatch"
        );
        return Ok(initial);
    }
    Err(session::pause(
        "initial verifier set has not been approved; resume at the verifier checkpoint with --activate",
    ))
}

pub async fn refresh_before_gateway(ctx: &DeployContext) -> Result<()> {
    let session = session::current()?;
    if session.get("AxelarGateway/gateway proxy").await.is_some() {
        // Signed constructor bytes are immutable, including after Ctrl+C.
        return Ok(());
    }
    let pinned = initial_set(ctx).await?;
    let current = super::verifier_set::query(&ctx.state)
        .await?
        .ok_or_else(|| session::pause("prover set disappeared before gateway creation"))?;
    if !super::verifier_set::same_set(&pinned, &current) {
        ui::info(
            "The prover set changed before gateway creation. Review its current set before continuing.",
        );
        super::verifier_set::approve(&ctx.state, current).await?;
    }
    Ok(())
}

pub(super) fn snapshot(
    identities: Vec<String>,
    signers: Vec<(Address, u128)>,
    threshold: u128,
    nonce: B256,
    set_id: String,
) -> InitialSigners {
    let hash = signer_hash(&signers, threshold, nonce);
    InitialSigners {
        identities,
        signers,
        threshold,
        nonce,
        set_id,
        hash,
    }
}

fn signer_hash(signers: &[(Address, u128)], threshold: u128, nonce: B256) -> B256 {
    let set = WeightedSigners {
        signers: signers
            .iter()
            .map(|(signer, weight)| WeightedSigner {
                signer: *signer,
                weight: *weight,
            })
            .collect(),
        threshold,
        nonce,
    };
    keccak256(set.abi_encode())
}
