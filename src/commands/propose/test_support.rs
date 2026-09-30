use super::types::ResolvedConfig;
use alloy::primitives::Address;

pub(super) fn config(lcd: String) -> ResolvedConfig {
    ResolvedConfig {
        edge_axelar_id: "flow".into(),
        asg_address: Address::from([2; 20]).to_string(),
        gateway_address: Address::from([3; 20]).to_string(),
        its_address: Some(Address::from([4; 20]).to_string()),
        edge_rpc: String::new(),
        multisig_prover: String::new(),
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
