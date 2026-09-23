//! Express-execution reimbursement monitor (observe-only, v1).
//!
//! Axelar "express execution" lets a relayer front tokens to the recipient on
//! the destination chain (via `expressExecute` on the ITS edge) *before* the
//! canonical GMP proof lands. The relayer is then **reimbursed** when the
//! canonical `ITS.execute` lands — `ExpressExecutionFulfilled` fires atomically
//! inside that execute tx. The signal this monitor reports is exactly that:
//! did the express executor get reimbursed?
//!
//! This module never express-executes anything itself. It observes via the
//! Axelarscan GMP API, so a reported reimbursement is always the real
//! `gmp-express-executor` service being paid back, never a stand-in. The
//! transfer under watch can be originated by
//! [`crate::commands::express_originate`] (`--originate`), which only sends
//! the qualifying source call and hands the tx hash here. Two modes:
//! - `--source-tx <hash>`: poll one message through both phases to
//!   terminal/timeout.
//! - else: for each requested chain, list the `--recent` newest express
//!   transfers and print their two-phase status.
//!
//! The `searchGMP` reqwest client and the `ExpressRecord` view (with the
//! `Phase1`/`Phase2` classifier) live in the shared [`crate::gmp_api`] module,
//! so the load-test verifier can reuse them for its final executed-state check.

use std::time::{Duration, Instant};

use eyre::{Result, eyre};

use crate::gmp_api::{self, AmountCheck, ExpressRecord, Phase1, Phase2};
use crate::timing::EXPRESS_POLL_INTERVAL;
use crate::types::Network;
use crate::ui;

/// Default number of recent express transfers to report per chain in scan mode.
pub(crate) const DEFAULT_RECENT: usize = 5;

/// How often the single-tx watch prints a still-waiting line.
///
/// Without one the monitor polls in silence: run 34350578640 logged nothing
/// between its second poll and the runner's SIGKILL 20 minutes later, leaving a
/// genuine stall indistinguishable from a hung process in the cron log.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// Rate-limits the still-waiting lines to one per [`HEARTBEAT_INTERVAL`], so a
/// long wait stays visible without burying the phase transitions.
struct Heartbeat {
    last: Instant,
}

impl Heartbeat {
    fn new() -> Self {
        Self {
            last: Instant::now(),
        }
    }

    fn tick(&mut self, waiting_for: &str, start: Instant) {
        if self.last.elapsed() < HEARTBEAT_INTERVAL {
            return;
        }
        self.last = Instant::now();
        ui::info(&format!(
            "{waiting_for} ({} elapsed)",
            ui::format_elapsed(start)
        ));
    }
}

pub async fn run_config(
    network: Network,
    chains: Vec<String>,
    source_tx: Option<String>,
    recent: usize,
    timeout_secs: u64,
) -> Result<()> {
    let base = api_base(network)?;

    ui::section("Express Execution Monitor");
    ui::kv("network", network.as_str());
    ui::kv("gmp api", base);

    match source_tx {
        Some(tx) => poll_single_tx(base, &tx, timeout_secs).await,
        None => scan_chains(base, &chains, recent).await,
    }
}

/// Mode A: poll one source tx through both express phases to terminal/timeout.
async fn poll_single_tx(base: &str, tx: &str, timeout_secs: u64) -> Result<()> {
    ui::section(&format!("Single-tx watch: {tx}"));
    let start = Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout_secs);

    let mut phase1_printed = false;
    let mut heartbeat = Heartbeat::new();

    loop {
        let record = gmp_api::search_by_tx(base, tx).await?;
        let Some(record) = record else {
            ui::info("not yet indexed by the GMP API");
            if Instant::now() >= deadline {
                return Err(eyre!(
                    "tx never indexed by the GMP API within {timeout_secs}s: the source call \
                     landed but Axelar never picked it up"
                ));
            }
            tokio::time::sleep(EXPRESS_POLL_INTERVAL).await;
            continue;
        };

        // A refused message can never reach phase 1, so waiting out the
        // deadline would only hide why. Fail now, naming the reason.
        if let Some(reason) = record.express_refusal() {
            return Err(eyre!("express execution was ruled out: {reason}"));
        }

        let (phase1, phase2) = record.phase_status();

        if !phase1_printed && matches!(&phase1, Phase1::Executed { .. }) {
            print_phase1(&phase1);
            phase1_printed = true;
        }

        match (&phase1, &phase2) {
            (Phase1::NotObserved, _) => {
                if Instant::now() >= deadline {
                    return Err(eyre!(
                        "no express execution observed within {timeout_secs}s ({})",
                        ui::format_elapsed(start)
                    ));
                }
                heartbeat.tick("waiting for the express executor to front the funds", start);
            }
            (Phase1::Executed { .. }, Phase2::Reimbursed { .. }) => {
                print_phase2(&phase2);
                let check = record.reimbursement_amount_check();
                report_amount_check(check.as_ref(), record.symbol.as_deref());
                if let Some(reason) = amount_check_failure(check.as_ref()) {
                    return Err(eyre!("express reimbursement amount check failed: {reason}"));
                }
                ui::success(&format!(
                    "express executor reimbursed in full ({})",
                    ui::format_elapsed(start)
                ));
                return Ok(());
            }
            (Phase1::Executed { .. }, _) => {
                if Instant::now() >= deadline {
                    return Err(eyre!(
                        "express executor fronted the funds but was not reimbursed within \
                         {timeout_secs}s: canonical execute never observed ({})",
                        ui::format_elapsed(start)
                    ));
                }
                heartbeat.tick("waiting for the canonical execute to reimburse", start);
            }
        }

        tokio::time::sleep(EXPRESS_POLL_INTERVAL).await;
    }
}

