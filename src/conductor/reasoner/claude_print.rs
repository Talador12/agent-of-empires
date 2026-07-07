//! `Reasoner` backend that shells out to `claude --print`. Mirrors
//! aoaoe's `reasoner/claude-code.ts`.

use anyhow::{Context, Result};
use tokio::process::Command;

use super::{Observation, Reasoner, ReasonerMode, Recommendation};

/// Reasoner that spawns `claude --print` per tick with the observation as
/// the prompt payload.
pub struct ClaudePrintReasoner {
    binary: String,
    system_prompt: String,
}

impl Default for ClaudePrintReasoner {
    fn default() -> Self {
        Self::for_mode(ReasonerMode::default())
    }
}

impl ClaudePrintReasoner {
    pub fn for_mode(mode: ReasonerMode) -> Self {
        Self {
            binary: "claude".to_string(),
            system_prompt: build_system_prompt(mode),
        }
    }

    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            system_prompt: build_system_prompt(ReasonerMode::default()),
        }
    }

    pub fn with_mode(mut self, mode: ReasonerMode) -> Self {
        self.system_prompt = build_system_prompt(mode);
        self
    }
}

pub(super) fn build_system_prompt(mode: ReasonerMode) -> String {
    format!(
        concat!(
            "You are the conductor for a fleet of AI coding agent sessions. ",
            "Given an observation of the fleet, recommend actions that keep work moving. ",
            "Reply with a single JSON object and nothing else: ",
            "{{\"recommendations\": [{{\"session_id\": \"...\", \"action\": {{\"kind\": \"...\"}}, ",
            "\"rationale\": \"...\", \"confidence\": 0.0}}]}}. ",
            "`confidence` is optional (0.0 to 1.0); include it when you're less than certain. ",
            "Each session snapshot may include content-derived signals: ",
            "activity (coding/testing/debugging/reading/idle), sentiment ",
            "(progress/success/blocked/error/frustrated/idle), error_match ",
            "(a named error pattern with remediation hint), goal_completed (bool), ",
            "and heartbeat fields (unchanged_ticks, potentially_stuck). ",
            "Weight your recommendations by these signals: an error sentiment or ",
            "potentially_stuck flag is a strong nudge/start hint, goal_completed is ",
            "a strong archive/no_op hint. ",
            "Valid action kinds: snooze (with minutes: integer), favorite, unfavorite, archive, ",
            "nudge (with message: string), start_session, stop_session, no_op. ",
            "Archive and stop_session are destructive; nudge is disruptive. ",
            "Each only takes effect if the user has opted in via the matching policy. ",
            "{}"
        ),
        mode.posture_line()
    )
}

impl Reasoner for ClaudePrintReasoner {
    async fn recommend(&self, observation: &Observation) -> Result<Vec<Recommendation>> {
        let payload = serde_json::to_string(observation).context("serialize observation")?;
        let prompt = format!("{}\n\nObservation:\n{}", self.system_prompt, payload);

        let output = Command::new(&self.binary)
            .arg("--print")
            .arg(&prompt)
            .output()
            .await
            .with_context(|| format!("spawn {} --print", self.binary))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "{} --print exited with status {}: {}",
                self.binary,
                output.status,
                stderr.trim()
            );
        }

        let stdout =
            String::from_utf8(output.stdout).context("reasoner stdout was not valid utf-8")?;
        parse_recommendations(&stdout)
    }
}

/// Extract and parse the JSON envelope. LLMs occasionally wrap responses
/// in ``` fences or trailing prose even when told not to; the extractor
/// pulls the first top-level `{...}` block out before parsing. Shared
/// with the OpenCode backend since both models are prompted to return
/// the same envelope shape.
pub(super) fn parse_recommendations(raw: &str) -> Result<Vec<Recommendation>> {
    #[derive(serde::Deserialize)]
    struct Envelope {
        recommendations: Vec<Recommendation>,
    }
    let json = extract_json_block(raw)
        .with_context(|| format!("no JSON object in reasoner output: {}", raw.trim()))?;
    let envelope: Envelope =
        serde_json::from_str(json).with_context(|| format!("parse reasoner JSON: {}", json))?;
    Ok(envelope.recommendations)
}

fn extract_json_block(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    Some(&raw[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::reasoner::Action;

    #[test]
    fn parses_snooze() {
        let json = r#"{"recommendations":[{"session_id":"abc","action":{"kind":"snooze","minutes":30},"rationale":"still waiting on human"}]}"#;
        let recs = parse_recommendations(json).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].session_id, "abc");
        assert_eq!(recs[0].action, Action::Snooze { minutes: 30 });
        assert_eq!(recs[0].rationale, "still waiting on human");
        assert_eq!(recs[0].confidence, None);
    }

    #[test]
    fn parses_confidence_when_present() {
        let json = r#"{"recommendations":[{"session_id":"abc","action":{"kind":"favorite"},"rationale":"","confidence":0.85}]}"#;
        let recs = parse_recommendations(json).unwrap();
        assert_eq!(recs[0].confidence, Some(0.85));
    }

    #[test]
    fn parses_every_action_kind() {
        // If the LLM's vocabulary drifts from the enum, one of these
        // fails loudly instead of the tick loop silently dropping actions.
        let json = r#"{"recommendations":[
            {"session_id":"a","action":{"kind":"snooze","minutes":10},"rationale":""},
            {"session_id":"b","action":{"kind":"favorite"},"rationale":""},
            {"session_id":"c","action":{"kind":"unfavorite"},"rationale":""},
            {"session_id":"d","action":{"kind":"archive"},"rationale":""},
            {"session_id":"e","action":{"kind":"nudge","message":"still there?"},"rationale":""},
            {"session_id":"f","action":{"kind":"start_session"},"rationale":""},
            {"session_id":"g","action":{"kind":"stop_session"},"rationale":""},
            {"session_id":"h","action":{"kind":"no_op"},"rationale":""}
        ]}"#;
        let recs = parse_recommendations(json).unwrap();
        assert_eq!(recs.len(), 8);
        assert_eq!(recs[0].action, Action::Snooze { minutes: 10 });
        assert_eq!(recs[1].action, Action::Favorite);
        assert_eq!(recs[2].action, Action::Unfavorite);
        assert_eq!(recs[3].action, Action::Archive);
        assert_eq!(
            recs[4].action,
            Action::Nudge {
                message: "still there?".into()
            }
        );
        assert_eq!(recs[5].action, Action::StartSession);
        assert_eq!(recs[6].action, Action::StopSession);
        assert_eq!(recs[7].action, Action::NoOp);
    }

    #[test]
    fn strips_code_fences() {
        let raw = "Here's the response:\n```json\n{\"recommendations\":[]}\n```\nThanks.";
        assert!(parse_recommendations(raw).unwrap().is_empty());
    }

    #[test]
    fn rejects_unknown_action_kind() {
        let json = r#"{"recommendations":[{"session_id":"x","action":{"kind":"self_destruct"},"rationale":""}]}"#;
        assert!(parse_recommendations(json).is_err());
    }

    #[test]
    fn rejects_missing_envelope() {
        assert!(parse_recommendations("not json").is_err());
    }

    #[test]
    fn constructs_with_custom_binary() {
        let r = ClaudePrintReasoner::with_binary("/opt/aoe/claude-shim");
        assert_eq!(r.binary, "/opt/aoe/claude-shim");
        // System prompt still the default.
        assert!(r.system_prompt.contains("conductor"));
    }
}
