//! Per-tx state, phase machine, and the real-time stats block. Everything
//! here is data shape + the logic that advances a tx between phases —
//! no I/O, no RPC. The actual checks live in [`super::pipeline`]; this module
//! is what they read from and write back into.

use std::time::{Duration, Instant};

use alloy::primitives::{Address, FixedBytes};

use super::super::metrics::AmplifierTiming;

/// If no transaction completes a phase for this long, we stop waiting.
/// Resets every time a tx makes progress, so large batches naturally get more time.
pub(super) const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(120);

/// Delay between poll attempts.
pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Interval for recalculating rolling throughput.
const THROUGHPUT_WINDOW: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Real-time stats (throughput + latency) for spinner display
// ---------------------------------------------------------------------------

pub(super) struct RealTimeStats {
    snapshot_time: Instant,
    snapshot_counts: [usize; 5], // voted, routed, hub_approved, approved, executed
    throughputs: [Option<f64>; 5],
    latencies: Vec<f64>, // sorted executed_secs for completed txs
}

impl RealTimeStats {
    pub(super) fn new() -> Self {
        Self {
            snapshot_time: Instant::now(),
            snapshot_counts: [0; 5],
            throughputs: [None; 5],
            latencies: Vec::new(),
        }
    }

    /// Update throughputs every THROUGHPUT_WINDOW and collect new latencies.
    #[allow(clippy::float_arithmetic)]
    pub(super) fn update(&mut self, counts: [usize; 5], txs: &[PendingTx]) {
        let elapsed = self.snapshot_time.elapsed();
        if elapsed >= THROUGHPUT_WINDOW {
            let secs = elapsed.as_secs_f64();
            for (i, &count) in counts.iter().enumerate() {
                let delta = count.saturating_sub(self.snapshot_counts[i]);
                self.throughputs[i] = if delta > 0 {
                    Some(delta as f64 / secs)
                } else {
                    self.throughputs[i] // keep last known value
                };
            }
            self.snapshot_counts = counts;
            self.snapshot_time = Instant::now();
        }

        // Rebuild latencies from all completed txs (simple and correct).
        let new_len = txs
            .iter()
            .filter(|t| t.timing.executed_secs.is_some())
            .count();
        if new_len != self.latencies.len() {
            self.latencies.clear();
            for tx in txs {
                if let Some(secs) = tx.timing.executed_secs {
                    self.latencies.push(secs);
                }
            }
            self.latencies
                .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        }
    }

    /// Format a single phase: "450/600(4.2/s)" or "450/600" if no throughput yet.
    fn fmt_phase(count: usize, total: usize, tps: Option<f64>) -> String {
        match tps {
            Some(t) => format!("{count}/{total}({t:.1}/s)"),
            None => format!("{count}/{total}"),
        }
    }

    /// Format latency summary: "e2e: avg 94.5s p50 92.1s p75 96.3s p99 102.1s"
    #[allow(clippy::float_arithmetic)]
    fn fmt_latency(&self) -> String {
        let n = self.latencies.len();
        if n == 0 {
            return String::new();
        }
        let sum: f64 = self.latencies.iter().sum();
        let avg = sum / n as f64;
        let pct = |p: f64| -> f64 {
            let idx = ((n as f64 * p) as usize).min(n - 1);
            self.latencies[idx]
        };
        let min = self.latencies[0];
        let max = self.latencies[n - 1];
        format!(
            " | e2e: avg {avg:.1}s p50 {:.1}s p75 {:.1}s p99 {:.1}s min {min:.1}s max {max:.1}s",
            pct(0.50),
            pct(0.75),
            pct(0.99),
        )
    }

    /// Build the full spinner message for GMP (no hub phase).
    pub(super) fn spinner_msg_gmp(
        &self,
        counts: [usize; 5],
        total: usize,
        err: Option<&str>,
        has_voting_verifier: bool,
    ) -> String {
        let [voted, routed, _, approved, executed] = counts;
        let [tv, tr, _, ta, te] = self.throughputs;
        let mut parts = Vec::new();
        if has_voting_verifier {
            parts.push(format!("voted: {}", Self::fmt_phase(voted, total, tv)));
        }
        parts.push(format!("routed: {}", Self::fmt_phase(routed, total, tr)));
        parts.push(format!(
            "approved: {}",
            Self::fmt_phase(approved, total, ta)
        ));
        parts.push(format!(
            "executed: {}",
            Self::fmt_phase(executed, total, te)
        ));
        let mut msg = parts.join("  ");
        msg.push_str(&self.fmt_latency());
        if let Some(e) = err {
            msg.push_str(&format!("  (err: {e})"));
        }
        msg
    }

