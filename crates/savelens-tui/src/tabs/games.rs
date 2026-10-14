//! Tab 1: tracked games, discovered games, watcher status and activity log.

use anyhow::{bail, Result};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;
use savelens_discovery::{DiscoveredGame, DiscoveryReport, GameRuntime};

use crate::app::{App, GameRow, Mode, PickItem, PickPurpose, Picker};
use crate::jobs::{JobKind, JobOutput};
use crate::theme;
use crate::widgets::{self, Level};

fn runtime_label(runtime: &str) -> String {
    match runtime.split_once(':') {
        Some((kind, _)) => kind.to_string(),
        None => runtime.to_string(),
    }
}

fn discovered_runtime(r: &GameRuntime) -> &'static str {
    match r {
        GameRuntime::Native => "native",
        GameRuntime::Proton { .. } => "proton",
        GameRuntime::Unknown => "unknown",
    }
}

impl App {
    /// The tracked game under the cursor, if the cursor is on a tracked game.
    fn highlighted_tracked(&self) -> Option<&GameRow> {
        self.games.get(self.games_sel)
    }

    /// Game targeted by game actions: the highlighted tracked game, or the selected one.
    fn action_game(&self) -> Result<savelens_cas::GameRecord> {
        match self.highlighted_tracked() {
            Some(g) => Ok(g.rec.clone()),
            None => self.require_game(),
        }
    }

