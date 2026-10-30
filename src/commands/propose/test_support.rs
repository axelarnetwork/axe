use super::helpers::AsgInfo;
use super::types::{EdgeCall, ProposalPlan, ProposalType, ResolvedConfig};
use alloy::primitives::{Address, Bytes};

pub(super) fn config(lcd: String) -> ResolvedConfig {
    ResolvedConfig {
        edge_axelar_id: "flow".into(),
        asg_address: Address::from([2; 20]).to_string(),
        gateway_address: Address::from([3; 20]).to_string(),
        its_address: Some(Address::from([4; 20]).to_string()),
        edge_rpc: String::new(),
        multisig_prover: Some(String::new()),
        axelar_rpc: String::new(),
        axelarnet_gateway: "axelar1gateway".into(),
        gov_module: "axelar1gov".into(),
        lcd,
        chain_id: "axelar-testnet-lisbon-3".into(),
        fee_denom: "uaxl".into(),
        gas_price: 0.007,
        deposit_amount: "2000000000".into(),
        expedited_deposit_amount: "3000000000".into(),
    }
}

/// A one-call plan whose ASG reports the config's gov module, as a real one would.
pub(super) fn plan(
    cfg: ResolvedConfig,
    ptype: ProposalType,
    target: Address,
    calldata: Bytes,
) -> ProposalPlan {
    let asg = AsgInfo {
        operator: Address::ZERO,
        governance_chain: "axelar".into(),
        governance_address: cfg.gov_module.clone(),
        minimum_time_lock_delay: 300,
    };
    let call = EdgeCall {
        chain: cfg.edge_axelar_id.clone(),
        cfg,
        asg,
        target,
        calldata,
    };
    ProposalPlan {
        ptype,
        action_label: "test".into(),
        calls: vec![call],
    }
}
