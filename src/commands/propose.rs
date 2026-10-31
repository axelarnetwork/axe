//! `axe propose` — submit an AxelarServiceGovernance proposal (operator
//! fast-path or time-lock) to edge-chain ASGs via an Axelar gov proposal
//! that calls `AxelarnetGateway.call_contract` once per edge chain. Verifies
//! each target is a real, correctly-wired ASG, submits, prints the vote
//! action, and monitors to a terminal status. With `--relay` it then delivers
//! each GMP to its edge chain and executes it (operator fast-path or
//! time-lock), see `relay.rs`.

mod helpers;
mod monitor;
mod recovery;
mod recovery_types;
mod relay;
mod relay_consensus;
mod relay_message;
mod submission_guard;
mod targets;
pub mod types;

#[cfg(test)]
mod test_support;

use std::path::Path;
use std::str::FromStr;

use alloy::primitives::{Address, Bytes};
use chrono::TimeZone;
use cosmrs::crypto::secp256k1::SigningKey;
use eyre::{Result, WrapErr};
use owo_colors::OwoColorize;
use serde_json::{Value, json};

use crate::config::ChainsConfig;
use crate::config_source;
use crate::cosmos::{
    build_execute_msg_any, build_submit_proposal_any, check_axelar_balance, derive_axelar_wallet,
    extract_proposal_id, sign_and_broadcast_cosmos_tx,
};
use crate::types::Network;
use crate::ui;

use helpers::AsgInfo;
use types::{EdgeCall, ProposalPlan, ProposalType, ResolvedConfig};

pub use types::ProposeArgs;

/// Extra seconds added to the ASG's minimum delay when auto-computing a
/// time-lock `eta`, so the schedule is comfortably in the future.
const ETA_BUFFER_SECS: u64 = 300;
/// Gas headroom required on top of the gov deposit, in `uaxl`.
const GAS_BUFFER_UAXL: u128 = 10_000_000;

/// One edge call's encoded governance payload and the `eta` baked into it.
struct EncodedCall {
    payload: Bytes,
    eta: u64,
}

pub async fn run(args: ProposeArgs) -> Result<()> {
    let network = args.network;
    gate_mainnet(network, args.confirm_mainnet)?;
    if network == Network::Mainnet {
        ui::require_interactive("axe propose on mainnet")?;
        if args.yes {
            ui::warn("--y is ignored on mainnet; every write is reviewed and approved");
        }
    }

    let config_source = config_source::resolve(network, None).await?;
    let config = ChainsConfig::load(config_source.path()).await?;
    let plan = build_plan(&args, &config).await?;

    let existing = recovery::find(&plan, &args).await?;
    let (proposal_id, payloads) = if let Some((proposal, payloads)) = existing {
        let id = proposal.number()?;
        ui::kv(
            "resuming existing proposal",
            &format!("{id} ({})", proposal.status),
        );
        (id, payloads)
    } else {
        let Some(submitted) = submit_new(&args, &plan, &config).await? else {
            return Ok(());
        };
        submitted
    };
    ui::info("Rerun this command to resume; it will reuse the existing proposal.");
    let lcd = &plan.hub().lcd;
    monitor::wait_for_passed(lcd, proposal_id).await?;
    if args.relay {
        let proposal = recovery::load(lcd, proposal_id).await?;
        let execution_time = proposal.execution_time()?;
        relay_all(&plan, &payloads, execution_time)
            .await
            .wrap_err_with(|| {
                format!("proposal {proposal_id} passed; rerun to resume its relay")
            })?;
    } else {
        ui::info("proposal passed; not relaying (pass --relay to relay + execute).");
    }
    Ok(())
}

