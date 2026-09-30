use std::time::Duration;

use super::FinalityProgress;

#[test]
fn estimates_from_finalized_progress_after_sampling() {
    let mut progress = FinalityProgress::new(100, Duration::ZERO);
    assert_eq!(progress.eta(200, Duration::ZERO), None);
    progress.observe(110, Duration::from_secs(10));
    assert_eq!(progress.eta(200, Duration::from_secs(10)), None);
    progress.observe(130, Duration::from_secs(30));
    assert_eq!(progress.total(200), 100);
    assert_eq!(progress.completed(200), 30);
    assert_eq!(
        progress.eta(200, Duration::from_secs(30)),
        Some(Duration::from_secs(70))
    );
}

#[test]
fn paused_finality_does_not_count_down_and_stale_eta_is_hidden() {
    let mut progress = FinalityProgress::new(100, Duration::ZERO);
    progress.observe(130, Duration::from_secs(30));
    progress.observe(130, Duration::from_secs(60));
    assert_eq!(
        progress.eta(200, Duration::from_secs(60)),
        Some(Duration::from_secs(140))
    );
    assert_eq!(progress.eta(200, Duration::from_secs(150)), None);
    assert!(
        progress
            .eta_label(200, Duration::from_secs(150))
            .starts_with("unavailable")
    );
    progress.observe(160, Duration::from_secs(180));
    assert_eq!(
        progress.eta(200, Duration::from_secs(180)),
        Some(Duration::from_secs(120))
    );
}

#[test]
fn regressing_rpc_height_resets_the_estimate_and_progress_baseline() {
    let mut progress = FinalityProgress::new(100, Duration::ZERO);
    progress.observe(130, Duration::from_secs(30));
    progress.observe(120, Duration::from_secs(40));
    assert_eq!(progress.total(200), 80);
    assert_eq!(progress.completed(200), 0);
    assert_eq!(progress.eta(200, Duration::from_secs(40)), None);
    progress.observe(150, Duration::from_secs(70));
    assert_eq!(
        progress.eta(200, Duration::from_secs(70)),
        Some(Duration::from_secs(50))
    );
}

#[test]
fn checkpoint_jumps_complete_without_overshooting_the_bar() {
    let mut progress = FinalityProgress::new(100, Duration::ZERO);
    progress.observe(220, Duration::from_secs(30));
    assert_eq!(progress.completed(200), progress.total(200));
    assert_eq!(
        progress.eta(200, Duration::from_secs(30)),
        Some(Duration::ZERO)
    );
    let already_finalized = FinalityProgress::new(220, Duration::ZERO);
    assert_eq!(already_finalized.total(200), 0);
    assert_eq!(already_finalized.completed(200), 0);
    assert_eq!(
        already_finalized.eta(200, Duration::ZERO),
        Some(Duration::ZERO)
    );
}
