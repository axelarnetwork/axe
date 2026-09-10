use super::{is_collision, reset_after_collision};
use crate::state::{StepStatus, default_steps};
use crate::steps::cosmos_tx::instantiate::types::Proposal;

#[test]
fn collision_recovery_only_resets_instantiation() {
    let mut steps = default_steps();
    for step in steps.iter_mut().take(6) {
        step.status = StepStatus::Completed;
    }
    let before = serde_json::to_value(&steps).unwrap();
    let proposal = Proposal {
        status: "PROPOSAL_STATUS_FAILED".into(),
        failed_reason: "contract address already exists, try a different combination of creator, checksum and salt: duplicate".into(),
    };
    assert!(reset_after_collision(&mut steps, &proposal));
    assert_eq!(steps[5].status, StepStatus::Pending);
    steps[5].status = StepStatus::Completed;
    assert_eq!(serde_json::to_value(&steps).unwrap(), before);
}

#[test]
fn pending_rejected_and_unrelated_failures_do_not_trigger_retry() {
    for (status, reason) in [
        ("PROPOSAL_STATUS_VOTING_PERIOD", ""),
        ("PROPOSAL_STATUS_PASSED", ""),
        (
            "PROPOSAL_STATUS_REJECTED",
            "contract address already exists",
        ),
        ("PROPOSAL_STATUS_FAILED", "out of gas"),
    ] {
        let mut steps = default_steps();
        let before = serde_json::to_value(&steps).unwrap();
        let proposal = Proposal {
            status: status.into(),
            failed_reason: reason.into(),
        };
        assert!(!is_collision(&proposal));
        assert!(!reset_after_collision(&mut steps, &proposal));
        assert_eq!(serde_json::to_value(&steps).unwrap(), before);
    }
}
