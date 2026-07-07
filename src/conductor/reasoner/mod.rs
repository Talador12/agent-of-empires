//! Pluggable source of action recommendations. Backends live in
//! submodules; the watch loop is generic over `Reasoner`.

use std::future::Future;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod claude_print;
pub mod opencode;

/// How aggressively the reasoner should recommend actions. Ports aoaoe's
/// `promptTemplate` mode selection. Each variant selects a system prompt
/// that shapes the model's default posture; the closed action vocabulary
/// is identical across modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasonerMode {
    /// Prefer `no_op` unless action is clearly needed.
    Conservative,
    /// Recommend actions when they would materially help. Default.
    #[default]
    Balanced,
    /// Actively suggest actions to keep sessions moving.
    Aggressive,
}

impl ReasonerMode {
    /// Parse the string form used by the CLI (`--mode <mode>`). Named
    /// `from_cli` rather than `from_str` to keep clippy's
    /// `should_implement_trait` quiet without implementing `FromStr` (the
    /// `--mode` clap parser is already the value_parser).
    pub fn from_cli(s: &str) -> anyhow::Result<Self> {
        match s {
            "conservative" => Ok(Self::Conservative),
            "balanced" => Ok(Self::Balanced),
            "aggressive" => Ok(Self::Aggressive),
            _ => anyhow::bail!(
                "unknown --mode {:?}; expected conservative|balanced|aggressive",
                s
            ),
        }
    }

    /// Postural instruction folded into the system prompt.
    pub fn posture_line(self) -> &'static str {
        match self {
            Self::Conservative => {
                "Prefer no_op. Only suggest actions when the fleet clearly needs them."
            }
            Self::Balanced => {
                "Recommend actions when they would materially help. Use no_op when in doubt."
            }
            Self::Aggressive => {
                "Actively surface actions that would keep sessions moving. Explain each one."
            }
        }
    }
}

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

/// One action the reasoner suggests. The executor is what actually
/// mutates session state; the reasoner never touches disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    pub session_id: String,
    pub action: Action,
    /// One-line human-readable rationale from the LLM.
    pub rationale: String,
    /// Optional model-reported confidence, `0.0..=1.0`. Ports aoaoe's
    /// low/high-confidence markers from `parse.ts`. Emitted through the
    /// tick log so a reviewer can spot low-confidence actions after the
    /// fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// Closed set of actions the reasoner may suggest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Push the session out of the attention queue for `minutes`.
    Snooze { minutes: u32 },
    /// Pin the session to the top of the attention sort.
    Favorite,
    /// Remove the favorite pin.
    Unfavorite,
    /// Move the session out of the active view. Gated by
    /// `ConductorPolicies::allow_destructive`.
    Archive,
    /// Send a text message to the session's running agent via
    /// `tmux send-keys`. Gated by `ConductorPolicies::allow_nudge`.
    Nudge { message: String },
    /// Bring a stopped or dead session back up. Runs the same startup path
    /// as `aoe session start`.
    StartSession,
    /// Stop the session's tmux pane (and its container if sandboxed).
    /// Gated by `ConductorPolicies::allow_destructive`.
    StopSession,
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
            confidence: None,
        };
        let stub = StubReasoner {
            canned: vec![rec.clone()],
        };
        let out = stub.recommend(&empty_observation()).await.unwrap();
        assert_eq!(out, vec![rec]);
    }

    #[test]
    fn mode_parses_known_labels() {
        assert_eq!(
            ReasonerMode::from_cli("conservative").unwrap(),
            ReasonerMode::Conservative
        );
        assert_eq!(
            ReasonerMode::from_cli("balanced").unwrap(),
            ReasonerMode::Balanced
        );
        assert_eq!(
            ReasonerMode::from_cli("aggressive").unwrap(),
            ReasonerMode::Aggressive
        );
    }

    #[test]
    fn mode_rejects_unknown_label() {
        assert!(ReasonerMode::from_cli("YOLO").is_err());
    }

    #[test]
    fn mode_default_is_balanced() {
        assert_eq!(ReasonerMode::default(), ReasonerMode::Balanced);
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
