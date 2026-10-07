//! Reward pool creation messages for batch 2 and direct journaled reward payments.

use cosmos_sdk_proto::cosmos::base::v1beta1::Coin as ProtoCoin;
use eyre::Result;
use serde_json::{Value, json};

use super::StepTxContext;
use crate::commands::deploy::DeployContext;
use crate::cosmos::{
    build_execute_msg_any, build_execute_msg_any_with_funds, read_axelar_contract_field,
    sign_and_broadcast_cosmos_tx,
};
use crate::ui;

pub(crate) fn reward_pool_messages(
    env: &str,
    chain: &str,
    voting_verifier: &str,
    multisig: &str,
) -> [Value; 2] {
    let (epoch_duration, participation_threshold, rewards_per_epoch) = match env {
        "devnet-amplifier" => ("100", json!(["7", "10"]), "100"),
        "mainnet" => ("14845", json!(["8", "10"]), "3424660000"),
        _ => ("600", json!(["7", "10"]), "100"),
    };
    let create = |contract: &str| {
        json!({
            "create_pool": {
                "params": {
                    "epoch_duration": epoch_duration,
                    "participation_threshold": participation_threshold,
                    "rewards_per_epoch": rewards_per_epoch
                },
                "pool_id": {
                    "chain_name": chain,
                    "contract": contract
                }
            }
        })
    };
    [create(voting_verifier), create(multisig)]
}

pub(super) async fn run_add_rewards(ctx: &DeployContext, tx: StepTxContext<'_>) -> Result<()> {
    let StepTxContext {
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        chain_axelar_id,
        ..
    } = tx;
    ui::info(&format!("adding rewards for {chain_axelar_id}..."));

    let rewards_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Rewards/address").await?;
    let multisig_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Multisig/address").await?;
    let voting_verifier_addr = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/VotingVerifier/{chain_axelar_id}/address"),
    )
    .await?;

    let reward_amount = ctx
        .state
        .hardened_plan
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing deployment plan"))?
        .reward_amount
        .as_str();
    let funds = vec![ProtoCoin {
        denom: fee_denom.to_string(),
        amount: reward_amount.to_string(),
    }];

    let msg1 = json!({
        "add_rewards": {
            "pool_id": {
                "chain_name": chain_axelar_id,
                "contract": multisig_addr
            }
        }
    });
    let msg2 = json!({
        "add_rewards": {
            "pool_id": {
                "chain_name": chain_axelar_id,
                "contract": voting_verifier_addr
            }
        }
    });

    let inner_msg1 =
        build_execute_msg_any_with_funds(axelar_address, &rewards_addr, &msg1, funds.clone())?;
    let inner_msg2 = build_execute_msg_any_with_funds(axelar_address, &rewards_addr, &msg2, funds)?;

    ui::info(&format!(
        "sending {reward_amount}{fee_denom} to each reward pool"
    ));
    let tx_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        vec![inner_msg1, inner_msg2],
    )
    .await?;

    let code = tx_resp
        .pointer("/tx_response/code")
        .and_then(|v: &Value| v.as_u64())
        .unwrap_or(0);
    if code != 0 {
        let raw_log = tx_resp
            .pointer("/tx_response/raw_log")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(eyre::eyre!(
            "add_rewards tx failed (code {code}): {raw_log}"
        ));
    }
    ui::success("rewards added to both pools");

    Ok(())
}

pub(super) async fn batch_messages(
    ctx: &DeployContext,
    sender: &str,
    chain: &str,
    env: &str,
) -> Result<Vec<cosmrs::Any>> {
    let rewards =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Rewards/address").await?;
    let multisig =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Multisig/address").await?;
    let verifier = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/VotingVerifier/{chain}/address"),
    )
    .await?;
    reward_pool_messages(env, chain, &verifier, &multisig)
        .iter()
        .map(|message| build_execute_msg_any(sender, &rewards, message))
        .collect()
}
