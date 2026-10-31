//! Consensus-chain delivery of a passed governance proposal.
//!
//! On consensus chains Axelar's validators sign the approval and the Axelar
//! relayer lands it on the legacy gateway, and axe cannot build that proof. So the
//! relay waits for the gateway's `ContractCallApproved` carrying our payload
//! hash, takes the on-chain `commandId` from it, consumes the approval on the
//! ASG (`execute`), and hands back to the shared operator/time-lock execute.

use std::str::FromStr;
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use eyre::Result;

use super::relay::{Approval, approve_evm, ensure_funded, pending_on_asg};
use super::types::{RelayPlan, ResolvedConfig};
use crate::evm::legacy_gateway::{ApprovedCall, find_contract_call_approved};
use crate::evm::{AxelarAmplifierGateway, AxelarServiceGovernance, broadcast_and_log};
use crate::ui;

const APPROVAL_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Wait for the validators' approval and consume it on the ASG. Returns the
/// connected signer when the ASG now holds something to execute, `None` when
/// everything already happened.
pub(super) async fn deliver<P: Provider, R: Provider>(
    cfg: &ResolvedConfig,
    plan: &RelayPlan,
    payload_hash: B256,
    execution_time: chrono::DateTime<chrono::FixedOffset>,
    read_provider: &R,
    connect: impl FnOnce() -> Result<(P, Address)>,
    approval: Approval,
) -> Result<Option<(P, Address)>> {
    let gateway = Address::from_str(&cfg.gateway_address)?;
    let asg_addr = Address::from_str(&cfg.asg_address)?;
    let from_block = first_block_at(read_provider, execution_time).await?;
    let approved =
        wait_for_approval(read_provider, gateway, asg_addr, payload_hash, from_block).await?;
    ui::kv("commandId", &approved.command_id.to_string());
    ui::kv(
        "source",
        &format!("{} / {}", approved.source_chain, approved.source_address),
    );
    let consumed = AxelarAmplifierGateway::new(gateway, read_provider)
        .isCommandExecuted(approved.command_id)
        .call()
        .await?;
    if consumed && !pending_on_asg(read_provider, asg_addr, plan).await? {
        ui::info(
            "governance message already consumed; no pending ASG approval/schedule (executed or cancelled); nothing to replay",
        );
        return Ok(None);
    }
    let (provider, relayer) = connect()?;
    ui::address("relayer", &relayer.to_string());
    ensure_funded(&provider, relayer).await?;
    if consumed {
        ui::info("governance message already consumed; skipping ASG.execute");
    } else {
        let consume = Consume {
            cfg: cfg.clone(),
            relayer,
            gateway,
            asg: asg_addr,
            approved,
            payload: plan.payload.clone(),
            payload_hash,
            approval,
        };
        consume_on_asg(&provider, consume).await?;
    }
    Ok(Some((provider, relayer)))
}

/// Everything `ASG.execute` needs to consume one validators' approval.
struct Consume {
    cfg: ResolvedConfig,
    relayer: Address,
    gateway: Address,
    asg: Address,
    approved: ApprovedCall,
    payload: alloy::primitives::Bytes,
    payload_hash: B256,
    approval: Approval,
}

/// Poll the gateway until the validators' approval for this payload appears.
async fn wait_for_approval<P: Provider>(
    provider: &P,
    gateway: Address,
    asg: Address,
    payload_hash: B256,
    from_block: u64,
) -> Result<ApprovedCall> {
    let spinner = ui::wait_spinner(
        "waiting for the validators' approval on the legacy gateway (Axelar relays it; axe cannot build this proof)...",
    );
    loop {
        if let Some(approved) =
            find_contract_call_approved(provider, gateway, asg, payload_hash, None, from_block)
                .await?
        {
            spinner.finish_and_clear();
            ui::success("governance call approved on the legacy gateway");
            return Ok(approved);
        }
        tokio::time::sleep(APPROVAL_POLL_INTERVAL).await;
    }
}

async fn consume_on_asg<P: Provider>(provider: &P, consume: Consume) -> Result<()> {
    let Consume {
        cfg,
        relayer,
        gateway,
        asg,
        approved,
        payload,
        payload_hash,
        approval,
    } = consume;
    let still_approved = AxelarAmplifierGateway::new(gateway, provider)
        .isContractCallApproved(
            approved.command_id,
            approved.source_chain.clone(),
            approved.source_address.clone(),
            asg,
            payload_hash,
        )
        .call()
        .await?;
    eyre::ensure!(
        still_approved,
        "approval {} is no longer valid on the gateway; rerun to re-check",
        approved.command_id
    );
    let call = AxelarServiceGovernance::executeCall {
        commandId: approved.command_id,
        sourceChain: approved.source_chain,
        sourceAddress: approved.source_address,
        payload,
    };
    approve_evm(
        approval,
        &cfg,
        "ASG.execute (consume approval)",
        relayer,
        asg,
        &call.abi_encode(),
    )
    .await?;
    let pending = AxelarServiceGovernance::new(asg, provider)
        .execute(
            call.commandId,
            call.sourceChain,
            call.sourceAddress,
            call.payload,
        )
        .send()
        .await?;
    broadcast_and_log(pending, "ASG.execute (consume) tx").await?;
    ui::success("governance message consumed on ASG");
    Ok(())
}

/// The first edge block at or after `time`, so the approval scan starts where
/// the proposal executed instead of at genesis. Binary search on header
/// timestamps, or the latest block when the chain has not reached `time` yet.
async fn first_block_at<P: Provider>(
    provider: &P,
    time: chrono::DateTime<chrono::FixedOffset>,
) -> Result<u64> {
    let want = u64::try_from(time.timestamp().max(0)).unwrap_or(u64::MAX);
    let latest = provider.get_block_number().await?;
    if timestamp(provider, latest).await? < want {
        return Ok(latest);
    }
    let mut upper = latest;
    let mut lower = 0;
    let mut distance = 64_u64;
    while lower < upper {
        let probe = upper.saturating_sub(distance);
        if timestamp(provider, probe).await? < want {
            lower = probe;
            break;
        }
        upper = probe;
        distance = distance.saturating_mul(2);
    }
    while lower + 1 < upper {
        let mid = lower + (upper - lower) / 2;
        if timestamp(provider, mid).await? < want {
            lower = mid;
        } else {
            upper = mid;
        }
    }
    Ok(upper)
}

async fn timestamp<P: Provider>(provider: &P, number: u64) -> Result<u64> {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await?
        .ok_or_else(|| eyre::eyre!("edge block {number} unavailable"))?;
    Ok(block.header.timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::propose::relay::tests::rpc;
    use alloy::providers::ProviderBuilder;

    #[tokio::test]
    async fn scan_starts_at_the_latest_block_when_the_chain_trails_the_vote() {
        let edge = rpc::serve(rpc::Route::Consensus, false, false).await;
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(edge.url.parse().unwrap());
        let time = chrono::DateTime::parse_from_rfc3339("2026-09-29T16:03:24Z").unwrap();
        assert_eq!(first_block_at(&provider, time).await.unwrap(), 1);
        let requests = edge.finish().await;
        assert_eq!(requests.len(), 2);
    }
}
