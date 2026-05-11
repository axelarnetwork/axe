//! Cross-cutting polling and timeout constants.
//!
//! These are the wait-loop knobs the deploy/test/load-test commands use to
//! drive the various Cosmos LCD, EVM RPC, and destination-chain checks.
//! Centralising them here keeps every retry loop honest about what it's
//! waiting for and makes the budgets reviewable in one place.

use std::time::Duration;

// ---------------------------------------------------------------------------
// EVM
// ---------------------------------------------------------------------------

/// How long we wait for a pending EVM tx to land before giving up. 120s is
/// generous for any production EVM and lets relayer races resolve cleanly.
pub const EVM_TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Cosmos LCD
// ---------------------------------------------------------------------------

/// Sleep between LCD `txs/{hash}` polls when waiting for a broadcast tx to
/// land in a block.
pub const LCD_WAIT_RETRY_INTERVAL: Duration = Duration::from_secs(3);

/// Number of `LCD_WAIT_RETRY_INTERVAL` ticks before `lcd_wait_for_tx` errors.
/// 30 × 3s = 90s — well above any sane block time.
pub const LCD_WAIT_MAX_ATTEMPTS: usize = 30;

// ---------------------------------------------------------------------------
// Amplifier verify/route/execute polling
// ---------------------------------------------------------------------------

/// Cadence for the verify_messages → end_poll → route_messages retry loops
/// (used in test_helpers and the legacy test_gmp/test_its retry blocks).
pub const AMPLIFIER_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// 5-minute budget at `AMPLIFIER_POLL_INTERVAL` cadence (60 × 5s).
pub const AMPLIFIER_POLL_ATTEMPTS_5MIN: usize = 60;

/// 10-minute budget at `AMPLIFIER_POLL_INTERVAL` cadence (120 × 5s). Used
/// for slower waits like `wait_for_proof` and AxelarnetGateway approval.
pub const AMPLIFIER_POLL_ATTEMPTS_10MIN: usize = 120;

// ---------------------------------------------------------------------------
// Destination-chain polling (relayer delivery)
// ---------------------------------------------------------------------------

/// Cadence for polling the destination chain for token deploy / balance
/// delta after a relay completes.
pub const DEST_CHAIN_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// 5-minute budget at `DEST_CHAIN_POLL_INTERVAL` cadence (30 × 10s).
pub const DEST_CHAIN_POLL_ATTEMPTS: usize = 30;

// ---------------------------------------------------------------------------
// Verifier-set rotation
// ---------------------------------------------------------------------------

/// Cadence for `wait_verifier_set` while polling the multisig prover for
/// the new verifier set after a rotation has been kicked off on cosmos.
pub const VERIFIER_SET_POLL_INTERVAL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Cosmos governance proposal polling
// ---------------------------------------------------------------------------

/// Cadence for `cosmos_poll` while waiting on a governance proposal to move
/// from voting → passed/rejected. 10s ≪ Axelar's voting period so the loop
/// won't miss the transition.
pub const COSMOS_PROPOSAL_POLL_INTERVAL: Duration = Duration::from_secs(10);
