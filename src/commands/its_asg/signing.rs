use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, ensure};

use super::types::TransactionAction;

pub const SENDER_KEY_ENV: &str = "ASG_PRIVATE_KEY";
pub const DEPLOYER_KEY_ENV: &str = "ASG_DEPLOYER_PRIVATE_KEY";
pub const OPERATOR_KEY_ENV: &str = "EVM_GOVERNANCE_OPERATOR_KEY";

pub(super) fn read_signer(
    variable: &str,
    expected: Address,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<PrivateKeySigner> {
    let key = lookup(variable)
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| eyre::eyre!("set {variable} for address {expected}"))?;
    let signer: PrivateKeySigner = key
        .trim()
        .parse()
        .map_err(|_| eyre::eyre!("invalid {variable}"))?;
    ensure!(
        signer.address() == expected,
        "{variable} must derive {expected}, got {}",
        signer.address()
    );
    Ok(signer)
}

pub fn key_variable(action: TransactionAction) -> &'static str {
    match action {
        TransactionAction::Deploy => DEPLOYER_KEY_ENV,
        TransactionAction::Handoff(_) => SENDER_KEY_ENV,
    }
}

pub fn sender(from: Address, action: TransactionAction) -> Result<PrivateKeySigner> {
    read_signer(key_variable(action), from, |name| std::env::var(name).ok())
}

pub fn require_operator(expected: Address) -> Result<()> {
    // Prove the network operator key is available before committing to ASG.
    // This key does not sign deployment or ownership transactions.
    read_signer(OPERATOR_KEY_ENV, expected, |name| std::env::var(name).ok())?;
    Ok(())
}
