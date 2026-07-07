//! Apply reasoner recommendations to session state. Loads the fleet,
//! mutates matching `Instance`s via the attention-stack primitives, and
//! writes atomically through `Storage::update`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;

use super::policies::ConductorPolicies;
use super::reasoner::{Action, Recommendation};
use super::tasks::TaskStore;
use crate::session::Storage;

pub struct Executor {
    profile: String,
    policies: ConductorPolicies,
    /// Per-session timestamp of the last action the executor applied.
    /// Consulted to enforce `policies.action_cooldown`; blocks
    /// recommendations that arrive too fast. Interior mutability so the
    /// executor can be shared behind an immutable reference without a
    /// `&mut self` on every call site.
    last_action_at: Mutex<HashMap<String, Instant>>,
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
            last_action_at: Mutex::new(HashMap::new()),
        }
    }

    /// Apply every recommendation. Session-scoped actions are batched
    /// through one `Storage::update` call so the whole batch is durable
    /// together; task-scoped actions (`ReportProgress`, `CompleteTask`)
    /// go through the `TaskStore` afterwards since they touch a different
    /// on-disk file.
    pub fn dispatch(&self, recs: &[Recommendation]) -> Result<Vec<Outcome>> {
        if recs.is_empty() {
            return Ok(vec![]);
        }

        let (session_recs, task_recs): (Vec<&Recommendation>, Vec<&Recommendation>) =
            recs.iter().partition(|r| {
                !matches!(
                    &r.action,
                    Action::ReportProgress { .. } | Action::CompleteTask { .. }
                )
            });

        let policies = self.policies.clone();
        // Snapshot the cooldown map so decisions inside the storage
        // closure are consistent, then commit the fresh timestamps after
        // the write succeeds. This keeps the closure free of the
        // executor's internal locks.
        let cooldown_snapshot: HashMap<String, Instant> = self
            .last_action_at
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        let session_recs_owned: Vec<Recommendation> =
            session_recs.iter().map(|r| (*r).clone()).collect();
        let storage = Storage::new_unwatched(&self.profile)?;
        let mut outcomes = storage.update(|instances, _groups| {
            let mut outcomes = Vec::with_capacity(session_recs_owned.len());
            for rec in &session_recs_owned {
                outcomes.push(apply_one(instances, rec, &policies, &cooldown_snapshot));
            }
            Ok(outcomes)
        })?;

        // Task-scoped actions.
        if !task_recs.is_empty() {
            let store = TaskStore::for_profile(&self.profile)?;
            for rec in task_recs {
                outcomes.push(dispatch_task_action(&store, rec));
            }
        }

        // Only bump the cooldown map for outcomes that actually mutated
        // state. Blocked / NoOp / UnknownSession do not count against
        // the cooldown, since the user's intent was not applied.
        let now = Instant::now();
        if let Ok(mut guard) = self.last_action_at.lock() {
            for outcome in &outcomes {
                if let Outcome::Applied { session_id, .. } = outcome {
                    guard.insert(session_id.clone(), now);
                }
            }
        }

        Ok(outcomes)
    }
}

fn dispatch_task_action(store: &TaskStore, rec: &Recommendation) -> Outcome {
    match &rec.action {
        Action::ReportProgress { task_id, note } => match store.append_progress(task_id, note) {
            Ok(true) => applied(rec),
            Ok(false) => Outcome::Blocked {
                session_id: rec.session_id.clone(),
                action: rec.action.clone(),
                reason: format!("unknown task_id {task_id:?}"),
            },
            Err(err) => Outcome::Blocked {
                session_id: rec.session_id.clone(),
                action: rec.action.clone(),
                reason: format!("task store write failed: {err}"),
            },
        },
        Action::CompleteTask { task_id } => match store.complete(task_id) {
            Ok(true) => applied(rec),
            Ok(false) => Outcome::Blocked {
                session_id: rec.session_id.clone(),
                action: rec.action.clone(),
                reason: format!("unknown task_id {task_id:?}"),
            },
            Err(err) => Outcome::Blocked {
                session_id: rec.session_id.clone(),
                action: rec.action.clone(),
                reason: format!("task store write failed: {err}"),
            },
        },
        _ => unreachable!("dispatch_task_action only handles task-scoped variants"),
    }
}

fn apply_one(
    instances: &mut [crate::session::Instance],
    rec: &Recommendation,
    policies: &ConductorPolicies,
    cooldown_snapshot: &HashMap<String, Instant>,
) -> Outcome {
    if matches!(rec.action, Action::NoOp) {
        return Outcome::NoOp {
            session_id: rec.session_id.clone(),
        };
    }

    // Cooldown check runs before the fleet lookup so a Blocked-by-cooldown
    // outcome reports even if the session was deleted since the tick that
    // recommended it. `elapsed >= cooldown` allows the very first action.
    if let Some(last) = cooldown_snapshot.get(&rec.session_id) {
        if last.elapsed() < policies.action_cooldown {
            return Outcome::Blocked {
                session_id: rec.session_id.clone(),
                action: rec.action.clone(),
                reason: format!(
                    "action_cooldown ({}s) not yet elapsed",
                    policies.action_cooldown.as_secs()
                ),
            };
        }
    }

    let Some(inst) = instances.iter_mut().find(|i| i.id == rec.session_id) else {
        return Outcome::UnknownSession {
            session_id: rec.session_id.clone(),
        };
    };

    match &rec.action {
        Action::Snooze { minutes } => {
            inst.snooze(*minutes);
            applied(rec)
        }
        Action::Favorite => {
            inst.favorite();
            applied(rec)
        }
        Action::Unfavorite => {
            inst.unfavorite();
            applied(rec)
        }
        Action::Archive => {
            if !policies.allow_destructive {
                blocked(rec, "policies.allow_destructive is off")
            } else {
                inst.archive();
                applied(rec)
            }
        }
        Action::Nudge { message } => {
            if !policies.allow_nudge {
                blocked(rec, "policies.allow_nudge is off")
            } else {
                match send_nudge(inst, message) {
                    Ok(()) => applied(rec),
                    Err(err) => blocked(rec, &format!("send-keys failed: {err}")),
                }
            }
        }
        Action::StartSession => match inst.start() {
            Ok(()) => applied(rec),
            Err(err) => blocked(rec, &format!("start failed: {err}")),
        },
        Action::StopSession => {
            if !policies.allow_destructive {
                blocked(rec, "policies.allow_destructive is off")
            } else {
                match inst.stop() {
                    Ok(()) => applied(rec),
                    Err(err) => blocked(rec, &format!("stop failed: {err}")),
                }
            }
        }
        Action::ReportProgress { .. } | Action::CompleteTask { .. } => {
            // Task-store dispatch happens outside `Storage::update` because
            // it touches a different on-disk file; see `dispatch_task_action`.
            // The apply_one path only handles session-scoped actions.
            blocked(
                rec,
                "task actions are dispatched outside the session-storage batch",
            )
        }
        Action::NoOp => unreachable!("NoOp handled above"),
    }
}

