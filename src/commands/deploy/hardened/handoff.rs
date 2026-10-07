use crate::{
    state::{State, next_pending_step},
    types::Network,
    ui,
};
use eyre::Result;

use super::verifiers::contract;

pub(super) fn resume_command(state: &State) -> String {
    let activate = next_pending_step(state).is_some_and(|(next, _)| {
        state
            .steps
            .iter()
            .position(|step| step.name == "WaitForVerifierSet")
            .is_some_and(|checkpoint| next >= checkpoint)
    });
    format!(
        "axe --network {} deploy run --axelar-id {}{}",
        state.env,
        state.axelar_id,
        if activate { " --activate" } else { "" }
    )
}

pub(super) fn stopped(state: &State, error: &eyre::Report, paused: bool) {
    ui::section(if paused {
        "DEPLOYMENT PAUSED"
    } else {
        "DEPLOYMENT STOPPED — ERROR"
    });
    ui::kv(
        "Deployment",
        &format!("{} / {}", state.env, state.axelar_id),
    );
    if let Some((index, step)) = next_pending_step(state) {
        ui::kv(
            "Next unfinished step",
            &format!("{}/{} {}", index + 1, state.steps.len(), step.name),
        );
        if let Some(id) = step.proposal_key().and_then(|key| state.proposals.get(key)) {
            ui::kv("Saved proposal", &id.to_string());
        }
    }
    ui::kv("Why axe stopped", &error.to_string());
    if paused {
        ui::info(
            "Progress saved. You can close this terminal; axe is not monitoring in the background.",
        );
        ui::info("Next: complete the action described above, then run the continue command below.");
    } else {
        ui::info(
            "Next: resolve the error below before continuing. Keep the deployment state and journal; do not restart with deploy init.",
        );
    }
    ui::info(
        "On resume, axe checks recorded transactions before continuing unfinished steps. A connection error does not mean a submitted transaction failed.",
    );
    ui::info("CONTINUE THIS DEPLOYMENT (after the required action):");
    println!("\n  {}\n", resume_command(state));
    ui::info("CHECK PROGRESS (read-only):");
    println!(
        "\n  axe --network {} deploy status --axelar-id {}\n",
        state.env, state.axelar_id
    );
    ui::info(
        "For proposal voters, add --votes to the status command. If recovery requires extra flags, add the flags shown in the error to the continue command.",
    );
}

pub(super) fn proposal(state: &State, id: u64, voting_end: Option<&str>) {
    let mut lines = proposal_actions(state.env, id);
    if let Some(end) = voting_end {
        lines.push(format!("Voting ends: {end} (UTC)."));
    }
    lines.extend([
        "Wait until the proposal status is PASSED. Casting a vote does not end the voting period.".into(),
        "Axe checks every 15 seconds and continues when the proposal passes. You will be asked to approve each new transaction.".into(),
        "Ctrl+C is safe while waiting. Progress and the proposal are saved; resuming will not submit another proposal.".into(),
        "If you stop axe, continue this deployment with:".into(),
        format!("  {}", resume_command(state)),
        "Check progress and votes from another terminal:".into(),
        format!("  axe --network {} deploy status --axelar-id {} --votes", state.env, state.axelar_id),
    ]);
    show(&lines);
}

pub(super) fn proposal_actions(network: Network, id: u64) -> Vec<String> {
    let mut lines = vec![format!(
        "Vote on proposal {id} on {network} if you have not already voted."
    )];
    match network {
        Network::Testnet => lines.extend([
            "From the axe repo, select the testnet kubectl context and replace YOUR_VALIDATOR_NAMESPACE with the namespace containing your validator pods:".into(),
            format!("  bash scripts/vote_testnet_proposal.sh \"YOUR_VALIDATOR_NAMESPACE\" {id}"),
            "This helper votes Yes from every matching validator pod in that namespace; review the proposal before running it. Axe does not run it for you.".into(),
        ]),
        Network::Mainnet => lines.push("Coordinate voting with the mainnet validators through the established governance process.".into()),
        Network::Stagenet => lines.push("Coordinate voting with the stagenet validator operators.".into()),
        Network::DevnetAmplifier => lines.push("Devnet deployment normally executes directly; investigate this unexpected governance checkpoint.".into()),
    }
    lines
}

pub(super) async fn verifiers(state: &State) -> Result<()> {
    let multisig = contract(state, "Multisig", false).await?;
    let prover = contract(state, "MultisigProver", true).await?;
    let verifier = contract(state, "VotingVerifier", true).await?;
    let lines = verifier_actions(
        state.env,
        state.axelar_id.as_str(),
        &multisig,
        &prover,
        &verifier,
    );
    show(&lines);
    Ok(())
}

pub(super) fn verifier_actions(
    network: Network,
    chain: &str,
    multisig: &str,
    prover: &str,
    verifier: &str,
) -> Vec<String> {
    let mut lines =
        vec!["Cosmos setup is complete. Set up verifiers before deploying the EVM gateway.".into()];
    if network == Network::Testnet {
        lines.extend([
            "1. Open the testnet verifier rollout PR.".into(),
            "   Repository: https://github.com/axelarnetwork/infrastructure".into(),
            "   Update infrastructure/testnet/apps/axelar-testnet/ampd/ampd-epsilon/helm-values.yaml".into(),
            "   and infrastructure/testnet/apps/axelar-testnet/ampd/ampd/helm-values.yaml.".into(),
            "   Add the chain configuration below to config_toml.grpc.blockchain_service.chains for each applicable verifier deployment.".into(),
            format!("   Add handlers.{chain} with handler_type: evm, enabled: true and the chain RPC. Reuse the EVM handler image/version and settings from the current infra configuration."),
            "   Include the testnet-amplifiers workers in the rollout; the registration script below targets that fleet.".into(),
        ]);
    } else {
        lines.push(format!("Coordinate the {network} verifier operators to deploy their EVM handlers using the chain configuration below."));
    }
    lines.extend([
        "   Chain configuration for the verifier rollout:".into(),
        String::new(),
        format!("     - chain_name: {chain}"),
        format!("       multisig: {multisig}"),
        format!("       multisig_prover: {prover}"),
        format!("       voting_verifier: {verifier}"),
        String::new(),
        "   Use the chain RPC configured for this deployment; keep RPC credentials out of the PR."
            .into(),
    ]);
    if network == Network::Testnet {
        lines.extend([
            "2. Wait for the PR to be merged, deployed, and the verifier workers to be healthy.".into(),
            "3. Register the testnet verifier keys and chain support.".into(),
            "   From the axe repo root, with kubectl on the testnet context, run:".into(),
            String::new(),
            format!("   bash scripts/register_chain_support.sh {chain}"),
            String::new(),
            "   This script registers ECDSA public keys and amplifier chain support for workers 0–21 in testnet-amplifiers. Axe does not run it for you.".into(),
        ]);
    } else {
        lines.extend([
            format!("Ensure each operator has registered its ECDSA public key and chain support for service {} and chain {chain}.", network.verifier_service_name()),
            "Wait for the rollout and registrations to complete; axe will enforce the registry's verifier-count requirements.".into(),
        ]);
    }
    lines.extend([
        String::new(),
        "After rollout and chain-support registration, run the resume command below with --activate.".into(),
        "Axe will show the discovered verifier names, keys and threshold for approval before deploying the gateway.".into(),
    ]);
    lines
}

fn show(lines: &[String]) {
    ui::action_required(&lines.iter().map(String::as_str).collect::<Vec<_>>());
}

#[cfg(test)]
mod tests;