/// Resolve the edge chain(s) named on the command line into a verified plan:
/// one ASG call per chain, every ASG checked on-chain, one operator across
/// all of them, and every destination name known to the hub's routers.
async fn build_plan(args: &ProposeArgs, config: &ChainsConfig) -> Result<ProposalPlan> {
    let targets = targets::select(args, config).await?;
    if targets.len() > 1 {
        eyre::ensure!(
            args.op.is_some(),
            "--chains applies one catalog --op to every chain; raw --calldata is single-chain only"
        );
    }
    let mut calls = Vec::with_capacity(targets.len());
    let mut failures = Vec::new();
    let mut action_label = String::new();
    for target in &targets {
        ui::section(&format!(
            "propose: {} on {} [{}]",
            args.op.map_or("raw call", types::Operation::label),
            target.chain,
            args.proposal_type.label()
        ));
        match resolve_call(args, target).await {
            Ok((call, label)) => {
                action_label = label;
                calls.push(call);
            }
            Err(error) => failures.push(format!("{}: {error:#}", target.chain)),
        }
    }
    eyre::ensure!(
        failures.is_empty(),
        "cannot verify every edge chain; fix the config or leave them out with --skip:\n  {}",
        failures.join("\n  ")
    );
    let plan = ProposalPlan::new(args.proposal_type, action_label, calls)?;
    if plan.calls.len() > 1 {
        let operator = single_operator(&plan)?;
        require_operator_key(operator)?;
    }
    targets::check_route_names(config, &targets).await?;
    Ok(plan)
}

/// One chain's call: the inner `(target, calldata)` and its verified ASG.
async fn resolve_call(args: &ProposeArgs, target: &targets::Target) -> Result<(EdgeCall, String)> {
    let (target_addr, calldata, label) = resolve_action(args, &target.cfg).await?;
    let asg = verify(&target.cfg).await?;
    let call = EdgeCall {
        chain: target.chain.clone(),
        cfg: target.cfg.clone(),
        asg,
        target: target_addr,
        calldata,
    };
    Ok((call, label))
}

/// Every ASG in a multi-chain plan must answer to the same operator, or the
/// one key we hold cannot finish the relay everywhere.
fn single_operator(plan: &ProposalPlan) -> Result<Address> {
    let first = &plan.calls[0];
    let others: Vec<String> = plan
        .calls
        .iter()
        .filter(|call| call.asg.operator != first.asg.operator)
        .map(|call| format!("{} ({})", call.cfg.edge_axelar_id, call.asg.operator))
        .collect();
    eyre::ensure!(
        others.is_empty(),
        "ASG operators differ: {} has {}, but: {}. Decide how to handle those chains (e.g. --skip) before proposing",
        first.cfg.edge_axelar_id,
        first.asg.operator,
        others.join(", ")
    );
    Ok(first.asg.operator)
}

fn require_operator_key(operator: Address) -> Result<()> {
    match relay::operator_key_status(operator) {
        relay::OperatorKey::Operator(_) => Ok(()),
        relay::OperatorKey::NotOperator(addr) => Err(eyre::eyre!(
            "EVM_GOVERNANCE_OPERATOR_KEY derives {addr}, not the ASG operator {operator}"
        )),
        relay::OperatorKey::Missing => Err(eyre::eyre!(
            "set EVM_GOVERNANCE_OPERATOR_KEY to the ASG operator {operator}; a multi-chain proposal is only submitted when it can be executed everywhere"
        )),
    }
}

/// Deliver and execute every call of a passed proposal, continuing past a
/// failed edge so one bad chain does not block the rest. The error lists the
/// chains still outstanding for the rerun.
async fn relay_all(
    plan: &ProposalPlan,
    payloads: &[Bytes],
    execution_time: chrono::DateTime<chrono::FixedOffset>,
) -> Result<()> {
    let total = plan.calls.len();
    let mut outstanding = Vec::new();
    for (index, (call, payload)) in plan.calls.iter().zip(payloads).enumerate() {
        ui::section(&format!(
            "relay {}/{total}: {}",
            index + 1,
            call.cfg.edge_axelar_id
        ));
        let relay_plan = call.relay_plan(plan.ptype, payload.clone());
        if let Err(error) = relay::relay(&call.cfg, &call.asg, &relay_plan, execution_time).await {
            ui::error(&format!("{}: {error:#}", call.cfg.edge_axelar_id));
            outstanding.push(call.cfg.edge_axelar_id.clone());
        }
    }
    eyre::ensure!(
        outstanding.is_empty(),
        "relay incomplete on: {}",
        outstanding.join(", ")
    );
    Ok(())
}

