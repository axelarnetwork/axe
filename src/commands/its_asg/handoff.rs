use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use eyre::{Result, ensure};
use serde_json::json;

use super::config::{self, GOVERNANCE_ADDRESS, GOVERNANCE_CHAIN};
use super::contracts::Managed;
use super::types::{Context, Options, Roles};
use super::{checks, transactions};
use crate::ui;

pub fn ownership_request(
    ctx: &Context,
    from: Address,
    target: Address,
    asg: Address,
) -> TransactionRequest {
    let input = Managed::transferOwnershipCall { newOwner: asg }.abi_encode();
    TransactionRequest::default()
        .from(from)
        .to(target)
        .with_chain_id(ctx.chain_id)
        .value(U256::ZERO)
        .input(input.into())
}

pub fn validate_roles(roles: &Roles, asg: Address, from: Address) -> Result<()> {
    ensure!(
        roles.gateway_operator != asg,
        "gateway already has ASG as operator, refusing to claim a separate-operator handoff"
    );
    ensure!(
        !roles.asg_is_its_operator,
        "ASG already has ITS operatorship, refusing to claim an ownership-only handoff"
    );
    for (name, owner) in [("ITS", roles.its_owner), ("gateway", roles.gateway_owner)] {
        ensure!(
            owner == from || owner == asg,
            "{name} owner is {owner}, not --from {from}. The actual owner must authorize the displayed transaction"
        );
    }
    Ok(())
}

pub fn operators_unchanged(before: &Roles, after: &Roles) -> Result<()> {
    ensure!(
        before.gateway_operator == after.gateway_operator
            && before.sender_is_its_operator == after.sender_is_its_operator
            && before.asg_is_its_operator == after.asg_is_its_operator,
        "operator roles changed during ownership handoff"
    );
    Ok(())
}

fn report(ctx: &Context, from: Address, asg: Address, roles: &Roles) -> Result<()> {
    checks::report(ctx, asg);
    ui::address(
        "current gateway owner",
        &roles.gateway_owner.to_checksum(None),
    );
    ui::address("current ITS owner", &roles.its_owner.to_checksum(None));
    ui::address(
        "gateway operator (retained)",
        &roles.gateway_operator.to_checksum(None),
    );
    ui::kv(
        "sender is ITS operator (retained)",
        &roles.sender_is_its_operator.to_string(),
    );
    ui::kv(
        "ASG is ITS operator",
        &roles.asg_is_its_operator.to_string(),
    );
    ui::warn(
        "Ownership grants ASG control of upgrades and owner functions. After transfer, owner actions use governance",
    );
    for (label, target) in [("ITS", ctx.its), ("gateway", ctx.gateway)] {
        ui::section(&format!(
            "{label}.transferOwnership({})",
            asg.to_checksum(None)
        ));
        transactions::preview(&ownership_request(ctx, from, target, asg))?;
    }
    Ok(())
}

pub async fn run<P: Provider>(
    provider: &P,
    ctx: &mut Context,
    options: &Options,
    selected: Option<Address>,
) -> Result<()> {
    let asg = selected.or(ctx.configured_asg).ok_or_else(|| {
        eyre::eyre!("deploy ASG first, or pass --asg to verify an existing deployment")
    })?;
    if let Some(configured) = ctx.configured_asg {
        ensure!(
            configured == asg,
            "--asg conflicts with configured ASG {configured}"
        );
    }
    let artifact = checks::artifact(ctx)?;
    let runtime =
        checks::expected_runtime(provider, checks::creation(ctx, &artifact), options.from).await?;
    checks::governance(provider, ctx, asg, &runtime).await?;
    let before = checks::roles(provider, ctx, options.from, asg).await?;
    report(ctx, options.from, asg, &before)?;
    if !options.apply {
        if let Err(error) = validate_roles(&before, asg, options.from) {
            ui::warn(&error.to_string());
        }
        ui::info("Preview only. Operatorship is retained. Use --apply for ownership transfers");
        return Ok(());
    }
    validate_roles(&before, asg, options.from)?;
    if before.gateway_owner == asg && before.its_owner == asg {
        ensure!(
            ui::confirm("Both owners are already ASG. Record verified state in config?").await,
            "declined"
        );
        record(ctx, asg, &runtime, &before)?;
    }
    for (label, target, owner) in [
        ("ITS", ctx.its, before.its_owner),
        ("gateway", ctx.gateway, before.gateway_owner),
    ] {
        if owner == asg {
            ui::success(&format!("{label} already owned by ASG"));
            continue;
        }
        checks::governance(provider, ctx, asg, &runtime).await?;
        let current = checks::roles(provider, ctx, options.from, asg).await?;
        operators_unchanged(&before, &current)?;
        validate_roles(&current, asg, options.from)?;
        ui::section(&format!("{label} ownership transfer to ASG"));
        transactions::send(
            provider,
            ctx,
            &ownership_request(ctx, options.from, target, asg),
            Some(asg),
        )
        .await?;
        let after = checks::roles(provider, ctx, options.from, asg).await?;
        operators_unchanged(&before, &after)?;
        let actual = if target == ctx.its {
            after.its_owner
        } else {
            after.gateway_owner
        };
        ensure!(
            actual == asg,
            "{label} ownership transfer did not take effect"
        );
        record(ctx, asg, &runtime, &after)?;
    }
    checks::governance(provider, ctx, asg, &runtime).await?;
    let after = checks::roles(provider, ctx, options.from, asg).await?;
    ensure!(
        after.gateway_owner == asg && after.its_owner == asg,
        "ownership changed before handoff completed"
    );
    operators_unchanged(&before, &after)?;
    ui::success("Gateway and ITS are owned by ASG, operational operators retained");
    Ok(())
}

fn record(ctx: &mut Context, asg: Address, runtime: &Bytes, roles: &Roles) -> Result<()> {
    config::save(
        ctx,
        &[
            (
                "AxelarServiceGovernance",
                json!({
                    "address": asg.to_checksum(None), "gateway": ctx.gateway.to_checksum(None),
                    "governanceChain": GOVERNANCE_CHAIN, "governanceAddress": GOVERNANCE_ADDRESS,
                    "operator": ctx.settings.operator.to_checksum(None), "minimumTimeDelay": ctx.settings.delay,
                    "codehash": keccak256(runtime),
                }),
            ),
            (
                "AxelarGateway",
                json!({"owner": roles.gateway_owner.to_checksum(None), "operator": roles.gateway_operator.to_checksum(None)}),
            ),
            (
                "InterchainTokenService",
                json!({"owner": roles.its_owner.to_checksum(None)}),
            ),
        ],
    )
}
