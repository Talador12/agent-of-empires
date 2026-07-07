//! `/api/conductor/*` handlers. Gate on `AOE_EXPERIMENTAL_AO_MODE`
//! matches the CLI and TUI; without it, every handler returns 403.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Serialize;

use crate::conductor::executor::{Executor, Outcome};
use crate::conductor::policies::ConductorPolicies;
use crate::conductor::reasoner::claude_print::ClaudePrintReasoner;
use crate::conductor::reasoner::{ReasonerMode, Recommendation};
use crate::conductor::tasks::{Task, TaskStore};
use crate::conductor::watcher::{Watcher, DEFAULT_POLL_INTERVAL};
use crate::conductor::{attention_score, is_enabled as conductor_enabled, EXPERIMENTAL_ENV};

use super::AppState;

#[derive(Serialize)]
pub struct ConductorStateResponse {
    pub enabled: bool,
    pub env_var: &'static str,
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
    if let Some(resp) = gate_response() {
        return resp;
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

/// Fleet-level health rollup: counts per status bucket + the top three
/// attention scores. Cheap enough to compute per request; the web
/// dashboard renders this as a "conductor health" strip.
#[derive(Serialize)]
pub struct ConductorHealthResponse {
    pub enabled: bool,
    pub total: usize,
    pub waiting: usize,
    pub running: usize,
    pub idle: usize,
    pub error: usize,
    pub stopped: usize,
    pub favorites: usize,
    pub unread: usize,
    pub top_scores: Vec<i64>,
}

pub async fn get_conductor_health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(resp) = gate_response() {
        return resp;
    }
    let instances = state.instances.read().await;
    let mut counts = [0usize; 5];
    let mut favorites = 0usize;
    let mut unread = 0usize;
    let mut scores: Vec<i64> = Vec::new();
    for inst in instances.iter() {
        match inst.status {
            crate::session::Status::Waiting => counts[0] += 1,
            crate::session::Status::Running => counts[1] += 1,
            crate::session::Status::Idle | crate::session::Status::Unknown => counts[2] += 1,
            crate::session::Status::Error => counts[3] += 1,
            crate::session::Status::Stopped => counts[4] += 1,
            _ => {}
        }
        if inst.favorited_at.is_some() {
            favorites += 1;
        }
        if inst.unread {
            unread += 1;
        }
        if let Some(s) = attention_score(inst) {
            scores.push(s);
        }
    }
    scores.sort_by(|a, b| b.cmp(a));
    scores.truncate(3);

    Json(ConductorHealthResponse {
        enabled: true,
        total: instances.len(),
        waiting: counts[0],
        running: counts[1],
        idle: counts[2],
        error: counts[3],
        stopped: counts[4],
        favorites,
        unread,
        top_scores: scores,
    })
    .into_response()
}

/// List every task from the daemon's task store for the active profile.
pub async fn list_conductor_tasks(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(resp) = gate_response() {
        return resp;
    }
    match TaskStore::for_profile(&state.profile).and_then(|s| s.load()) {
        Ok(tasks) => Json(TaskListResponse { tasks }).into_response(),
        Err(err) => internal_error(&format!("task store load failed: {err}")),
    }
}

#[derive(Serialize)]
pub struct TaskListResponse {
    pub tasks: Vec<Task>,
}

/// Fire one manual tick of the conductor loop. Uses `ClaudePrintReasoner`
/// with the default mode. The web dashboard uses this to show the
/// reasoner's current view without needing a long-running daemon.
#[derive(Serialize)]
pub struct ConductorTickResponse {
    pub recommendations: Vec<Recommendation>,
}

pub async fn post_conductor_tick(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(resp) = gate_response() {
        return resp;
    }
    let reasoner = ClaudePrintReasoner::for_mode(ReasonerMode::default());
    let watcher = Watcher::new(state.profile.clone(), reasoner, DEFAULT_POLL_INTERVAL);
    match watcher.tick().await {
        Ok(recommendations) => Json(ConductorTickResponse { recommendations }).into_response(),
        Err(err) => internal_error(&format!("tick failed: {err}")),
    }
}

/// Dispatch one action from the web dashboard, subject to policies. The
/// caller opts into destructive/nudge behavior in the JSON body so the
/// URL surface stays static.
#[derive(serde::Deserialize)]
pub struct ConductorActionRequest {
    pub recommendation: Recommendation,
    #[serde(default)]
    pub allow_destructive: bool,
    #[serde(default)]
    pub allow_nudge: bool,
}

#[derive(Serialize)]
pub struct ConductorActionResponse {
    pub outcomes: Vec<Outcome>,
}

pub async fn post_conductor_action(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ConductorActionRequest>,
) -> impl IntoResponse {
    if let Some(resp) = gate_response() {
        return resp;
    }
    let policies = ConductorPolicies {
        allow_destructive: req.allow_destructive,
        allow_nudge: req.allow_nudge,
        ..ConductorPolicies::default()
    };
    let executor = Executor::new(state.profile.clone(), policies);
    match executor.dispatch(&[req.recommendation]) {
        Ok(outcomes) => Json(ConductorActionResponse { outcomes }).into_response(),
        Err(err) => internal_error(&format!("dispatch failed: {err}")),
    }
}

fn gate_response() -> Option<axum::response::Response> {
    if conductor_enabled() {
        return None;
    }
    Some(
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "conductor is experimental",
                "hint": format!("set {}=1 on the daemon and restart aoe serve", EXPERIMENTAL_ENV),
                "env_var": EXPERIMENTAL_ENV,
            })),
        )
            .into_response(),
    )
}

fn internal_error(message: &str) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}
