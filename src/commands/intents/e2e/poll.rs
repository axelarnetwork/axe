use std::future::Future;
use std::time::Duration;

use eyre::Result;
use tokio::time::Instant;

use super::report::Output;
use crate::commands::intents::types::{StatusResponse, TransferState};

#[derive(Clone, Copy, Debug)]
pub struct PollLimits {
    pub interval: Duration,
    pub timeout: Duration,
}

/// How a status watch ended.
#[derive(Debug)]
pub enum PollEnd {
    Done(StatusResponse),
    /// A terminal state other than DONE.
    Failed(StatusResponse),
    /// The timeout passed first, with the last status read, if any.
    TimedOut(Option<StatusResponse>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminal {
    Done,
    Failed,
}

/// A failed poll is retried, not fatal: an upstream outage answers 502.
pub async fn watch_status<F, Fut>(mut fetch: F, limits: PollLimits, output: Output) -> PollEnd
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<StatusResponse>>,
{
    let deadline = Instant::now() + limits.timeout;
    let mut last: Option<StatusResponse> = None;
    while Instant::now() < deadline {
        match fetch().await {
            Err(error) => output.poll_failed(&error),
            Ok(status) => {
                if last.as_ref().map(|last| last.state) != Some(status.state) {
                    output.transition(&status);
                    if refund_in_progress(status.state) {
                        output.warn("the transfer is being refunded and will not reach DONE");
                    }
                }
                match terminal(status.state) {
                    Some(Terminal::Done) => return PollEnd::Done(status),
                    Some(Terminal::Failed) => return PollEnd::Failed(status),
                    None => last = Some(status),
                }
            }
        }
        tokio::time::sleep(limits.interval).await;
    }
    PollEnd::TimedOut(last)
}

/// A refund rfq-api has decided or submitted but not yet seen land. The
/// watch goes on until the refund settles or the timeout passes.
pub const fn refund_in_progress(state: TransferState) -> bool {
    matches!(
        state,
        TransferState::RefundPending | TransferState::RefundInitiated
    )
}

const fn terminal(state: TransferState) -> Option<Terminal> {
    match state {
        TransferState::Done => Some(Terminal::Done),
        TransferState::Failed | TransferState::NotFound | TransferState::Refunded => {
            Some(Terminal::Failed)
        }
        TransferState::AwaitingDeposit
        | TransferState::Pending
        | TransferState::RefundPending
        | TransferState::RefundInitiated => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use eyre::eyre;
    use serde_json::json;

    use super::*;

    const LIMITS: PollLimits = PollLimits {
        interval: Duration::from_secs(15),
        timeout: Duration::from_secs(2700),
    };

    fn status(state: &str) -> StatusResponse {
        serde_json::from_value(json!({
            "quoteId": "its_quote",
            "state": state,
            "backend": { "type": "its", "name": "axelar", "tracking": {}, "metadata": {} },
            "source": null,
            "destination": null,
            "output": null,
            "refund": null,
            "details": {}
        }))
        .unwrap()
    }

    /// Answers each poll with the next scripted result, then repeats the
    /// last one, and counts the polls made.
    fn scripted(
        mut answers: VecDeque<Result<StatusResponse>>,
    ) -> (
        impl FnMut() -> std::future::Ready<Result<StatusResponse>>,
        Arc<Mutex<u32>>,
    ) {
        let polls = Arc::new(Mutex::new(0));
        let counter = Arc::clone(&polls);
        let fetch = move || {
            *counter.lock().unwrap() += 1;
            let answer = if answers.len() > 1 {
                answers.pop_front().unwrap()
            } else {
                match answers.front().unwrap() {
                    Ok(status) => Ok(status.clone()),
                    Err(error) => Err(eyre!("{error}")),
                }
            };
            std::future::ready(answer)
        };
        (fetch, polls)
    }

    #[test]
    fn only_done_passes_and_awaiting_or_pending_keep_waiting() {
        assert_eq!(terminal(TransferState::Done), Some(Terminal::Done));
        for state in [
            TransferState::Failed,
            TransferState::NotFound,
            TransferState::Refunded,
        ] {
            assert_eq!(terminal(state), Some(Terminal::Failed));
        }
        for state in [
            TransferState::AwaitingDeposit,
            TransferState::Pending,
            TransferState::RefundPending,
            TransferState::RefundInitiated,
        ] {
            assert_eq!(terminal(state), None);
        }
    }

    #[test]
    fn only_the_unsettled_refund_states_count_as_a_refund_in_progress() {
        assert!(refund_in_progress(TransferState::RefundPending));
        assert!(refund_in_progress(TransferState::RefundInitiated));
        for state in [
            TransferState::AwaitingDeposit,
            TransferState::Pending,
            TransferState::Done,
            TransferState::Refunded,
            TransferState::Failed,
            TransferState::NotFound,
        ] {
            assert!(!refund_in_progress(state));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn keeps_polling_through_refund_states_until_the_refund_lands() {
        let (fetch, polls) = scripted(VecDeque::from([
            Ok(status("PENDING")),
            Ok(status("REFUND_PENDING")),
            Ok(status("REFUND_INITIATED")),
            Ok(status("REFUND_INITIATED")),
            Ok(status("REFUNDED")),
        ]));

        let end = watch_status(fetch, LIMITS, Output::Json).await;

        assert!(matches!(end, PollEnd::Failed(status) if status.state == TransferState::Refunded));
        assert_eq!(*polls.lock().unwrap(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unsettled_refund_runs_into_the_timeout() {
        let (fetch, _) = scripted(VecDeque::from([
            Ok(status("REFUND_PENDING")),
            Ok(status("REFUND_INITIATED")),
        ]));

        let end = watch_status(fetch, LIMITS, Output::Json).await;

        assert!(matches!(
            end,
            PollEnd::TimedOut(Some(status)) if status.state == TransferState::RefundInitiated
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn waits_through_pending_states_and_failed_polls_until_done() {
        let (fetch, polls) = scripted(VecDeque::from([
            Ok(status("AWAITING_DEPOSIT")),
            Err(eyre!("HTTP 502")),
            Ok(status("PENDING")),
            Ok(status("PENDING")),
            Ok(status("DONE")),
        ]));

        let end = watch_status(fetch, LIMITS, Output::Json).await;

        assert!(matches!(end, PollEnd::Done(status) if status.state == TransferState::Done));
        assert_eq!(*polls.lock().unwrap(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_and_not_found_end_the_watch_as_failures() {
        for state in ["FAILED", "NOT_FOUND"] {
            let (fetch, _) = scripted(VecDeque::from([Ok(status("PENDING")), Ok(status(state))]));

            let end = watch_status(fetch, LIMITS, Output::Json).await;

            assert!(matches!(end, PollEnd::Failed(status) if status.state != TransferState::Done));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_with_the_last_status_after_polling_every_interval() {
        let started = Instant::now();
        let (fetch, polls) = scripted(VecDeque::from([Ok(status("PENDING"))]));

        let end = watch_status(fetch, LIMITS, Output::Json).await;

        assert!(
            matches!(end, PollEnd::TimedOut(Some(status)) if status.state == TransferState::Pending)
        );
        assert_eq!(*polls.lock().unwrap(), 180);
        assert!(started.elapsed() >= LIMITS.timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_with_every_poll_failing_has_no_last_status() {
        let (fetch, _) = scripted(VecDeque::from([Err(eyre!("HTTP 502"))]));

        let end = watch_status(fetch, LIMITS, Output::Json).await;

        assert!(matches!(end, PollEnd::TimedOut(None)));
    }
}
