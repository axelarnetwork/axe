//! `RegisterItsOnHub` step. Tells the InterchainTokenService Hub about the
//! chain's ITS edge contract + ABI translator. Wrapped in a governance
//! proposal except on devnet-amplifier.

use eyre::Result;
use serde_json::{Value, json};

use super::StepTxContext;
use crate::commands::deploy::DeployContext;
use crate::cosmos::{build_execute_msg_any, read_axelar_contract_field};
use crate::ui;

struct ItsHubRegistration {
    hub_address: String,
    governance_address: String,
    edge_contract: String,
    message_translator: String,
}

async fn read_registration(ctx: &DeployContext) -> Result<ItsHubRegistration> {
    let content = tokio::fs::read_to_string(&ctx.target_json).await?;
    let root: Value = serde_json::from_str(&content)?;
    let edge_contract = root
        .pointer(&format!(
            "/chains/{}/contracts/InterchainTokenService/address",
            ctx.axelar_id
        ))
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            eyre::eyre!(
                "no InterchainTokenService address for {} — run DeployInterchainTokenService first",
                ctx.axelar_id
            )
        })?
        .to_string();
    let message_translator = root
        .pointer("/axelar/contracts/ItsAbiTranslator/address")
        .and_then(|value| value.as_str())
        .ok_or_else(|| eyre::eyre!("no axelar.contracts.ItsAbiTranslator.address in target JSON"))?
        .to_string();
    Ok(ItsHubRegistration {
        hub_address: read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/contracts/InterchainTokenService/address",
        )
        .await?,
        governance_address: read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/governanceAddress",
        )
        .await?,
        edge_contract,
        message_translator,
    })
}

pub(super) async fn run_register_its_on_hub(
    ctx: &mut DeployContext,
    tx: StepTxContext<'_>,
) -> Result<()> {
    let StepTxContext {
        axelar_address,
        chain_axelar_id,
        ..
    } = tx;
    ui::info(&format!("registering {chain_axelar_id} on ITS Hub..."));

    let registration = read_registration(ctx).await?;

    let execute_msg = json!({
        "register_chains": {
            "chains": [{
                "chain": chain_axelar_id,
                "its_edge_contract": registration.edge_contract,
                "msg_translator": registration.message_translator,
                "truncation": {
                    "max_uint_bits": 256,
                    "max_decimals_when_truncating": 255
                }
            }]
        }
    });

    let json_str = serde_json::to_string_pretty(&execute_msg)?;
    ui::info(&format!(
        "execute msg: {}",
        ui::truncated_json(&json_str, 3)
    ));

    let sender = if ctx.state.env.deployment_uses_governance() {
        registration.governance_address.as_str()
    } else {
        axelar_address
    };
    let inner_msg = build_execute_msg_any(sender, &registration.hub_address, &execute_msg)?;

    let title = format!("Register {chain_axelar_id} on ITS Hub");
    super::submission::submit(ctx, tx, vec![inner_msg], &title).await
}