async fn submit_new(
    args: &ProposeArgs,
    plan: &ProposalPlan,
    config: &ChainsConfig,
) -> Result<Option<(u64, Vec<Bytes>)>> {
    let marker = submission_guard::path(plan)?;
    submission_guard::check(&marker, args.new_proposal).await?;
    for call in &plan.calls {
        helpers::check_not_already_proposed(
            &call.cfg,
            plan.ptype,
            call.target,
            call.calldata.clone(),
        )
        .await?;
    }
    // Check both signers before confirmation or spending the deposit.
    let mnemonic = std::env::var("MNEMONIC")
        .map_err(|_| eyre::eyre!("MNEMONIC not set — needed to sign the gov proposal"))?;
    let (signing_key, submitter) = derive_axelar_wallet(&mnemonic)?;
    if args.relay {
        for call in &plan.calls {
            relay::preflight_relayer(&call.cfg).await?;
        }
    }
    let encoded: Vec<EncodedCall> = plan
        .calls
        .iter()
        .map(|call| encode_call(args, plan.ptype, call))
        .collect();
    if !confirm_submit(args, plan, config, &encoded).await {
        ui::info("aborted — no proposal submitted");
        return Ok(None);
    }
    ui::address("submitter", &submitter);
    let payloads: Vec<Bytes> = encoded.into_iter().map(|call| call.payload).collect();
    let id = submit(plan, args, &payloads, &signing_key, &submitter, &marker).await?;
    ui::kv("proposal submitted", &id.to_string());
    ui::action_required(&[
        "Vote on the proposal:",
        &format!(
            "./scripts/vote_{env}_proposal.sh {env}-nodes {id}",
            env = vote_env(args.network)
        ),
    ]);
    Ok(Some((id, payloads)))
}

fn encode_call(args: &ProposeArgs, ptype: ProposalType, call: &EdgeCall) -> EncodedCall {
    let eta = compute_eta(ptype, args, &call.asg);
    let payload = helpers::encode_governance_payload(
        ptype.command(),
        call.target,
        call.calldata.clone(),
        eta,
    );
    EncodedCall { payload, eta }
}

async fn verify(cfg: &ResolvedConfig) -> Result<AsgInfo> {
    let spinner = ui::wait_spinner("verifying ASG on-chain (read-only)...");
    let asg = helpers::verify_asg(cfg).await;
    spinner.finish_and_clear();
    let asg = asg?;

    ui::address("ASG", &cfg.asg_address);
    ui::address("operator", &asg.operator.to_string());
    ui::kv("governanceChain", &asg.governance_chain);
    ui::kv("governanceAddress", &asg.governance_address);
    ui::kv(
        "minTimeLockDelay",
        &format!("{}s", asg.minimum_time_lock_delay),
    );
    Ok(asg)
}

/// The inner `AxelarnetGateway.call_contract` message for one edge call,
/// exactly what the gov proposal carries, shown verbatim before submission.
fn call_contract_msg(call: &EdgeCall, payload: &[u8]) -> Value {
    json!({
        "call_contract": {
            "destination_chain": call.cfg.edge_axelar_id,
            "destination_address": call.cfg.asg_address,
            "payload": alloy::hex::encode(payload),
        }
    })
}

