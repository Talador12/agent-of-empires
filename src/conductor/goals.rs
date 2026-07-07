//! Goal signals derived from pane content. Ports aoaoe's `goal-detector`
//! and `drift-detector` in the smallest fidelity that keeps their intent:
//! spot completion signals (tests passing, git push, ship), and warn
//! when a session's goal keywords have vanished from recent output.

use serde::Serialize;

/// Marker signals the tests / build / deploy pipeline emits when a task
/// completes successfully. Ported from `goal-detector.ts`.
const COMPLETION_MARKERS: &[&str] = &[
    "test result: ok",
    "all tests passed",
    "build succeeded",
    "successfully built",
    "successfully deployed",
    "pull request opened",
    "pr opened",
    "pushed to origin",
    "pushed to remote",
    "release published",
    "task complete",
];

/// True when the pane output shows any completion marker.
pub fn goal_completed(pane: &str) -> bool {
    if pane.trim().is_empty() {
        return false;
    }
    let lower = pane.to_ascii_lowercase();
    COMPLETION_MARKERS.iter().any(|m| lower.contains(m))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftLevel {
    /// Every goal keyword appears in the recent pane content.
    Focused,
    /// At least one goal keyword is missing.
    Partial,
    /// No goal keyword appears; the session may be off-track.
    Drifted,
}

/// Compare recent output against the caller-provided goal keywords and
/// classify how well the session's activity aligns with the stated goal.
/// Ports the intent of aoaoe's `drift-detector`. Case-insensitive.
pub fn detect_drift(pane: &str, keywords: &[&str]) -> DriftLevel {
    if keywords.is_empty() {
        return DriftLevel::Focused;
    }
    let lower = pane.to_ascii_lowercase();
    let hits = keywords
        .iter()
        .filter(|k| lower.contains(&k.to_ascii_lowercase()))
        .count();
    if hits == keywords.len() {
        DriftLevel::Focused
    } else if hits > 0 {
        DriftLevel::Partial
    } else {
        DriftLevel::Drifted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pane_no_completion() {
        assert!(!goal_completed(""));
    }

    #[test]
    fn tests_passing_is_completion() {
        assert!(goal_completed("test result: ok. 42 passed"));
        assert!(goal_completed("All tests passed"));
    }

    #[test]
    fn pushed_to_origin_is_completion() {
        assert!(goal_completed("pushed to origin/main"));
    }

    #[test]
    fn empty_keywords_means_focused() {
        assert_eq!(detect_drift("anything", &[]), DriftLevel::Focused);
    }

    #[test]
    fn all_keywords_present_is_focused() {
        let pane = "editing conductor.rs and running cargo test";
        assert_eq!(
            detect_drift(pane, &["conductor", "test"]),
            DriftLevel::Focused
        );
    }

    #[test]
    fn some_keywords_missing_is_partial() {
        let pane = "editing conductor.rs";
        assert_eq!(
            detect_drift(pane, &["conductor", "test"]),
            DriftLevel::Partial
        );
    }

    #[test]
    fn no_keywords_present_is_drifted() {
        let pane = "reading unrelated notes";
        assert_eq!(
            detect_drift(pane, &["conductor", "test"]),
            DriftLevel::Drifted
        );
    }

    #[test]
    fn case_insensitive_matching() {
        let pane = "Editing Conductor.rs";
        assert_eq!(detect_drift(pane, &["conductor"]), DriftLevel::Focused);
    }
}
