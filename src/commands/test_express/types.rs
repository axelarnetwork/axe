//! Typed view over the Axelarscan GMP API records this monitor reads.
//!
//! The live `/gmp/searchGMP` record carries ~50 fields; we model only the
//! subset the two-phase express-reimbursement check needs. Everything is
//! `Option` / `#[serde(default)]` so a partial record never fails to parse.

use serde::Deserialize;

/// One GMP message record as returned by `searchGMP`.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpressRecord {
    #[serde(default)]
    pub command_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// Present only when an express execution happened on the destination.
    #[serde(default)]
    pub express_executed: Option<ExpressExecuted>,
    /// Present once the canonical `ITS.execute` landed (carries the
    /// `ExpressExecutionFulfilled` reimbursement atomically).
    #[serde(default)]
    pub executed: Option<Executed>,
    #[serde(default)]
    pub interchain_transfer: Option<InterchainTransfer>,
    #[serde(default)]
    pub call: Option<Call>,
}

/// The `express_executed` sub-object: who fronted the funds and where.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpressExecuted {
    #[serde(default)]
    pub chain: Option<String>,
    #[serde(rename = "sourceChain", default)]
    pub source_chain: Option<String>,
    #[serde(rename = "transactionHash", default)]
    pub transaction_hash: Option<String>,
    /// The express executor contract (e.g. the Squid router).
    #[serde(default)]
    pub contract_address: Option<String>,
    /// Top-level `from` mirrors `receipt.from` (the relayer EOA).
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub receipt: Option<Receipt>,
}

impl ExpressExecuted {
    /// The EOA that fronted the funds: `receipt.from`, falling back to the
    /// mirrored top-level `from`.
    pub fn relayer_eoa(&self) -> Option<&str> {
        self.receipt
            .as_ref()
            .and_then(|r| r.from.as_deref())
            .or(self.from.as_deref())
    }
}

/// The `executed` sub-object: the canonical execute that triggers reimbursement.
/// The `ExpressExecutionFulfilled` reimbursement fires atomically inside this tx.
#[derive(Debug, Clone, Deserialize)]
pub struct Executed {
    #[serde(rename = "transactionHash", default)]
    pub transaction_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Receipt {
    #[serde(default)]
    pub from: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InterchainTransfer {
    #[serde(rename = "destinationChain", default)]
    pub destination_chain: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Call {
    #[serde(rename = "returnValues", default)]
    pub return_values: Option<CallReturnValues>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallReturnValues {
    #[serde(rename = "sourceChain", default)]
    pub source_chain: Option<String>,
    #[serde(rename = "destinationChain", default)]
    pub destination_chain: Option<String>,
}

/// Whether the express (front-the-funds) leg has been observed.
#[derive(Debug, Clone)]
pub enum Phase1 {
    /// `express_executed` present. Carries the executor EOA / contract and the
    /// express tx hash for the report.
    Executed {
        executor_eoa: Option<String>,
        executor_contract: Option<String>,
        express_tx: Option<String>,
    },
    /// No `express_executed` on this record.
    NotObserved,
}

/// Whether the canonical execute (which reimburses the express executor) landed.
#[derive(Debug, Clone)]
pub enum Phase2 {
    /// `executed` present → `ExpressExecutionFulfilled` fired atomically.
    Reimbursed { execute_tx: Option<String> },
    /// Express happened but canonical execute hasn't landed yet.
    Pending,
    /// Phase 1 never happened, so reimbursement is not applicable.
    NotApplicable,
}

impl ExpressRecord {
    /// Classify this record into its two express-reimbursement phases.
    pub fn phase_status(&self) -> (Phase1, Phase2) {
        let Some(ee) = &self.express_executed else {
            return (Phase1::NotObserved, Phase2::NotApplicable);
        };

        let phase1 = Phase1::Executed {
            executor_eoa: ee.relayer_eoa().map(str::to_owned),
            executor_contract: ee.contract_address.clone(),
            express_tx: ee.transaction_hash.clone(),
        };

        let phase2 = match &self.executed {
            Some(ex) => Phase2::Reimbursed {
                execute_tx: ex.transaction_hash.clone(),
            },
            None => Phase2::Pending,
        };

        (phase1, phase2)
    }

    /// Best-effort source chain for display.
    pub fn source_chain(&self) -> Option<&str> {
        self.express_executed
            .as_ref()
            .and_then(|e| e.source_chain.as_deref())
            .or_else(|| {
                self.call
                    .as_ref()
                    .and_then(|c| c.return_values.as_ref())
                    .and_then(|r| r.source_chain.as_deref())
            })
    }

    /// Best-effort destination chain for display.
    pub fn destination_chain(&self) -> Option<&str> {
        self.express_executed
            .as_ref()
            .and_then(|e| e.chain.as_deref())
            .or_else(|| {
                self.interchain_transfer
                    .as_ref()
                    .and_then(|t| t.destination_chain.as_deref())
            })
            .or_else(|| {
                self.call
                    .as_ref()
                    .and_then(|c| c.return_values.as_ref())
                    .and_then(|r| r.destination_chain.as_deref())
            })
    }
}
