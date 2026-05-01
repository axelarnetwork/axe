//! Default values for cosmos governance / reward / voting-verifier
//! parameters. Called out into one place so that drifting them is a one-line
//! change and so the constants document where they came from.

/// Default deposit (in `uaxl`) attached to a cosmos governance proposal.
/// Falls back to this when `.env` doesn't override `PROPOSAL_DEPOSIT`.
pub(super) const DEFAULT_PROPOSAL_DEPOSIT_UAXL: &str = "3000000000";

/// Multisig proposal `reward_amount` per signer, in `uaxl`.
pub(super) const DEFAULT_REWARD_AMOUNT_UAXL: &str = "1000000";

/// Default `block_expiry` for the VotingVerifier when the chain config
/// doesn't supply one. 50 blocks ≈ poll-window default Axelar advertises.
pub(super) const DEFAULT_VV_BLOCK_EXPIRY: u64 = 50;