async fn submit(
    plan: &ProposalPlan,
    args: &ProposeArgs,
    payloads: &[Bytes],
    signing_key: &SigningKey,
    submitter: &str,
    marker: &Path,
) -> Result<u64> {
    let hub = plan.hub();
    let expedited = !args.standard;
    let deposit = if expedited {
        &hub.expedited_deposit_amount
    } else {
        &hub.deposit_amount
    };
    let min_balance = deposit.parse::<u128>()? + GAS_BUFFER_UAXL;
    check_axelar_balance(
        &hub.lcd,
        &hub.chain_id,
        &submitter.parse::<cosmrs::AccountId>()?,
        &hub.fee_denom.parse::<cosmrs::Denom>()?,
        min_balance,
    )
    .await?;

    let inner = plan
        .calls
        .iter()
        .zip(payloads)
        .map(|(call, payload)| {
            build_execute_msg_any(
                &hub.gov_module,
                &hub.axelarnet_gateway,
                &call_contract_msg(call, payload),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let title = proposal_title(plan);
    let summary = format!(
        "Service-Governance {} proposal: {} on {}. Submitted via axe propose.",
        plan.ptype.label(),
        plan.action_label,
        plan.edge_ids().join(", ")
    );
    let proposal = build_submit_proposal_any(
        submitter,
        inner,
        &title,
        &summary,
        deposit,
        &hub.fee_denom,
        expedited,
    )?;

    submission_guard::claim(marker, args.new_proposal).await?;
    let tx_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        submitter,
        &hub.lcd,
        &hub.chain_id,
        &hub.fee_denom,
        hub.gas_price,
        vec![proposal],
    )
    .await?;
    extract_proposal_id(&tx_resp)
}

fn proposal_title(plan: &ProposalPlan) -> String {
    match plan.calls.as_slice() {
        [single] => format!(
            "axe: {} on {}",
            plan.action_label, single.cfg.edge_axelar_id
        ),
        calls => format!("axe: {} on {} chains", plan.action_label, calls.len()),
    }
}

async fn resolve_action(
    args: &ProposeArgs,
    cfg: &ResolvedConfig,
) -> Result<(Address, Bytes, String)> {
    if let Some(op) = args.op {
        let trust_api = if helpers::needs_trust_api(op) {
            let api = helpers::probe_trust_api(cfg).await?;
            ui::kv(
                &format!("{} ITS trust API", cfg.edge_axelar_id),
                api.label(),
            );
            api
        } else {
            helpers::TrustApi::TrustedChain
        };
        let (target, calldata) =
            helpers::build_call(op, cfg, args.its_chain.as_deref(), trust_api)?;
        return Ok((target, calldata, op.label().to_string()));
    }

    let calldata_hex = args
        .calldata
        .as_deref()
        .ok_or_else(|| eyre::eyre!("provide --op <name> or --calldata <hex> --target <addr>"))?;
    let target_raw = args
        .target
        .as_deref()
        .ok_or_else(|| eyre::eyre!("--calldata requires --target <addr>"))?;
    let target = Address::from_str(target_raw)
        .map_err(|e| eyre::eyre!("invalid --target '{target_raw}': {e}"))?;
    let stripped = calldata_hex.strip_prefix("0x").unwrap_or(calldata_hex);
    let calldata: Bytes = alloy::hex::decode(stripped)
        .map_err(|e| eyre::eyre!("invalid --calldata hex: {e}"))?
        .into();
    Ok((target, calldata, format!("raw call to {target_raw}")))
}

async fn confirm_submit(
    args: &ProposeArgs,
    plan: &ProposalPlan,
    config: &ChainsConfig,
    encoded: &[EncodedCall],
) -> bool {
    if args.yes && args.network != Network::Mainnet {
        return true;
    }
    let hub = plan.hub();
    let expedited = !args.standard;
    let deposit = if expedited {
        &hub.expedited_deposit_amount
    } else {
        &hub.deposit_amount
    };
    ui::section("review before submit");
    ui::kv("proposal type", plan.ptype.label());
    ui::kv("edge calls", &plan.calls.len().to_string());
    for (call, encoded) in plan.calls.iter().zip(encoded) {
        review_call(args, config, call, encoded);
    }
    ui::kv(
        "deposit",
        &format!(
            "{} ({})",
            format_deposit(deposit, &hub.fee_denom),
            if expedited { "expedited" } else { "standard" }
        ),
    );
    ui::confirm("Submit this proposal?").await
}

/// Everything about one edge call a reviewer needs: where it goes, what the
/// ASG will run (decoded), and the exact hub message carrying it.
fn review_call(args: &ProposeArgs, config: &ChainsConfig, call: &EdgeCall, encoded: &EncodedCall) {
    ui::kv("chain", &call.cfg.edge_axelar_id);
    match helpers::target_name(config, &call.chain, call.target) {
        Some(name) => ui::kv("target", &format!("{}  ({name})", call.target)),
        None => ui::kv(
            "target",
            &format!("{}  {}", call.target, "Unknown Destination".red().bold()),
        ),
    }
    let calldata_hex = format!("0x{}", alloy::hex::encode(&call.calldata));
    ui::kv("calldata", &calldata_hex);
    // Pretty-print the calldata with the same decoder as `axe decode`, for both
    // catalog ops and raw calls. A decode failure on a user-supplied raw call is
    // flagged in red; catalog calldata is known-good, so a selector the decoder
    // doesn't recognise is just left unannotated.
    let decoded = crate::commands::decode::run(&calldata_hex).is_ok();
    if !decoded && args.op.is_none() {
        println!("  {}", "Unknown Calldata".red().bold());
    }
    if encoded.eta > 0 {
        ui::kv("timelock eta", &format_eta(encoded.eta));
    }
    match serde_json::to_string_pretty(&call_contract_msg(call, &encoded.payload)) {
        Ok(message) => ui::info(&message),
        Err(error) => ui::warn(&format!("cannot render hub message: {error}")),
    }
    if args.relay {
        show_relay_readiness(args.proposal_type, call.asg.operator);
    }
}

/// With `--relay`, tell the user up front whether the environment can actually
/// finish the relay — so the yes/no is informed. The operator fast-path needs
/// the operator's key for the final `executeOperatorProposal`; the time-lock
/// path's execute is permissionless but still needs a funded EVM key for gas.
fn show_relay_readiness(ptype: ProposalType, operator: Address) {
    match (ptype, relay::operator_key_status(operator)) {
        (ProposalType::Operator, relay::OperatorKey::Operator(addr)) => {
            ui::kv("relay", &format!("execute as operator {addr} ✓"));
        }
        (ProposalType::Operator, relay::OperatorKey::NotOperator(addr)) => ui::warn(&format!(
            "relay: EVM key {addr} is NOT the operator {operator} — the proposal will be relayed + \
             APPROVED but NOT executed; you'll get a cast command to run as the operator"
        )),
        (ProposalType::Operator, relay::OperatorKey::Missing) => ui::warn(&format!(
            "relay: no EVM_GOVERNANCE_OPERATOR_KEY / EVM_PRIVATE_KEY set — cannot relay (operator is {operator})"
        )),
        (ProposalType::Timelock, relay::OperatorKey::Missing) => ui::warn(
            "relay: no EVM_GOVERNANCE_OPERATOR_KEY / EVM_PRIVATE_KEY set — cannot relay (time-lock \
             execute is permissionless but still needs a funded EVM key for gas)",
        ),
        (
            ProposalType::Timelock,
            relay::OperatorKey::Operator(addr) | relay::OperatorKey::NotOperator(addr),
        ) => {
            ui::kv(
                "relay",
                &format!("submitProof + executeProposal after eta with {addr}"),
            );
        }
    }
}

/// Render a micro-denom amount (e.g. `"3000000000"`, `"uaxl"`) as a human
/// figure (`"3000 AXL"`). Micro denoms (`u…`, 1e6 base) are converted and the
/// `u` prefix dropped + upper-cased; anything else is shown verbatim.
fn format_deposit(amount: &str, micro_denom: &str) -> String {
    let (Ok(micro), Some(unit)) = (amount.parse::<u128>(), micro_denom.strip_prefix('u')) else {
        return format!("{amount} {micro_denom}");
    };
    let unit = unit.to_uppercase();
    let whole = micro / 1_000_000;
    let frac = micro % 1_000_000;
    if frac == 0 {
        format!("{whole} {unit}")
    } else {
        let frac = format!("{frac:06}");
        format!("{whole}.{} {unit}", frac.trim_end_matches('0'))
    }
}

/// Render a unix-seconds timestamp as local wall-clock time plus the raw value.
fn format_eta(eta: u64) -> String {
    match chrono::Local.timestamp_opt(eta as i64, 0).single() {
        Some(dt) => format!("{} ({eta})", dt.format("%H:%M:%S %d %b %Y")),
        None => eta.to_string(),
    }
}

fn compute_eta(ptype: ProposalType, args: &ProposeArgs, asg: &AsgInfo) -> u64 {
    match ptype {
        ProposalType::Operator => 0,
        ProposalType::Timelock => args.eta.unwrap_or_else(|| {
            let now = chrono::Utc::now().timestamp().max(0) as u64;
            now + asg.minimum_time_lock_delay + ETA_BUFFER_SECS
        }),
    }
}

fn gate_mainnet(network: Network, confirmed: bool) -> Result<()> {
    if network == Network::Mainnet && !confirmed {
        return Err(eyre::eyre!(
            "refusing to propose on mainnet without --confirm-mainnet (giga-gated)"
        ));
    }
    Ok(())
}

/// Map a network to the short env name used by the `vote_<env>_proposal.sh`
/// scripts and the `<env>-nodes` kube context (devnet-amplifier → `devnet`).
fn vote_env(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
        Network::Stagenet => "stagenet",
        Network::DevnetAmplifier => "devnet",
    }
}

#[cfg(test)]
mod tests {
    use super::{format_deposit, proposal_title};
    use crate::commands::propose::test_support::{config, plan};
    use crate::commands::propose::types::ProposalType;
    use alloy::primitives::Address;

    #[test]
    fn format_deposit_converts_micro_to_whole_axl() {
        assert_eq!(format_deposit("3000000000", "uaxl"), "3000 AXL");
        assert_eq!(format_deposit("2000000000", "uaxl"), "2000 AXL");
    }

    #[test]
    fn format_deposit_trims_fractional_axl() {
        assert_eq!(format_deposit("1500000", "uaxl"), "1.5 AXL");
        assert_eq!(format_deposit("1", "uaxl"), "0.000001 AXL");
    }

    #[test]
    fn format_deposit_passes_through_non_micro_denoms() {
        assert_eq!(format_deposit("123", "axl"), "123 axl");
        assert_eq!(format_deposit("not-a-number", "uaxl"), "not-a-number uaxl");
    }

    #[test]
    fn operator_gate_names_the_chains_that_differ() {
        let mut two = plan(
            config(String::new()),
            ProposalType::Operator,
            Address::ZERO,
            vec![].into(),
        );
        let mut other = plan(
            config(String::new()),
            ProposalType::Operator,
            Address::ZERO,
            vec![].into(),
        )
        .calls
        .remove(0);
        other.cfg.edge_axelar_id = "hedera".into();
        two.calls.push(other);
        assert_eq!(super::single_operator(&two).unwrap(), Address::ZERO);
        two.calls[1].asg.operator = Address::from([9; 20]);
        let error = super::single_operator(&two).unwrap_err().to_string();
        assert!(error.contains("hedera"), "{error}");
        assert!(!error.contains("flow ("), "{error}");
    }

    #[test]
    fn title_names_the_single_chain_or_counts_many() {
        let mut one = plan(
            config(String::new()),
            ProposalType::Operator,
            Address::ZERO,
            vec![].into(),
        );
        one.action_label = "ITS set-trusted-chain".into();
        assert_eq!(proposal_title(&one), "axe: ITS set-trusted-chain on flow");
        let second = plan(
            config(String::new()),
            ProposalType::Operator,
            Address::ZERO,
            vec![].into(),
        )
        .calls
        .remove(0);
        one.calls.push(second);
        assert_eq!(
            proposal_title(&one),
            "axe: ITS set-trusted-chain on 2 chains"
        );
    }
}
