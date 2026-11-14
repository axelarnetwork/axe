use std::future::Future;
use std::time::Duration;

use eyre::{Result, eyre};
use reqwest::StatusCode;
use tokio::time::Instant;

use super::report::Output;
use super::types::RfqRoute;
use crate::commands::intents::client::RfqHttpError;
use crate::commands::intents::types::{StatusResponse, TransferState};

const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// How a status watch ended.
#[derive(Debug)]
pub enum PollEnd {
    /// DONE, or a state that already fails the route: a terminal failure, a
    /// refund in any stage, or NOT_FOUND, which for an ITS or gateway quote
    /// means rfq-api does not know the quote ID.
    Settled(StatusResponse),
    /// The timeout passed first, with the last status read, if any.
    TimedOut(Option<StatusResponse>),
}

/// Poll a quote's status until it settles or `timeout` passes. A failed poll
/// is retried, since an upstream outage answers 502, unless the API refused
/// the request itself.
pub async fn watch_status<F, Fut>(
    mut fetch: F,
    quote_id: &str,
    route: RfqRoute,
    timeout: Duration,
    output: Output,
) -> Result<PollEnd>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<StatusResponse>>,
{
    let deadline = Instant::now() + timeout;
    let mut last: Option<StatusResponse> = None;
    while Instant::now() < deadline {
        match fetch().await {
            Err(error) if is_refused(&error) => return Err(error),
            Err(error) => output.poll_failed(route, &error),
            Ok(status) if status.quote_id != quote_id => {
                return Err(eyre!(
                    "RFQ status for quote {quote_id} returned quote {}",
                    status.quote_id
                ));
            }
            Ok(status) => {
                if last.as_ref().map(|last| last.state) != Some(status.state) {
                    output.transition(route, &status);
                }
                if status.state.is_terminal()
                    || status.state.refund_in_progress()
                    || status.state == TransferState::NotFound
                {
                    return Ok(PollEnd::Settled(status));
                }
                last = Some(status);
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Ok(PollEnd::TimedOut(last))
}

/// A 4xx other than rate limiting: polling again would get the same answer.
fn is_refused(error: &eyre::Report) -> bool {
    error.downcast_ref::<RfqHttpError>().is_some_and(|error| {
        error.status.is_client_error() && error.status != StatusCode::TOO_MANY_REQUESTS
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(2700);

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

    async fn watch(answers: VecDeque<Result<StatusResponse>>) -> (PollEnd, u32) {
        let (fetch, polls) = scripted(answers);
        let end = watch_status(fetch, "its_quote", RfqRoute::Its, TIMEOUT, Output::Json)
            .await
            .unwrap();
        let polls = *polls.lock().unwrap();
        (end, polls)
    }

    fn settled_state(end: &PollEnd) -> Option<TransferState> {
        match end {
            PollEnd::Settled(status) => Some(status.state),
            PollEnd::TimedOut(_) => None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waits_through_pending_states_and_failed_polls_until_done() {
        let (end, polls) = watch(VecDeque::from([
            Ok(status("AWAITING_DEPOSIT")),
            Err(eyre!("HTTP 502")),
            Ok(status("PENDING")),
            Ok(status("PENDING")),
            Ok(status("DONE")),
        ]))
        .await;

        assert_eq!(settled_state(&end), Some(TransferState::Done));
        assert_eq!(polls, 5);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_and_not_found_settle_the_watch() {
        for (state, expected) in [
            ("FAILED", TransferState::Failed),
            ("NOT_FOUND", TransferState::NotFound),
        ] {
            let (end, _) = watch(VecDeque::from([Ok(status("PENDING")), Ok(status(state))])).await;

            assert_eq!(settled_state(&end), Some(expected));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_refund_state_settles_the_watch() {
        for (state, expected) in [
            ("REFUND_PENDING", TransferState::RefundPending),
            ("REFUND_INITIATED", TransferState::RefundInitiated),
            ("REFUNDED", TransferState::Refunded),
        ] {
            let (end, polls) =
                watch(VecDeque::from([Ok(status("PENDING")), Ok(status(state))])).await;

            assert_eq!(settled_state(&end), Some(expected));
            assert_eq!(polls, 2);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_with_the_last_status_after_polling_every_interval() {
        let started = Instant::now();
        let (end, polls) = watch(VecDeque::from([
            Ok(status("AWAITING_DEPOSIT")),
            Ok(status("PENDING")),
        ]))
        .await;

        assert!(matches!(
            end,
            PollEnd::TimedOut(Some(status)) if status.state == TransferState::Pending
        ));
        assert_eq!(polls, 180);
        assert!(started.elapsed() >= TIMEOUT);
    }

    fn http_error(status: StatusCode) -> eyre::Report {
        eyre::Report::new(RfqHttpError {
            method: "GET".to_owned(),
            url: "https://rfq.example/status".to_owned(),
            status,
            message: "error".to_owned(),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_request_ends_the_watch_but_rate_limits_and_outages_retry() {
        let (fetch, polls) = scripted(VecDeque::from([
            Err(http_error(StatusCode::TOO_MANY_REQUESTS)),
            Err(http_error(StatusCode::BAD_GATEWAY)),
            Err(http_error(StatusCode::BAD_REQUEST)),
            Ok(status("DONE")),
        ]));

        let error = watch_status(fetch, "its_quote", RfqRoute::Its, TIMEOUT, Output::Json)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("400 Bad Request"));
        assert_eq!(*polls.lock().unwrap(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_with_every_poll_failing_has_no_last_status() {
        let (end, _) = watch(VecDeque::from([Err(eyre!("HTTP 502"))])).await;

        assert!(matches!(end, PollEnd::TimedOut(None)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_status_for_another_quote_ends_the_watch_with_an_error() {
        let mut other = status("DONE");
        other.quote_id = "its_other".to_owned();
        let (fetch, _) = scripted(VecDeque::from([Ok(other)]));

        let error = watch_status(fetch, "its_quote", RfqRoute::Its, TIMEOUT, Output::Json)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("returned quote its_other"));
    }
}
