//! Project a fleet of `Instance`s into the `Observation` the reasoner
//! sees. Optionally attaches content-derived signals (activity, sentiment,
//! error match, goal completion, heartbeat) when a `HeartbeatTracker` is
//! provided; otherwise ships a lean snapshot without touching tmux.

use chrono::Utc;

use super::heartbeat::HeartbeatTracker;
use super::{
    attention_score, errors::match_error, goals::goal_completed, reasoner::Observation,
    reasoner::SessionSnapshot, signals::classify_activity, signals::classify_sentiment,
};
use crate::session::Instance;

/// Number of tail lines to grab from each pane. Enough for the signal
/// classifiers to hit their tokens, cheap enough to invoke every tick.
const PANE_TAIL_LINES: usize = 200;

/// Build an observation with no pane-content signals. Used by the
/// read-only surfaces (`aoe conductor status`, the TUI panel, the web
/// endpoint) so they stay snappy and don't spawn tmux per row.
pub fn build_observation(instances: &[Instance]) -> Observation {
    build_observation_with_signals(instances, None)
}

/// Build an observation with content-derived signals. The watcher uses
/// this on live ticks so the reasoner sees enough to detect stuck
/// sessions and match errors.
pub fn build_observation_with_signals(
    instances: &[Instance],
    tracker: Option<&HeartbeatTracker>,
) -> Observation {
    let sessions: Vec<SessionSnapshot> = instances
        .iter()
        .filter_map(|inst| {
            let score = attention_score(inst)?;
            let (activity, sentiment, error_match, goal_complete, heartbeat) = match tracker {
                Some(tracker) => match capture_pane(inst) {
                    Some(pane) => {
                        let hb = tracker.observe(&inst.id, &pane);
                        (
                            Some(classify_activity(&pane)),
                            Some(classify_sentiment(&pane)),
                            match_error(&pane),
                            goal_completed(&pane),
                            Some(hb),
                        )
                    }
                    None => (None, None, None, false, None),
                },
                None => (None, None, None, false, None),
            };
            Some(SessionSnapshot {
                id: inst.id.clone(),
                title: inst.title.clone(),
                status: format!("{:?}", inst.status),
                attention_score: score,
                favorited: inst.favorited_at.is_some(),
                unread: inst.unread,
                archived: false,
                snoozed_until: inst.snoozed_until,
                last_accessed_at: inst.last_accessed_at,
                activity,
                sentiment,
                error_match,
                goal_completed: goal_complete,
                unchanged_ticks: heartbeat.map(|h| h.unchanged_ticks).unwrap_or(0),
                potentially_stuck: heartbeat.map(|h| h.potentially_stuck).unwrap_or(false),
            })
        })
        .collect();
    if let Some(tracker) = tracker {
        let live: std::collections::HashSet<String> =
            sessions.iter().map(|s| s.id.clone()).collect();
        tracker.retain(|id| live.contains(id));
    }
    Observation {
        captured_at: Utc::now(),
        sessions,
    }
}

fn capture_pane(inst: &Instance) -> Option<String> {
    let session = crate::tmux::Session::new(&inst.id, &inst.title).ok()?;
    session.capture_pane(PANE_TAIL_LINES).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Status;
    use chrono::Duration;

    fn inst(title: &str, status: Status) -> Instance {
        let mut i = Instance::new(title, "/tmp");
        i.status = status;
        i
    }

    #[test]
    fn skips_archived_and_snoozed() {
        let mut archived = inst("a", Status::Waiting);
        archived.archived_at = Some(Utc::now());
        let mut snoozed = inst("b", Status::Waiting);
        snoozed.snoozed_until = Some(Utc::now() + Duration::hours(1));
        let visible = inst("c", Status::Waiting);
        let obs = build_observation(&[archived, snoozed, visible]);
        assert_eq!(obs.sessions.len(), 1);
        assert_eq!(obs.sessions[0].title, "c");
    }

    #[test]
    fn preserves_status_ordering_context() {
        let err = inst("e", Status::Error);
        let wait = inst("w", Status::Waiting);
        let obs = build_observation(&[err, wait]);
        // Both included; ranking is the reasoner's job. The builder just
        // projects fields.
        assert_eq!(obs.sessions.len(), 2);
    }

    #[test]
    fn projects_favorited_and_unread() {
        let mut favorited = inst("f", Status::Waiting);
        favorited.favorited_at = Some(Utc::now());
        favorited.unread = true;
        let obs = build_observation(&[favorited]);
        assert!(obs.sessions[0].favorited);
        assert!(obs.sessions[0].unread);
    }
}
