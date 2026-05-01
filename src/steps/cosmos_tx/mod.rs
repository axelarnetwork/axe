//! Cosmos-side `Step` runners that interact with Axelar amplifier contracts
//! by submitting tx — either directly from the relayer wallet or wrapped in
//! a governance proposal on non-devnet networks. Each handler matches one
//! `step_name` and lives in its own submodule.

mod defaults;
mod instantiate;
mod register_deployment;
mod register_its;
mod reward_pools;

use std::fs;

use eyre::Result;
use serde_json::Value;

use crate::commands::deploy::DeployContext;
use crate::cosmos::{derive_axelar_wallet, read_axelar_config};
use crate::state::Step;
use crate::types::Network;

pub async fn run(ctx: &mut DeployContext, step: &Step, step_name: &str) -> Result<()> {
    let mnemonic = ctx.state.mnemonic.clone();
    let env = ctx.state.env;
    let (signing_key, axelar_address) = derive_axelar_wallet(&mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = read_axelar_config(&ctx.target_json)?;
    let use_governance = env != Network::DevnetAmplifier;

    let chain_axelar_id = {
        let content = fs::read_to_string(&ctx.target_json)?;
        let root: Value = serde_json::from_str(&content)?;
        root.pointer(&format!("/chains/{}/axelarId", ctx.axelar_id))
            .and_then(|v| v.as_str())
            .unwrap_or(&ctx.axelar_id)
            .to_string()
    };

    let proposal_key = step.proposal_key().unwrap_or("").to_string();

    match step_name {
        "InstantiateChainContracts" => {
            instantiate::run_instantiate(
                ctx,
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                use_governance,
                &chain_axelar_id,
                env.as_str(),
                &proposal_key,
            )
            .await?;
        }
        "RegisterDeployment" => {
            register_deployment::run_register_deployment(
                ctx,
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                use_governance,
                &chain_axelar_id,
                env.as_str(),
                &proposal_key,
            )
            .await?;
        }
        "CreateRewardPools" => {
            reward_pools::run_create_reward_pools(
                ctx,
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                use_governance,
                &chain_axelar_id,
                env.as_str(),
                &proposal_key,
            )
            .await?;
        }
        "AddRewards" => {
            reward_pools::run_add_rewards(
                ctx,
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                &chain_axelar_id,
            )
            .await?;
        }
        "RegisterItsOnHub" => {
            register_its::run_register_its_on_hub(
                ctx,
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                use_governance,
                &chain_axelar_id,
                env.as_str(),
                &proposal_key,
            )
            .await?;
        }
        _ => {
            return Err(eyre::eyre!("unknown cosmos-tx step: {step_name}"));
        }
    }

    Ok(())
}
