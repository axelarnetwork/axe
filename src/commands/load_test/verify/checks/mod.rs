//! Per-chain destination check helpers used by the polling pipeline and
//! the orchestrators in this `verify` module.

mod evm;
mod solana;

pub(super) use evm::check_evm_is_message_approved;
pub(super) use solana::{batch_check_solana_incoming_messages, check_solana_incoming_message};
