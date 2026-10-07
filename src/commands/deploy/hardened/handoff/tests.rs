use super::{proposal_actions, resume_command, verifier_actions};
use crate::{state::StepStatus, types::Network};

#[test]
fn resume_uses_axe_and_activation_only_when_ready() {
    let mut state = crate::commands::deploy::hardened::tests::initial_state();
    assert!(resume_command(&state).starts_with("axe --network "));
    assert!(!resume_command(&state).contains("--activate"));
    for step in &mut state.steps {
        if step.name == "WaitForVerifierSet" {
            break;
        }
        step.status = StepStatus::Completed;
    }
    assert!(resume_command(&state).ends_with(" --activate"));
    for step in &mut state.steps {
        if step.name == "RegisterItsOnHub" {
            break;
        }
        step.status = StepStatus::Completed;
    }
    assert!(resume_command(&state).ends_with(" --activate"));
}

#[test]
fn testnet_helpers_are_network_scoped_and_use_the_actual_proposal_and_chain() {
    let voting = proposal_actions(Network::Testnet, 651).join("\n");
    assert!(
        voting.contains("bash scripts/vote_testnet_proposal.sh \"YOUR_VALIDATOR_NAMESPACE\" 651")
    );
    let setup = verifier_actions(
        Network::Testnet,
        "unichain-sepolia",
        "multisig",
        "prover",
        "verifier",
    )
    .join("\n");
    assert!(
        setup.contains(
            "infrastructure/testnet/apps/axelar-testnet/ampd/ampd-epsilon/helm-values.yaml"
        )
    );
    assert!(setup.contains("config_toml.grpc.blockchain_service.chains"));
    assert!(setup.contains("handlers.unichain-sepolia"));
    assert!(setup.contains("bash scripts/register_chain_support.sh unichain-sepolia"));
    let configuration = setup.find("- chain_name: unichain-sepolia").unwrap();
    let wait = setup.find("2. Wait").unwrap();
    let register = setup.find("3. Register").unwrap();
    assert!(setup.find("1. Open").unwrap() < configuration);
    assert!(configuration < wait && wait < register);
    assert!(
        register
            < setup
                .find("bash scripts/register_chain_support.sh")
                .unwrap()
    );
    for network in [
        Network::Mainnet,
        Network::Stagenet,
        Network::DevnetAmplifier,
    ] {
        assert!(
            !proposal_actions(network, 651)
                .join("\n")
                .contains("vote_testnet_proposal.sh")
        );
        assert!(
            !verifier_actions(network, "chain", "multisig", "prover", "verifier")
                .join("\n")
                .contains("scripts/register_chain_support.sh")
        );
    }
}