    pub(crate) fn key_games(&mut self, key: KeyEvent) -> Result<()> {
        let total = self.games.len() + self.untracked().len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.games_sel = widgets::step(self.games_sel, total, 1),
            KeyCode::Char('k') | KeyCode::Up => self.games_sel = widgets::step(self.games_sel, total, -1),
            KeyCode::Char('g') | KeyCode::Home => self.games_sel = 0,
            KeyCode::Char('G') | KeyCode::End => self.games_sel = total.saturating_sub(1),
            KeyCode::Enter => self.games_enter()?,
            KeyCode::Char('d') => self.start_discovery(),
            KeyCode::Char('s') => {
                let g = self.action_game()?;
                self.select_game(g.id);
                self.snapshot("manual snapshot (TUI)")?;
            }
            KeyCode::Char('t') => {
                let g = self.action_game()?;
                let on = !g.trainer_enabled;
                self.engine.store().set_trainer_enabled(g.id, on)?;
                self.engine.store().log_event(
                    Some(g.id),
                    "trainer",
                    &format!("disk trainer {}", if on { "enabled" } else { "disabled" }),
                )?;
                self.toast(
                    Level::Info,
                    format!("{}: disk trainer {}", g.name, if on { "ON (freeze-on-save active)" } else { "off" }),
                );
                self.refresh();
            }
            KeyCode::Char('W') => {
                let g = self.action_game()?;
                let on = !g.watch_enabled;
                self.engine.store().set_watch_enabled(g.id, on)?;
                self.toast(Level::Info, format!("{}: watching {}", g.name, if on { "enabled" } else { "disabled" }));
                self.refresh();
            }
            _ => {}
        }
        Ok(())
    }

    fn games_enter(&mut self) -> Result<()> {
        if let Some(g) = self.highlighted_tracked() {
            let (id, name) = (g.rec.id, g.rec.name.clone());
            self.select_game(id);
            self.toast(Level::Info, format!("Selected {name}"));
            return Ok(());
        }
        let idx = self.games_sel.saturating_sub(self.games.len());
        let Some(game) = self.untracked().get(idx).map(|g| (*g).clone()) else {
            return Ok(());
        };
        match game.save_locations.len() {
            0 => bail!("no save location known for {} (track it manually with :track)", game.name),
            1 => self.track_discovered(&game, 0),
            _ => {
                let items = game
                    .save_locations
                    .iter()
                    .enumerate()
                    .map(|(i, l)| PickItem {
                        label: l.root.display().to_string(),
                        detail: format!("{} file(s) · {}", l.files.len(), l.patterns.join(" ")),
                        value: i.to_string(),
                    })
                    .collect();
                self.mode = Mode::Picker(Picker {
                    title: format!("Save location of {}", game.name),
                    items,
                    sel: 0,
                    purpose: PickPurpose::TrackLocation { game: Box::new(game) },
                });
                Ok(())
            }
        }
    }

    /// Tracks a discovered game.
    pub(crate) fn track_discovered(&mut self, game: &DiscoveredGame, location: usize) -> Result<()> {
        let (rec, commit) = self.engine.track_discovered(game, location)?;
        let what = commit.map(|c| c.summary).unwrap_or_else(|| "no files yet".into());
        self.toast(Level::Success, format!("Tracking {} ({what})", rec.name));
        self.refresh();
        self.select_game(rec.id);
        if let Some(i) = self.games.iter().position(|g| g.rec.id == rec.id) {
            self.games_sel = i;
        }
        Ok(())
    }

    /// Starts discovery on a worker thread.
    pub(crate) fn start_discovery(&mut self) {
        if self.jobs.is_running(JobKind::Discovery) {
            self.toast(Level::Info, "Discovery is already running");
            return;
        }
        let engine = self.engine.clone();
        let cfg = self.discovery_cfg.clone();
        self.jobs.spawn(JobKind::Discovery, "Discovering games", move || {
            JobOutput::Discovery(engine.discover(&cfg).map_err(|e| e.to_string()))
        });
    }

    pub(crate) fn on_discovery(&mut self, res: Result<DiscoveryReport, String>) {
        match res {
            Ok(report) => {
                let found = report.games.len();
                self.discovered = report.games;
                self.discovery_ran = true;
                let untracked = self.untracked().len();
                self.toast(
                    Level::Success,
                    format!(
                        "Discovery: {found} game(s) in {} Steam librar{}, {untracked} not tracked yet",
                        report.libraries.len(),
                        if report.libraries.len() == 1 { "y" } else { "ies" }
                    ),
                );
                if !report.warnings.is_empty() {
                    self.toast(
                        Level::Warning,
                        format!("Discovery: {} warning(s), first: {}", report.warnings.len(), report.warnings[0]),
                    );
                }
            }
            Err(e) => self.toast(Level::Error, format!("Discovery failed: {e}")),
        }
    }

    pub(crate) fn draw_games(&mut self, f: &mut Frame, area: Rect) {
        let [left, right] = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(area);
        let untracked_len = self.untracked().len() as u16;
        let disc_h = untracked_len.saturating_add(3).clamp(5, 12);
        let [games_area, disc_area] = Layout::vertical([Constraint::Min(5), Constraint::Length(disc_h)]).areas(left);
        self.draw_tracked(f, games_area);
        self.draw_discovered(f, disc_area);
        let [watch_area, store_area, log_area] =
            Layout::vertical([Constraint::Length(7), Constraint::Length(8), Constraint::Min(3)]).areas(right);
        self.draw_watcher(f, watch_area);
        self.draw_storage(f, store_area);
        self.draw_activity(f, log_area);
    }

    fn draw_tracked(&self, f: &mut Frame, area: Rect) {
        let focused = self.games_sel < self.games.len();
        let header =
            Row::new(["", "Game", "App ID", "Runtime", "Branch", "Files", "W T", "History"]).style(theme::header_row());
        let rows: Vec<Row> = self
            .games
            .iter()
            .map(|g| {
                let current = Some(g.rec.id) == self.game;
                let flags = Line::from(vec![
                    Span::styled(
                        if g.rec.watch_enabled { "W" } else { "·" },
                        Style::default().fg(if g.rec.watch_enabled { theme::GREEN } else { theme::MUTED }),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        if g.rec.trainer_enabled { "T" } else { "·" },
                        Style::default().fg(if g.rec.trainer_enabled { theme::PINK } else { theme::MUTED }),
                    ),
                ]);
                Row::new(vec![
                    Cell::from(Span::styled(if current { "▶" } else { " " }, theme::bold_accent())),
                    Cell::from(Span::styled(
                        g.rec.name.clone(),
                        if current { theme::bold_accent() } else { Style::default().fg(theme::TEXT) },
                    )),
                    Cell::from(g.rec.app_id.map(|a| a.to_string()).unwrap_or_else(|| "—".into())),
                    Cell::from(Span::styled(runtime_label(&g.rec.runtime), theme::subtle())),
                    Cell::from(Span::styled(
                        format!("⎇ {}", g.rec.current_branch),
                        Style::default().fg(theme::ACCENT2),
                    )),
                    Cell::from(g.files.to_string()),
                    Cell::from(flags),
                    Cell::from(Span::styled(widgets::bytes(g.logical), theme::subtle())),
                ])
            })
            .collect();
        let title = format!("Tracked games ({})", self.games.len());
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Min(14),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(12),
                Constraint::Length(5),
                Constraint::Length(4),
                Constraint::Length(10),
            ],
        )
        .header(header)
        .block(theme::block(&title, focused))
        .row_highlight_style(if focused { theme::selected() } else { theme::selected_dim() })
        .style(theme::panel());
        if self.games.is_empty() {
            let p = Paragraph::new(vec![
                Line::from(Span::styled("No games tracked yet.", theme::subtle())),
                Line::from(""),
                Line::from(vec![
                    Span::styled("d", theme::bold_accent()),
                    Span::styled(" discover installed games   ", theme::muted()),
                    Span::styled(":track <name> <dir> [pattern…]", theme::bold_accent()),
                    Span::styled(" track a folder", theme::muted()),
                ]),
            ])
            .wrap(Wrap { trim: true })
            .block(theme::block(&title, focused));
            f.render_widget(p, area);
            return;
        }
        let mut state = TableState::default().with_selected(focused.then_some(self.games_sel));
        f.render_stateful_widget(table, area, &mut state);
    }

    fn draw_discovered(&self, f: &mut Frame, area: Rect) {
        let untracked = self.untracked();
        let focused = self.games_sel >= self.games.len() && !untracked.is_empty();
        let running = self.jobs.is_running(JobKind::Discovery);
        let title = if running {
            format!("Discovered {} scanning…", widgets::spinner(self.frame))
        } else {
            format!("Discovered, not tracked ({})", untracked.len())
        };
        if untracked.is_empty() {
            let msg = if running {
                "Scanning Steam libraries, Proton prefixes and save-path rules…"
            } else if self.discovery_ran {
                "Every discovered game is already tracked."
            } else {
                "Press d to discover installed games (Steam, Proton, save-path rules)."
            };
            f.render_widget(
                Paragraph::new(Span::styled(msg, theme::muted()))
                    .wrap(Wrap { trim: true })
                    .block(theme::block(&title, false)),
                area,
            );
            return;
        }
        let items: Vec<ListItem> = untracked
            .iter()
            .map(|g| {
                let engine = g.engine.map(|e| format!("{e:?}")).unwrap_or_default();
                ListItem::new(Line::from(vec![
                    Span::styled("＋ ", Style::default().fg(theme::GREEN)),
                    Span::styled(g.name.clone(), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
                    Span::styled(g.app_id.map(|a| format!("  ({a})")).unwrap_or_default(), theme::muted()),
                    Span::styled(format!("  {}", discovered_runtime(&g.runtime)), theme::subtle()),
                    Span::styled(format!("  {} location(s)", g.save_locations.len()), theme::subtle()),
                    Span::styled(format!("  {engine}"), Style::default().fg(theme::ACCENT2)),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(theme::block(&title, focused))
            .highlight_style(theme::selected())
            .highlight_symbol("▶ ");
        let sel = focused.then(|| self.games_sel - self.games.len());
        let mut state = ListState::default().with_selected(sel);
        f.render_stateful_widget(list, area, &mut state);
    }

    fn draw_watcher(&self, f: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        if let Some(since) = self.daemon_since {
            lines.push(widgets::kv(
                "Status",
                format!("● running (embedded, up {}s)", since.elapsed().as_secs()),
                theme::GREEN,
            ));
        } else if let Some(st) = &self.daemon_status {
            lines.push(widgets::kv("Status", format!("● running (pid {})", st.pid), theme::TEAL));
        } else {
            lines.push(widgets::kv("Status", "○ stopped (press w to start)", theme::MUTED));
        }
        match &self.daemon_status {
            Some(st) => {
                lines.push(widgets::kv("Heartbeat", widgets::age(st.heartbeat), theme::SUBTEXT));
                lines.push(widgets::kv("Info", st.info.clone(), theme::SUBTEXT));
            }
            None => lines.push(widgets::kv("Heartbeat", "none", theme::MUTED)),
        }
        let cfg = self.engine.config();
        lines.push(widgets::kv(
            "Commit",
            format!("{} · debounce {} ms", if cfg.auto_commit { "auto" } else { "manual only" }, cfg.debounce_ms),
            theme::SUBTEXT,
        ));
        if !self.daemon_hotkeys.is_empty() {
            lines.push(widgets::kv("Hotkeys", self.daemon_hotkeys.join(" · "), theme::YELLOW));
        }
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(theme::block("Watcher daemon", false)),
            area,
        );
    }

    fn draw_storage(&self, f: &mut Frame, area: Rect) {
        let s = &self.stats;
        let lines = vec![
            widgets::kv("Logical", widgets::bytes(s.logical_bytes), theme::TEXT),
            widgets::kv("On disk", format!("{} (dedup + zstd)", widgets::bytes(s.physical_bytes)), theme::GREEN),
            widgets::kv("Ratio", format!("{:.1}×", s.dedup_ratio()), theme::ACCENT),
            widgets::kv("Objects", format!("{} blobs · {} chunks", s.blobs, s.chunks), theme::SUBTEXT),
            widgets::kv(
                "History",
                format!("{} commits · {} safety backups", s.commits, s.safety_backups),
                theme::SUBTEXT,
            ),
            widgets::kv("Store", self.engine.store().root().display().to_string(), theme::MUTED),
        ];
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(theme::block("Storage", false)), area);
    }

    fn draw_activity(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .events
            .iter()
            .map(|e| {
                let color = match e.kind.as_str() {
                    "commit" => theme::TEAL,
                    "mutation" => theme::PEACH,
                    "panic-restore" => theme::RED,
                    "trainer" => theme::PINK,
                    "daemon" => theme::GREEN,
                    "branch" | "track" => theme::ACCENT2,
                    _ => theme::SUBTEXT,
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", widgets::short_time(e.at)), theme::muted()),
                    Span::styled(format!("{:<8} ", e.kind), Style::default().fg(color)),
                    Span::styled(e.message.clone(), Style::default().fg(theme::TEXT)),
                ]))
            })
            .collect();
        let list = List::new(items).block(theme::block("Recent activity", false));
        f.render_widget(list, area);
    }
}
