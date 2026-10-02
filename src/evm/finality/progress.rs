use std::time::Duration;

use crate::ui;

#[cfg(test)]
mod tests;

const MIN_SAMPLE: Duration = Duration::from_secs(30);
const STALE_AFTER: Duration = Duration::from_secs(120);

pub(super) struct FinalityProgress {
    initial_height: u64,
    initial_elapsed: Duration,
    latest_height: u64,
    last_advance: Duration,
}

impl FinalityProgress {
    pub(super) fn new(height: u64, elapsed: Duration) -> Self {
        Self {
            initial_height: height,
            initial_elapsed: elapsed,
            latest_height: height,
            last_advance: elapsed,
        }
    }

    pub(super) fn observe(&mut self, height: u64, elapsed: Duration) {
        if height < self.latest_height {
            *self = Self::new(height, elapsed);
        } else if height > self.latest_height {
            self.latest_height = height;
            self.last_advance = elapsed;
        }
    }

    pub(super) fn total(&self, target: u64) -> u64 {
        target.saturating_sub(self.initial_height)
    }

    pub(super) fn completed(&self, target: u64) -> u64 {
        self.latest_height
            .saturating_sub(self.initial_height)
            .min(self.total(target))
    }

    fn eta(&self, target: u64, elapsed: Duration) -> Option<Duration> {
        if self.latest_height >= target {
            return Some(Duration::ZERO);
        }
        let sample = elapsed.saturating_sub(self.initial_elapsed);
        let advanced = self.latest_height.saturating_sub(self.initial_height);
        if sample < MIN_SAMPLE || advanced == 0 || self.stalled(elapsed) {
            return None;
        }
        let seconds = (target - self.latest_height) as f64 * sample.as_secs_f64() / advanced as f64;
        Duration::try_from_secs_f64(seconds).ok()
    }

    fn stalled(&self, elapsed: Duration) -> bool {
        elapsed.saturating_sub(self.last_advance) >= STALE_AFTER
    }

    pub(super) fn eta_label(&self, target: u64, elapsed: Duration) -> String {
        match self.eta(target, elapsed) {
            Some(eta) => format!("~{}", ui::format_duration(eta)),
            None if self.stalled(elapsed) => "unavailable (no recent finality progress)".into(),
            None => "estimating".into(),
        }
    }
}
