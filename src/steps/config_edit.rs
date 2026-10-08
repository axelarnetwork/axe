use eyre::Result;
use serde_json::{Value, json};

use crate::commands::deploy::DeployContext;
use crate::ui;

pub async fn run(ctx: &DeployContext) -> Result<()> {
    let predicted_addr = ctx
        .state
        .predicted_gateway_address
        .ok_or_else(|| {
            eyre::eyre!("no predictedGatewayAddress in state. Run predict-address step first")
        })?
        .to_string();
    let plan = ctx
        .state
        .hardened_plan
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing deployment plan"))?;

    let content = tokio::fs::read_to_string(&ctx.target_json).await?;
    let mut root: Value = serde_json::from_str(&content)?;

    let chain_axelar_id = root
        .pointer(&format!("/chains/{}/axelarId", ctx.axelar_id))
        .and_then(|v| v.as_str())
        .unwrap_or(&ctx.axelar_id)
        .to_string();

    let governance_address = root["axelar"]["governanceAddress"].clone();

    // Add VotingVerifier chain config
    let voting_verifier_config = json!({
        "governanceAddress": governance_address,
        "serviceName": ctx.state.env.verifier_service_name(),
        "sourceGatewayAddress": predicted_addr,
        "votingThreshold": plan.voting_threshold.map(|v| v.to_string()),
        "blockExpiry": plan.block_expiry,
        "confirmationHeight": plan.confirmation_height,
        "msgIdFormat": "hex_tx_hash_and_event_index",
        "addressFormat": "eip55"
    });

    let vv = root
        .pointer_mut("/axelar/contracts/VotingVerifier")
        .ok_or_else(|| eyre::eyre!("no axelar.contracts.VotingVerifier in target json"))?
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("VotingVerifier is not an object"))?;
    vv.insert(chain_axelar_id.clone(), voting_verifier_config);
    ui::success(&format!("added VotingVerifier.{chain_axelar_id} config"));

    // Add MultisigProver chain config
    let multisig_prover_config = json!({
        "governanceAddress": governance_address,
        "adminAddress": plan.prover_admin,
        "signingThreshold": plan.signing_threshold.map(|v| v.to_string()),
        "serviceName": ctx.state.env.verifier_service_name(),
        "verifierSetDiffThreshold": 0,
        "encoder": "abi",
        "keyType": "ecdsa"
    });

    let mp = root
        .pointer_mut("/axelar/contracts/MultisigProver")
        .ok_or_else(|| eyre::eyre!("no axelar.contracts.MultisigProver in target json"))?
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("MultisigProver is not an object"))?;
    mp.insert(chain_axelar_id.clone(), multisig_prover_config);
    ui::success(&format!("added MultisigProver.{chain_axelar_id} config"));

    crate::commands::deploy::hardened::storage::atomic_config_write(
        &ctx.target_json,
        (serde_json::to_string_pretty(&root)? + "\n").as_bytes(),
    )?;
    ui::success(&format!("updated {}", ctx.target_json.display()));

    Ok(())
}
