//! Background runs and their report artifacts.
//!
//! A load test, an intent sweep or a traffic simulation can run far longer
//! than a client will hold a request open, and a client that times out is
//! expected to cancel. Cancelling a flow that has already submitted
//! transactions loses the record of money already spent, so these runs
//! detach: starting one returns an identifier, and the report is read back
//! once it lands.
//!
//! Finished runs persist a JSON report named after their identifier, so the
//! artifact on disk is the store. This registry tracks only what is still in
//! flight, which is why a completed run survives a restart and an in-flight
//! one does not.
//!
//! Runs spend funds from shared accounts, so the registry admits one at a
//! time: a second start while one is in flight is refused rather than queued,
//! and the refusal names the run that is holding the slot.

use std::collections::HashMap;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use serde::Serialize;
use tokio::task::JoinHandle;

/// What a caller learns about a run.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum RunState {
    /// Still executing in this process.
    Running { run_id: RunId },
    /// Finished, with its report.
    Finished {
        run_id: RunId,
        report: serde_json::Value,
    },
    /// No report, and not running here. Either it failed before writing one,
    /// or it was started by a server that has since restarted. Deliberately
    /// distinct from running: a caller must not read "no report yet" as
    /// "still working". Carries the caller's text as given, since it may
    /// not be a run identifier at all.
    Unknown { run_id: String },
}

/// One line of the run listing.
#[derive(Debug, Serialize)]
pub struct RunListEntry {
    pub run_id: RunId,
    pub kind: Option<RunKind>,
    pub state: RunStatus,
}

/// Whether a listed run is still going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Finished,
}

/// What a caller gets back when a load test is accepted.
#[derive(Debug, Serialize)]
pub struct RunStarted {
    pub run_id: RunId,
    pub network: String,
    pub source_chain: String,
    pub destination_chain: String,
    pub transactions: u64,
}

/// What a caller gets back when an intents run is accepted.
///
/// Every field is a bound the run will stop at, because an agent that started
/// one cannot watch it: what it needs back is when this will be over and how
/// much it may spend before then.
#[derive(Debug, Serialize)]
pub struct IntentsRunStarted {
    pub run_id: RunId,
    pub network: String,
    pub flow: RunKind,
    /// The reservation taken against the operator's budget.
    pub max_intents: u64,
    pub sweeps: Option<u64>,
    pub duration_seconds: Option<u64>,
}

/// Why a start was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartRefused {
    /// This process is already running one.
    RunInFlight { run_id: RunId },
    /// A blocking tool in this same server holds the slot.
    SlotHeldHere,
    /// Another axe process on this machine holds the run lock.
    HeldByAnotherProcess { lock: PathBuf },
    /// The lock file could not be created or locked. Fails closed: a slot
    /// that cannot be claimed is not free.
    LockUnavailable { lock: PathBuf, error: String },
}

impl Display for StartRefused {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::RunInFlight { run_id } => write!(
                f,
                "a run is already in flight: {run_id}. Runs spend from shared accounts, \
                 so one is admitted at a time. Wait for it, or read its report with \
                 run_report"
            ),
            Self::SlotHeldHere => write!(
                f,
                "another tool in this server is spending right now, and flows that spend are \
                 admitted one at a time. Wait for it to finish"
            ),
            Self::HeldByAnotherProcess { lock } => write!(
                f,
                "another axe mcp process on this machine is running a flow that spends \
                 (lock held at {}). Runs spend from shared accounts, so one is admitted at \
                 a time; wait for it to finish",
                lock.display()
            ),
            Self::LockUnavailable { lock, error } => {
                write!(
                    f,
                    "could not take the run lock at {}: {error}",
                    lock.display()
                )
            }
        }
    }
}

/// The file every axe process on this machine locks while a run is in
/// flight. The lock is released when the run ends, or by the kernel if the
/// process dies, so a crash cannot leave it stuck.
const LOCK_FILE: &str = "spend-run.lock";