/// Mode B: for each chain, list recent express transfers and report both phases.
async fn scan_chains(base: &str, chains: &[String], recent: usize) -> Result<()> {
    let recent = if recent == 0 { DEFAULT_RECENT } else { recent };

    if chains.is_empty() {
        ui::warn("no chains given — pass express-supported chain ids to scan");
        return Ok(());
    }

    for chain in chains {
        scan_one_chain(base, chain, recent).await?;
    }
    Ok(())
}

async fn scan_one_chain(base: &str, chain: &str, recent: usize) -> Result<()> {
    ui::section(&format!(
        "Chain: {chain} (latest {recent} express transfers)"
    ));
    let records = gmp_api::search_recent_express(base, Some(chain), recent).await?;

    if records.is_empty() {
        ui::info("no express transfers observed for this chain");
        return Ok(());
    }

    let total = records.len();
    for (i, record) in records.iter().enumerate() {
        print_record_report(i + 1, total, record);
    }
    Ok(())
}

/// One transfer's two-phase report block in scan mode.
fn print_record_report(index: usize, total: usize, record: &ExpressRecord) {
    let route = format!(
        "{} → {}",
        record.source_chain().unwrap_or("?"),
        record.destination_chain().unwrap_or("?"),
    );
    ui::step_header(index, total, &route);

    if let Some(mid) = &record.message_id {
        ui::kv("message_id", mid);
    }
    if let Some(cid) = &record.command_id {
        ui::kv("command_id", cid);
    }
    if let Some(status) = &record.status {
        ui::kv("status", status);
    }

    let (phase1, phase2) = record.phase_status();
    print_phase1(&phase1);
    print_phase2(&phase2);
    if matches!(phase2, Phase2::Reimbursed { .. }) {
        let check = record.reimbursement_amount_check();
        report_amount_check(check.as_ref(), record.symbol.as_deref());
    }
}

fn print_phase1(phase1: &Phase1) {
    match phase1 {
        Phase1::Executed {
            executor_eoa,
            executor_contract,
            express_tx,
        } => {
            ui::success("Phase 1: express executed (funds fronted)");
            if let Some(eoa) = executor_eoa {
                ui::address("executor EOA", eoa);
            }
            if let Some(contract) = executor_contract {
                ui::address("executor contract", contract);
            }
            if let Some(tx) = express_tx {
                ui::tx_hash("express tx", tx);
            }
        }
        Phase1::NotObserved => {
            ui::warn("Phase 1: no express execution observed");
        }
    }
}

fn print_phase2(phase2: &Phase2) {
    match phase2 {
        Phase2::Reimbursed { execute_tx } => {
            ui::success("Phase 2: executor reimbursed (canonical execute landed)");
            if let Some(tx) = execute_tx {
                ui::tx_hash("execute tx", tx);
            }
        }
        Phase2::Pending => {
            ui::warn("Phase 2: reimbursement PENDING — canonical execute not yet observed");
        }
        Phase2::NotApplicable => {
            ui::info("Phase 2: n/a (no express execution to reimburse)");
        }
    }
}

