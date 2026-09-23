//! Operator-set limits on what a fund-spending tool may do.
//!
//! The client's approval prompt is a gate a human can click through, and the
//! model can be talked into asking. These caps are different: they are chosen
//! when the server starts and no tool argument can move them, so a run that
//! exceeds them is refused before anything is prepared or signed.
//!
//! The lifetime count is written to a ledger file after every change, so a
//! budget survives a restart instead of silently starting over.

use std::fmt::{Display, Formatter, Result as FmtResult};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

/// How many transactions one run may send when the operator does not say.
pub const DEFAULT_MAX_TXS_PER_RUN: u64 = 10;

/// The caps as the operator stated them on the command line.
#[derive(Debug, Clone)]
pub struct SpendLimits {
    /// The most transactions a single run may send.
    pub max_txs_per_run: u64,
    /// The most transactions this server may send over its lifetime. `None`
    /// means no lifetime budget.
    pub max_txs_total: Option<u64>,
    /// Chains a run may use as source or destination, by axelar id. Empty
    /// means any chain.
    pub allowed_chains: Vec<String>,
}

impl Default for SpendLimits {
    fn default() -> Self {
        Self {
            max_txs_per_run: DEFAULT_MAX_TXS_PER_RUN,
            max_txs_total: None,
            allowed_chains: Vec::new(),
        }
    }
}

/// Why a spend was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyViolation {
    RunTooLarge {
        requested: u64,
        max: u64,
    },
    BudgetExhausted {
        requested: u64,
        remaining: u64,
        max: u64,
    },
    ChainNotAllowed {
        chain: String,
        allowed: Vec<String>,
    },
    /// The ledger could not be written, so the spend was not admitted. Fails
    /// closed: a budget that cannot be recorded is not a budget.
    LedgerUnavailable {
        path: PathBuf,
        error: String,
    },
}

impl Display for PolicyViolation {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::RunTooLarge { requested, max } => write!(
                f,
                "{requested} transactions exceed the per-run cap of {max} set by the operator"
            ),
            Self::BudgetExhausted {
                requested,
                remaining,
                max,
            } => write!(
                f,
                "{requested} transactions exceed the remaining budget of {remaining} \
                 (server lifetime cap {max} set by the operator)"
            ),
            Self::ChainNotAllowed { chain, allowed } => write!(
                f,
                "chain {chain} is not in the operator's allowlist: {}",
                allowed.join(", ")
            ),
            Self::LedgerUnavailable { path, error } => write!(
                f,
                "could not record the spend in {}: {error}",
                path.display()
            ),
        }
    }
}

/// What the ledger file holds.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    /// Transactions admitted through this server so far.
    transactions: u64,
}

/// The running count and, when persistent, where it is written.
#[derive(Debug)]
struct Spent {
    count: u64,
    ledger: Option<PathBuf>,
}

/// The caps plus the running count they are checked against.
///
/// Clones share the count, so every handler sees one budget.
#[derive(Debug, Clone)]
pub struct SpendPolicy {
    limits: SpendLimits,
    spent: Arc<Mutex<Spent>>,
}

impl Default for SpendPolicy {
    fn default() -> Self {
        Self::new(SpendLimits::default())
    }
}

impl SpendPolicy {
    /// Caps with an in-memory count that starts at zero.
    pub fn new(limits: SpendLimits) -> Self {
        Self {
            limits,
            spent: Arc::new(Mutex::new(Spent {
                count: 0,
                ledger: None,
            })),
        }
    }

    /// Caps with a count that survives restarts, read from and written to
    /// `ledger`. A ledger that exists but cannot be read is an error, not a
    /// fresh start.
    pub fn persistent(limits: SpendLimits, ledger: PathBuf) -> Result<Self> {
        let count = read_ledger(&ledger)?.transactions;
        Ok(Self {
            limits,
            spent: Arc::new(Mutex::new(Spent {
                count,
                ledger: Some(ledger),
            })),
        })
    }

    /// The chains a spend may use, by axelar id. Empty means any.
    ///
    /// A tool that names its chains checks them with [`check_chain`]. A flow
    /// that discovers its own routes takes this list instead and narrows what
    /// it can discover, which comes to the same thing a step earlier.
    ///
    /// [`check_chain`]: Self::check_chain
    pub fn allowed_chains(&self) -> &[String] {
        &self.limits.allowed_chains
    }

