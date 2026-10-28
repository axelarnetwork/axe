use super::{
    plan, preflight,
    tests::{example_plan, initial_state},
};
use crate::state::{StepKind, StepStatus};

#[test]
fn new_flow_has_two_proposals_and_keeps_funding_and_activation_after_verification() {
    let state = initial_state();
    plan::validate_steps(&state).unwrap();
    let keys: Vec<_> = state
        .steps
        .iter()
        .filter_map(|step| match &step.kind {
            StepKind::CosmosPoll { proposal_key } => Some(proposal_key.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(keys, ["instantiate", "itsHubRegister"]);
    let position = |name| {
        state
            .steps
            .iter()
            .position(|step| step.name == name)
            .unwrap()
    };
    assert!(position("WaitInstantiateProposal") < position("SaveDeployedContracts"));
    assert!(position("SaveDeployedContracts") < position("AddRewards"));
    assert!(position("AddRewards") < position("WaitForVerifierSet"));
    assert!(position("WaitForVerifierSet") < position("AxelarGateway"));
    assert!(position("DeployInterchainTokenService") < position("RegisterItsOnHub"));
}

#[test]
fn changed_step_layout_is_rejected() {
    let mut state = initial_state();
    plan::validate_steps(&state).unwrap();
    state.steps.insert(8, state.steps[5].clone());
    assert!(plan::validate_steps(&state).is_err());
    state.steps = plan::steps(&example_plan()).unwrap();
    state.steps.swap(6, 7);
    assert!(plan::validate_steps(&state).is_err());
    state.steps = plan::steps(&example_plan()).unwrap();
    state.steps.pop();
    assert!(plan::validate_steps(&state).is_err());
}

#[test]
fn deposit_reservation_tracks_the_two_proposals() {
    let mut state = initial_state();
    assert_eq!(
        preflight::proposal_deposit_status(&state, &|_| false),
        (true, false)
    );
    // Submission is journaled but the proposal ID/state have not yet been saved.
    assert_eq!(
        preflight::proposal_deposit_status(&state, &|s| s == "InstantiateChainContracts"),
        (true, true)
    );
    state.proposals.insert("instantiate".into(), 1);
    assert_eq!(
        preflight::proposal_deposit_status(&state, &|_| false),
        (true, true)
    );
    for step in &mut state.steps {
        step.status = StepStatus::Completed;
    }
    state.proposals.insert("itsHubRegister".into(), 2);
    assert_eq!(
        preflight::proposal_deposit_status(&state, &|_| false),
        (false, false)
    );
}