/// Which flow a run is executing.
///
/// The kind is carried in the identifier rather than beside it, so a report
/// file found on disk after a restart still says what produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunKind {
    LoadTest,
    IntentsSend,
    IntentsRoundtrip,
    IntentsSweep,
    IntentsTraffic,
    IntentsStress,
}

impl RunKind {
    /// Every kind, so parsing and listing cannot miss one.
    const ALL: &'static [Self] = &[
        Self::LoadTest,
        Self::IntentsSend,
        Self::IntentsRoundtrip,
        Self::IntentsSweep,
        Self::IntentsTraffic,
        Self::IntentsStress,
    ];

    const fn slug(self) -> &'static str {
        match self {
            Self::LoadTest => "load-test",
            Self::IntentsSend => "intents-send",
            Self::IntentsRoundtrip => "intents-roundtrip",
            Self::IntentsSweep => "intents-sweep",
            Self::IntentsTraffic => "intents-traffic",
            Self::IntentsStress => "intents-stress",
        }
    }

    /// What every identifier of this kind starts with. Anything in the
    /// reports directory carrying no kind's prefix is not a run, whatever its
    /// extension.
    fn prefix(self) -> String {
        format!("axe-{}-", self.slug())
    }
}

impl Display for RunKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.slug())
    }
}

/// A run identifier: the kind's prefix plus the milliseconds it was minted at.
///
/// Owning the prefixes here is what keeps every other file in the reports
/// directory, the lock file and the spend ledger included, from ever reading
/// as a run: a caller's text becomes a `RunId` only through [`RunId::parse`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Mint an identifier for a new run of `kind`.
    ///
    /// Milliseconds since the epoch, forced strictly increasing within this
    /// process. That keeps identifiers unique and sortable, so listing newest
    /// first is a reverse sort rather than a stat of every file.
    fn mint(kind: RunKind) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let now = u64::try_from(now).unwrap_or(u64::MAX);

        let previous = LAST_RUN_MILLIS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| {
                Some(now.max(last.saturating_add(1)))
            })
            .unwrap_or(now);
        let millis = now.max(previous.saturating_add(1));

        Self(format!("{}{millis}", kind.prefix()))
    }

    /// Accept caller-supplied text only if this registry could have minted it.
    pub fn parse(text: &str) -> Option<Self> {
        Self::kind_of(text).map(|_| Self(text.to_string()))
    }

    /// The flow an identifier names.
    fn kind_of(text: &str) -> Option<RunKind> {
        RunKind::ALL
            .iter()
            .copied()
            .find(|kind| text.starts_with(&kind.prefix()))
    }

    pub fn kind(&self) -> Option<RunKind> {
        Self::kind_of(&self.0)
    }

    /// When this was minted, for ordering runs of different kinds against
    /// each other. Sorting the identifiers as text would group them by flow
    /// instead, and the listing promises newest first.
    fn minted_millis(&self) -> u64 {
        Self::kind_of(&self.0)
            .and_then(|kind| self.0.strip_prefix(&kind.prefix()))
            .and_then(|millis| millis.parse().ok())
            .unwrap_or_default()
    }

    /// The run a report file belongs to, or `None` for any other file.
    fn from_report_file_name(name: &str) -> Option<Self> {
        name.strip_suffix(".json").and_then(Self::parse)
    }
}

impl Display for RunId {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(&self.0)
    }
}

/// The machine-wide run slot, held for as long as this lives.
///
/// Opaque on purpose: holding it is the whole contract, and the lock is
/// released by dropping it or, if the process dies, by the kernel.
pub struct RunSlot {
    #[allow(dead_code)]
    lock: Flock<File>,
    /// Decremented on drop, so this server can tell its own blocking tool
    /// from another process holding the file lock.
    claims: Arc<AtomicUsize>,
}

