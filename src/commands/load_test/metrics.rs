use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Per-transaction metrics collected during load testing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxMetrics {
    pub signature: String,
    pub submit_time_ms: u64,
    pub confirm_time_ms: Option<u64>,
    pub latency_ms: Option<u64>,
    pub compute_units: Option<u64>,
    pub slot: Option<u64>,
    pub success: bool,
    pub error: Option<String>,

    /// keccak256 of the payload, hex-encoded (no 0x prefix).
    #[serde(default)]
    pub payload_hash: String,
    /// The source address of the signer.
    #[serde(default)]
    pub source_address: String,
    /// Raw payload bytes (kept in-memory for verification, not serialized).
    #[serde(skip)]
    pub payload: Vec<u8>,
    /// Instant the tx was submitted (for computing T+X timing).
    #[serde(skip)]
    pub send_instant: Option<Instant>,
    /// GMP-level destination chain from ContractCall event (e.g. "axelar" for ITS hub routing).
    #[serde(default)]
    pub gmp_destination_chain: String,
    /// GMP-level destination address from ContractCall event (e.g. ITS Hub contract for ITS).
    #[serde(default)]
    pub gmp_destination_address: String,
    /// Amplifier pipeline timing (populated during verification phase).
    pub amplifier_timing: Option<AmplifierTiming>,
}

/// Per-step timing through the Amplifier pipeline, relative to tx send time.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AmplifierTiming {
    /// Seconds from send to message verified on VotingVerifier (quorum reached).
    pub voted_secs: Option<f64>,
    /// Seconds from send to message routed on destination Cosmos Gateway.
    pub routed_secs: Option<f64>,
    /// Seconds from send to message approved on AxelarnetGateway hub (ITS only).
    pub hub_approved_secs: Option<f64>,
    /// Seconds from send to isMessageApproved on EVM gateway.
    pub approved_secs: Option<f64>,
    /// Seconds from send to execution on destination contract.
    pub executed_secs: Option<f64>,
    /// Whether execution succeeded.
    pub executed_ok: Option<bool>,
    /// The message stored by SenderReceiver (if readable).
    pub stored_message: Option<String>,
}

/// Comprehensive load test report containing all metrics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoadTestReport {
    pub source_chain: String,
    pub destination_chain: String,
    pub destination_address: String,
    pub num_txs: u64,
    pub num_keys: usize,

    pub total_submitted: u64,
    pub total_confirmed: u64,
    pub total_failed: u64,
    pub test_duration_secs: f64,
    pub tps_submitted: f64,
    pub tps_confirmed: f64,
    pub landing_rate: f64,

    pub avg_latency_ms: Option<f64>,
    pub min_latency_ms: Option<u64>,
    pub max_latency_ms: Option<u64>,
    pub avg_compute_units: Option<f64>,
    pub min_compute_units: Option<u64>,
    pub max_compute_units: Option<u64>,

    pub verification: Option<VerificationReport>,
    pub transactions: Vec<TxMetrics>,
}

/// Report from transaction verification phase.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VerificationReport {
    pub total_verified: u64,
    pub successful: u64,
    pub pending: u64,
    pub failed: u64,
    pub success_rate: f64,
    pub failure_reasons: Vec<FailureCategory>,
    pub avg_voted_secs: Option<f64>,
    pub avg_routed_secs: Option<f64>,
    pub avg_hub_approved_secs: Option<f64>,
    pub avg_approved_secs: Option<f64>,
    pub avg_executed_secs: Option<f64>,
    pub min_executed_secs: Option<f64>,
    pub max_executed_secs: Option<f64>,
    /// Seconds from earliest send to last successful execution (for throughput).
    pub time_to_last_success_secs: Option<f64>,
    /// Peak throughput (tx/s) observed per pipeline step in a 5s sliding window.
    #[serde(default)]
    pub peak_throughput: PeakThroughput,
    /// Number of txs that timed out before completing all phases.
    pub stuck: u64,
    /// Which phase each stuck tx got stuck at.
    pub stuck_at: Vec<FailureCategory>,
}

/// Peak throughput per pipeline step, measured in 5-second sliding windows.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PeakThroughput {
    pub voted_tps: Option<f64>,
    pub routed_tps: Option<f64>,
    pub approved_tps: Option<f64>,
    pub executed_tps: Option<f64>,
}

/// Categorized failure count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureCategory {
    pub reason: String,
    pub count: u64,
}
