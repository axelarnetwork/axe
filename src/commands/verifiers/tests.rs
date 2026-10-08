use std::collections::BTreeSet;

use super::{TESTNET_VERIFIERS, lookup_name};
use crate::types::Network;

#[test]
fn axelar_testnet_fleet_addresses_are_valid_unique_and_network_scoped() {
    let fleet: Vec<_> = TESTNET_VERIFIERS
        .iter()
        .filter(|(_, name)| *name == "Axelar testnet")
        .collect();
    assert_eq!(fleet.len(), 22);
    let mut addresses = BTreeSet::new();
    for (address, _) in fleet {
        let account: cosmrs::AccountId = address.parse().unwrap();
        assert_eq!(account.prefix(), "axelar");
        assert!(addresses.insert(address));
        assert_eq!(
            lookup_name(Network::Testnet, address),
            Some("Axelar testnet")
        );
        for network in [
            Network::Mainnet,
            Network::Stagenet,
            Network::DevnetAmplifier,
        ] {
            assert_eq!(lookup_name(network, address), None);
        }
    }
}
