//! `--relay`: deliver a passed governance proposal to the edge chain and run it.
//!
//! Second leg of the GMP, the part the amplifier relayer often skips for
//! gov-originated messages: find the `wasm-contract_called` the gov module
//! emitted, then deliver it to the edge gateway (amplifier: `construct_proof`,
//! wait for signing, submit the proof. Consensus: wait for the validators'
//! approval, see `relay_consensus.rs`), then `ASG.execute` (consume, then
//! schedule/approve), then `executeOperatorProposal` (operator fast-path) or
//! wait-for-eta + `executeProposal` (time-lock). Every write is shown in full
//! and approved on the terminal first.

use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use cosmrs::crypto::secp256k1::SigningKey;
use eyre::{Result, WrapErr};
use serde_json::Value;

use super::relay_message::find_governance_message;
use crate::commands::test_helpers::{extract_event_attr, wait_for_proof};
use crate::cosmos::{build_execute_msg_any, sign_and_broadcast_cosmos_tx};
use crate::evm::{AxelarAmplifierGateway, AxelarServiceGovernance, broadcast_and_log};
use crate::ui;

use super::helpers::AsgInfo;
use super::types::{EdgeRoute, GovMessage, ProposalType, RelayPlan, ResolvedConfig};

const ETA_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Calldata longer than this is previewed as length + keccak, like deployments.
const PREVIEW_INLINE_BYTES: usize = 1024;

#[cfg(test)]
pub(super) mod tests;

/// Who approves each write: the terminal, or nobody (tests only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    /// Show the full transaction and wait for `y`.
    Prompt,
    /// Proceed without asking. Only for tests against local fixtures.
    Automatic,
}

impl Approval {
    async fn approve(self, prompt: &str) -> bool {
        match self {
            Self::Prompt => ui::confirm(prompt).await,
            Self::Automatic => true,
        }
    }
}

pub async fn relay(
    cfg: &ResolvedConfig,
    asg_info: &AsgInfo,
    plan: &RelayPlan,
    execution_time: chrono::DateTime<chrono::FixedOffset>,
) -> Result<()> {
    relay_with_provider(
        cfg,
        asg_info,
        plan,
        execution_time,
        || {
            let signer = load_evm_signer()?;
            let relayer = signer.address();
            let provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(cfg.edge_rpc.parse().wrap_err("invalid edge rpc url")?);
            Ok((provider, relayer))
        },
        Approval::Prompt,
    )
    .await
}

async fn relay_with_provider<P: Provider>(
    cfg: &ResolvedConfig,
    asg_info: &AsgInfo,
    plan: &RelayPlan,
    execution_time: chrono::DateTime<chrono::FixedOffset>,
    connect: impl FnOnce() -> Result<(P, Address)>,
    approval: Approval,
) -> Result<()> {
    ui::section("relay to edge chain");
    ui::kv("route", cfg.route().label());
    let payload_hash = keccak256(&plan.payload);
    let msg = find_governance_message(cfg, payload_hash, execution_time).await?;
    ui::kv("message_id", &msg.message_id);
    let read_provider = ProviderBuilder::new().connect_http(cfg.edge_rpc.parse()?);
    let delivered = match cfg.route() {
        EdgeRoute::Amplifier => {
            deliver_amplifier(
                cfg,
                plan,
                &msg,
                payload_hash,
                &read_provider,
                connect,
                approval,
            )
            .await?
        }
        EdgeRoute::Consensus => {
            super::relay_consensus::deliver(
                cfg,
                plan,
                payload_hash,
                execution_time,
                &read_provider,
                connect,
                approval,
            )
            .await?
        }
    };
    let Some((provider, relayer)) = delivered else {
        return Ok(());
    };
    let asg_addr = Address::from_str(&cfg.asg_address)?;
    match plan.ptype {
        ProposalType::Operator => {
            execute_operator(&provider, cfg, asg_addr, asg_info, relayer, plan, approval).await
        }
        ProposalType::Timelock => {
            execute_timelock(&provider, cfg, asg_addr, relayer, plan, approval).await
        }
    }
}

