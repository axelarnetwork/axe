//! Transaction-result types returned by [`super::rpc::StellarClient`] after a
//! successful submit + poll cycle.

use stellar_xdr::curr::ScVal;

#[derive(Debug, Clone)]
pub struct InvokedTx {
    pub tx_hash_hex: String,
    pub success: bool,
    /// Flat index (across all ops) of the ContractEvent emitted by the filter
    /// contract. `None` if no filter provided or no matching event.
    pub event_index: Option<u32>,
    /// The Soroban contract's return value, if any. Populated for SUCCESS
    /// responses on `TransactionMeta::V3` (testnet) and `V4`. Used to read
    /// out e.g. the token_id returned by `deploy_interchain_token`.
    pub return_value: Option<ScVal>,
}
