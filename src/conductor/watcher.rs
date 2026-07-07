//! Tick loop that turns a fleet snapshot into recommendations and,
//! optionally, applied actions via the executor.

use std::time::Duration;

use anyhow::Result;
use tokio::select;
use tokio::sync::oneshot;

use super::executor::{Executor, Outcome};
use super::observation::build_observation;
use super::policies::QuietHours;
use super::reasoner::{Reasoner, Recommendation};
use crate::session::Storage;
use chrono::{Local, Timelike};

/// Minimum poll interval accepted from the CLI. Prevents runaway subprocess
/// spawns if a user typos `--poll-interval 0`.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Default poll interval when the user does not override. Chosen generously
/// because a `claude --print` roundtrip is not free (both latency and cost).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

pub struct Watcher<R: Reasoner> {
    profile: String,
    reasoner: R,
    poll_interval: Duration,
    /// When `Some`, recommendations are dispatched through the executor
    /// (live mode). When `None`, they are only logged (dry-run mode).
    executor: Option<Executor>,
    /// When set, the tick loop skips reasoning during this daily window.
    /// Ports aoaoe's `quietHours`.
    quiet_hours: Option<QuietHours>,
}

impl<R: Reasoner> Watcher<R> {
    pub fn new(profile: impl Into<String>, reasoner: R, poll_interval: Duration) -> Self {
        Self {
            profile: profile.into(),
            reasoner,
            poll_interval: poll_interval.max(MIN_POLL_INTERVAL),
            executor: None,
            quiet_hours: None,
        }
    }

    /// Enable live mode: recommendations are applied via `Executor` after
    /// each tick instead of being logged as dry-run.
    pub fn with_executor(mut self, executor: Executor) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Set the daily quiet-hours window during which reasoning is
    /// skipped. The watcher still wakes each `poll_interval`; it just
    /// short-circuits before spawning the reasoner subprocess.
    pub fn with_quiet_hours(mut self, quiet_hours: QuietHours) -> Self {
        self.quiet_hours = Some(quiet_hours);
        self
    }

    /// One iteration: read sessions, build an observation, call the
    /// reasoner. Returns the recommendations (which the caller decides
    /// what to do with: the CLI logs them; the TUI will render them.
    pub async fn tick(&self) -> Result<Vec<Recommendation>> {
        let storage = Storage::new_unwatched(&self.profile)?;
        let (mut instances, _) = storage.load_with_groups()?;
        crate::tmux::refresh_session_cache();
        for inst in &mut instances {
            inst.update_status();
        }
        let observation = build_observation(&instances);
        self.reasoner.recommend(&observation).await
    }

    /// Long-running loop. Sleeps for `poll_interval` between ticks. Exits
    /// cleanly on shutdown signal. Reasoner errors are logged but do not
    /// stop the loop; a transient `claude` failure should not tear the
    /// watcher down.
    pub async fn run(&self, mut shutdown: oneshot::Receiver<()>) -> Result<()> {
        tracing::info!(
            profile = %self.profile,
            poll_interval_secs = self.poll_interval.as_secs(),
            "conductor watch started"
        );
        loop {
            if self.in_quiet_hours() {
                tracing::debug!("conductor tick skipped: quiet hours");
            } else {
                match self.tick().await {
                    Ok(recs) => {
                        log_recommendations(&recs);
                        if let Some(executor) = &self.executor {
                            match executor.dispatch(&recs) {
                                Ok(outcomes) => log_outcomes(&outcomes),
                                Err(err) => {
                                    tracing::warn!(error = %err, "conductor dispatch failed")
                                }
                            }
                        }
                    }
                    Err(err) => tracing::warn!(error = %err, "conductor tick failed"),
                }
            }
            select! {
                _ = tokio::time::sleep(self.poll_interval) => {}
                _ = &mut shutdown => {
                    tracing::info!("conductor watch shutting down");
                    return Ok(());
                }
            }
        }
    }

    fn in_quiet_hours(&self) -> bool {
        let Some(window) = self.quiet_hours else {
            return false;
        };
        let now = Local::now();
        let minute_of_day = now.hour() * 60 + now.minute();
        window.contains(minute_of_day)
    }
}

fn log_outcomes(outcomes: &[Outcome]) {
    for o in outcomes {
        match o {
            Outcome::Applied { session_id, action } => {
                tracing::info!(session_id = %session_id, action = ?action, "applied");
            }
            Outcome::Blocked {
                session_id,
                action,
                reason,
            } => {
                tracing::info!(session_id = %session_id, action = ?action, reason = %reason, "blocked");
            }
            Outcome::UnknownSession { session_id } => {
                tracing::info!(session_id = %session_id, "unknown session, skipped");
            }
            Outcome::NoOp { session_id } => {
                tracing::debug!(session_id = %session_id, "no_op");
            }
        }
    }
}

fn log_recommendations(recs: &[Recommendation]) {
    if recs.is_empty() {
        tracing::info!("no recommendations this tick");
        return;
    }
    for r in recs {
        tracing::info!(
            session_id = %r.session_id,
            action = ?r.action,
            rationale = %r.rationale,
            confidence = ?r.confidence,
            "recommendation"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::reasoner::{Action, Observation};

    struct StubReasoner {
        canned: Vec<Recommendation>,
    }

    impl Reasoner for StubReasoner {
        async fn recommend(&self, _obs: &Observation) -> Result<Vec<Recommendation>> {
            Ok(self.canned.clone())
        }
    }

    #[test]
    fn poll_interval_floor_enforced() {
        let w = Watcher::new(
            "test",
            StubReasoner { canned: vec![] },
            Duration::from_secs(1),
        );
        assert!(w.poll_interval >= MIN_POLL_INTERVAL);
    }

    #[test]
    fn poll_interval_preserved_when_above_floor() {
        let w = Watcher::new(
            "test",
            StubReasoner { canned: vec![] },
            Duration::from_secs(120),
        );
        assert_eq!(w.poll_interval, Duration::from_secs(120));
    }

    #[test]
    fn log_recommendations_empty_no_panic() {
        log_recommendations(&[]);
    }

    #[test]
    fn log_recommendations_populated_no_panic() {
        let recs = vec![Recommendation {
            session_id: "abc".into(),
            action: Action::Snooze { minutes: 15 },
            rationale: "quiet hours".into(),
            confidence: Some(0.9),
        }];
        log_recommendations(&recs);
    }
}
