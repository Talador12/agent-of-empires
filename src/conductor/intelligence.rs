//! Ports of the pure-computation "intelligence modules" from aoaoe that
//! fit aoe's current state model without a session-output-pane feed.
//! Everything here is a function of `Instance` fields (or scalar inputs
//! the caller controls); anything that needs raw pane content stays as
//! follow-up work called out in DESIGN.md.

use std::collections::HashSet;
use std::time::Duration;

use chrono::Utc;

use crate::session::{Instance, Status};

/// How stale a session is relative to its last user interaction. Ports
/// `src/session-idle-detector.ts` (aoaoe): escalates nudge severity as a
/// session sits untouched. The conductor uses this to boost the attention
/// score without adding a new Instance field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEscalation {
    Fresh,
    Cooling,
    Stale,
    Cold,
}

impl IdleEscalation {
    /// Return the escalation level for a session's `last_accessed_at`.
    /// Sessions with no recorded access default to `Fresh` because the
    /// conductor treats them as newly minted rather than abandoned.
    pub fn for_instance(inst: &Instance) -> Self {
        let Some(last) = inst.last_accessed_at else {
            return IdleEscalation::Fresh;
        };
        let elapsed = (Utc::now() - last).num_minutes();
        if elapsed < 15 {
            IdleEscalation::Fresh
        } else if elapsed < 60 {
            IdleEscalation::Cooling
        } else if elapsed < 8 * 60 {
            IdleEscalation::Stale
        } else {
            IdleEscalation::Cold
        }
    }

    /// Score bump applied on top of the base `attention_score`. Chosen so
    /// `Cold` alone still ranks below `Waiting`, keeping the status tier
    /// the primary attention driver.
    pub fn score_bonus(self) -> i64 {
        match self {
            IdleEscalation::Fresh => 0,
            IdleEscalation::Cooling => 10,
            IdleEscalation::Stale => 40,
            IdleEscalation::Cold => 80,
        }
    }
}

/// Exponential backoff with jitter for the spawn-from-issues path. Ports
/// the retry math from `src/task-retry.ts` (aoaoe). Deterministic when
/// `jitter_bp` is 0, so tests can pin an exact schedule.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    jitter_bp: u32,
}

impl Backoff {
    pub const fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            jitter_bp: 0,
        }
    }

    /// Add proportional jitter, expressed in basis points of the delay
    /// (100 bp = 1%). Uses the standard-library RNG (not `rand`) to avoid
    /// widening the crate's dependency set for a small nice-to-have.
    pub const fn with_jitter_bp(mut self, jitter_bp: u32) -> Self {
        self.jitter_bp = jitter_bp;
        self
    }

    /// Delay for `attempt` (zero-based). Cap at `max`.
    pub fn delay(&self, attempt: u32) -> Duration {
        // Any doubling >= 30 (2^30 s = ~34 years even for a 1s base) is
        // already past any reasonable `max`, so clamp the shift itself
        // rather than paper over `u32 * Duration` overflow with wrapping.
        let doubling: u32 = if attempt >= 30 {
            u32::MAX
        } else {
            1u32 << attempt
        };
        let raw = self.base.saturating_mul(doubling);
        let capped = raw.min(self.max);
        if self.jitter_bp == 0 {
            return capped;
        }
        let jitter_ratio = jitter_ratio_from_hash(attempt, self.jitter_bp);
        let millis = capped.as_millis() as f64 * jitter_ratio;
        Duration::from_millis(millis.round().min(u64::MAX as f64) as u64).min(self.max)
    }
}

fn jitter_ratio_from_hash(attempt: u32, jitter_bp: u32) -> f64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    (attempt, jitter_bp).hash(&mut h);
    let raw = h.finish();
    let unit = (raw as f64 / u64::MAX as f64) * 2.0 - 1.0;
    1.0 + (jitter_bp as f64 / 10_000.0) * unit
}

/// Cap the number of concurrently active sessions the conductor will
/// create. Ports `src/session-pool.ts` (aoaoe). "Active" means Running,
/// Waiting, or Idle; Stopped / Archived / Trashed do not count. Used by
/// spawn-from-issues to know when to stop consuming the issue list.
pub struct SessionPool {
    limit: usize,
}

impl SessionPool {
    pub const fn new(limit: usize) -> Self {
        Self { limit }
    }

    /// How many more sessions can be added before the cap is reached.
    /// Returns 0 when at or over the limit.
    pub fn slots_remaining(&self, fleet: &[Instance]) -> usize {
        let active = fleet.iter().filter(|i| is_active(i)).count();
        self.limit.saturating_sub(active)
    }

