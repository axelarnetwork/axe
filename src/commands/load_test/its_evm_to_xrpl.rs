//! EVM -> XRPL ITS load test.
//!
//! Scope: transfers `axlXRP` (the interchain token registered for XRP on the
//! EVM side) from an EVM source to XRPL, where the Axelar multisig pays out
//! native XRP to the recipient address. No trust lines are needed because
//! the payout is native XRP drops.
//!
//! Full wiring of the XRPL-destination checker is staged behind a follow-up
//! change so the XRPL → EVM direction (which has simpler destination-side
//! semantics) can land first.

use std::time::Instant;

use eyre::{Result, eyre};

use super::LoadTestArgs;

pub async fn run(_args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    Err(eyre!(
        "EVM → XRPL ITS load tests are not yet supported in this build. \
         The XRPL → EVM direction is implemented. \
         EVM → XRPL requires the destination-side XRPL verifier, which is pending."
    ))
}
