//! `/api/conductor/*` handlers. Experimental orchestrator surface (issue
//! #553) exposed to the web dashboard and any scripted consumer that
//! wants a fleet-wide attention view without spawning the CLI. Gate on
//! `AOE_EXPERIMENTAL_AO_MODE` matches the CLI and TUI; without it, the
//! endpoint returns 403 with a hint.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Serialize;

use crate::conductor::{attention_score, is_enabled as conductor_enabled, EXPERIMENTAL_ENV};

use super::AppState;

#[derive(Serialize)]
pub struct ConductorStateResponse {
    /// True when `AOE_EXPERIMENTAL_AO_MODE` is set on the daemon process.
    /// The web client uses this to render an "opt-in required" banner
    /// instead of silently showing an empty queue.
    pub enabled: bool,
    /// Env var name the daemon checks, echoed so the client does not have
    /// to hard-code the string.
    pub env_var: &'static str,
    /// Attention-ranked queue. Same shape as `aoe conductor status --json`,
    /// sorted descending by score.
    pub queue: Vec<ConductorSessionRow>,
}

#[derive(Serialize)]
pub struct ConductorSessionRow {
    pub id: String,
    pub title: String,
    pub status: String,
    pub attention_score: i64,
    pub favorited: bool,
    pub unread: bool,
}

pub async fn get_conductor_state(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if !conductor_enabled() {
        let body = Json(serde_json::json!({
            "error": "conductor is experimental",
            "hint": format!("set {}=1 on the daemon and restart aoe serve", EXPERIMENTAL_ENV),
            "env_var": EXPERIMENTAL_ENV,
        }));
        return (StatusCode::FORBIDDEN, body).into_response();
    }

    let instances = state.instances.read().await;
    let mut scored: Vec<(i64, ConductorSessionRow)> = instances
        .iter()
        .filter_map(|inst| {
            attention_score(inst).map(|score| {
                (
                    score,
                    ConductorSessionRow {
                        id: inst.id.clone(),
                        title: inst.title.clone(),
                        status: format!("{:?}", inst.status),
                        attention_score: score,
                        favorited: inst.favorited_at.is_some(),
                        unread: inst.unread,
                    },
                )
            })
        })
        .collect();
    scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));

    let queue: Vec<ConductorSessionRow> = scored.into_iter().map(|(_, row)| row).collect();
    Json(ConductorStateResponse {
        enabled: true,
        env_var: EXPERIMENTAL_ENV,
        queue,
    })
    .into_response()
}
