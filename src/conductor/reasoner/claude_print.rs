//! `Reasoner` backend that shells out to `claude --print`. Mirrors
//! aoaoe's `reasoner/claude-code.ts`.

use anyhow::{Context, Result};
use tokio::process::Command;

use super::{Observation, Reasoner, Recommendation};

/// Default system prompt handed to the model. Constrains the response to
/// a JSON envelope with a closed action vocabulary so the parser is
/// deterministic. See `parse_recommendations` for the envelope shape.
const DEFAULT_SYSTEM_PROMPT: &str = concat!(
    "You are the conductor for a fleet of AI coding agent sessions. ",
    "Given an observation of the fleet, recommend actions that keep work moving. ",
    "Reply with a single JSON object and nothing else: ",
    "{\"recommendations\": [{\"session_id\": \"...\", \"action\": {\"kind\": \"...\"}, \"rationale\": \"...\"}]}. ",
    "Valid action kinds: snooze (with minutes: integer), favorite, unfavorite, archive, ",
    "nudge (with message: string), no_op. ",
    "Be conservative. Prefer no_op when nothing is clearly needed. ",
    "Archive is destructive and only takes effect if the user has opted in."
);

/// Reasoner that spawns `claude --print` per tick with the observation as
/// the prompt payload.
pub struct ClaudePrintReasoner {
    binary: String,
    system_prompt: String,
}

impl Default for ClaudePrintReasoner {
    fn default() -> Self {
        Self {
            binary: "claude".to_string(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
        }
    }
}

impl ClaudePrintReasoner {
    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
        }
    }
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
/// pulls the first top-level `{...}` block out before parsing.
fn parse_recommendations(raw: &str) -> Result<Vec<Recommendation>> {
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
            {"session_id":"f","action":{"kind":"no_op"},"rationale":""}
        ]}"#;
        let recs = parse_recommendations(json).unwrap();
        assert_eq!(recs.len(), 6);
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
        assert_eq!(recs[5].action, Action::NoOp);
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
