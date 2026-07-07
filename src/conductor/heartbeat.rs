//! Session-heartbeat classifier ported from aoaoe's `session-heartbeat.ts`.
//! Tracks per-session output hashes across ticks; a session whose hash
//! stops changing across N consecutive observations is likely stuck.

use std::collections::HashMap;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

/// Threshold for "possibly stuck". aoaoe's default is 5 unchanged ticks.
pub const STUCK_TICKS: u32 = 5;

/// Per-session hash + unchanged-tick counter. Interior mutability so a
/// shared `HeartbeatTracker` can update per-session state without threading
/// `&mut` through the observation builder.
#[derive(Default)]
pub struct HeartbeatTracker {
    inner: Mutex<HashMap<String, HeartbeatEntry>>,
}

#[derive(Debug, Clone)]
struct HeartbeatEntry {
    hash: Vec<u8>,
    unchanged_ticks: u32,
}

/// What `HeartbeatTracker::observe` returns to the observation builder.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatState {
    pub unchanged_ticks: u32,
    pub potentially_stuck: bool,
}

impl HeartbeatTracker {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Update the entry for a session with fresh pane content. Returns
    /// the current unchanged-tick count and a stuck flag.
    pub fn observe(&self, session_id: &str, pane: &str) -> HeartbeatState {
        let hash: Vec<u8> = Sha256::digest(pane.as_bytes()).to_vec();
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match guard.get_mut(session_id) {
            Some(entry) => {
                if entry.hash == hash {
                    entry.unchanged_ticks = entry.unchanged_ticks.saturating_add(1);
                } else {
                    entry.hash = hash;
                    entry.unchanged_ticks = 0;
                }
                HeartbeatState {
                    unchanged_ticks: entry.unchanged_ticks,
                    potentially_stuck: entry.unchanged_ticks >= STUCK_TICKS,
                }
            }
            None => {
                guard.insert(
                    session_id.to_string(),
                    HeartbeatEntry {
                        hash,
                        unchanged_ticks: 0,
                    },
                );
                HeartbeatState {
                    unchanged_ticks: 0,
                    potentially_stuck: false,
                }
            }
        }
    }

    /// Drop entries for sessions that no longer exist. Called by the
    /// observation builder at the end of each tick to keep the map from
    /// growing unbounded on churning fleets.
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|k, _| keep(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_observation_is_zero_ticks() {
        let t = HeartbeatTracker::new();
        let s = t.observe("a", "output-v1");
        assert_eq!(s.unchanged_ticks, 0);
        assert!(!s.potentially_stuck);
    }

    #[test]
    fn same_output_increments_counter() {
        let t = HeartbeatTracker::new();
        t.observe("a", "output-v1");
        let s = t.observe("a", "output-v1");
        assert_eq!(s.unchanged_ticks, 1);
        assert!(!s.potentially_stuck);
    }

    #[test]
    fn stuck_after_threshold_ticks() {
        let t = HeartbeatTracker::new();
        t.observe("a", "output-v1");
        for _ in 0..STUCK_TICKS {
            t.observe("a", "output-v1");
        }
        let s = t.observe("a", "output-v1");
        assert!(s.potentially_stuck, "unchanged_ticks={}", s.unchanged_ticks);
    }

    #[test]
    fn changed_output_resets_counter() {
        let t = HeartbeatTracker::new();
        t.observe("a", "output-v1");
        t.observe("a", "output-v1");
        t.observe("a", "output-v1");
        let s = t.observe("a", "output-v2");
        assert_eq!(s.unchanged_ticks, 0);
    }

    #[test]
    fn retain_prunes_removed_sessions() {
        let t = HeartbeatTracker::new();
        t.observe("keep", "a");
        t.observe("drop", "b");
        t.retain(|id| id == "keep");
        assert_eq!(
            t.inner.lock().unwrap().len(),
            1,
            "retain should have pruned the removed session"
        );
    }
}