    /// Refuse a chain the operator did not allow.
    pub fn check_chain(&self, chain: &str) -> Result<(), PolicyViolation> {
        let allowed = &self.limits.allowed_chains;
        if allowed.is_empty() || allowed.iter().any(|a| a.eq_ignore_ascii_case(chain)) {
            return Ok(());
        }
        Err(PolicyViolation::ChainNotAllowed {
            chain: chain.to_string(),
            allowed: allowed.clone(),
        })
    }

    /// Claim `num_txs` against the caps, under one lock, so two concurrent
    /// starts cannot both fit inside the last of the budget.
    ///
    /// A caller that fails to start the run afterwards must [`release`] what
    /// it claimed.
    ///
    /// [`release`]: Self::release
    pub fn reserve(&self, num_txs: u64) -> Result<(), PolicyViolation> {
        if num_txs > self.limits.max_txs_per_run {
            return Err(PolicyViolation::RunTooLarge {
                requested: num_txs,
                max: self.limits.max_txs_per_run,
            });
        }

        let mut spent = self.spent.lock().unwrap_or_else(PoisonError::into_inner);
        let total = spent.count.saturating_add(num_txs);
        if let Some(max) = self.limits.max_txs_total
            && total > max
        {
            return Err(PolicyViolation::BudgetExhausted {
                requested: num_txs,
                remaining: max.saturating_sub(spent.count),
                max,
            });
        }

        let before = spent.count;
        spent.count = total;
        if let Err(violation) = write_ledger(&spent) {
            spent.count = before;
            return Err(violation);
        }
        Ok(())
    }

    /// Hand back a reservation for a run that was never started.
    pub fn release(&self, num_txs: u64) {
        let mut spent = self.spent.lock().unwrap_or_else(PoisonError::into_inner);
        spent.count = spent.count.saturating_sub(num_txs);
        // A failed write here leaves the ledger over-counting, which errs on
        // the side of spending less. Nothing useful to do about it.
        let _ = write_ledger(&spent);
    }

    /// The caps in one sentence, for the server instructions, so an agent
    /// knows the limits before it asks for a run that would be refused.
    pub fn describe(&self) -> String {
        let mut parts = vec![format!(
            "at most {} transactions per load test",
            self.limits.max_txs_per_run
        )];
        if let Some(max) = self.limits.max_txs_total {
            let spent = self
                .spent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .count;
            parts.push(format!(
                "{} of {max} transactions remaining over the server's lifetime",
                max.saturating_sub(spent)
            ));
        }
        if !self.limits.allowed_chains.is_empty() {
            parts.push(format!(
                "only these chains: {}",
                self.limits.allowed_chains.join(", ")
            ));
        }
        format!("Operator caps: {}.", parts.join("; "))
    }
}

/// The ledger on disk, or an empty one when there is no file yet.
fn read_ledger(path: &Path) -> Result<Ledger> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .wrap_err_with(|| format!("spend ledger {} is not readable", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(e) => Err(e).wrap_err_with(|| format!("could not read {}", path.display())),
    }
}