    /// Filter a set of intended new-session ids down to the ones that
    /// still fit under the cap. Preserves input order so the caller can
    /// use it directly.
    pub fn filter_to_capacity(
        &self,
        fleet: &[Instance],
        intended: impl IntoIterator<Item = String>,
    ) -> Vec<String> {
        let slots = self.slots_remaining(fleet);
        let mut kept: Vec<String> = Vec::with_capacity(slots);
        let mut seen: HashSet<String> = HashSet::new();
        for id in intended {
            if kept.len() >= slots {
                break;
            }
            if seen.insert(id.clone()) {
                kept.push(id);
            }
        }
        kept
    }
}

fn is_active(inst: &Instance) -> bool {
    if inst.archived_at.is_some() || inst.trashed_at.is_some() {
        return false;
    }
    matches!(
        inst.status,
        Status::Running | Status::Waiting | Status::Idle
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn baseline() -> Instance {
        Instance::new("t", "/tmp")
    }

    #[test]
    fn idle_no_access_scores_fresh() {
        let inst = baseline();
        assert_eq!(IdleEscalation::for_instance(&inst), IdleEscalation::Fresh);
    }

    #[test]
    fn idle_escalates_by_elapsed_time() {
        let mut inst = baseline();
        inst.last_accessed_at = Some(Utc::now() - ChronoDuration::minutes(5));
        assert_eq!(IdleEscalation::for_instance(&inst), IdleEscalation::Fresh);
        inst.last_accessed_at = Some(Utc::now() - ChronoDuration::minutes(30));
        assert_eq!(IdleEscalation::for_instance(&inst), IdleEscalation::Cooling);
        inst.last_accessed_at = Some(Utc::now() - ChronoDuration::hours(2));
        assert_eq!(IdleEscalation::for_instance(&inst), IdleEscalation::Stale);
        inst.last_accessed_at = Some(Utc::now() - ChronoDuration::hours(12));
        assert_eq!(IdleEscalation::for_instance(&inst), IdleEscalation::Cold);
    }

    #[test]
    fn score_bonus_monotonic() {
        assert!(IdleEscalation::Fresh.score_bonus() < IdleEscalation::Cooling.score_bonus());
        assert!(IdleEscalation::Cooling.score_bonus() < IdleEscalation::Stale.score_bonus());
        assert!(IdleEscalation::Stale.score_bonus() < IdleEscalation::Cold.score_bonus());
    }

    #[test]
    fn backoff_doubles_until_cap() {
        let b = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        assert_eq!(b.delay(0), Duration::from_secs(1));
        assert_eq!(b.delay(1), Duration::from_secs(2));
        assert_eq!(b.delay(2), Duration::from_secs(4));
        assert_eq!(b.delay(3), Duration::from_secs(8));
        assert_eq!(b.delay(10), Duration::from_secs(30));
        assert_eq!(b.delay(50), Duration::from_secs(30));
    }

    #[test]
    fn backoff_jitter_stays_within_band() {
        let b = Backoff::new(Duration::from_secs(1), Duration::from_secs(30)).with_jitter_bp(1000);
        let raw = b.delay(3).as_millis() as i64;
        assert!(
            raw >= 7_100 && raw <= 8_900,
            "attempt 3 jitter out of band: {}",
            raw
        );
    }

    fn active(id: &str) -> Instance {
        let mut i = Instance::new("t", "/tmp");
        i.id = id.into();
        i.status = Status::Running;
        i
    }

    #[test]
    fn pool_counts_only_active() {
        let mut fleet = vec![active("a"), active("b")];
        fleet[1].status = Status::Stopped;
        let mut archived = active("c");
        archived.archived_at = Some(Utc::now());
        fleet.push(archived);
        let mut trashed = active("d");
        trashed.trashed_at = Some(Utc::now());
        fleet.push(trashed);
        let pool = SessionPool::new(5);
        assert_eq!(pool.slots_remaining(&fleet), 4);
    }

    #[test]
    fn pool_filter_respects_limit_and_dedupes() {
        let fleet = vec![active("a"), active("b")];
        let pool = SessionPool::new(3);
        let intended = ["x", "y", "z", "x", "w"].iter().map(|s| s.to_string());
        let kept = pool.filter_to_capacity(&fleet, intended);
        assert_eq!(kept, vec!["x".to_string()]);
    }

    #[test]
    fn pool_at_limit_returns_empty_slots() {
        let fleet = vec![active("a"), active("b"), active("c")];
        let pool = SessionPool::new(3);
        assert_eq!(pool.slots_remaining(&fleet), 0);
        let intended = ["x"].iter().map(|s| s.to_string());
        assert!(pool.filter_to_capacity(&fleet, intended).is_empty());
    }
}
