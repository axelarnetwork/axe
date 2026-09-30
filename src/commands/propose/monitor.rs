use eyre::{Result, WrapErr};
use serde_json::Value;

use super::types::ProposalOutcome;
use crate::{cosmos::lcd_query_proposal, timing::COSMOS_PROPOSAL_POLL_INTERVAL, ui};

#[cfg(test)]
mod tests;

pub async fn wait_for_passed(lcd: &str, proposal_id: u64) -> Result<()> {
    match monitor_proposal(lcd, proposal_id).await.wrap_err_with(|| {
        format!("proposal {proposal_id} already exists; rerun to resume monitoring without submitting another proposal")
    })? {
        ProposalOutcome::Passed => Ok(()),
        ProposalOutcome::Rejected(reason) | ProposalOutcome::Failed(reason) => Err(reason.wrap_err(
            "proposal reached a terminal failure; rerunning will recover the same result. To intentionally submit a replacement, remove --proposal-id if present and use --new-proposal",
        )),
    }
}

/// Poll until a terminal vote result. Read errors remain separate from failed votes.
async fn monitor_proposal(lcd: &str, proposal_id: u64) -> Result<ProposalOutcome> {
    let spinner = ui::wait_spinner(&format!(
        "monitoring proposal {proposal_id} (vote in another terminal)..."
    ));
    loop {
        let proposal = lcd_query_proposal(lcd, proposal_id).await?;
        let status = proposal["status"].as_str().unwrap_or("UNKNOWN");
        spinner.set_message(format!(
            "proposal {proposal_id}: {status}{}",
            voting_eta_suffix(&proposal)
        ));

        match status {
            "PROPOSAL_STATUS_PASSED" => {
                spinner.finish_and_clear();
                ui::success(&format!("proposal {proposal_id} passed"));
                return Ok(ProposalOutcome::Passed);
            }
            "PROPOSAL_STATUS_REJECTED" | "PROPOSAL_STATUS_FAILED" => {
                spinner.finish_and_clear();
                let failure = proposal_failure(proposal_id, status, &proposal);
                return Ok(if status == "PROPOSAL_STATUS_REJECTED" {
                    ProposalOutcome::Rejected(failure)
                } else {
                    ProposalOutcome::Failed(failure)
                });
            }
            _ => tokio::time::sleep(COSMOS_PROPOSAL_POLL_INTERVAL).await,
        }
    }
}

/// An "ends in 3m07s (~14:22:31)" suffix for a voting-period proposal,
/// derived from `voting_end_time`. Empty when the field is absent/unparseable.
fn voting_eta_suffix(proposal: &Value) -> String {
    let Some(end) = proposal["voting_end_time"].as_str() else {
        return String::new();
    };
    let Ok(end_dt) = chrono::DateTime::parse_from_rfc3339(end) else {
        return String::new();
    };
    let remaining = end_dt.timestamp() - chrono::Utc::now().timestamp();
    if remaining <= 0 {
        return " — voting closed, tallying".to_string();
    }
    let local = end_dt.with_timezone(&chrono::Local).format("%H:%M:%S");
    format!(" — ends in {} (~{local})", human_secs(remaining))
}

fn human_secs(total: i64) -> String {
    let minutes = total / 60;
    let seconds = total % 60;
    if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn proposal_failure(proposal_id: u64, status: &str, proposal: &Value) -> eyre::Report {
    let reason = proposal["failed_reason"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("no reason provided");
    let tally = &proposal["final_tally_result"];
    eyre::eyre!(
        "proposal {proposal_id} {status}\n  reason: {reason}\n  tally: yes={} no={} abstain={} no_with_veto={}",
        tally["yes_count"].as_str().unwrap_or("?"),
        tally["no_count"].as_str().unwrap_or("?"),
        tally["abstain_count"].as_str().unwrap_or("?"),
        tally["no_with_veto_count"].as_str().unwrap_or("?"),
    )
}
