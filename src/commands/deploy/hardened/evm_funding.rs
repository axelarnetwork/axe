use super::types::{Journal, Transaction};
use alloy::primitives::{Address, U256};
use eyre::Result;
use std::collections::BTreeMap;

/// Replacement hashes at one nonce compete. Attempts at retired nonces remain spent.
pub(super) fn liability(
    journal: &Journal,
    sender: Address,
    candidate: Option<(u64, U256)>,
) -> Result<U256> {
    let mut nonces = BTreeMap::<u64, U256>::new();
    for tx in journal
        .actions
        .values()
        .chain(journal.attempts.values().flatten())
    {
        if let Transaction::Evm {
            sender: from,
            nonce,
            gas_cost,
            ..
        } = tx
            && *from == sender
        {
            let maximum = nonces.entry(*nonce).or_default();
            *maximum = (*maximum).max(gas_cost.parse()?);
        }
    }
    if let Some((nonce, cost)) = candidate {
        let maximum = nonces.entry(nonce).or_default();
        *maximum = (*maximum).max(cost);
    }
    nonces.values().try_fold(U256::ZERO, |sum, cost| {
        sum.checked_add(*cost)
            .ok_or_else(|| eyre::eyre!("gas budget overflow"))
    })
}