/// Print the fronted-vs-reimbursed amount verdict. Amounts are raw token base
/// units (the GMP API does not surface the token contract).
fn report_amount_check(check: Option<&AmountCheck>, symbol: Option<&str>) {
    let unit = symbol.map(|s| format!(" {s}")).unwrap_or_default();
    match check {
        Some(AmountCheck::Match { amount }) => {
            ui::success(&format!(
                "amount check: fronted == reimbursed = {amount}{unit} (base units)"
            ));
        }
        Some(AmountCheck::Mismatch {
            fronted,
            reimbursed,
        }) => {
            ui::error(&format!(
                "amount MISMATCH: fronted {fronted}{unit} != reimbursed {reimbursed}{unit} (base units)"
            ));
        }
        Some(AmountCheck::MissingInbound { fronted }) => {
            ui::error(&format!(
                "amount check: executor fronted {fronted}{unit} but received nothing back in the execute tx"
            ));
        }
        Some(AmountCheck::NoFrontedTransfer) => {
            ui::warn(
                "amount check: no executor outbound transfer in the express tx — cannot assert (non-EVM / non-ERC-20 leg?)",
            );
        }
        None => {
            ui::info("amount check: unavailable (execute receipt not yet indexed)");
        }
    }
}

/// The reasons a reimbursement must be treated as a failure rather than a pass:
/// the amounts disagree, or the executor was never paid back.
fn amount_check_failure(check: Option<&AmountCheck>) -> Option<String> {
    match check {
        Some(AmountCheck::Mismatch {
            fronted,
            reimbursed,
        }) => Some(format!(
            "fronted {fronted} != reimbursed {reimbursed} (base units)"
        )),
        Some(AmountCheck::MissingInbound { fronted }) => Some(format!(
            "executor fronted {fronted} (base units) but no inbound transfer in the execute tx"
        )),
        _ => None,
    }
}

/// One express transfer's two phases, as data.
///
/// Built from the record's own `phase_status` classification rather than by
/// re-reading the raw API shape, so the tool and the printed report agree on
/// whether a transfer was fronted and whether the executor was paid back.
#[derive(Debug, serde::Serialize)]
pub(crate) struct ExpressPhases {
    pub source_chain: Option<String>,
    pub destination_chain: Option<String>,
    pub message_id: Option<String>,
    pub command_id: Option<String>,
    pub status: Option<String>,
    pub symbol: Option<String>,
    pub phase1: Phase1Status,
    pub express_tx: Option<String>,
    pub executor: Option<String>,
    pub phase2: Phase2Status,
    pub execute_tx: Option<String>,
}

/// [`Phase1`] without its payload: the transaction and executor it carries
/// are reported as their own fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase1Status {
    /// The express executor fronted the funds.
    Executed,
    NotObserved,
}

/// [`Phase2`] without its payload: the execute transaction is reported as its
/// own field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase2Status {
    /// The canonical execute landed and paid the executor back.
    Reimbursed,
    /// Phase 1 happened, the canonical execute has not landed yet.
    Pending,
    /// Phase 1 never happened, so there is nothing to reimburse.
    NotApplicable,
}

fn express_phases(record: &ExpressRecord) -> ExpressPhases {
    let (phase1, phase2) = record.phase_status();

    let (phase1_status, express_tx, executor) = match &phase1 {
        Phase1::Executed {
            executor_eoa,
            executor_contract,
            express_tx,
        } => (
            Phase1Status::Executed,
            express_tx.clone(),
            executor_eoa.clone().or_else(|| executor_contract.clone()),
        ),
        Phase1::NotObserved => (Phase1Status::NotObserved, None, None),
    };

    let (phase2_status, execute_tx) = match &phase2 {
        Phase2::Reimbursed { execute_tx } => (Phase2Status::Reimbursed, execute_tx.clone()),
        Phase2::Pending => (Phase2Status::Pending, None),
        Phase2::NotApplicable => (Phase2Status::NotApplicable, None),
    };

    ExpressPhases {
        source_chain: record.source_chain().map(str::to_string),
        destination_chain: record.destination_chain().map(str::to_string),
        message_id: record.message_id.clone(),
        command_id: record.command_id.clone(),
        status: record.status.clone(),
        symbol: record.symbol.clone(),
        phase1: phase1_status,
        express_tx,
        executor,
        phase2: phase2_status,
        execute_tx,
    }
}

/// The Axelarscan GMP API this network is served by.
fn api_base(network: Network) -> Result<&'static str> {
    gmp_api::base_url(network).ok_or_else(|| {
        eyre!(
            "network {} has no Axelarscan GMP API deployment",
            network.as_str()
        )
    })
}

