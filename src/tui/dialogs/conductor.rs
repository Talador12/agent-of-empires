//! Full-screen conductor panel. Ports aoaoe's slash-command interaction
//! surface: a query line at the bottom lets the user swap between views
//! (queue, tasks, health, help) and drive the conductor without leaving
//! the panel.

use std::time::Instant;

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use crate::conductor::reasoner::ReasonerMode;
use crate::conductor::tasks::{Task, TaskStatus, TaskStore};
use crate::conductor::{attention_score, is_enabled as conductor_enabled, EXPERIMENTAL_ENV};
use crate::session::{Instance, Status, Storage};
use crate::tui::styles::Theme;

pub enum ConductorAction {
    Continue,
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Queue,
    Tasks,
    Health,
    Help,
}

#[derive(Clone)]
struct RankedRow {
    title: String,
    status: String,
    score: i64,
    favorited: bool,
    unread: bool,
}

#[derive(Default)]
struct FleetHealth {
    total: usize,
    waiting: usize,
    running: usize,
    idle: usize,
    error: usize,
    stopped: usize,
    favorites: usize,
    unread: usize,
    top_scores: Vec<i64>,
}

pub struct ConductorView {
    profile: String,
    view: ViewMode,
    mode: ReasonerMode,
    ranked: Vec<RankedRow>,
    tasks: Vec<Task>,
    health: FleetHealth,
    input_open: bool,
    input_buffer: String,
    status_line: Option<String>,
    last_refreshed: Option<Instant>,
    last_refresh_error: Option<String>,
}

