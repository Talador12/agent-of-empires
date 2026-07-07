//! Task manager ported from aoaoe's `task-manager.ts`. Persists a list
//! of tasks the conductor is trying to shepherd across sessions in a
//! per-profile JSON file at `<app_dir>/<profile>/conductor-tasks.json`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const TASKS_FILE: &str = "conductor-tasks.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub goal: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default = "TaskStatus::default_status")]
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_session_id: Option<String>,
    #[serde(default)]
    pub progress_notes: Vec<ProgressNote>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

impl TaskStatus {
    fn default_status() -> Self {
        Self::Pending
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressNote {
    pub note: String,
    pub at: DateTime<Utc>,
}

/// Handle to the on-disk task store for a profile.
pub struct TaskStore {
    path: PathBuf,
}

impl TaskStore {
    pub fn for_profile(profile: &str) -> Result<Self> {
        let app_dir = crate::session::get_app_dir()?;
        let path = app_dir.join(profile).join(TASKS_FILE);
        Ok(Self { path })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn load(&self) -> Result<Vec<Task>> {
        if !self.path.exists() {
            return Ok(vec![]);
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("read tasks from {}", self.path.display()))?;
        if raw.trim().is_empty() {
            return Ok(vec![]);
        }
        serde_json::from_str(&raw)
            .with_context(|| format!("parse tasks from {}", self.path.display()))
    }

    fn save(&self, tasks: &[Task]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create task-store dir {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(tasks).context("serialize tasks")?;
        std::fs::write(&self.path, raw)
            .with_context(|| format!("write tasks to {}", self.path.display()))
    }

    pub fn add(&self, task: Task) -> Result<()> {
        let mut tasks = self.load()?;
        if tasks.iter().any(|t| t.id == task.id) {
            anyhow::bail!("task with id {:?} already exists", task.id);
        }
        tasks.push(task);
        self.save(&tasks)
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        let mut tasks = self.load()?;
        let before = tasks.len();
        tasks.retain(|t| t.id != id);
        let removed = before != tasks.len();
        if removed {
            self.save(&tasks)?;
        }
        Ok(removed)
    }

    pub fn link_session(&self, id: &str, session_id: &str) -> Result<bool> {
        self.mutate(id, |t| {
            t.linked_session_id = Some(session_id.to_string());
            if matches!(t.status, TaskStatus::Pending) {
                t.status = TaskStatus::InProgress;
            }
        })
    }

    pub fn append_progress(&self, id: &str, note: &str) -> Result<bool> {
        self.mutate(id, |t| {
            t.progress_notes.push(ProgressNote {
                note: note.to_string(),
                at: Utc::now(),
            });
            if matches!(t.status, TaskStatus::Pending) {
                t.status = TaskStatus::InProgress;
            }
        })
    }

    pub fn complete(&self, id: &str) -> Result<bool> {
        self.mutate(id, |t| {
            t.status = TaskStatus::Completed;
            t.completed_at = Some(Utc::now());
        })
    }

    fn mutate(&self, id: &str, f: impl FnOnce(&mut Task)) -> Result<bool> {
        let mut tasks = self.load()?;
        let Some(task) = tasks.iter_mut().find(|t| t.id == id) else {
            return Ok(false);
        };
        f(task);
        self.save(&tasks)?;
        Ok(true)
    }
}

impl Task {
    pub fn new(id: impl Into<String>, title: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            goal: goal.into(),
            keywords: Vec::new(),
            status: TaskStatus::Pending,
            linked_session_id: None,
            progress_notes: Vec::new(),
            created_at: Utc::now(),
            completed_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store_in_temp(dir: &TempDir) -> TaskStore {
        TaskStore {
            path: dir.path().join("tasks.json"),
        }
    }

    #[test]
    fn empty_when_no_file() {
        let dir = TempDir::new().unwrap();
        let store = store_in_temp(&dir);
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn add_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = store_in_temp(&dir);
        store
            .add(Task::new("t1", "Ship conductor", "Land the PR"))
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "t1");
        assert_eq!(loaded[0].status, TaskStatus::Pending);
    }

    #[test]
    fn add_rejects_duplicate_id() {
        let dir = TempDir::new().unwrap();
        let store = store_in_temp(&dir);
        store.add(Task::new("t1", "A", "goal")).unwrap();
        let err = store.add(Task::new("t1", "B", "goal")).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn remove_returns_true_when_present() {
        let dir = TempDir::new().unwrap();
        let store = store_in_temp(&dir);
        store.add(Task::new("t1", "A", "goal")).unwrap();
        assert!(store.remove("t1").unwrap());
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn remove_returns_false_when_missing() {
        let dir = TempDir::new().unwrap();
        let store = store_in_temp(&dir);
        assert!(!store.remove("ghost").unwrap());
    }

    #[test]
    fn link_flips_to_in_progress() {
        let dir = TempDir::new().unwrap();
        let store = store_in_temp(&dir);
        store.add(Task::new("t1", "A", "goal")).unwrap();
        assert!(store.link_session("t1", "s1").unwrap());
        let t = &store.load().unwrap()[0];
        assert_eq!(t.linked_session_id.as_deref(), Some("s1"));
        assert_eq!(t.status, TaskStatus::InProgress);
    }

    #[test]
    fn append_progress_records_note_and_ticks_status() {
        let dir = TempDir::new().unwrap();
        let store = store_in_temp(&dir);
        store.add(Task::new("t1", "A", "goal")).unwrap();
        assert!(store.append_progress("t1", "wrote the tests").unwrap());
        let t = &store.load().unwrap()[0];
        assert_eq!(t.progress_notes.len(), 1);
        assert_eq!(t.progress_notes[0].note, "wrote the tests");
        assert_eq!(t.status, TaskStatus::InProgress);
    }

    #[test]
    fn complete_marks_finished_at() {
        let dir = TempDir::new().unwrap();
        let store = store_in_temp(&dir);
        store.add(Task::new("t1", "A", "goal")).unwrap();
        assert!(store.complete("t1").unwrap());
        let t = &store.load().unwrap()[0];
        assert_eq!(t.status, TaskStatus::Completed);
        assert!(t.completed_at.is_some());
    }
}
