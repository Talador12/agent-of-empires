//! Experimental orchestrator that ranks sessions by attention score and
//! (when authorized) applies non-destructive actions on the user's
//! behalf. Gated behind `AOE_EXPERIMENTAL_AO_MODE`. See issue #553.

use chrono::Utc;

use crate::session::{Instance, Status};

pub mod executor;
pub mod github;
pub mod intelligence;
pub mod observation;
pub mod policies;
pub mod reasoner;
pub mod watcher;

/// Env var that gates every conductor surface. Absent or set to `0` / `false`
/// means the feature stays inert even if the subcommand is invoked.
pub const EXPERIMENTAL_ENV: &str = "AOE_EXPERIMENTAL_AO_MODE";

/// True when the user has opted in to the experimental conductor. Accepts the
/// same off values (`0`, `false`) that other AOE env-var toggles use so a
/// scripted `AOE_EXPERIMENTAL_AO_MODE=0` reliably disables it.
pub fn is_enabled() -> bool {
    match std::env::var(EXPERIMENTAL_ENV) {
        Ok(v) => !(v == "0" || v.eq_ignore_ascii_case("false") || v.is_empty()),
        Err(_) => false,
    }
}

/// Fail fast with a human-readable hint when the gate is closed.
pub fn require_enabled() -> anyhow::Result<()> {
    if is_enabled() {
        Ok(())
    } else {
        anyhow::bail!(
            "The conductor is experimental. Set {}=1 to enable it.",
            EXPERIMENTAL_ENV
        )
    }
}

/// Bucket a session lands in for the attention sort. Snoozed / archived /
/// trashed rows never surface, so the score computation returns `None` for
/// them rather than a real number.
pub fn attention_score(inst: &Instance) -> Option<i64> {
    if inst.archived_at.is_some() || inst.trashed_at.is_some() {
        return None;
    }
    if let Some(until) = inst.snoozed_until {
        if until > Utc::now() {
            return None;
        }
    }

    let mut score: i64 = match inst.status {
        Status::Error => 400,
        Status::Waiting => 300,
        Status::Idle | Status::Unknown => 100,
        Status::Running => 40,
        Status::Starting => 20,
        Status::Stopped => 10,
        Status::Deleting | Status::Creating => 0,
    };

    if inst.favorited_at.is_some()
        && matches!(
            inst.status,
            Status::Waiting | Status::Error | Status::Idle | Status::Unknown
        )
    {
        score += 500;
    }

    if inst.unread {
        score += 150;
    }

    if let Some(last) = inst.last_accessed_at {
        let stale_hours = (Utc::now() - last).num_hours().max(0);
        score += stale_hours.min(24);
    }

    // Idle escalation bonus (see `intelligence::IdleEscalation`). Capped
    // small enough that status tier still dominates the ranking.
    score += intelligence::IdleEscalation::for_instance(inst).score_bonus();

    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn baseline() -> Instance {
        Instance::new("conductor-test", "/tmp")
    }

    #[test]
    fn env_gate_defaults_off() {
        let saved = std::env::var(EXPERIMENTAL_ENV).ok();
        // SAFETY: single-threaded test.
        unsafe { std::env::remove_var(EXPERIMENTAL_ENV) };
        assert!(!is_enabled());
        if let Some(v) = saved {
            unsafe { std::env::set_var(EXPERIMENTAL_ENV, v) };
        }
    }

    #[test]
    fn env_gate_rejects_off_values() {
        let saved = std::env::var(EXPERIMENTAL_ENV).ok();
        // SAFETY: single-threaded test.
        unsafe {
            std::env::set_var(EXPERIMENTAL_ENV, "0");
            assert!(!is_enabled());
            std::env::set_var(EXPERIMENTAL_ENV, "false");
            assert!(!is_enabled());
            std::env::set_var(EXPERIMENTAL_ENV, "1");
            assert!(is_enabled());
        }
        match saved {
            Some(v) => unsafe { std::env::set_var(EXPERIMENTAL_ENV, v) },
            None => unsafe { std::env::remove_var(EXPERIMENTAL_ENV) },
        }
    }

    #[test]
    fn archived_scores_none() {
        let mut inst = baseline();
        inst.archived_at = Some(Utc::now());
        assert!(attention_score(&inst).is_none());
    }

    #[test]
    fn trashed_scores_none() {
        let mut inst = baseline();
        inst.trashed_at = Some(Utc::now());
        assert!(attention_score(&inst).is_none());
    }

    #[test]
    fn future_snooze_scores_none() {
        let mut inst = baseline();
        inst.snoozed_until = Some(Utc::now() + Duration::hours(1));
        assert!(attention_score(&inst).is_none());
    }

    #[test]
    fn past_snooze_still_ranks() {
        let mut inst = baseline();
        inst.status = Status::Waiting;
        inst.snoozed_until = Some(Utc::now() - Duration::hours(1));
        assert!(attention_score(&inst).is_some());
    }

    #[test]
    fn favorite_pins_help_states() {
        let mut waiting = baseline();
        waiting.status = Status::Waiting;
        let mut favorited = waiting.clone();
        favorited.favorited_at = Some(Utc::now());
        assert!(attention_score(&favorited) > attention_score(&waiting));
    }

    #[test]
    fn favorite_running_does_not_preempt() {
        // Running is not a "needs help" tier; the star is decorative only.
        let mut running = baseline();
        running.status = Status::Running;
        let plain = attention_score(&running).unwrap();
        running.favorited_at = Some(Utc::now());
        assert_eq!(attention_score(&running).unwrap(), plain);
    }

    #[test]
    fn error_beats_waiting_beats_running() {
        let mut error = baseline();
        error.status = Status::Error;
        let mut waiting = baseline();
        waiting.status = Status::Waiting;
        let mut running = baseline();
        running.status = Status::Running;
        assert!(attention_score(&error) > attention_score(&waiting));
        assert!(attention_score(&waiting) > attention_score(&running));
    }
}