    /// Build the full spinner message for ITS (with hub phase).
    pub(super) fn spinner_msg_its(
        &self,
        counts: [usize; 5],
        total: usize,
        err: Option<&str>,
    ) -> String {
        let [voted, routed, hub, approved, executed] = counts;
        let [tv, tr, th, ta, te] = self.throughputs;
        let mut msg = format!(
            "voted: {}  hub: {}  routed: {}  approved: {}  executed: {}",
            Self::fmt_phase(voted, total, tv),
            Self::fmt_phase(hub, total, th),
            Self::fmt_phase(routed, total, tr),
            Self::fmt_phase(approved, total, ta),
            Self::fmt_phase(executed, total, te),
        );
        msg.push_str(&self.fmt_latency());
        if let Some(e) = err {
            msg.push_str(&format!("  (err: {e})"));
        }
        msg
    }
}

// ---------------------------------------------------------------------------
// Phase tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Voted,
    Routed,
    HubApproved,
    DiscoverSecondLeg,
    Approved,
    Executed,
    Done,
}

pub(super) enum ApprovalResult {
    Approved,
    AlreadyExecuted,
    NotYet,
}

// ---------------------------------------------------------------------------
// Per-tx state
// ---------------------------------------------------------------------------

/// Per-tx state tracked during batch verification.
pub struct PendingTx {
    pub(super) idx: usize,
    pub(super) message_id: String,
    pub(super) send_instant: Instant,
    pub(super) source_address: String,
    pub(super) contract_addr: Address,
    pub(super) payload_hash: FixedBytes<32>,
    pub(super) payload_hash_hex: String,
    /// Pre-computed command ID for Solana destination checks.
    pub(super) command_id: Option<[u8; 32]>,
    /// GMP-level destination chain from ContractCall event (e.g. "axelar" for ITS).
    pub(super) gmp_destination_chain: String,
    /// GMP-level destination address from ContractCall event (e.g. ITS Hub contract).
    pub(super) gmp_destination_address: String,
    pub(super) timing: AmplifierTiming,
    pub(super) failed: bool,
    pub(super) fail_reason: Option<String>,
    pub(super) phase: Phase,
    /// Second-leg message_id discovered from hub execution tx (ITS only).
    pub(super) second_leg_message_id: Option<String>,
    /// Second-leg payload_hash discovered from hub execution tx (ITS only).
    pub(super) second_leg_payload_hash: Option<String>,
    /// Second-leg source_address (e.g. ITS Hub contract on Axelar).
    pub(super) second_leg_source_address: Option<String>,
    /// Second-leg destination_address (e.g. ITS proxy on destination chain).
    pub(super) second_leg_destination_address: Option<String>,
}

// ---------------------------------------------------------------------------
// Check outcome — returned from parallel checks, applied to txs afterward
// ---------------------------------------------------------------------------

pub(super) enum CheckOutcome {
    /// No change (not ready yet, or tx was already terminal).
    NotYet,
    /// Phase completed — record timing and advance.
    PhaseComplete { elapsed: f64 },
    /// Voted phase: no VotingVerifier, just advance to Routed.
    SkipVoting,
    /// Approved check found the tx already executed — skip to Done.
    AlreadyExecuted { elapsed: f64 },
    /// Second-leg discovered — carries the extracted info.
    SecondLegDiscovered {
        message_id: String,
        payload_hash: String,
        source_address: String,
        destination_address: String,
    },
    /// Check returned an error.
    Error(String),
}

/// Per-tx snapshot used to drive a single phase check without holding a
/// borrow into `txs` across `await` points.
pub(super) struct TxPhaseSnapshot {
    pub(super) phase: Phase,
    pub(super) message_id: String,
    pub(super) source_address: String,
    pub(super) contract_addr: Address,
    pub(super) payload_hash: FixedBytes<32>,
    pub(super) payload_hash_hex: String,
    pub(super) send_instant: Instant,
    pub(super) command_id: Option<[u8; 32]>,
}

