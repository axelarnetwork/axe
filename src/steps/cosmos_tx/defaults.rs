//! Default values for cosmos governance / reward / voting-verifier
//! parameters. Called out into one place so that drifting them is a one-line
//! change and so the constants document where they came from.

/// Default `block_expiry` for the VotingVerifier when the chain config
/// doesn't supply one. 50 blocks ≈ poll-window default Axelar advertises.
pub(super) const DEFAULT_VV_BLOCK_EXPIRY: u64 = 50;