/// Write the count to the ledger, when there is one.
fn write_ledger(spent: &Spent) -> Result<(), PolicyViolation> {
    let Some(path) = &spent.ledger else {
        return Ok(());
    };
    let ledger = Ledger {
        transactions: spent.count,
    };
    let unavailable = |e: &dyn Display| PolicyViolation::LedgerUnavailable {
        path: path.clone(),
        error: e.to_string(),
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| unavailable(&e))?;
    }
    let text = serde_json::to_string(&ledger).map_err(|e| unavailable(&e))?;
    std::fs::write(path, text).map_err(|e| unavailable(&e))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{PolicyViolation, SpendLimits, SpendPolicy};

    static LEDGERS: AtomicUsize = AtomicUsize::new(0);

    fn scratch_ledger() -> PathBuf {
        std::env::temp_dir().join(format!(
            "axe-mcp-ledger-{}-{}/spend-ledger.json",
            std::process::id(),
            LEDGERS.fetch_add(1, Ordering::SeqCst)
        ))
    }

    fn limits(max_txs_per_run: u64, max_txs_total: Option<u64>, chains: &[&str]) -> SpendLimits {
        SpendLimits {
            max_txs_per_run,
            max_txs_total,
            allowed_chains: chains.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn policy(max_txs_per_run: u64, max_txs_total: Option<u64>, chains: &[&str]) -> SpendPolicy {
        SpendPolicy::new(limits(max_txs_per_run, max_txs_total, chains))
    }

    #[test]
    fn default_caps_a_run_but_not_the_lifetime_or_the_chains() {
        let policy = SpendPolicy::default();
        assert_eq!(policy.reserve(10), Ok(()));
        assert!(matches!(
            policy.reserve(11),
            Err(PolicyViolation::RunTooLarge {
                requested: 11,
                max: 10
            })
        ));
        for _ in 0..100 {
            assert_eq!(policy.reserve(10), Ok(()), "no lifetime budget by default");
        }
        assert_eq!(policy.check_chain("anything"), Ok(()));
    }

    #[test]
    fn lifetime_budget_is_claimed_across_runs_and_released_on_failure() {
        let policy = policy(5, Some(8), &[]);
        assert_eq!(policy.reserve(5), Ok(()));
        assert_eq!(
            policy.reserve(5),
            Err(PolicyViolation::BudgetExhausted {
                requested: 5,
                remaining: 3,
                max: 8
            })
        );
        assert_eq!(policy.reserve(3), Ok(()));

        policy.release(3);
        assert_eq!(policy.reserve(3), Ok(()));
        assert!(policy.reserve(1).is_err(), "budget is spent");
    }

    /// A run bounded at ten that sends two has not spent ten. Without the
    /// refund a lifetime budget counts down by what was reserved, so runs
    /// that find few routes exhaust it while spending almost nothing.
    #[test]
    fn the_unspent_part_of_a_reservation_goes_back_to_the_budget() {
        let policy = policy(10, Some(12), &[]);
        assert_eq!(policy.reserve(10), Ok(()));
        policy.release(10 - 2);

        assert_eq!(policy.reserve(10), Ok(()), "only 2 of the 12 were spent");
        assert!(policy.reserve(1).is_err(), "and now the budget is gone");
    }

    #[test]
    fn budget_is_shared_between_clones() {
        let policy = policy(5, Some(5), &[]);
        let other = policy.clone();
        assert_eq!(policy.reserve(5), Ok(()));
        assert!(other.reserve(1).is_err());
    }

    #[test]
    fn budget_survives_a_restart_through_the_ledger() {
        let ledger = scratch_ledger();

        let first = SpendPolicy::persistent(limits(5, Some(8), &[]), ledger.clone()).unwrap();
        assert_eq!(first.reserve(5), Ok(()));
        drop(first);

        let restarted = SpendPolicy::persistent(limits(5, Some(8), &[]), ledger.clone()).unwrap();
        assert_eq!(
            restarted.reserve(4),
            Err(PolicyViolation::BudgetExhausted {
                requested: 4,
                remaining: 3,
                max: 8
            })
        );
        assert_eq!(restarted.reserve(3), Ok(()));
        assert_eq!(
            std::fs::read_to_string(&ledger).unwrap(),
            r#"{"transactions":8}"#
        );
    }

    #[test]
    fn a_corrupt_ledger_refuses_to_start_rather_than_resetting() {
        let ledger = scratch_ledger();
        std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        std::fs::write(&ledger, "not json").unwrap();

        let err = SpendPolicy::persistent(limits(5, Some(8), &[]), ledger)
            .expect_err("a corrupt ledger must not read as zero");
        assert!(err.to_string().contains("not readable"), "{err}");
    }

    #[test]
    fn chain_allowlist_is_case_insensitive_and_names_the_allowed_set() {
        let policy = policy(5, None, &["solana", "flow"]);
        assert_eq!(policy.check_chain("Solana"), Ok(()));
        assert_eq!(
            policy.check_chain("ethereum"),
            Err(PolicyViolation::ChainNotAllowed {
                chain: "ethereum".into(),
                allowed: vec!["solana".into(), "flow".into()],
            })
        );
    }

    #[test]
    fn description_states_every_cap_that_is_set_and_what_remains() {
        assert_eq!(
            SpendPolicy::default().describe(),
            "Operator caps: at most 10 transactions per load test."
        );
        let policy = policy(2, Some(20), &["solana", "flow"]);
        assert_eq!(policy.reserve(2), Ok(()));
        assert_eq!(
            policy.describe(),
            "Operator caps: at most 2 transactions per load test; 18 of 20 transactions \
             remaining over the server's lifetime; only these chains: solana, flow."
        );
    }
}