/// Outcome of folding a [`CheckOutcome`] back into a tx — drives whether the
/// outer pipeline updates `last_progress` and/or the error spinner.
pub(super) enum ApplyResult {
    Progress,
    NoChange,
    Error(String),
}

/// Take a single (tx, check-outcome) pair and apply it: advance the phase,
/// record timing, or surface an error. Centralises the phase-transition logic
/// so the outer poll loop stays a thin orchestrator.
pub(super) fn apply_check_outcome(
    tx: &mut PendingTx,
    outcome: CheckOutcome,
    has_axelarnet: bool,
) -> ApplyResult {
    match outcome {
        CheckOutcome::NotYet | CheckOutcome::SecondLegDiscovered { .. } => ApplyResult::NoChange,
        CheckOutcome::PhaseComplete { elapsed } => {
            advance_phase_on_complete(tx, elapsed, has_axelarnet);
            ApplyResult::Progress
        }
        CheckOutcome::SkipVoting => {
            tx.phase = next_phase_after_skip(tx.phase, has_axelarnet);
            ApplyResult::Progress
        }
        CheckOutcome::AlreadyExecuted { elapsed } => {
            if tx.timing.approved_secs.is_none() {
                tx.timing.approved_secs = Some(elapsed);
            }
            tx.timing.executed_secs = Some(elapsed);
            tx.timing.executed_ok = Some(true);
            tx.phase = Phase::Done;
            ApplyResult::Progress
        }
        CheckOutcome::Error(msg) => ApplyResult::Error(msg),
    }
}

/// Advance a tx's phase and stamp the matching timing field after the current
/// phase check returned PhaseComplete.
fn advance_phase_on_complete(tx: &mut PendingTx, elapsed: f64, has_axelarnet: bool) {
    match tx.phase {
        Phase::Voted => {
            tx.timing.voted_secs = Some(elapsed);
            tx.phase = Phase::Routed;
        }
        Phase::Routed => {
            tx.timing.routed_secs = Some(elapsed);
            tx.phase = if has_axelarnet {
                Phase::HubApproved
            } else {
                Phase::Approved
            };
        }
        Phase::HubApproved => {
            tx.timing.hub_approved_secs = Some(elapsed);
            tx.phase = Phase::Approved;
        }
        Phase::Approved => {
            tx.timing.approved_secs = Some(elapsed);
            tx.phase = Phase::Executed;
        }
        Phase::Executed => {
            tx.timing.executed_secs = Some(elapsed);
            tx.timing.executed_ok = Some(true);
            tx.phase = Phase::Done;
        }
        Phase::DiscoverSecondLeg | Phase::Done => {}
    }
}

/// When a phase check reports SkipVoting, advance the phase without recording
/// a timing field. (Voting is the only phase that can be skipped.)
fn next_phase_after_skip(current: Phase, has_axelarnet: bool) -> Phase {
    match current {
        Phase::Voted => Phase::Routed,
        Phase::Routed => {
            if has_axelarnet {
                Phase::HubApproved
            } else {
                Phase::Approved
            }
        }
        Phase::HubApproved => Phase::Approved,
        // SkipVoting should never fire for later phases; if it does, leave the
        // phase unchanged to avoid looping back.
        other => other,
    }
}

/// Count how many txs have reached each phase (cumulative).
pub(super) fn phase_counts(txs: &[PendingTx]) -> (usize, usize, usize, usize, usize) {
    let mut voted = 0;
    let mut routed = 0;
    let mut hub_approved = 0;
    let mut approved = 0;
    let mut executed = 0;
    for tx in txs {
        if tx.timing.voted_secs.is_some() {
            voted += 1;
        }
        if tx.timing.routed_secs.is_some() {
            routed += 1;
        }
        if tx.timing.hub_approved_secs.is_some() {
            hub_approved += 1;
        }
        if tx.timing.approved_secs.is_some() {
            approved += 1;
        }
        if tx.timing.executed_secs.is_some() {
            executed += 1;
        }
    }
    (voted, routed, hub_approved, approved, executed)
}
