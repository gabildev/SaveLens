//! Tab 2: semantic log, per-file changelogs and branches.

use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, List, ListItem, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;
use savelens_cas::{CommitId, CommitRecord};
use savelens_watcher::{FileStatus, WORKING};

use crate::app::{Action, App, InputPurpose, Mode, PickItem, PickPurpose, Picker};
use crate::theme;
use crate::widgets::{self, Level};

/// Colour of a changelog line (`(+300)` green, `(-5)` red, …).
pub(crate) fn change_color(line: &str) -> Color {
    if line.starts_with("+ ") || line.contains("(+") {
        theme::GREEN
    } else if line.starts_with("- ") || line.contains("(-") {
        theme::RED
    } else if line.contains("->") {
        theme::YELLOW
    } else {
        theme::TEXT
    }
}

impl App {
    fn highlighted_commit(&self) -> Result<CommitRecord> {
        self.timeline.commits.get(self.timeline.sel).cloned().ok_or_else(|| anyhow!("no commit highlighted"))
    }

    pub(crate) fn key_timeline(&mut self, key: KeyEvent) -> Result<()> {
        let len = self.timeline.commits.len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.timeline.sel = widgets::step(self.timeline.sel, len, 1),
            KeyCode::Char('k') | KeyCode::Up => self.timeline.sel = widgets::step(self.timeline.sel, len, -1),
            KeyCode::PageDown => self.timeline.sel = widgets::step(self.timeline.sel, len, 10),
            KeyCode::PageUp => self.timeline.sel = widgets::step(self.timeline.sel, len, -10),
            KeyCode::Char('g') | KeyCode::Home => self.timeline.sel = 0,
            KeyCode::Char('G') | KeyCode::End => self.timeline.sel = len.saturating_sub(1),
            KeyCode::Char('c') => {
                let c = self.highlighted_commit()?;
                let lines = vec![
                    Line::from(vec![
                        Span::styled(format!("#{} ", c.id), theme::bold_accent()),
                        widgets::badge(c.kind.as_str(), theme::kind_color(c.kind)),
                        Span::raw(" "),
                        Span::styled(c.summary.clone(), Style::default().fg(theme::TEXT)),
                    ]),
                    Line::from(Span::styled(c.message.clone(), theme::muted())),
                    Line::from(""),
                    Line::from("Every tracked file that differs on disk is overwritten atomically."),
                    Line::from(Span::styled(
                        "A safety/pre-mutation backup is taken first (undo with P).",
                        Style::default().fg(theme::YELLOW),
                    )),
                ];
                self.confirm(
                    &format!("Checkout #{}?", c.id),
                    lines,
                    Action::Checkout { rev: format!("#{}", c.id) },
                    false,
                );
            }
            KeyCode::Char('b') => {
                let c = self.highlighted_commit()?;
                self.prompt(
                    InputPurpose::NewBranch { from: c.id },
                    &format!("New branch from #{}", c.id),
                    "",
                    "letters, digits, - _ . / (e.g. exp/no-death)",
                );
            }
            KeyCode::Char('B') => {
                let game = self.require_game()?;
                let items: Vec<PickItem> = self
                    .timeline
                    .branches
                    .iter()
                    .map(|b| PickItem {
                        label: format!("{} {}", if b.name == game.current_branch { "●" } else { "○" }, b.name),
                        detail: b.head.map(|h| format!("head #{h}")).unwrap_or_else(|| "empty".into()),
                        value: b.name.clone(),
                    })
                    .collect();
                let sel = self.timeline.branches.iter().position(|b| b.name == game.current_branch).unwrap_or(0);
                self.mode = Mode::Picker(Picker {
                    title: "Switch branch (files on disk follow the branch head)".into(),
                    items,
                    sel,
                    purpose: PickPurpose::SwitchBranch,
                });
            }
            KeyCode::Char('T') => {
                let c = self.highlighted_commit()?;
                self.prompt(
                    InputPurpose::Tag { commit: c.id },
                    &format!("Tag #{}", c.id),
                    "",
                    "tag name (safety/ is reserved)",
                );
            }
            KeyCode::Char('d') => self.timeline_diff()?,
            _ => {}
        }
        Ok(())
    }

    fn timeline_diff(&mut self) -> Result<()> {
        let game = self.require_game()?;
        let c = self.highlighted_commit()?;
        let diffs = self.engine.diff(game.id, &format!("#{}", c.id), WORKING, None)?;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut any = false;
        for d in &diffs {
            let (label, color) = match d.status {
                FileStatus::Added => ("added", theme::GREEN),
                FileStatus::Removed => ("removed", theme::RED),
                FileStatus::Modified => ("modified", theme::YELLOW),
                FileStatus::Unchanged => ("unchanged", theme::MUTED),
            };
            any |= d.status != FileStatus::Unchanged;
            lines.push(Line::from(vec![
                Span::styled(d.rel_path.clone(), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
                Span::raw(" "),
                widgets::badge(label, color),
                Span::raw(" "),
                Span::styled(d.summary.clone(), theme::subtle()),
            ]));
            for l in &d.lines {
                lines.push(Line::from(Span::styled(format!("  {l}"), Style::default().fg(change_color(l)))));
            }
            lines.push(Line::from(""));
        }
        if !any {
            lines.insert(0, Line::from(Span::styled(format!("WORKING is identical to #{}", c.id), theme::subtle())));
        }
        self.mode = Mode::Viewer { title: format!("Diff #{} → WORKING", c.id), lines, scroll: 0 };
        Ok(())
    }

    pub(crate) fn timeline_checkout(&mut self, rev: &str) -> Result<()> {
        let game = self.require_game()?;
        let rep = self.engine.checkout(game.id, rev, None)?;
        if rep.written.is_empty() {
            self.toast(Level::Info, format!("Checkout {rev}: files on disk already match #{}", rep.target));
        } else {
            self.toast(
                Level::Success,
                format!(
                    "Checked out #{}: {} file(s) written, backup {}",
                    rep.target,
                    rep.written.len(),
                    rep.safety_tags.last().map(String::as_str).unwrap_or("-")
                ),
            );
        }
        self.timeline.sel = 0;
        self.refresh();
        self.ast_reload_if_working();
        Ok(())
    }

    pub(crate) fn timeline_create_branch(&mut self, name: &str, from: CommitId) -> Result<()> {
        let game = self.require_game()?;
        let b = self.engine.create_branch(game.id, name.trim(), Some(&format!("#{from}")))?;
        self.toast(Level::Success, format!("Created branch {} at #{from} (B to switch)", b.name));
        self.refresh();
        Ok(())
    }

    pub(crate) fn timeline_switch_branch(&mut self, name: &str) -> Result<()> {
        let game = self.require_game()?;
        if game.current_branch == name {
            self.toast(Level::Info, format!("Already on {name}"));
            return Ok(());
        }
        let rep = self.engine.switch_branch(game.id, name)?;
        self.toast(Level::Success, format!("Switched to {name}: {} file(s) written", rep.written.len()));
        self.timeline.sel = 0;
        self.timeline.changelogs.clear();
        self.refresh();
        self.ast_reload_if_working();
        Ok(())
    }

    pub(crate) fn timeline_tag(&mut self, name: &str, commit: CommitId) -> Result<()> {
        let game = self.require_game()?;
        let t = self.engine.store().create_tag(game.id, name.trim(), commit)?;
        self.toast(Level::Success, format!("Tagged #{} as {}", t.commit_id, t.name));
        self.refresh();
        Ok(())
    }

    fn ensure_changelog(&mut self, id: CommitId) {
        if self.timeline.changelogs.contains_key(&id) {
            return;
        }
        match self.engine.store().changelogs(id) {
            Ok(c) => {
                self.timeline.changelogs.insert(id, c);
            }
            Err(e) => self.toast(Level::Error, format!("changelog of #{id}: {e}")),
        }
    }

    pub(crate) fn draw_timeline(&mut self, f: &mut Frame, area: Rect) {
        if self.game.is_none() {
            let p = Paragraph::new(Span::styled("Select or track a game on the Games tab (1).", theme::muted()))
                .block(theme::block("Timeline", true));
            f.render_widget(p, area);
            return;
        }
        let [left, right] = Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).areas(area);
        let branch_h = (self.timeline.branches.len() as u16).saturating_add(2).clamp(3, 9);
        let [log_area, branch_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(branch_h)]).areas(right);
        self.draw_commits(f, left);
        if let Some(id) = self.timeline.commits.get(self.timeline.sel).map(|c| c.id) {
            self.ensure_changelog(id);
        }
        self.draw_changelog(f, log_area);
        self.draw_branches(f, branch_area);
    }

    fn draw_commits(&self, f: &mut Frame, area: Rect) {
        let branch = self.game_rec().map(|g| g.current_branch.clone()).unwrap_or_default();
        let header = Row::new(["Time", "#", "Kind", "Summary"]).style(theme::header_row());
        let rows: Vec<Row> = self
            .timeline
            .commits
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut summary = Vec::new();
                if i == 0 {
                    summary.push(Span::styled("HEAD ", Style::default().fg(theme::BG).bg(theme::ACCENT2)));
                    summary.push(Span::raw(" "));
                }
                for t in self.timeline.tags.iter().filter(|t| t.commit_id == c.id) {
                    summary.push(Span::styled(format!("⚑ {} ", t.name), Style::default().fg(theme::YELLOW)));
                }
                summary.push(Span::styled(c.summary.clone(), Style::default().fg(theme::TEXT)));
                Row::new(vec![
                    Cell::from(Span::styled(widgets::short_time(c.created_at), theme::muted())),
                    Cell::from(Span::styled(c.id.to_string(), theme::subtle())),
                    Cell::from(widgets::badge(c.kind.as_str(), theme::kind_color(c.kind))),
                    Cell::from(Line::from(summary)),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [Constraint::Length(11), Constraint::Length(5), Constraint::Length(13), Constraint::Min(10)],
        )
        .header(header)
        .block(theme::block(&format!("Semantic log ⎇ {branch} ({} commits)", self.timeline.commits.len()), true))
        .row_highlight_style(theme::selected())
        .highlight_symbol("▶");
        let mut state =
            TableState::default().with_selected((!self.timeline.commits.is_empty()).then_some(self.timeline.sel));
        f.render_stateful_widget(table, area, &mut state);
    }

    fn draw_changelog(&self, f: &mut Frame, area: Rect) {
        let Some(c) = self.timeline.commits.get(self.timeline.sel) else {
            f.render_widget(
                Paragraph::new(Span::styled("No commits yet: press s on the Games tab to snapshot.", theme::muted()))
                    .block(theme::block("Changes", false)),
                area,
            );
            return;
        };
        let mut lines: Vec<Line> = vec![
            Line::from(vec![
                widgets::badge(c.kind.as_str(), theme::kind_color(c.kind)),
                Span::raw(" "),
                Span::styled(savelens_cas::format_ts(c.created_at), theme::subtle()),
                Span::styled(
                    c.parent.map(|p| format!("  parent #{p}")).unwrap_or_else(|| "  root".into()),
                    theme::muted(),
                ),
            ]),
            Line::from(Span::styled(c.message.clone(), theme::muted())),
            Line::from(""),
        ];
        match self.timeline.changelogs.get(&c.id) {
            Some(logs) if !logs.is_empty() => {
                for log in logs {
                    lines.push(Line::from(vec![
                        Span::styled("▸ ", theme::accent()),
                        Span::styled(
                            log.rel_path.clone(),
                            Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("  {}", log.summary), theme::subtle()),
                    ]));
                    for l in &log.lines {
                        lines.push(Line::from(Span::styled(format!("   {l}"), Style::default().fg(change_color(l)))));
                    }
                }
            }
            _ => lines.push(Line::from(Span::styled("no per-file changelog recorded", theme::muted()))),
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(theme::block(&format!("Changes in #{}", c.id), false)),
            area,
        );
    }

    fn draw_branches(&self, f: &mut Frame, area: Rect) {
        let current = self.game_rec().map(|g| g.current_branch.clone()).unwrap_or_default();
        let items: Vec<ListItem> = self
            .timeline
            .branches
            .iter()
            .map(|b| {
                let is_cur = b.name == current;
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if is_cur { "● " } else { "○ " },
                        Style::default().fg(if is_cur { theme::GREEN } else { theme::MUTED }),
                    ),
                    Span::styled(
                        b.name.clone(),
                        if is_cur {
                            Style::default().fg(theme::ACCENT2).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme::TEXT)
                        },
                    ),
                    Span::styled(
                        b.head.map(|h| format!("  #{h}")).unwrap_or_else(|| "  (empty)".into()),
                        theme::muted(),
                    ),
                    Span::styled(if is_cur { "  current" } else { "" }, Style::default().fg(theme::GREEN)),
                ]))
            })
            .collect();
        f.render_widget(List::new(items).block(theme::block("Branches (B switch · b new)", false)), area);
    }
}
