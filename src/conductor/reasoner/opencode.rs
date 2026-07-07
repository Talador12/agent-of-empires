//! `Reasoner` backend that talks to a local `opencode serve` daemon over
//! HTTP. Ports aoaoe's `reasoner/opencode.ts`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{Observation, Reasoner, Recommendation};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:4096";

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

/// Reasoner backed by a running `opencode serve` daemon. The user is
/// expected to have that daemon running out of band; the conductor does
/// not start or stop it (matching how aoe treats `gh` and `claude`).
pub struct OpenCodeReasoner {
    endpoint: String,
    system_prompt: String,
}

impl Default for OpenCodeReasoner {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_string(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
        }
    }
}

impl OpenCodeReasoner {
    pub fn with_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
        }
    }
}

impl Reasoner for OpenCodeReasoner {
    async fn recommend(&self, observation: &Observation) -> Result<Vec<Recommendation>> {
        let payload = serde_json::to_string(observation).context("serialize observation")?;
        let prompt = format!("{}\n\nObservation:\n{}", self.system_prompt, payload);

        let client = reqwest::Client::new();

        let session: CreateSessionResponse = client
            .post(format!("{}/session", self.endpoint))
            .json(&CreateSessionRequest {
                title: format!(
                    "aoe-conductor-{}",
                    chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
                ),
            })
            .send()
            .await
            .with_context(|| format!("connect to opencode serve at {}", self.endpoint))?
            .error_for_status()
            .context("opencode POST /session")?
            .json()
            .await
            .context("parse opencode session id")?;

        let reply: MessageResponse = client
            .post(format!("{}/session/{}/message", self.endpoint, session.id))
            .json(&MessageRequest {
                no_reply: false,
                parts: vec![MessagePart {
                    kind: "text",
                    text: &prompt,
                }],
            })
            .send()
            .await
            .context("opencode POST /session/{id}/message")?
            .error_for_status()
            .context("opencode message returned non-2xx")?
            .json()
            .await
            .context("parse opencode message response")?;

        if let Some(err) = reply.info.and_then(|i| i.error) {
            let code = err
                .data
                .as_ref()
                .and_then(|d| d.status_code)
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            let msg = err
                .data
                .and_then(|d| d.message)
                .or(err.name)
                .unwrap_or_else(|| "unknown".into());
            anyhow::bail!("opencode API error{code}: {msg}");
        }

        let text = reply
            .parts
            .into_iter()
            .filter_map(|p| if p.kind == "text" { p.text } else { None })
            .collect::<Vec<_>>()
            .join("\n");
        super::claude_print::parse_recommendations(&text)
    }
}

#[derive(Serialize)]
struct CreateSessionRequest {
    title: String,
}

#[derive(Deserialize)]
struct CreateSessionResponse {
    id: String,
}

#[derive(Serialize)]
struct MessageRequest<'a> {
    #[serde(rename = "noReply")]
    no_reply: bool,
    parts: Vec<MessagePart<'a>>,
}

#[derive(Serialize)]
struct MessagePart<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[derive(Deserialize)]
struct MessageResponse {
    #[serde(default)]
    info: Option<MessageInfo>,
    #[serde(default)]
    parts: Vec<MessageResponsePart>,
}

#[derive(Deserialize)]
struct MessageInfo {
    #[serde(default)]
    error: Option<MessageError>,
}

#[derive(Deserialize)]
struct MessageError {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    data: Option<MessageErrorData>,
}

#[derive(Deserialize)]
struct MessageErrorData {
    #[serde(default)]
    message: Option<String>,
    #[serde(rename = "statusCode", default)]
    status_code: Option<u16>,
}

#[derive(Deserialize)]
struct MessageResponsePart {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_target_local_daemon() {
        let r = OpenCodeReasoner::default();
        assert_eq!(r.endpoint, DEFAULT_ENDPOINT);
        assert!(r.system_prompt.contains("conductor"));
    }

    #[test]
    fn accepts_custom_endpoint() {
        let r = OpenCodeReasoner::with_endpoint("http://example.internal:9000");
        assert_eq!(r.endpoint, "http://example.internal:9000");
    }
}
