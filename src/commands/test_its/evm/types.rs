use alloy::primitives::{Address, B256, U256};
use alloy::providers::DynProvider;

use super::super::{ConfigRoute, LegacyRelayContext};

pub(super) struct Context {
    pub route: ConfigRoute,
    pub relay: LegacyRelayContext,
    pub axelar_rpc: String,
    pub source: DynProvider,
    pub destination: DynProvider,
    pub factory: Address,
    pub source_its: Address,
    pub signer: Address,
    pub amount: U256,
    pub gas_value: U256,
}

pub(super) struct Token {
    pub id: B256,
    pub local: Address,
    pub remote: Address,
}
