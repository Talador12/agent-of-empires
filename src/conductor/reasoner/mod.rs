//! The reasoner produces action recommendations from an `Observation`
//! snapshot of the fleet. Pluggable so the watch loop (commit 3) can be
//! generic over the backend; only `ClaudePrintReasoner` ships today, with
//! a subprocess call to `claude --print`. OpenCode HTTP and direct
//! Anthropic SDK backends are follow-up work.

use std::future::Future;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod claude_print;

/// Snapshot of the fleet the reasoner sees on one tick. Kept lean here; the
/// intelligence-module port (later commits) enriches it with health scores,
/// error-pattern hits, and idle-detection signals without changing the
/// trait.
#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    pub captured_at: DateTime<Utc>,
    pub sessions: Vec<SessionSnapshot>,
}

/// Per-session projection fed to the reasoner. Fields mirror the subset of
/// `Instance` state the LLM needs to make a recommendation. Everything is
/// derived from durable session state, so a fresh reasoner call after a
/// restart produces the same output.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshot {
    pub id: String,
    pub title: String,
    pub status: String,
    pub attention_score: i64,
    pub favorited: bool,
    pub unread: bool,
    pub archived: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_accessed_at: Option<DateTime<Utc>>,
}

/// One action the reasoner suggests. The executor (commit 4) is what
/// actually mutates session state; the reasoner never touches disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recommendation {
    pub session_id: String,
    pub action: Action,
    /// One-line human-readable rationale from the LLM.
    pub rationale: String,
}

/// The set of actions the reasoner is allowed to suggest. Deliberately
/// small so the LLM has a closed vocabulary. Later commits widen this as
/// each executor path lands (nudge messaging, archive/unarchive, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Push the session out of the attention queue for `minutes`.
    Snooze { minutes: u32 },
    /// Pin the session to the top of the attention sort.
    Favorite,
    /// Remove the favorite pin.
    Unfavorite,
    /// Send a nudge message to the session's agent. Executor support lands
    /// with the send-input policy in a later commit; the reasoner is
    /// allowed to suggest it now so the JSON vocabulary is stable.
    Nudge { message: String },
    /// Explicitly recommend doing nothing this tick.
    NoOp,
}

/// Source of recommendations. Static dispatch is enough for the tick loop
/// today; if the TUI ever needs to swap backends at runtime, the trait can
/// grow a `Box<dyn Reasoner>`-compatible alternative without breaking the
/// call sites.
///
/// The RPITIT form (`impl Future + Send`) is used deliberately to avoid
/// the `async_fn_in_trait` lint that fires with a plain `async fn` here.
pub trait Reasoner: Send + Sync {
    fn recommend(
        &self,
        observation: &Observation,
    ) -> impl Future<Output = anyhow::Result<Vec<Recommendation>>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub reasoner returning canned recommendations. Exercises the
    /// trait shape and gives commit 3's tick loop a deterministic backend
    /// to test against.
    struct StubReasoner {
        canned: Vec<Recommendation>,
    }

    impl Reasoner for StubReasoner {
        async fn recommend(
            &self,
            _observation: &Observation,
        ) -> anyhow::Result<Vec<Recommendation>> {
            Ok(self.canned.clone())
        }
    }

    fn empty_observation() -> Observation {
        Observation {
            captured_at: Utc::now(),
            sessions: vec![],
        }
    }

    #[tokio::test]
    async fn stub_reasoner_returns_canned() {
        let rec = Recommendation {
            session_id: "abc".into(),
            action: Action::NoOp,
            rationale: "nothing to do".into(),
        };
        let stub = StubReasoner {
            canned: vec![rec.clone()],
        };
        let out = stub.recommend(&empty_observation()).await.unwrap();
        assert_eq!(out, vec![rec]);
    }

    #[test]
    fn action_serializes_with_kind_tag() {
        let json = serde_json::to_string(&Action::Snooze { minutes: 30 }).unwrap();
        assert_eq!(json, r#"{"kind":"snooze","minutes":30}"#);
        let json = serde_json::to_string(&Action::NoOp).unwrap();
        assert_eq!(json, r#"{"kind":"no_op"}"#);
    }

    #[test]
    fn observation_omits_none_timestamps() {
        let snap = SessionSnapshot {
            id: "x".into(),
            title: "t".into(),
            status: "Idle".into(),
            attention_score: 42,
            favorited: false,
            unread: false,
            archived: false,
            snoozed_until: None,
            last_accessed_at: None,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("snoozed_until"));
        assert!(!json.contains("last_accessed_at"));
    }
}
