//! Full-screen conductor panel. Follows the `ServeView` shape: owns its
//! own state, returns `ConductorAction` from `handle_key`, and takes over
//! the whole screen while open. Display-only in this commit; the tick
//! trigger + apply flow lands in a follow-up commit tracked in the PR
//! description.

use std::time::Instant;

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use crate::conductor::{attention_score, is_enabled as conductor_enabled, EXPERIMENTAL_ENV};
use crate::session::{Instance, Storage};
use crate::tui::styles::Theme;

/// Actions returned by [`ConductorView::handle_key`], mirroring the
/// full-page pattern of `ServeAction`.
pub enum ConductorAction {
    Continue,
    Close,
}

/// Snapshot of one session, projected for the panel's table.
#[derive(Clone)]
struct RankedRow {
    title: String,
    status: String,
    score: i64,
    favorited: bool,
    unread: bool,
}

pub struct ConductorView {
    profile: String,
    ranked: Vec<RankedRow>,
    last_refreshed: Option<Instant>,
    last_refresh_error: Option<String>,
}

impl ConductorView {
    pub fn new(profile: impl Into<String>) -> Self {
        let mut view = Self {
            profile: profile.into(),
            ranked: Vec::new(),
            last_refreshed: None,
            last_refresh_error: None,
        };
        view.refresh_now();
        view
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ConductorAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => ConductorAction::Close,
            KeyCode::Char('r') | KeyCode::F(5) => {
                self.refresh_now();
                ConductorAction::Continue
            }
            _ => ConductorAction::Continue,
        }
    }

    fn refresh_now(&mut self) {
        self.last_refreshed = Some(Instant::now());
        match load_ranked(&self.profile) {
            Ok(rows) => {
                self.ranked = rows;
                self.last_refresh_error = None;
            }
            Err(err) => {
                self.last_refresh_error = Some(err.to_string());
                self.ranked.clear();
            }
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);
        self.render_header(frame, vertical[0], theme);
        self.render_body(frame, vertical[1], theme);
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
        let header = vec![
            Line::from(vec![
                Span::styled("Conductor", title_style),
                Span::raw("  (experimental)"),
            ]),
            Line::from(vec![
                Span::raw("profile: "),
                Span::styled(self.profile.clone(), accent_style),
                Span::raw("    "),
                Span::raw(format!("{}=", EXPERIMENTAL_ENV)),
                gate,
            ]),
        ];
        let block = Block::default().borders(Borders::BOTTOM);
        frame.render_widget(Paragraph::new(header).block(block), area);
    }

    fn render_body(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);
        self.render_ranked_table(frame, cols[0], theme);
        self.render_help_pane(frame, cols[1], theme);
    }

    fn render_ranked_table(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
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

    fn render_help_pane(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let title_style = Style::default().fg(theme.title).bold();
        let cmd_style = Style::default().fg(theme.accent);
        let hint = vec![
            Line::from(Span::styled("What the conductor does", title_style)),
            Line::from(""),
            Line::from("Ranks every active session by an attention score"),
            Line::from("derived from status, unread, favorite, and staleness."),
            Line::from("Archived, trashed, and actively snoozed sessions are"),
            Line::from("excluded from the queue."),
            Line::from(""),
            Line::from(Span::styled("From the CLI", title_style)),
            Line::from(vec![
                Span::styled("  aoe conductor status", cmd_style),
                Span::raw("  (this same table)"),
            ]),
            Line::from(vec![
                Span::styled("  aoe conductor watch --once", cmd_style),
                Span::raw("  (one reasoner tick)"),
            ]),
            Line::from(vec![
                Span::styled("  aoe conductor watch --live", cmd_style),
                Span::raw("  (loop + apply)"),
            ]),
            Line::from(vec![Span::styled(
                "  aoe conductor spawn --repo o/r",
                cmd_style,
            )]),
            Line::from(vec![
                Span::raw("      "),
                Span::raw("(one session / issue)"),
            ]),
            Line::from(""),
            Line::from(Span::styled("This panel", title_style)),
            Line::from("Display-only in this release. Interactive tick"),
            Line::from("and apply land in a follow-up. See issue #553."),
        ];
        let block = Block::default().borders(Borders::ALL).title(" How to use ");
        frame.render_widget(
            Paragraph::new(hint).block(block).wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let key_style = Style::default().fg(theme.help_key).bold();
        let refreshed = self
            .last_refreshed
            .map(|t| format!("refreshed {}s ago", t.elapsed().as_secs()))
            .unwrap_or_else(|| "not yet refreshed".to_string());
        let footer = Line::from(vec![
            Span::styled("Esc/q ", key_style),
            Span::raw("close    "),
            Span::styled("r ", key_style),
            Span::raw("refresh now    "),
            Span::raw(refreshed),
            Span::raw("    "),
            Span::raw(format!("at {}", Utc::now().format("%H:%M:%S"))),
        ]);
        frame.render_widget(Paragraph::new(footer), area);
    }
}

fn load_ranked(profile: &str) -> anyhow::Result<Vec<RankedRow>> {
    let storage = Storage::new_unwatched(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;
    crate::tmux::refresh_session_cache();
    for inst in &mut instances {
        inst.update_status();
    }
    let mut scored: Vec<(i64, Instance)> = instances
        .into_iter()
        .filter_map(|i| attention_score(&i).map(|s| (s, i)))
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    Ok(scored
        .into_iter()
        .map(|(score, inst)| RankedRow {
            title: inst.title,
            status: format!("{:?}", inst.status),
            score,
            favorited: inst.favorited_at.is_some(),
            unread: inst.unread,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn esc_closes() {
        let mut v = ConductorView::new("test-profile");
        let action = v.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, ConductorAction::Close));
    }

    #[test]
    fn q_closes() {
        let mut v = ConductorView::new("test-profile");
        let action = v.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(matches!(action, ConductorAction::Close));
    }

    #[test]
    fn r_refreshes_and_continues() {
        let mut v = ConductorView::new("test-profile");
        let before = v.last_refreshed;
        std::thread::sleep(std::time::Duration::from_millis(5));
        let action = v.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(action, ConductorAction::Continue));
        assert!(v.last_refreshed > before);
    }

    #[test]
    fn other_keys_continue() {
        let mut v = ConductorView::new("test-profile");
        let action = v.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(action, ConductorAction::Continue));
    }
}