/// Amplifier delivery: prove the message on the edge gateway if needed, then
/// consume it on the ASG. Returns the connected signer when the ASG now holds
/// something to execute, `None` when everything already happened.
async fn deliver_amplifier<P: Provider, R: Provider>(
    cfg: &ResolvedConfig,
    plan: &RelayPlan,
    msg: &GovMessage,
    payload_hash: B256,
    read_provider: &R,
    connect: impl FnOnce() -> Result<(P, Address)>,
    approval: Approval,
) -> Result<Option<(P, Address)>> {
    let gateway = Address::from_str(&cfg.gateway_address)?;
    let asg_addr = Address::from_str(&cfg.asg_address)?;
    let consumed = AxelarAmplifierGateway::new(gateway, read_provider)
        .isMessageExecuted(msg.source_chain.clone(), msg.message_id.clone())
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
        ui::info("governance message already consumed; skipping proof and ASG.execute");
        return Ok(Some((provider, relayer)));
    }
    if message_approved(&provider, gateway, asg_addr, msg, payload_hash).await? {
        ui::info("message already approved on edge gateway; skipping proof submission");
    } else {
        let mnemonic = std::env::var("MNEMONIC")
            .wrap_err("MNEMONIC not set — needed to construct the governance proof")?;
        let (signing_key, submitter) = crate::cosmos::derive_axelar_wallet(&mnemonic)?;
        let execute_data = build_proof(cfg, &signing_key, &submitter, msg, approval).await?;
        submit_proof(&provider, cfg, relayer, gateway, &execute_data, approval).await?;
    }
    assert_approved(&provider, gateway, asg_addr, msg, payload_hash).await?;
    consume_on_asg(
        &provider,
        cfg,
        relayer,
        asg_addr,
        msg,
        &plan.payload,
        approval,
    )
    .await?;
    Ok(Some((provider, relayer)))
}

pub(super) async fn pending_on_asg<P: Provider>(
    provider: &P,
    asg: Address,
    plan: &RelayPlan,
) -> Result<bool> {
    let contract = AxelarServiceGovernance::new(asg, provider);
    match plan.ptype {
        ProposalType::Operator => Ok(contract
            .isOperatorProposalApproved(plan.target, plan.calldata.clone(), U256::ZERO)
            .call()
            .await?),
        ProposalType::Timelock => Ok(contract
            .getProposalEta(plan.target, plan.calldata.clone(), U256::ZERO)
            .call()
            .await?
            != U256::ZERO),
    }
}

async fn message_approved<P: Provider>(
    provider: &P,
    gateway: Address,
    asg: Address,
    msg: &GovMessage,
    payload_hash: B256,
) -> Result<bool> {
    Ok(AxelarAmplifierGateway::new(gateway, provider)
        .isMessageApproved(
            msg.source_chain.clone(),
            msg.message_id.clone(),
            msg.source_address.clone(),
            asg,
            payload_hash,
        )
        .call()
        .await?)
}

async fn build_proof(
    cfg: &ResolvedConfig,
    signing_key: &SigningKey,
    axelar_address: &str,
    msg: &GovMessage,
    approval: Approval,
) -> Result<String> {
    let prover = cfg.multisig_prover.as_deref().ok_or_else(|| {
        eyre::eyre!(
            "no MultisigProver for {} — not an amplifier chain",
            cfg.edge_axelar_id
        )
    })?;
    ui::address("multisig prover", prover);
    let construct_proof_msg = serde_json::json!({
        "construct_proof": [{
            "source_chain": msg.source_chain,
            "message_id": msg.message_id,
        }]
    });
    approve_cosmos(
        approval,
        cfg,
        "construct_proof",
        axelar_address,
        prover,
        &construct_proof_msg,
    )
    .await?;
    let any = build_execute_msg_any(axelar_address, prover, &construct_proof_msg)?;
    let resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        &cfg.lcd,
        &cfg.chain_id,
        &cfg.fee_denom,
        cfg.gas_price,
        vec![any],
    )
    .await?;
    let session_id = extract_event_attr(&resp, "multisig_session_id")?;
    ui::kv("multisig_session_id", &session_id);

    let proof = wait_for_proof(&cfg.lcd, prover, &session_id).await?;
    proof["status"]["completed"]["execute_data"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| eyre::eyre!("no execute_data in completed proof"))
}

async fn submit_proof<P: Provider>(
    provider: &P,
    cfg: &ResolvedConfig,
    relayer: Address,
    gateway: Address,
    execute_data_hex: &str,
    approval: Approval,
) -> Result<()> {
    let execute_data = alloy::hex::decode(execute_data_hex)?;
    approve_evm(
        approval,
        cfg,
        "gateway submitProof",
        relayer,
        gateway,
        &execute_data,
    )
    .await?;
    let tx = TransactionRequest::default()
        .to(gateway)
        .input(Bytes::from(execute_data).into());
    let pending = provider.send_transaction(tx).await?;
    broadcast_and_log(pending, "submitProof tx").await?;
    Ok(())
}

async fn assert_approved<P: Provider>(
    provider: &P,
    gateway: Address,
    asg: Address,
    msg: &GovMessage,
    payload_hash: B256,
) -> Result<()> {
    let approved = AxelarAmplifierGateway::new(gateway, provider)
        .isMessageApproved(
            msg.source_chain.clone(),
            msg.message_id.clone(),
            msg.source_address.clone(),
            asg,
            payload_hash,
        )
        .call()
        .await?;
    if !approved {
        return Err(eyre::eyre!(
            "message not approved on the edge gateway after submitProof"
        ));
    }
    ui::success("message approved on edge gateway");
    Ok(())
}