fn applied(rec: &Recommendation) -> Outcome {
    Outcome::Applied {
        session_id: rec.session_id.clone(),
        action: rec.action.clone(),
    }
}

fn blocked(rec: &Recommendation, reason: &str) -> Outcome {
    Outcome::Blocked {
        session_id: rec.session_id.clone(),
        action: rec.action.clone(),
        reason: reason.to_string(),
    }
}

/// Deliver a nudge message to the session's tmux pane, mirroring the
/// aoaoe `send_input` executor action. Uses the shared
/// `TmuxSession::send_keys` helper so behavior matches `aoe session send`.
fn send_nudge(inst: &crate::session::Instance, message: &str) -> anyhow::Result<()> {
    let session = crate::tmux::Session::new(&inst.id, &inst.title)?;
    session.send_keys(message)
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
            confidence: None,
        }
    }

    fn inst(id: &str) -> Instance {
        let mut i = Instance::new("t", "/tmp");
        i.id = id.into();
        i.status = Status::Waiting;
        i
    }

    fn empty_cooldown() -> HashMap<String, Instant> {
        HashMap::new()
    }

    #[test]
    fn no_op_shortcircuits_without_session() {
        let mut fleet: Vec<Instance> = vec![];
        let out = apply_one(
            &mut fleet,
            &rec("ghost", Action::NoOp),
            &ConductorPolicies::default(),
            &empty_cooldown(),
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
            &empty_cooldown(),
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
            &empty_cooldown(),
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
            &empty_cooldown(),
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
            &empty_cooldown(),
        );
        assert!(matches!(out, Outcome::Applied { .. }));
        assert!(fleet[0].favorited_at.is_none());
    }

    #[test]
    fn archive_blocked_when_policy_off() {
        let mut fleet = vec![inst("a")];
        let out = apply_one(
            &mut fleet,
            &rec("a", Action::Archive),
            &ConductorPolicies::default(),
            &empty_cooldown(),
        );
        match out {
            Outcome::Blocked { reason, .. } => {
                assert!(reason.contains("allow_destructive"));
            }
            _ => panic!("expected Blocked"),
        }
        assert!(fleet[0].archived_at.is_none());
    }

    #[test]
    fn archive_applied_when_opted_in() {
        let mut fleet = vec![inst("a")];
        let mut policies = ConductorPolicies::default();
        policies.allow_destructive = true;
        let out = apply_one(
            &mut fleet,
            &rec("a", Action::Archive),
            &policies,
            &empty_cooldown(),
        );
        assert!(matches!(out, Outcome::Applied { .. }));
        assert!(fleet[0].archived_at.is_some());
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
            &empty_cooldown(),
        );
        match out {
            Outcome::Blocked { reason, .. } => {
                assert!(reason.contains("allow_nudge"));
            }
            _ => panic!("expected Blocked"),
        }
    }

    #[test]
    fn stop_session_blocked_when_policy_off() {
        let mut fleet = vec![inst("a")];
        let out = apply_one(
            &mut fleet,
            &rec("a", Action::StopSession),
            &ConductorPolicies::default(),
            &empty_cooldown(),
        );
        match out {
            Outcome::Blocked { reason, .. } => {
                assert!(reason.contains("allow_destructive"));
            }
            _ => panic!("expected Blocked"),
        }
    }

    #[test]
    fn cooldown_blocks_repeat_actions() {
        let mut fleet = vec![inst("a")];
        let mut cooldown = HashMap::new();
        cooldown.insert("a".to_string(), Instant::now());
        let out = apply_one(
            &mut fleet,
            &rec("a", Action::Favorite),
            &ConductorPolicies::default(),
            &cooldown,
        );
        match out {
            Outcome::Blocked { reason, .. } => {
                assert!(reason.contains("action_cooldown"));
            }
            _ => panic!("expected Blocked by cooldown"),
        }
        // Instance was not mutated.
        assert!(fleet[0].favorited_at.is_none());
    }

    #[test]
    fn cooldown_allows_first_action() {
        let mut fleet = vec![inst("a")];
        let out = apply_one(
            &mut fleet,
            &rec("a", Action::Favorite),
            &ConductorPolicies::default(),
            &empty_cooldown(),
        );
        assert!(matches!(out, Outcome::Applied { .. }));
    }
}