/// Scan recent express transfers on one or more chains, without printing.
///
/// Observe-only: this reads the Axelarscan GMP API and spends nothing, which
/// is why it belongs with the read-only tools rather than behind a spend gate.
pub(crate) async fn resolve_scan(
    network: Network,
    chains: &[String],
    recent: usize,
) -> Result<Vec<ExpressPhases>> {
    let base = api_base(network)?;

    let recent = if recent == 0 { DEFAULT_RECENT } else { recent };
    let mut out = Vec::new();
    for chain in chains {
        let records = gmp_api::search_recent_express(base, Some(chain), recent).await?;
        out.extend(records.iter().map(express_phases));
    }
    Ok(out)
}

/// Where a watched transfer had got to when the watch stopped.
///
/// A ceiling reached is a state, not a failure: the MCP watch waits far less
/// than the CLI does, so "still waiting" is the ordinary answer and the caller
/// is expected to ask again. Only the two verdicts that can never improve --
/// a refusal, and a reimbursement that arrived short -- are terminal failures.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub(crate) enum WatchOutcome {
    /// The GMP API has no record of the transaction yet.
    NotIndexed,
    /// Axelar ruled the message out for express execution.
    Refused { reason: String },
    /// Indexed, but no executor has fronted the funds yet.
    AwaitingExpressExecution,
    /// Fronted, waiting for the canonical execute to pay the executor back.
    AwaitingReimbursement,
    /// Fronted and reimbursed in full.
    Reimbursed,
    /// Reimbursed, but not for what was fronted.
    ReimbursementShort { reason: String },
}

impl WatchOutcome {
    /// Whether waiting longer could still change this.
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Refused { .. } | Self::Reimbursed | Self::ReimbursementShort { .. }
        )
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::NotIndexed => "not indexed by the GMP API yet",
            Self::Refused { .. } => "ruled out for express execution",
            Self::AwaitingExpressExecution => "waiting for an executor to front the funds",
            Self::AwaitingReimbursement => "waiting for the canonical execute to reimburse",
            Self::Reimbursed => "reimbursed in full",
            Self::ReimbursementShort { .. } => "reimbursed short",
        }
    }
}

/// One transfer watched through both phases, as data.
#[derive(Debug, serde::Serialize)]
pub(crate) struct ExpressWatch {
    pub source_tx: String,
    #[serde(flatten)]
    pub outcome: WatchOutcome,
    pub waited_seconds: u64,
    /// Absent until the GMP API has indexed the transaction.
    pub phases: Option<ExpressPhases>,
}

/// Watch one source transaction through both express phases, without printing.
///
/// Polls until the outcome is terminal or `wait` runs out, whichever comes
/// first. Unlike the CLI's monitor, running out of time is reported rather
/// than raised: the caller is expected to ask again.
pub(crate) async fn resolve_watch(
    network: Network,
    source_tx: &str,
    wait: std::time::Duration,
) -> Result<ExpressWatch> {
    let base = api_base(network)?;
    let start = Instant::now();
    let deadline = start + wait;

    loop {
        let (outcome, phases) = watch_once(base, source_tx).await?;

        if outcome.is_terminal() || Instant::now() >= deadline {
            return Ok(ExpressWatch {
                source_tx: source_tx.to_string(),
                outcome,
                waited_seconds: start.elapsed().as_secs(),
                phases,
            });
        }

        tokio::time::sleep(EXPRESS_POLL_INTERVAL).await;
    }
}

/// One poll of the GMP API, classified.
async fn watch_once(base: &str, source_tx: &str) -> Result<(WatchOutcome, Option<ExpressPhases>)> {
    let Some(record) = gmp_api::search_by_tx(base, source_tx).await? else {
        return Ok((WatchOutcome::NotIndexed, None));
    };

    if let Some(reason) = record.express_refusal() {
        return Ok((
            WatchOutcome::Refused { reason },
            Some(express_phases(&record)),
        ));
    }

    let outcome = match record.phase_status() {
        (Phase1::NotObserved, _) => WatchOutcome::AwaitingExpressExecution,
        (Phase1::Executed { .. }, Phase2::Reimbursed { .. }) => {
            let check = record.reimbursement_amount_check();
            match amount_check_failure(check.as_ref()) {
                Some(reason) => WatchOutcome::ReimbursementShort { reason },
                None => WatchOutcome::Reimbursed,
            }
        }
        (Phase1::Executed { .. }, _) => WatchOutcome::AwaitingReimbursement,
    };

    Ok((outcome, Some(express_phases(&record))))
}