async fn consume_on_asg<P: Provider>(
    provider: &P,
    cfg: &ResolvedConfig,
    relayer: Address,
    asg: Address,
    msg: &GovMessage,
    payload: &Bytes,
    approval: Approval,
) -> Result<()> {
    let call = AxelarServiceGovernance::executeCall {
        commandId: command_id(&msg.source_chain, &msg.message_id),
        sourceChain: msg.source_chain.clone(),
        sourceAddress: msg.source_address.clone(),
        payload: payload.clone(),
    };
    approve_evm(
        approval,
        cfg,
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

/// EVM gateway approval key: `keccak256(sourceChain + "_" + messageId)`.
fn command_id(source_chain: &str, message_id: &str) -> B256 {
    let mut buf = Vec::with_capacity(source_chain.len() + 1 + message_id.len());
    buf.extend_from_slice(source_chain.as_bytes());
    buf.push(b'_');
    buf.extend_from_slice(message_id.as_bytes());
    keccak256(&buf)
}

async fn execute_operator<P: Provider>(
    provider: &P,
    cfg: &ResolvedConfig,
    asg: Address,
    asg_info: &AsgInfo,
    relayer: Address,
    plan: &RelayPlan,
    approval: Approval,
) -> Result<()> {
    if relayer != asg_info.operator {
        ui::warn(&format!(
            "relayer {relayer} is not the ASG operator {} — the operator proposal is APPROVED \
             on-chain but NOT executed (we won't sign as a non-operator).",
            asg_info.operator
        ));
        ui::action_required(&[
            "Run this with the operator key to finish (executeOperatorProposal):",
            &format!(
                "cast send {asg} \"executeOperatorProposal(address,bytes,uint256)\" {} 0x{} 0 \
                 --rpc-url {} --private-key \"$EVM_GOVERNANCE_OPERATOR_KEY\"",
                plan.target,
                alloy::hex::encode(&plan.calldata),
                cfg.edge_rpc,
            ),
        ]);
        return Ok(());
    }
    let asg_contract = AxelarServiceGovernance::new(asg, provider);
    let approved = asg_contract
        .isOperatorProposalApproved(plan.target, plan.calldata.clone(), U256::ZERO)
        .call()
        .await?;
    if !approved {
        return Err(eyre::eyre!(
            "operator proposal not approved after consume — aborting before execute"
        ));
    }
    let call = AxelarServiceGovernance::executeOperatorProposalCall {
        target: plan.target,
        callData: plan.calldata.clone(),
        nativeValue: U256::ZERO,
    };
    approve_evm(
        approval,
        cfg,
        "ASG.executeOperatorProposal",
        relayer,
        asg,
        &call.abi_encode(),
    )
    .await?;
    let pending = asg_contract
        .executeOperatorProposal(plan.target, plan.calldata.clone(), U256::ZERO)
        .send()
        .await?;
    broadcast_and_log(pending, "executeOperatorProposal tx").await?;
    ui::success("operator proposal executed — target call ran");
    Ok(())
}

async fn execute_timelock<P: Provider>(
    provider: &P,
    cfg: &ResolvedConfig,
    asg: Address,
    relayer: Address,
    plan: &RelayPlan,
    approval: Approval,
) -> Result<()> {
    let asg_contract = AxelarServiceGovernance::new(asg, provider);
    let spinner = ui::wait_spinner("waiting for time-lock eta...");
    loop {
        let eta = asg_contract
            .getProposalEta(plan.target, plan.calldata.clone(), U256::ZERO)
            .call()
            .await?;
        let eta = u64::try_from(eta).unwrap_or(u64::MAX);
        if eta == 0 {
            spinner.finish_and_clear();
            return Err(eyre::eyre!("proposal not scheduled (eta=0) after consume"));
        }
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        if now >= eta {
            spinner.finish_and_clear();
            break;
        }
        spinner.set_message(format!("time-lock eta in {}s", eta - now));
        tokio::time::sleep(ETA_POLL_INTERVAL).await;
    }
    let call = AxelarServiceGovernance::executeProposalCall {
        target: plan.target,
        callData: plan.calldata.clone(),
        nativeValue: U256::ZERO,
    };
    approve_evm(
        approval,
        cfg,
        "ASG.executeProposal",
        relayer,
        asg,
        &call.abi_encode(),
    )
    .await?;
    let pending = asg_contract
        .executeProposal(plan.target, plan.calldata.clone(), U256::ZERO)
        .send()
        .await?;
    broadcast_and_log(pending, "executeProposal tx").await?;
    ui::success("time-lock proposal executed — target call ran");
    Ok(())
}

/// Show an EVM write in full (chain, signer, destination, calldata) and
/// wait for approval. Declining stops the relay before anything is sent. The
/// relay is idempotent, so a rerun picks up at the same write.
pub(super) async fn approve_evm(
    approval: Approval,
    cfg: &ResolvedConfig,
    label: &str,
    from: Address,
    to: Address,
    input: &[u8],
) -> Result<()> {
    if approval == Approval::Automatic {
        return Ok(());
    }
    ui::section(label);
    ui::kv("chain", &cfg.edge_axelar_id);
    ui::address("signer", &from.to_string());
    ui::address("to", &to.to_string());
    let preview = if input.len() > PREVIEW_INLINE_BYTES {
        serde_json::json!({"bytes": input.len(), "keccak256": keccak256(input)})
    } else {
        Value::String(format!("0x{}", alloy::hex::encode(input)))
    };
    ui::kv("input", &preview.to_string());
    // Pretty-print the calldata with the `axe decode` decoder. Unknown
    // selectors are simply left unannotated.
    let calldata_hex = format!("0x{}", alloy::hex::encode(input));
    let _ = crate::commands::decode::run(&calldata_hex);
    eyre::ensure!(
        approval.approve("Send this transaction?").await,
        "{label} declined; nothing was sent. Rerun to resume the relay"
    );
    Ok(())
}

/// Show a Cosmos write in full and wait for approval.
async fn approve_cosmos(
    approval: Approval,
    cfg: &ResolvedConfig,
    label: &str,
    signer: &str,
    contract: &str,
    msg: &Value,
) -> Result<()> {
    if approval == Approval::Automatic {
        return Ok(());
    }
    ui::section(&format!("Cosmos transaction: {label}"));
    ui::kv("chain", &cfg.chain_id);
    ui::address("signer", signer);
    ui::address("contract", contract);
    ui::info(&serde_json::to_string_pretty(msg)?);
    eyre::ensure!(
        approval.approve("Submit this transaction?").await,
        "{label} declined; nothing was submitted. Rerun to resume the relay"
    );
    Ok(())
}

/// Whether the environment holds a usable key for the operator execute step,
/// resolved before submitting so the confirm prompt can warn (and refuse the
/// operator-execute we can't sign).
pub enum OperatorKey {
    /// Resolved key whose address *is* the ASG operator — can executeOperatorProposal.
    Operator(Address),
    /// Resolved key, but not the operator — can relay + approve only.
    NotOperator(Address),
    /// No EVM key in the environment at all.
    Missing,
}

/// Classify the EVM key in the environment against the ASG operator address.
pub fn operator_key_status(operator: Address) -> OperatorKey {
    match env_signer() {
        Some(signer) if signer.address() == operator => OperatorKey::Operator(operator),
        Some(signer) => OperatorKey::NotOperator(signer.address()),
        None => OperatorKey::Missing,
    }
}

/// `EVM_GOVERNANCE_OPERATOR_KEY` (preferred) or `EVM_PRIVATE_KEY`, parsed.
fn env_signer() -> Option<PrivateKeySigner> {
    let key = std::env::var("EVM_GOVERNANCE_OPERATOR_KEY")
        .or_else(|_| std::env::var("EVM_PRIVATE_KEY"))
        .ok()?;
    key.parse().ok()
}

/// Pre-submit guard (when `--relay`): resolve the EVM relayer key and confirm
/// it's funded on the edge chain, so we refuse *before* spending a proposal we
/// couldn't relay. Errors if no key is set or its balance is zero.
pub async fn preflight_relayer(cfg: &ResolvedConfig) -> Result<()> {
    let signer = load_evm_signer()?;
    let relayer = signer.address();
    let provider =
        ProviderBuilder::new().connect_http(cfg.edge_rpc.parse().wrap_err("invalid edge rpc url")?);
    ensure_funded(&provider, relayer).await?;
    ui::kv(
        "relay preflight",
        &format!("{relayer} funded on {} ✓", cfg.edge_axelar_id),
    );
    Ok(())
}

fn load_evm_signer() -> Result<PrivateKeySigner> {
    env_signer().ok_or_else(|| {
        eyre::eyre!(
            "set EVM_GOVERNANCE_OPERATOR_KEY (preferred) or EVM_PRIVATE_KEY to relay/execute on the edge chain"
        )
    })
}

pub(super) async fn ensure_funded<P: Provider>(provider: &P, addr: Address) -> Result<()> {
    let balance = provider.get_balance(addr).await?;
    if balance.is_zero() {
        return Err(eyre::eyre!(
            "relayer {addr} has zero balance on the edge chain — fund it for gas"
        ));
    }
    Ok(())
}