impl ConductorView {
    pub fn new(profile: impl Into<String>) -> Self {
        let mut view = Self {
            profile: profile.into(),
            view: ViewMode::Queue,
            mode: ReasonerMode::default(),
            ranked: Vec::new(),
            tasks: Vec::new(),
            health: FleetHealth::default(),
            input_open: false,
            input_buffer: String::new(),
            status_line: None,
            last_refreshed: None,
            last_refresh_error: None,
        };
        view.refresh_now();
        view
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ConductorAction {
        if self.input_open {
            return self.handle_input_key(key);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => ConductorAction::Close,
            KeyCode::Char('r') | KeyCode::F(5) => {
                self.refresh_now();
                ConductorAction::Continue
            }
            // `:` and `/` both enter input mode so the user can pick the
            // habit they already know from `vim` / `less`.
            KeyCode::Char(':') | KeyCode::Char('/') => {
                self.input_open = true;
                self.input_buffer.clear();
                self.status_line = None;
                ConductorAction::Continue
            }
            _ => ConductorAction::Continue,
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> ConductorAction {
        match key.code {
            KeyCode::Esc => {
                self.input_open = false;
                self.input_buffer.clear();
                ConductorAction::Continue
            }
            KeyCode::Enter => {
                let cmd = std::mem::take(&mut self.input_buffer);
                self.input_open = false;
                self.execute(cmd.trim())
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
                ConductorAction::Continue
            }
            KeyCode::Char(c) => {
                self.input_buffer.push(c);
                ConductorAction::Continue
            }
            _ => ConductorAction::Continue,
        }
    }

    fn execute(&mut self, cmd: &str) -> ConductorAction {
        let cmd = cmd.trim_start_matches([':', '/']);
        let mut parts = cmd.split_whitespace();
        let head = parts.next().unwrap_or("");
        let tail: Vec<&str> = parts.collect();

        match head {
            "" => ConductorAction::Continue,
            "q" | "quit" | "exit" => ConductorAction::Close,
            "h" | "help" => {
                self.view = ViewMode::Help;
                self.status_line = Some("help view".into());
                ConductorAction::Continue
            }
            "r" | "refresh" => {
                self.refresh_now();
                ConductorAction::Continue
            }
            "queue" | "sessions" => {
                self.view = ViewMode::Queue;
                self.refresh_now();
                ConductorAction::Continue
            }
            "tasks" => {
                self.view = ViewMode::Tasks;
                self.refresh_now();
                ConductorAction::Continue
            }
            "health" => {
                self.view = ViewMode::Health;
                self.refresh_now();
                ConductorAction::Continue
            }
            "mode" => {
                let Some(arg) = tail.first() else {
                    self.status_line = Some("usage: :mode conservative|balanced|aggressive".into());
                    return ConductorAction::Continue;
                };
                match ReasonerMode::from_cli(arg) {
                    Ok(mode) => {
                        self.mode = mode;
                        self.status_line = Some(format!("mode = {:?}", self.mode));
                    }
                    Err(e) => self.status_line = Some(format!("bad mode: {e}")),
                }
                ConductorAction::Continue
            }
            other => {
                self.status_line = Some(format!("unknown command: {other} (try :help)"));
                ConductorAction::Continue
            }
        }
    }

    fn refresh_now(&mut self) {
        self.last_refreshed = Some(Instant::now());
        match load_ranked(&self.profile) {
            Ok((rows, health)) => {
                self.ranked = rows;
                self.health = health;
                self.last_refresh_error = None;
            }
            Err(err) => {
                self.last_refresh_error = Some(err.to_string());
                self.ranked.clear();
            }
        }
        // Best-effort load; a bad task file is not fatal for the panel.
        self.tasks = TaskStore::for_profile(&self.profile)
            .and_then(|s| s.load())
            .unwrap_or_default();
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let footer_lines = if self.input_open || self.status_line.is_some() {
            2
        } else {
            1
        };
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(footer_lines),
            ])
            .split(area);
        self.render_header(frame, vertical[0], theme);
        match self.view {
            ViewMode::Queue => self.render_queue(frame, vertical[1], theme),
            ViewMode::Tasks => self.render_tasks(frame, vertical[1], theme),
            ViewMode::Health => self.render_health(frame, vertical[1], theme),
            ViewMode::Help => self.render_help(frame, vertical[1], theme),
        }
        self.render_footer(frame, vertical[2], theme);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let title_style = Style::default().fg(theme.title).bold();
        let accent_style = Style::default().fg(theme.accent);
        let gate = if conductor_enabled() {
            Span::styled("enabled", Style::default().fg(theme.running).bold())
        } else {
            Span::styled("disabled", Style::default().fg(theme.dimmed).bold())
        };
        let view_label = match self.view {
            ViewMode::Queue => "queue",
            ViewMode::Tasks => "tasks",
            ViewMode::Health => "health",
            ViewMode::Help => "help",
        };
        let header = vec![
            Line::from(vec![
                Span::styled("Conductor", title_style),
                Span::raw("  (experimental)"),
                Span::raw("    view: "),
                Span::styled(view_label, accent_style),
            ]),
            Line::from(vec![
                Span::raw("profile: "),
                Span::styled(self.profile.clone(), accent_style),
                Span::raw("    "),
                Span::raw(format!("{}=", EXPERIMENTAL_ENV)),
                gate,
                Span::raw("    mode: "),
                Span::styled(format!("{:?}", self.mode), accent_style),
            ]),
        ];
        let block = Block::default().borders(Borders::BOTTOM);
        frame.render_widget(Paragraph::new(header).block(block), area);
    }

    fn render_queue(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Attention queue ");
        if let Some(err) = &self.last_refresh_error {
            let msg = format!("Could not load sessions: {err}");
            frame.render_widget(
                Paragraph::new(msg).block(block).wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        if self.ranked.is_empty() {
            let msg = "No sessions in this profile yet.";
            frame.render_widget(
                Paragraph::new(msg)
                    .block(block)
                    .alignment(Alignment::Center),
                area,
            );
            return;
        }
        let header = Row::new(vec![
            Cell::from("SCORE"),
            Cell::from("STATUS"),
            Cell::from("TITLE"),
            Cell::from("FLAGS"),
        ])
        .style(Style::default().fg(theme.title).bold());
        let rows: Vec<Row> = self
            .ranked
            .iter()
            .map(|r| {
                let mut flags = String::new();
                if r.favorited {
                    flags.push('★');
                }
                if r.unread {
                    flags.push('•');
                }
                Row::new(vec![
                    Cell::from(r.score.to_string()),
                    Cell::from(r.status.clone()),
                    Cell::from(r.title.clone()),
                    Cell::from(flags),
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Min(10),
            Constraint::Length(6),
        ];
        let table = Table::new(rows, widths).header(header).block(block);
        frame.render_widget(table, area);
    }

    fn render_tasks(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default().borders(Borders::ALL).title(" Tasks ");
        if self.tasks.is_empty() {
            frame.render_widget(
                Paragraph::new("No tasks. Add one with `aoe conductor task add`.")
                    .block(block)
                    .alignment(Alignment::Center),
                area,
            );
            return;
        }
        let header = Row::new(vec![
            Cell::from("ID"),
            Cell::from("STATUS"),
            Cell::from("SESSION"),
            Cell::from("TITLE"),
        ])
        .style(Style::default().fg(theme.title).bold());
        let rows: Vec<Row> = self
            .tasks
            .iter()
            .map(|t| {
                let status = match t.status {
                    TaskStatus::Pending => "pending",
                    TaskStatus::InProgress => "in_progress",
                    TaskStatus::Completed => "completed",
                };
                let session = t.linked_session_id.as_deref().unwrap_or("-").to_string();
                Row::new(vec![
                    Cell::from(t.id.clone()),
                    Cell::from(status),
                    Cell::from(session),
                    Cell::from(t.title.clone()),
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(20),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Min(10),
        ];
        let table = Table::new(rows, widths).header(header).block(block);
        frame.render_widget(table, area);
    }

    fn render_health(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let title_style = Style::default().fg(theme.title).bold();
        let accent_style = Style::default().fg(theme.accent);
        let h = &self.health;
        let top = if h.top_scores.is_empty() {
            "(none)".to_string()
        } else {
            h.top_scores
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let lines = vec![
            Line::from(Span::styled("Fleet health", title_style)),
            Line::from(""),
            Line::from(vec![
                Span::raw("Total     "),
                Span::styled(h.total.to_string(), accent_style),
            ]),
            Line::from(vec![
                Span::raw("Waiting   "),
                Span::styled(h.waiting.to_string(), accent_style),
            ]),
            Line::from(vec![
                Span::raw("Running   "),
                Span::styled(h.running.to_string(), accent_style),
            ]),
            Line::from(vec![
                Span::raw("Idle      "),
                Span::styled(h.idle.to_string(), accent_style),
            ]),
            Line::from(vec![
                Span::raw("Error     "),
                Span::styled(h.error.to_string(), accent_style),
            ]),
            Line::from(vec![
                Span::raw("Stopped   "),
                Span::styled(h.stopped.to_string(), accent_style),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("Favorites "),
                Span::styled(h.favorites.to_string(), accent_style),
            ]),
            Line::from(vec![
                Span::raw("Unread    "),
                Span::styled(h.unread.to_string(), accent_style),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("Top scores  "),
                Span::styled(top, accent_style),
            ]),
        ];
        let block = Block::default().borders(Borders::ALL).title(" Health ");
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_help(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let title_style = Style::default().fg(theme.title).bold();
        let cmd_style = Style::default().fg(theme.accent);
        let lines = vec![
            Line::from(Span::styled("Slash commands", title_style)),
            Line::from(""),
            Line::from(vec![
                Span::styled(":queue", cmd_style),
                Span::raw("      switch to the ranked queue"),
            ]),
            Line::from(vec![
                Span::styled(":tasks", cmd_style),
                Span::raw("      switch to the task list"),
            ]),
            Line::from(vec![
                Span::styled(":health", cmd_style),
                Span::raw("     switch to the fleet-health strip"),
            ]),
            Line::from(vec![
                Span::styled(":refresh", cmd_style),
                Span::raw("    reload from disk"),
            ]),
            Line::from(vec![
                Span::styled(":mode <m>", cmd_style),
                Span::raw("   set reasoner posture: conservative|balanced|aggressive"),
            ]),
            Line::from(vec![
                Span::styled(":help", cmd_style),
                Span::raw("       this screen"),
            ]),
            Line::from(vec![
                Span::styled(":quit", cmd_style),
                Span::raw("       close the panel"),
            ]),
            Line::from(""),
            Line::from(Span::styled("Keys (normal mode)", title_style)),
            Line::from(vec![
                Span::styled("  r / F5", cmd_style),
                Span::raw("     refresh"),
            ]),
            Line::from(vec![
                Span::styled("  : or /", cmd_style),
                Span::raw("     open the command line"),
            ]),
            Line::from(vec![
                Span::styled("  Esc / q", cmd_style),
                Span::raw("    close"),
            ]),
        ];
        let block = Block::default().borders(Borders::ALL).title(" Help ");
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let key_style = Style::default().fg(theme.help_key).bold();
        let refreshed = self
            .last_refreshed
            .map(|t| format!("refreshed {}s ago", t.elapsed().as_secs()))
            .unwrap_or_else(|| "not yet refreshed".to_string());

        let mut lines: Vec<Line> = Vec::new();
        if self.input_open {
            lines.push(Line::from(vec![
                Span::styled(":", key_style),
                Span::raw(self.input_buffer.clone()),
                Span::styled("_", Style::default().fg(theme.accent)),
            ]));
        } else if let Some(status) = &self.status_line {
            lines.push(Line::from(Span::styled(
                status.clone(),
                Style::default().fg(theme.hint),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled("Esc/q ", key_style),
            Span::raw("close    "),
            Span::styled("r ", key_style),
            Span::raw("refresh    "),
            Span::styled(": ", key_style),
            Span::raw("command    "),
            Span::raw(refreshed),
            Span::raw("    "),
            Span::raw(format!("at {}", Utc::now().format("%H:%M:%S"))),
        ]));
        frame.render_widget(Paragraph::new(lines), area);
    }
}

fn load_ranked(profile: &str) -> anyhow::Result<(Vec<RankedRow>, FleetHealth)> {
    let storage = Storage::new_unwatched(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;
    crate::tmux::refresh_session_cache();
    for inst in &mut instances {
        inst.update_status();
    }
    let health = compute_health(&instances);
    let mut scored: Vec<(i64, Instance)> = instances
        .into_iter()
        .filter_map(|i| attention_score(&i).map(|s| (s, i)))
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    let rows = scored
        .into_iter()
        .map(|(score, inst)| RankedRow {
            title: inst.title,
            status: format!("{:?}", inst.status),
            score,
            favorited: inst.favorited_at.is_some(),
            unread: inst.unread,
        })
        .collect();
    Ok((rows, health))
}

fn compute_health(instances: &[Instance]) -> FleetHealth {
    let mut h = FleetHealth {
        total: instances.len(),
        ..FleetHealth::default()
    };
    for inst in instances {
        match inst.status {
            Status::Waiting => h.waiting += 1,
            Status::Running => h.running += 1,
            Status::Idle | Status::Unknown => h.idle += 1,
            Status::Error => h.error += 1,
            Status::Stopped => h.stopped += 1,
            _ => {}
        }
        if inst.favorited_at.is_some() {
            h.favorites += 1;
        }
        if inst.unread {
            h.unread += 1;
        }
        if let Some(s) = attention_score(inst) {
            h.top_scores.push(s);
        }
    }
    h.top_scores.sort_by(|a, b| b.cmp(a));
    h.top_scores.truncate(3);
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn esc_closes_from_normal_mode() {
        let mut v = ConductorView::new("test-profile");
        let action = v.handle_key(key(KeyCode::Esc));
        assert!(matches!(action, ConductorAction::Close));
    }

    #[test]
    fn colon_opens_input_mode() {
        let mut v = ConductorView::new("test-profile");
        v.handle_key(key(KeyCode::Char(':')));
        assert!(v.input_open);
    }

    #[test]
    fn esc_from_input_returns_to_normal() {
        let mut v = ConductorView::new("test-profile");
        v.handle_key(key(KeyCode::Char(':')));
        v.handle_key(key(KeyCode::Esc));
        assert!(!v.input_open);
    }

    #[test]
    fn typing_command_and_enter_switches_view() {
        let mut v = ConductorView::new("test-profile");
        v.handle_key(key(KeyCode::Char(':')));
        for c in "tasks".chars() {
            v.handle_key(key(KeyCode::Char(c)));
        }
        v.handle_key(key(KeyCode::Enter));
        assert!(matches!(v.view, ViewMode::Tasks));
    }

    #[test]
    fn quit_command_closes_panel() {
        let mut v = ConductorView::new("test-profile");
        v.handle_key(key(KeyCode::Char(':')));
        for c in "quit".chars() {
            v.handle_key(key(KeyCode::Char(c)));
        }
        let action = v.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, ConductorAction::Close));
    }

    #[test]
    fn mode_command_changes_reasoner_mode() {
        let mut v = ConductorView::new("test-profile");
        v.handle_key(key(KeyCode::Char(':')));
        for c in "mode aggressive".chars() {
            v.handle_key(key(KeyCode::Char(c)));
        }
        v.handle_key(key(KeyCode::Enter));
        assert_eq!(v.mode, ReasonerMode::Aggressive);
    }

    #[test]
    fn unknown_command_sets_status_line() {
        let mut v = ConductorView::new("test-profile");
        v.handle_key(key(KeyCode::Char(':')));
        for c in "unknown".chars() {
            v.handle_key(key(KeyCode::Char(c)));
        }
        v.handle_key(key(KeyCode::Enter));
        assert!(v.status_line.as_deref().unwrap_or("").contains("unknown"));
    }
}