impl Drop for RunSlot {
    fn drop(&mut self) {
        self.claims.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How often a draining server checks whether its runs have finished.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The last identifier minted, so two runs started in the same millisecond
/// still get distinct, ordered identifiers.
static LAST_RUN_MILLIS: AtomicU64 = AtomicU64::new(0);

/// Tracks load-test runs started through this server.
#[derive(Clone)]
pub struct RunRegistry {
    /// Blocking spend tools holding the slot in this process.
    claims: Arc<AtomicUsize>,
    reports_dir: PathBuf,
    in_flight: Arc<Mutex<HashMap<RunId, JoinHandle<()>>>>,
}

impl RunRegistry {
    pub fn new(reports_dir: PathBuf) -> Self {
        Self {
            reports_dir,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            claims: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Mint an identifier, run a flow under it on its own thread, and record
    /// it as in flight. Refused while another run is in flight.
    ///
    /// The identifier is minted and the handle recorded under one lock, so two
    /// concurrent starts cannot both find the slot free.
    ///
    /// Deliberately not `tokio::spawn`: the load-test future is not `Send`, so
    /// it cannot be moved onto the server's runtime. Building it inside a
    /// fresh thread means it is created and polled in one place and never
    /// crosses a thread boundary, which is what removes the `Send`
    /// requirement. The cost is one thread and one runtime per run, which is
    /// acceptable for a flow that runs for minutes.
    ///
    /// `make_flow` is a closure rather than a future for the same reason: the
    /// future must not exist until it is on the thread that will poll it. It
    /// receives the identifier so the flow can name its report after it.
    pub fn start<M, F>(&self, kind: RunKind, make_flow: M) -> Result<RunId, StartRefused>
    where
        M: FnOnce(RunId) -> F + Send + 'static,
        F: Future<Output = ()>,
    {
        let mut runs = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        runs.retain(|_, handle| !handle.is_finished());
        if let Some(run_id) = runs.keys().next() {
            return Err(StartRefused::RunInFlight {
                run_id: run_id.clone(),
            });
        }
        let machine_lock = self.lock_machine_slot()?;

        let run_id = RunId::mint(kind);
        let flow_id = run_id.clone();
        let handle = tokio::task::spawn_blocking(move || {
            // Held for as long as the run lives on this thread.
            let _machine_lock = machine_lock;
            // Nothing to report a build failure to: the caller already holds
            // its identifier and will see the run as unknown, which is
            // accurate.
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(make_flow(flow_id));
        });
        runs.insert(run_id.clone(), handle);

        Ok(run_id)
    }

    /// Hold the machine-wide run slot for a flow that blocks rather than
    /// detaching.
    ///
    /// Detaching is about outliving a request, not about exclusion: a flow
    /// short enough to answer inside one still spends from the same wallets,
    /// so it takes the same slot. The guard releases it when dropped.
    pub fn claim_slot(&self) -> Result<RunSlot, StartRefused> {
        let lock = self.lock_machine_slot()?;
        self.claims.fetch_add(1, Ordering::SeqCst);
        Ok(RunSlot {
            lock,
            claims: Arc::clone(&self.claims),
        })
    }

    /// Take the machine-wide run slot, without waiting.
    ///
    /// An advisory `flock` on a file in the reports directory. The in-memory
    /// map above answers for this process; this answers for every other axe
    /// process sharing the data directory, which share the wallets too.
    fn lock_machine_slot(&self) -> Result<Flock<File>, StartRefused> {
        // The file lock cannot tell this server's own blocking tool from
        // another process, and both hit EWOULDBLOCK. Asking here first is
        // what keeps the refusal from sending an operator to look for a
        // second process that does not exist.
        if self.claims.load(Ordering::SeqCst) > 0 {
            return Err(StartRefused::SlotHeldHere);
        }

        let lock = self.reports_dir.join(LOCK_FILE);
        let unavailable = |error: &dyn Display| StartRefused::LockUnavailable {
            lock: lock.clone(),
            error: error.to_string(),
        };

        std::fs::create_dir_all(&self.reports_dir).map_err(|e| unavailable(&e))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock)
            .map_err(|e| unavailable(&e))?;

        Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, errno)| match errno {
            Errno::EWOULDBLOCK => StartRefused::HeldByAnotherProcess { lock: lock.clone() },
            other => unavailable(&other),
        })
    }

    /// Identifiers of the runs still executing in this process.
    pub fn running(&self) -> Vec<RunId> {
        self.in_flight
            .lock()
            .map(|runs| {
                runs.iter()
                    .filter(|(_, handle)| !handle.is_finished())
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Wait until nothing is executing in this process.
    ///
    /// For a stdio server whose client has gone: exiting now would take a run
    /// that has already spent funds with it and leave no report. Polling is
    /// enough, since nothing else is happening by then.
    pub async fn wait_for_in_flight(&self) {
        while !self.running().is_empty() {
            tokio::time::sleep(DRAIN_POLL_INTERVAL).await;
        }
    }

    /// Whether a run is still executing in this process.
    fn is_running(&self, run_id: &RunId) -> bool {
        self.in_flight
            .lock()
            .is_ok_and(|runs| runs.get(run_id).is_some_and(|h| !h.is_finished()))
    }

    /// The state of one run, reading its artifact if it has landed.
    ///
    /// Only identifiers this registry could have minted are looked up, so a
    /// caller cannot read an arbitrary file in the reports directory as a
    /// report.
    pub async fn state(&self, text: &str) -> RunState {
        let Some(run_id) = RunId::parse(text) else {
            return RunState::Unknown {
                run_id: text.to_string(),
            };
        };
        if let Some(report) = self.read_report(&run_id).await {
            return RunState::Finished { run_id, report };
        }
        if self.is_running(&run_id) {
            return RunState::Running { run_id };
        }
        RunState::Unknown {
            run_id: text.to_string(),
        }
    }

    /// Write a run's report artifact.
    ///
    /// The load test writes its own, because it already had a report type and
    /// a place to put it. The flows that did not — the intent runs — hand
    /// theirs here, so every detached run is read back the same way and a
    /// failed one still leaves a record of what it had spent.
    pub fn record_report<T: Serialize>(&self, run_id: &RunId, report: &T) {
        let path = self.reports_dir.join(format!("{run_id}.json"));
        let written = serde_json::to_vec_pretty(report)
            .map_err(std::io::Error::other)
            .and_then(|body| std::fs::write(&path, body));

        // Nothing is waiting on this: the caller already holds its identifier
        // and the run has finished. A run whose report could not be written
        // reads back as unknown, which is what it is.
        if let Err(error) = written {
            crate::mcp::activity::report_unwritable(run_id, &path, &error);
        }
    }

    async fn read_report(&self, run_id: &RunId) -> Option<serde_json::Value> {
        let path = self.reports_dir.join(format!("{run_id}.json"));
        let text = tokio::fs::read_to_string(path).await.ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Known runs, newest first.
    ///
    /// Reports outlive the process, so this finds runs from earlier sessions
    /// too. Keying off the artifact rather than memory is the point.
    pub async fn list(&self) -> Vec<RunListEntry> {
        let mut ids = Vec::new();

        if let Ok(mut entries) = tokio::fs::read_dir(&self.reports_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(id) = RunId::from_report_file_name(&entry.file_name().to_string_lossy())
                {
                    ids.push(id);
                }
            }
        }

        // A handle that finished without writing a report is a failed run
        // with nothing to show. Pruned here so it reads as missing, not as
        // finished.
        if let Ok(mut runs) = self.in_flight.lock() {
            runs.retain(|_, handle| !handle.is_finished());
            for id in runs.keys() {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
        }

        ids.sort_unstable_by_key(|id| (std::cmp::Reverse(id.minted_millis()), id.clone()));

        ids.into_iter()
            .map(|run_id| {
                let state = if self.is_running(&run_id) {
                    RunStatus::Running
                } else {
                    RunStatus::Finished
                };
                RunListEntry {
                    kind: run_id.kind(),
                    run_id,
                    state,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;

    use super::{RunId, RunKind, RunRegistry, RunState, RunStatus, StartRefused};

    static DIRS: AtomicUsize = AtomicUsize::new(0);

    /// A fresh, empty reports directory per test.
    fn scratch_reports_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "axe-mcp-runs-{}-{}",
            std::process::id(),
            DIRS.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_report(dir: &std::path::Path, run_id: &str, report: &serde_json::Value) {
        std::fs::write(dir.join(format!("{run_id}.json")), report.to_string()).unwrap();
    }

    async fn wait_until_finished(registry: &RunRegistry, run_id: &RunId) {
        for _ in 0..500 {
            if !registry.is_running(run_id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{run_id} did not finish");
    }

    #[test]
    fn run_ids_are_unique_and_ascending_within_a_burst() {
        let ids: Vec<RunId> = (0..50).map(|_| RunId::mint(RunKind::LoadTest)).collect();
        for pair in ids.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{} should sort before {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[tokio::test]
    async fn unknown_run_has_no_state() {
        let registry = RunRegistry::new(scratch_reports_dir());
        assert!(matches!(
            registry.state("axe-load-test-0").await,
            RunState::Unknown { run_id } if run_id == "axe-load-test-0"
        ));
    }

    #[tokio::test]
    async fn finished_run_is_read_from_its_report_file() {
        let dir = scratch_reports_dir();
        let report = json!({"total_txs": 3, "network": "testnet"});
        write_report(&dir, "axe-load-test-1700000000000", &report);
        let registry = RunRegistry::new(dir);

        match registry.state("axe-load-test-1700000000000").await {
            RunState::Finished {
                run_id,
                report: read,
            } => {
                assert_eq!(run_id.to_string(), "axe-load-test-1700000000000");
                assert_eq!(read, report);
            }
            other => panic!("expected finished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unparseable_report_reads_as_unknown() {
        let dir = scratch_reports_dir();
        std::fs::write(dir.join("axe-load-test-5.json"), "not json").unwrap();
        let registry = RunRegistry::new(dir);
        assert!(matches!(
            registry.state("axe-load-test-5").await,
            RunState::Unknown { .. }
        ));
    }

    #[tokio::test]
    async fn listing_is_newest_first_and_ignores_other_files() {
        let dir = scratch_reports_dir();
        write_report(&dir, "axe-load-test-1700000000001", &json!({}));
        write_report(&dir, "axe-load-test-1700000000002", &json!({}));
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let registry = RunRegistry::new(dir);

        let listed = registry.list().await;
        let ids: Vec<String> = listed
            .iter()
            .map(|entry| entry.run_id.to_string())
            .collect();
        assert_eq!(
            ids,
            ["axe-load-test-1700000000002", "axe-load-test-1700000000001"]
        );
        assert!(
            listed
                .iter()
                .all(|entry| entry.state == RunStatus::Finished)
        );
    }

    #[tokio::test]
    async fn only_one_run_is_admitted_at_a_time() {
        let registry = RunRegistry::new(scratch_reports_dir());
        let (release, held) = tokio::sync::oneshot::channel::<()>();

        let first = registry
            .start(RunKind::LoadTest, move |_| async move {
                let _ = held.await;
            })
            .unwrap();
        assert!(matches!(
            registry.state(&first.to_string()).await,
            RunState::Running { .. }
        ));
        assert_eq!(
            registry.start(RunKind::LoadTest, |_| async {}),
            Err(StartRefused::RunInFlight {
                run_id: first.clone()
            })
        );

        drop(release);
        wait_until_finished(&registry, &first).await;

        let second = registry.start(RunKind::LoadTest, |_| async {}).unwrap();
        assert!(second > first, "identifiers keep ascending across runs");
        wait_until_finished(&registry, &second).await;
        assert!(registry.list().await.is_empty(), "no report, no listing");
    }

    /// A blocking spend takes the same slot as a detached one: both spend
    /// from the same wallets, so neither may run while the other does.
    #[tokio::test]
    async fn a_claimed_slot_refuses_a_run_until_it_is_dropped() {
        let registry = RunRegistry::new(scratch_reports_dir());

        let slot = registry.claim_slot().expect("the slot starts free");
        // Named as this server's own doing: the file lock cannot tell the
        // difference, so the registry answers before reaching it.
        assert_eq!(
            registry.start(RunKind::LoadTest, |_| async {}),
            Err(StartRefused::SlotHeldHere)
        );

        drop(slot);
        let run_id = registry
            .start(RunKind::LoadTest, |_| async {})
            .expect("the slot is free again");
        wait_until_finished(&registry, &run_id).await;
    }

    #[tokio::test]
    async fn a_run_records_its_own_report() {
        let dir = scratch_reports_dir();
        let registry = RunRegistry::new(dir);
        let run_id = RunId::mint(RunKind::IntentsSend);

        registry.record_report(&run_id, &json!({"outcome": "completed"}));

        assert!(matches!(
            registry.state(&run_id.to_string()).await,
            RunState::Finished { report, .. } if report == json!({"outcome": "completed"})
        ));
    }

    #[tokio::test]
    async fn flow_receives_the_identifier_it_was_started_under() {
        let registry = RunRegistry::new(scratch_reports_dir());
        let (send_id, seen_id) = tokio::sync::oneshot::channel::<RunId>();

        let run_id = registry
            .start(RunKind::LoadTest, move |id| async move {
                let _ = send_id.send(id);
            })
            .unwrap();

        assert_eq!(seen_id.await.unwrap(), run_id);
    }

    /// Identifiers sort as text by their kind first, so ordering by the text
    /// would group the listing by flow and call it newest-first.
    #[tokio::test]
    async fn listing_orders_runs_of_different_kinds_by_when_they_were_minted() {
        let dir = scratch_reports_dir();
        write_report(&dir, "axe-load-test-1700000000003", &json!({}));
        write_report(&dir, "axe-intents-traffic-1700000000005", &json!({}));
        write_report(&dir, "axe-intents-sweep-1700000000004", &json!({}));
        let registry = RunRegistry::new(dir);

        let listed = registry.list().await;
        let ids: Vec<String> = listed
            .iter()
            .map(|entry| entry.run_id.to_string())
            .collect();
        assert_eq!(
            ids,
            [
                "axe-intents-traffic-1700000000005",
                "axe-intents-sweep-1700000000004",
                "axe-load-test-1700000000003",
            ]
        );
        assert_eq!(listed[0].kind, Some(RunKind::IntentsTraffic));
    }

    #[tokio::test]
    async fn files_without_the_run_prefix_are_not_runs() {
        let dir = scratch_reports_dir();
        std::fs::write(dir.join("spend-ledger.json"), r#"{"transactions":2}"#).unwrap();
        write_report(&dir, "axe-load-test-1700000000001", &json!({}));
        let registry = RunRegistry::new(dir);

        let listed = registry.list().await;
        let ids: Vec<String> = listed
            .iter()
            .map(|entry| entry.run_id.to_string())
            .collect();
        assert_eq!(ids, ["axe-load-test-1700000000001"]);
        assert!(matches!(
            registry.state("spend-ledger").await,
            RunState::Unknown { .. }
        ));
    }

    #[tokio::test]
    async fn waiting_for_in_flight_runs_returns_once_they_finish() {
        let registry = RunRegistry::new(scratch_reports_dir());
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let run_id = registry
            .start(RunKind::LoadTest, move |_| async move {
                let _ = held.await;
            })
            .unwrap();
        assert_eq!(registry.running(), [run_id]);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(release);
        });
        registry.wait_for_in_flight().await;
        assert!(registry.running().is_empty());
    }

    #[tokio::test]
    async fn a_second_registry_on_the_same_directory_is_refused_while_a_run_holds_the_lock() {
        let dir = scratch_reports_dir();
        let first_server = RunRegistry::new(dir.clone());
        let second_server = RunRegistry::new(dir.clone());
        let (release, held) = tokio::sync::oneshot::channel::<()>();

        let run_id = first_server
            .start(RunKind::LoadTest, move |_| async move {
                let _ = held.await;
            })
            .unwrap();
        assert_eq!(
            second_server.start(RunKind::LoadTest, |_| async {}),
            Err(StartRefused::HeldByAnotherProcess {
                lock: dir.join(super::LOCK_FILE)
            })
        );

        drop(release);
        wait_until_finished(&first_server, &run_id).await;
        let second = second_server
            .start(RunKind::LoadTest, |_| async {})
            .unwrap();
        wait_until_finished(&second_server, &second).await;
    }
}
