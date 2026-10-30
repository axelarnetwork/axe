//! The ITS Hub's chain registry: which chains the hub routes to today and
//! under which exact names. Both halves of the trust matrix start here, so
//! the list is what the hub knows, not what a config file remembers.

use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::json;

use crate::config::{AxelarGlobalContract, ChainsConfig};
use crate::cosmos::lcd_cosmwasm_smart_query;
use crate::types::Network;

/// Mainnet chains whose networks were shut down or whose integrations Axelar
/// discontinued. The ITS Hub may still list some of them as live.
pub const MAINNET_DEPRECATED: &[&str] = &["moonbeam", "berachain", "flow", "plume"];

/// Page size for the hub's `its_chains` query.
const PAGE: u32 = 100;

/// One `its_chains` entry on the ITS Hub. `chain` is the hub's own spelling.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RegisteredChain {
    pub chain: String,
    pub its_edge_contract: String,
    pub frozen: bool,
}

/// Every chain registered on the ITS Hub, in registry order.
pub async fn registered_chains(config: &ChainsConfig) -> Result<Vec<RegisteredChain>> {
    let (lcd, _, _, _) = config.axelar.cosmos_tx_params()?;
    let hub = config
        .axelar
        .global_contract_address(AxelarGlobalContract::InterchainTokenService)?;
    let mut chains: Vec<RegisteredChain> = Vec::new();
    let mut start_after: Option<String> = None;
    loop {
        let query = json!({"its_chains": {"start_after": start_after, "limit": PAGE}});
        let page: Vec<RegisteredChain> = serde_json::from_value(
            lcd_cosmwasm_smart_query(&lcd, hub, &query)
                .await
                .wrap_err("cannot list ITS Hub chains")?,
        )
        .wrap_err("unexpected ITS Hub its_chains response")?;
        let count = page.len();
        start_after = page.last().map(|chain| chain.chain.clone());
        chains.extend(page);
        if count < PAGE as usize {
            return Ok(chains);
        }
    }
}

/// Whether `chain` (config key or hub spelling) is discontinued on this network.
pub fn is_deprecated(network: Network, chain: &str) -> bool {
    network == Network::Mainnet
        && MAINNET_DEPRECATED
            .iter()
            .any(|deprecated| deprecated.eq_ignore_ascii_case(chain))
}
