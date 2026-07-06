//! Spawn one session per GitHub issue. Wraps the `gh` CLI (already the
//! canonical GitHub client for aoe) rather than talking to the REST API
//! directly, matching the "no new deps" bar the rest of the module holds.
//! The demo scenario @Seluj78 and @jerome-benoit asked for in issue #553.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Minimal shape of a GitHub issue as returned by
/// `gh issue list --json number,title,url`. Extra fields (labels, assignees,
/// state) can be requested later without breaking the deser.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct GitHubIssue {
    pub number: u64,
    pub title: String,
    pub url: String,
}

/// Ask `gh` for open issues on `repo`. `label` is an optional single-label
/// filter; multi-label filtering would be a follow-up and is not covered
/// by the current CLI surface. Errors from `gh` bubble as `anyhow`
/// contexts; the auth-not-configured case is surfaced explicitly.
pub async fn fetch_issues(
    repo: &str,
    state: &str,
    limit: u32,
    label: Option<&str>,
) -> Result<Vec<GitHubIssue>> {
    let mut cmd = Command::new("gh");
    cmd.arg("issue")
        .arg("list")
        .arg("--repo")
        .arg(repo)
        .arg("--state")
        .arg(state)
        .arg("--limit")
        .arg(limit.to_string())
        .arg("--json")
        .arg("number,title,url");
    if let Some(label) = label {
        cmd.arg("--label").arg(label);
    }
    let output = cmd
        .output()
        .await
        .context("spawn `gh` (the GitHub CLI). Install from https://cli.github.com/ if missing.")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not authenticated") || stderr.contains("gh auth") {
            anyhow::bail!("gh is not authenticated. Run `gh auth login` and retry.");
        }
        anyhow::bail!("gh issue list failed: {}", stderr.trim());
    }
    parse_issues_json(&output.stdout)
}

fn parse_issues_json(bytes: &[u8]) -> Result<Vec<GitHubIssue>> {
    let issues: Vec<GitHubIssue> = serde_json::from_slice(bytes)
        .with_context(|| format!("parse gh json: {}", String::from_utf8_lossy(bytes).trim()))?;
    Ok(issues)
}

/// Convert an issue into the session title the user will see. Kept as a
/// standalone function so a future config knob can override it without
/// threading a formatter through the spawn code path.
pub fn session_title_for_issue(prefix: &str, issue: &GitHubIssue) -> String {
    format!("{}#{} {}", prefix, issue.number, issue.title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gh_output() {
        let json =
            br#"[{"number":42,"title":"add conductor","url":"https://github.com/x/y/issues/42"}]"#;
        let issues = parse_issues_json(json).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 42);
        assert_eq!(issues[0].title, "add conductor");
    }

    #[test]
    fn parses_empty_list() {
        let issues = parse_issues_json(b"[]").unwrap();
        assert!(issues.is_empty());
    }

    #[test]
    fn rejects_malformed_json() {
        let err = parse_issues_json(b"not json").unwrap_err();
        // Context chain includes the raw bytes for debuggability.
        assert!(format!("{err:?}").contains("not json"));
    }

    #[test]
    fn title_formatter_uses_prefix() {
        let issue = GitHubIssue {
            number: 7,
            title: "orchestrator agent".into(),
            url: "https://github.com/x/y/issues/7".into(),
        };
        assert_eq!(
            session_title_for_issue("bug/", &issue),
            "bug/#7 orchestrator agent"
        );
    }
}
