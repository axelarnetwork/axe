//! Result types the tools own.
//!
//! Most tools hand back a type the command layer already produces. These are
//! the exceptions: shapes that exist because a tool has to say something the
//! CLI never had to, such as what a spend had already submitted when the rest
//! of it failed, or how a detached run ended once nobody was watching.

use serde::Serialize;

use crate::commands::test_express::ExpressWatch;
use crate::mcp::runs::RunKind;

/// An originated express transfer, and how far it had got.
///
/// The transaction hash is present whenever the transfer was sent, including
/// when the watch afterwards could not report. That is the point of the
/// shape: an agent that retried a spend because the result only said "failed"
/// would pay twice.
#[derive(Debug, Serialize)]
pub struct OriginatedTransfer {
    pub source_tx: String,
    /// Express-asset base units, as sent.
    pub amount: String,
    pub source_chain: String,
    pub destination_chain: String,
    pub watch: Option<ExpressWatch>,
    /// Why the transfer could not be watched, when it could not. The transfer
    /// itself was still sent.
    pub watch_error: Option<String>,
}

/// How a detached intent run ended.
///
/// Written to the run's report artifact by the flow itself, so a run that
/// failed still leaves a record naming the bounds it ran under. The flows
/// narrate to the operator's terminal as they go; this is what the agent that
/// started the run reads back.
#[derive(Debug, Serialize)]
pub struct IntentsRunReport<T> {
    pub flow: RunKind,
    pub bounds: RunBounds,
    #[serde(flatten)]
    pub outcome: RunOutcome<T>,
}

impl<T: Serialize> IntentsRunReport<T> {
    pub fn completed(flow: RunKind, bounds: RunBounds, result: T) -> Self {
        Self {
            flow,
            bounds,
            outcome: RunOutcome::Completed { result },
        }
    }

    /// A run that stopped early with nothing to show. A flow that failed part
    /// way often has no result to hand back, and the wallet plus the terminal
    /// log are then the only record.
    pub fn failed(flow: RunKind, bounds: RunBounds, error: &eyre::Report) -> Self {
        Self {
            flow,
            bounds,
            outcome: RunOutcome::Failed {
                error: crate::ui::scrub_urls(&format!("{error:#}")),
                result: None,
            },
        }
    }

    /// A run that finished but did not fully succeed, and can still say what
    /// it did. A stress run where one deposit of two hundred never confirmed
    /// failed, but the other hundred and ninety-nine were paid for and belong
    /// in the report.
    pub fn failed_with(flow: RunKind, bounds: RunBounds, error: &str, result: T) -> Self {
        Self {
            flow,
            bounds,
            outcome: RunOutcome::Failed {
                error: crate::ui::scrub_urls(error),
                result: Some(result),
            },
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RunOutcome<T> {
    Completed {
        result: T,
    },
    Failed {
        error: String,
        /// What the run managed before it failed, when it can say.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<T>,
    },
}

/// The limits a run was admitted under, repeated in its report so the report
/// stands on its own.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RunBounds {
    pub max_intents: u64,
    pub sweeps: Option<u64>,
    pub duration_seconds: Option<u64>,
}
