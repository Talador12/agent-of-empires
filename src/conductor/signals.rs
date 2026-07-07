//! Content-derived signals from a session's tmux pane, ported from
//! aoaoe's `session-summarizer.ts` and `session-sentiment.ts`. Every
//! classifier here is a pure function of the last few lines of pane
//! output so tests are cheap and reasoning is deterministic.

use serde::{Deserialize, Serialize};

/// Coarse-grained activity classification. Ports aoaoe's
/// `session-summarizer` bins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Coding,
    Testing,
    Debugging,
    Reading,
    Idle,
}

/// Emotional tone the pane output conveys. Ports aoaoe's
/// `session-sentiment` bins. Reasoner uses this to prioritize sessions
/// whose humans are stuck without waiting for the human to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sentiment {
    Progress,
    Success,
    Blocked,
    Error,
    Frustrated,
    Idle,
}

/// Classify recent activity by scanning the tail of the pane output for
/// characteristic tokens. Case-insensitive; last-match wins so a session
/// currently running tests still classifies as `Testing` even if it
/// wrote code earlier in the buffer.
pub fn classify_activity(pane: &str) -> Activity {
    if pane.trim().is_empty() {
        return Activity::Idle;
    }
    let lower = pane.to_ascii_lowercase();
    let mut latest = (0usize, Activity::Idle);
    for (needle, kind) in ACTIVITY_TOKENS {
        if let Some(pos) = lower.rfind(needle) {
            if pos >= latest.0 {
                latest = (pos, *kind);
            }
        }
    }
    latest.1
}

const ACTIVITY_TOKENS: &[(&str, Activity)] = &[
    ("cargo test", Activity::Testing),
    ("pytest", Activity::Testing),
    ("npm test", Activity::Testing),
    ("go test", Activity::Testing),
    ("running tests", Activity::Testing),
    ("test result", Activity::Testing),
    ("cargo build", Activity::Coding),
    ("cargo check", Activity::Coding),
    ("cargo clippy", Activity::Coding),
    ("npm run build", Activity::Coding),
    ("compiling", Activity::Coding),
    ("editing", Activity::Coding),
    ("panic", Activity::Debugging),
    ("traceback", Activity::Debugging),
    ("stack trace", Activity::Debugging),
    ("assertion failed", Activity::Debugging),
    ("cat ", Activity::Reading),
    ("less ", Activity::Reading),
    ("man ", Activity::Reading),
    ("view ", Activity::Reading),
];

/// Classify sentiment from the tail of the pane output. Priority is
/// error > blocked > frustrated > progress > success > idle so the
/// worst signal wins when several are present.
pub fn classify_sentiment(pane: &str) -> Sentiment {
    if pane.trim().is_empty() {
        return Sentiment::Idle;
    }
    let lower = pane.to_ascii_lowercase();
    for (needles, sentiment) in SENTIMENT_TOKENS {
        if needles.iter().any(|n| lower.contains(n)) {
            return *sentiment;
        }
    }
    Sentiment::Progress
}

const SENTIMENT_TOKENS: &[(&[&str], Sentiment)] = &[
    (
        &[
            "error:",
            "error[e",
            "fatal:",
            "panicked at",
            "unhandled exception",
            "failed to compile",
        ],
        Sentiment::Error,
    ),
    (
        &[
            "waiting for",
            "blocked on",
            "cannot find",
            "unresolved",
            "missing dependency",
        ],
        Sentiment::Blocked,
    ),
    (
        &[
            "this doesn't work",
            "still broken",
            "not sure why",
            "ugh",
            "give up",
        ],
        Sentiment::Frustrated,
    ),
    (
        &[
            "test result: ok",
            "all tests passed",
            "build succeeded",
            "successfully",
        ],
        Sentiment::Success,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pane_is_idle() {
        assert_eq!(classify_activity(""), Activity::Idle);
        assert_eq!(classify_sentiment(""), Sentiment::Idle);
    }

    #[test]
    fn testing_is_recognized() {
        assert_eq!(
            classify_activity("running cargo test now"),
            Activity::Testing
        );
        assert_eq!(classify_activity("PYTEST session ..."), Activity::Testing);
    }

    #[test]
    fn coding_is_recognized() {
        assert_eq!(
            classify_activity("Compiling agent-of-empires v1.12.0"),
            Activity::Coding
        );
    }

    #[test]
    fn latest_activity_wins() {
        // Coding earlier, testing later; classifier picks testing.
        let pane = "cargo build\n... success\ncargo test\nrunning 5 tests";
        assert_eq!(classify_activity(pane), Activity::Testing);
    }

    #[test]
    fn error_beats_success() {
        let pane = "test result: ok. 3 passed\nerror: compile failed later";
        assert_eq!(classify_sentiment(pane), Sentiment::Error);
    }

    #[test]
    fn blocked_recognized() {
        assert_eq!(
            classify_sentiment("waiting for user input"),
            Sentiment::Blocked
        );
    }

    #[test]
    fn frustrated_recognized() {
        assert_eq!(
            classify_sentiment("Ugh, this doesn't work"),
            Sentiment::Frustrated
        );
    }

    #[test]
    fn success_recognized() {
        assert_eq!(
            classify_sentiment("test result: ok. 42 passed"),
            Sentiment::Success
        );
    }
}
