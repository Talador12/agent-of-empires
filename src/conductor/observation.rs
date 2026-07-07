//! Project a fleet of `Instance`s into the `Observation` the reasoner
//! sees. Skips the same rows `attention_score` skips.

use chrono::Utc;

use super::{attention_score, reasoner::Observation, reasoner::SessionSnapshot};
use crate::session::Instance;

pub fn build_observation(instances: &[Instance]) -> Observation {
    let sessions: Vec<SessionSnapshot> = instances
        .iter()
        .filter_map(|inst| {
            let score = attention_score(inst)?;
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
            })
        })
        .collect();
    Observation {
        captured_at: Utc::now(),
        sessions,
    }
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
