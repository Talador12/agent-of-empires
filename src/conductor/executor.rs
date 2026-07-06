//! Apply reasoner recommendations to real session state. Loads the fleet,
//! mutates the matching `Instance` via the attention-stack primitives on
//! it (`snooze`, `favorite`, `unfavorite`), and writes atomically through
//! `Storage::update`. Nothing here talks to tmux or an agent process yet;
//! nudge lands with the send-input pipe in a later commit.

use anyhow::Result;
use serde::Serialize;

use super::policies::ConductorPolicies;
use super::reasoner::{Action, Recommendation};
use crate::session::Storage;

pub struct Executor {
    profile: String,
    policies: ConductorPolicies,
}

/// What happened to a single recommendation. Reported back to callers so
/// the log and (later) the TUI can render "skipped because policy X" the
/// same way they render "applied" and "session gone".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Applied {
        session_id: String,
        action: Action,
    },
    /// The action was blocked by a policy switch (e.g. `allow_nudge`
    /// off). Reported, not silently dropped.
    Blocked {
        session_id: String,
        action: Action,
        reason: String,
    },
    /// The reasoner named a session id that is not in the current fleet.
    /// Sessions get deleted between the observation snapshot and the
    /// dispatch, so this is expected to happen occasionally.
    UnknownSession {
        session_id: String,
    },
    /// Recommendation was a `NoOp`; nothing to do.
    NoOp {
        session_id: String,
    },
}

impl Executor {
    pub fn new(profile: impl Into<String>, policies: ConductorPolicies) -> Self {
        Self {
            profile: profile.into(),
            policies,
        }
    }

    /// Apply every recommendation in one `Storage::update` call so the
    /// whole batch is durable together. Errors from the storage layer
    /// (permissions, disk full) bubble; per-recommendation issues become
    /// `Outcome` values in the returned list.
    pub fn dispatch(&self, recs: &[Recommendation]) -> Result<Vec<Outcome>> {
        if recs.is_empty() {
            return Ok(vec![]);
        }

        let storage = Storage::new_unwatched(&self.profile)?;
        let recs_owned: Vec<Recommendation> = recs.to_vec();
        let policies = self.policies.clone();

        storage.update(|instances, _groups| {
            let mut outcomes = Vec::with_capacity(recs_owned.len());
            for rec in &recs_owned {
                outcomes.push(apply_one(instances, rec, &policies));
            }
            Ok(outcomes)
        })
    }
}

fn apply_one(
    instances: &mut [crate::session::Instance],
    rec: &Recommendation,
    policies: &ConductorPolicies,
) -> Outcome {
    if matches!(rec.action, Action::NoOp) {
        return Outcome::NoOp {
            session_id: rec.session_id.clone(),
        };
    }

    let Some(inst) = instances.iter_mut().find(|i| i.id == rec.session_id) else {
        return Outcome::UnknownSession {
            session_id: rec.session_id.clone(),
        };
    };

    match &rec.action {
        Action::Snooze { minutes } => {
            inst.snooze(*minutes);
            Outcome::Applied {
                session_id: rec.session_id.clone(),
                action: rec.action.clone(),
            }
        }
        Action::Favorite => {
            inst.favorite();
            Outcome::Applied {
                session_id: rec.session_id.clone(),
                action: rec.action.clone(),
            }
        }
        Action::Unfavorite => {
            inst.unfavorite();
            Outcome::Applied {
                session_id: rec.session_id.clone(),
                action: rec.action.clone(),
            }
        }
        Action::Nudge { .. } => {
            if !policies.allow_nudge {
                Outcome::Blocked {
                    session_id: rec.session_id.clone(),
                    action: rec.action.clone(),
                    reason: "policies.allow_nudge is off".into(),
                }
            } else {
                // Send-input plumbing lands in a follow-up commit; until
                // then, an opted-in user still gets a clear log entry.
                Outcome::Blocked {
                    session_id: rec.session_id.clone(),
                    action: rec.action.clone(),
                    reason: "nudge dispatch not yet implemented".into(),
                }
            }
        }
        Action::NoOp => unreachable!("NoOp handled above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Instance, Status};

    fn rec(session_id: &str, action: Action) -> Recommendation {
        Recommendation {
            session_id: session_id.into(),
            action,
            rationale: "test".into(),
        }
    }

    fn inst(id: &str) -> Instance {
        let mut i = Instance::new("t", "/tmp");
        i.id = id.into();
        i.status = Status::Waiting;
        i
    }

    #[test]
    fn no_op_shortcircuits_without_session() {
        let mut fleet: Vec<Instance> = vec![];
        let out = apply_one(
            &mut fleet,
            &rec("ghost", Action::NoOp),
            &ConductorPolicies::default(),
        );
        assert!(matches!(out, Outcome::NoOp { .. }));
    }

    #[test]
    fn unknown_session_reports_ghost() {
        let mut fleet: Vec<Instance> = vec![inst("a")];
        let out = apply_one(
            &mut fleet,
            &rec("b", Action::Favorite),
            &ConductorPolicies::default(),
        );
        assert!(matches!(out, Outcome::UnknownSession { .. }));
    }

    #[test]
    fn favorite_mutates_instance() {
        let mut fleet = vec![inst("a")];
        let out = apply_one(
            &mut fleet,
            &rec("a", Action::Favorite),
            &ConductorPolicies::default(),
        );
        assert!(matches!(out, Outcome::Applied { .. }));
        assert!(fleet[0].favorited_at.is_some());
    }

    #[test]
    fn snooze_sets_deadline() {
        let mut fleet = vec![inst("a")];
        let out = apply_one(
            &mut fleet,
            &rec("a", Action::Snooze { minutes: 30 }),
            &ConductorPolicies::default(),
        );
        assert!(matches!(out, Outcome::Applied { .. }));
        assert!(fleet[0].snoozed_until.is_some());
    }

    #[test]
    fn unfavorite_clears_favorite() {
        let mut fleet = vec![inst("a")];
        fleet[0].favorite();
        let out = apply_one(
            &mut fleet,
            &rec("a", Action::Unfavorite),
            &ConductorPolicies::default(),
        );
        assert!(matches!(out, Outcome::Applied { .. }));
        assert!(fleet[0].favorited_at.is_none());
    }

    #[test]
    fn nudge_blocked_when_policy_off() {
        let mut fleet = vec![inst("a")];
        let out = apply_one(
            &mut fleet,
            &rec(
                "a",
                Action::Nudge {
                    message: "hi".into(),
                },
            ),
            &ConductorPolicies::default(),
        );
        match out {
            Outcome::Blocked { reason, .. } => {
                assert!(reason.contains("allow_nudge"));
            }
            _ => panic!("expected Blocked"),
        }
    }

    #[test]
    fn nudge_blocked_when_opted_in_pending_impl() {
        let mut fleet = vec![inst("a")];
        let policies = ConductorPolicies {
            allow_destructive: false,
            allow_nudge: true,
        };
        let out = apply_one(
            &mut fleet,
            &rec(
                "a",
                Action::Nudge {
                    message: "hi".into(),
                },
            ),
            &policies,
        );
        match out {
            Outcome::Blocked { reason, .. } => {
                assert!(reason.contains("not yet implemented"));
            }
            _ => panic!("expected Blocked pending implementation"),
        }
    }
}
