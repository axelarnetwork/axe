use super::*;
use crate::http::tests::{response, serve};
use serde_json::json;

#[tokio::test]
async fn terminal_votes_recommend_explicit_replacement_not_resume() {
    for status in ["PROPOSAL_STATUS_REJECTED", "PROPOSAL_STATUS_FAILED"] {
        let body = json!({"proposal": {
            "status": status,
            "failed_reason": "permission denied",
            "final_tally_result": {"yes_count":"1", "no_count":"2"}
        }});
        let (url, task) = serve(vec![response("200 OK", &body.to_string())]).await;
        let error = wait_for_passed(url.as_str().trim_end_matches('/'), 645)
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(status));
        assert!(message.contains("permission denied"));
        assert!(message.contains("yes=1 no=2"));
        assert!(message.contains("--new-proposal"));
        assert!(message.contains("remove --proposal-id"));
        assert!(!message.contains("rerun to resume monitoring"));
        assert_eq!(task.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn read_failure_recommends_resume_without_a_new_deposit() {
    let (url, task) = serve(vec![response("403 Forbidden", "{}")]).await;
    let error = wait_for_passed(url.as_str().trim_end_matches('/'), 645)
        .await
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("proposal 645 already exists"));
    assert!(message.contains("rerun to resume monitoring"));
    assert!(!message.contains("--new-proposal"));
    assert_eq!(task.await.unwrap().len(), 1);
}

#[tokio::test]
async fn monitor_survives_connection_loss_after_submission() {
    let body = r#"{"proposal":{"id":"645","status":"PROPOSAL_STATUS_PASSED"}}"#;
    let (url, task) =
        crate::http::tests::serve(vec![None, crate::http::tests::response("200 OK", body)]).await;
    wait_for_passed(url.as_str().trim_end_matches('/'), 645)
        .await
        .unwrap();
    let requests = task.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|r| r.starts_with("GET /cosmos/gov/v1/proposals/645 "))
    );
}
