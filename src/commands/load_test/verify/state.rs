//! Per-tx state types tracked during batch verification:
//! [`PendingTx`], the [`Phase`] enum that drives state transitions, and
//! [`RealTimeStats`] which keeps the spinner display fed with rolling
//! throughput + latency numbers.

use std::time::Instant;

use alloy::primitives::{Address, FixedBytes};

use super::THROUGHPUT_WINDOW;
use crate::commands::load_test::metrics::AmplifierTiming;

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

/// Per-tx state tracked during batch verification.
///
/// Visibility note: the struct is `pub(in crate::commands::load_test)` (one
/// level above `verify/`) because `verify::tx_to_pending_*` constructors
/// hand `PendingTx` instances back to the per-pair load-test runners that
/// own the verifier `mpsc::Sender`. Fields stay `pub(super)` so only
/// `verify/`-internal code reads them.
pub(in crate::commands::load_test) struct PendingTx {
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

/// Real-time stats (throughput + latency) for spinner display.
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

/// Count how many txs have each phase's timing populated (voted, routed,
/// hub_approved, approved, executed). Used by both the spinner refresh and
/// the report's "stuck at X" diagnostics.
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
