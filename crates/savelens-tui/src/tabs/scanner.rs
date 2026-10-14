//! Tab 3: Save-State Scanner ("Cheat Engine without touching RAM") and the
//! cheat table.

use anyhow::{anyhow, bail, Context, Result};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;
use savelens_core::{Alignment as ScanAlign, CandidateView, Location, Node, ScalarValue, ScanFilter, ScanSession};
use savelens_watcher::EditTarget;

use crate::app::{
    align_label, Action, App, EndianChoice, InputPurpose, Mode, PickPurpose, Picker, ScanPane, SCAN_TYPES,
};
use crate::jobs::{JobKind, JobOutput};
use crate::theme;
use crate::widgets::{self, Level};

/// Number of entries of the options dialog.
const OPTION_COUNT: usize = SCAN_TYPES.len() + 6;

impl App {
    fn scan_page(&self) -> usize {
        self.scanner_page.max(1)
    }

    /// The candidate under the cursor.
    fn selected_candidate(&self) -> Option<CandidateView> {
        self.scanner.session.candidates(self.scanner.cand_sel, 1).into_iter().next()
    }

    /// Alias of a pinned location of the current file.
    fn alias_of(&self, location: &Location) -> Option<String> {
        let file = self.file.as_ref()?;
        self.scanner
            .cheats
            .iter()
            .find(|c| &c.entry.rel_path == file && &c.entry.location == location)
            .map(|c| c.entry.alias.clone())
    }

    pub(crate) fn key_scanner(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('v') => {
                self.scanner.pane = match self.scanner.pane {
                    ScanPane::Candidates => ScanPane::Cheats,
                    ScanPane::Cheats => ScanPane::Candidates,
                };
                return Ok(());
            }
            KeyCode::Char('i') | KeyCode::Char('/') => {
                let current = self.scanner.filter.clone();
                self.prompt(
                    InputPurpose::ScanFilter,
                    if self.scanner.session.is_fresh() { "First scan filter" } else { "Next scan filter" },
                    &current,
                    "1500 · 1500 -> 1300 · +300 · - 300 · 10..20 · inc · dec · changed · unchanged · ? (unknown)",
                );
                return Ok(());
            }
            KeyCode::Char('n') => return self.scanner_run(),
            KeyCode::Char('R') => {
                self.scanner.reset();
                self.toast(Level::Info, "Scan reset: the next scan is a first scan");
                return Ok(());
            }
            KeyCode::Char('f') => return self.pick_file(),
            KeyCode::Char('s') => {
                self.require_game()?;
                let items = self.revision_items();
                let sel = items.iter().position(|i| i.value == self.scanner.source).unwrap_or(0);
                self.mode = Mode::Picker(Picker {
                    title: "Scan snapshot source".into(),
                    items,
                    sel,
                    purpose: PickPurpose::ScanSource,
                });
                return Ok(());
            }
            KeyCode::Char('o') => {
                self.mode = Mode::ScanOptions { sel: 0 };
                return Ok(());
            }
            _ => {}
        }
        match self.scanner.pane {
            ScanPane::Candidates => self.key_candidates(key),
            ScanPane::Cheats => self.key_cheats(key),
        }
    }

    fn key_candidates(&mut self, key: KeyEvent) -> Result<()> {
        let len = self.scanner.session.explicit_count();
        let page = self.scan_page() as isize;
        let sel = self.scanner.cand_sel;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.scanner.cand_sel = widgets::step(sel, len, 1),
            KeyCode::Char('k') | KeyCode::Up => self.scanner.cand_sel = widgets::step(sel, len, -1),
            KeyCode::PageDown | KeyCode::Char(']') => self.scanner.cand_sel = widgets::step(sel, len, page),
            KeyCode::PageUp | KeyCode::Char('[') => self.scanner.cand_sel = widgets::step(sel, len, -page),
            KeyCode::Char('g') | KeyCode::Home => self.scanner.cand_sel = 0,
            KeyCode::Char('G') | KeyCode::End => self.scanner.cand_sel = len.saturating_sub(1),
            KeyCode::Enter => return self.scanner_run(),
            KeyCode::Char('a') => {
                let c = self.selected_candidate().ok_or_else(|| anyhow!("no candidate selected"))?;
                let initial = self.alias_of(&c.location).unwrap_or_default();
                self.prompt(
                    InputPurpose::PinAlias { location: c.location.clone(), freeze: false },
                    &format!("Pin {} as", c.location),
                    &initial,
                    "alias shown in changelogs and the cheat table (e.g. Gold)",
                );
            }
            KeyCode::Char('e') => {
                let c = self.selected_candidate().ok_or_else(|| anyhow!("no candidate selected"))?;
                self.prompt(
                    InputPurpose::EditCandidate { location: c.location.clone() },
                    &format!("Write {} on disk", c.location),
                    &c.current.to_string(),
                    "new value (the save is re-encoded, re-signed and backed up first)",
                );
            }
            KeyCode::Char('F') => {
                let c = self.selected_candidate().ok_or_else(|| anyhow!("no candidate selected"))?;
                match self.alias_of(&c.location) {
                    Some(alias) => self.scanner_toggle_freeze(&alias)?,
                    None => self.prompt(
                        InputPurpose::PinAlias { location: c.location.clone(), freeze: true },
                        &format!("Pin & freeze {} as", c.location),
                        "",
                        "alias; the current value is re-applied after every game save while the trainer is on",
                    ),
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn key_cheats(&mut self, key: KeyEvent) -> Result<()> {
        let len = self.scanner.cheats.len();
        let sel = self.scanner.cheat_sel;
        let current = self.scanner.cheats.get(sel).cloned();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.scanner.cheat_sel = widgets::step(sel, len, 1),
            KeyCode::Char('k') | KeyCode::Up => self.scanner.cheat_sel = widgets::step(sel, len, -1),
            KeyCode::Char('g') | KeyCode::Home => self.scanner.cheat_sel = 0,
            KeyCode::Char('G') | KeyCode::End => self.scanner.cheat_sel = len.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('e') => {
                let c = current.ok_or_else(|| anyhow!("the cheat table is empty"))?;
                let initial = c.current.map(|v| v.to_string()).unwrap_or_default();
                self.prompt(
                    InputPurpose::EditCheat { alias: c.entry.alias.clone() },
                    &format!("Set {} on disk", c.entry.alias),
                    &initial,
                    "new value (Apply & Sign Save)",
                );
            }
            KeyCode::Char('F') => {
                let c = current.ok_or_else(|| anyhow!("the cheat table is empty"))?;
                self.scanner_toggle_freeze(&c.entry.alias)?;
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                let c = current.ok_or_else(|| anyhow!("the cheat table is empty"))?;
                self.confirm(
                    &format!("Delete {}?", c.entry.alias),
                    vec![
                        Line::from(format!("Remove `{}` ({}) from the cheat table.", c.entry.alias, c.entry.location)),
                        Line::from(Span::styled("The save file itself is not modified.", theme::muted())),
                    ],
                    Action::DeleteCheat { alias: c.entry.alias.clone() },
                    false,
                );
            }
            _ => {}
        }
        Ok(())
    }

    /// Runs a first scan (fresh session) or a next scan on a worker thread.
    pub(crate) fn scanner_run(&mut self) -> Result<()> {
        if self.jobs.is_running(JobKind::Scan) {
            bail!("a scan is already running");
        }
        let game = self.require_game()?;
        let rel = self.require_file()?;
        if self.scanner.filter.trim().is_empty() {
            bail!("type a filter first (press i): e.g. 1500, 1500 -> 1300, +300, inc, ?");
        }
        let filter =
            ScanFilter::parse(&self.scanner.filter).with_context(|| format!("bad filter `{}`", self.scanner.filter))?;
        let scope = Some((game.id, rel.clone()));
        if self.scanner.scope.is_some() && self.scanner.scope != scope {
            self.scanner.reset();
        }
        let first = self.scanner.session.is_fresh();
        if first && filter.is_relational() {
            bail!("`{filter}` compares with a previous scan: start with a value, a range or ? (unknown)");
        }
        if !first && matches!(filter, ScanFilter::Unknown) {
            bail!("`?` is only valid for a first scan (press R to reset)");
        }
        self.scanner.scope = scope;
        self.scanner.last_error = None;
        let session = std::mem::replace(&mut self.scanner.session, ScanSession::new(self.scanner.config.clone()));
        let engine = self.engine.clone();
        let source = self.scanner.source.clone();
        let raw = self.scanner.raw_payload;
        let label =
            format!("{} scan `{}` on {source}", if first { "First" } else { "Next" }, self.scanner.filter.trim());
        let job_label = label.clone();
        self.jobs.spawn(JobKind::Scan, label, move || {
            let mut session = session;
            let root = match engine.open_version(game.id, &source, &rel) {
                Ok(o) if raw => Ok(o.into_raw_mode().root()),
                Ok(o) => Ok(o.root()),
                Err(e) => Err(e.to_string()),
            };
            match root {
                Ok(root) => {
                    let res = if first {
                        session.first_scan(&root, &filter, &source)
                    } else {
                        session.next_scan(&root, &filter, &source)
                    };
                    JobOutput::Scan {
                        session: Box::new(session),
                        snapshot: Some(Box::new(root)),
                        result: res.map_err(|e| e.to_string()),
                        label: job_label,
                    }
                }
                Err(e) => {
                    JobOutput::Scan { session: Box::new(session), snapshot: None, result: Err(e), label: job_label }
                }
            }
        });
        Ok(())
    }

    pub(crate) fn on_scan_done(
        &mut self,
        session: ScanSession,
        snapshot: Option<Node>,
        result: Result<usize, String>,
        label: String,
    ) {
        self.scanner.session = session;
        if snapshot.is_some() {
            self.scanner.snapshot = snapshot;
        }
        self.scanner.cand_sel = 0;
        match result {
            Ok(n) => {
                let level = if n == 0 { Level::Warning } else { Level::Success };
                self.toast(level, format!("{label}: {n} candidate(s)"));
            }
            Err(e) => {
                self.scanner.last_error = Some(e.clone());
                self.toast(Level::Error, format!("{label} failed: {e}"));
            }
        }
    }

    pub(crate) fn scanner_pin(&mut self, alias: &str, location: Location, freeze: bool) -> Result<()> {
        let alias = alias.trim();
        if alias.is_empty() {
            bail!("the alias cannot be empty");
        }
        let game = self.require_game()?;
        let rel = self.require_file()?;
        let entry = self.engine.pin(game.id, &rel, alias, &location)?;
        self.toast(Level::Success, format!("Pinned {} as {}", entry.location, entry.alias));
        if freeze {
            self.engine.freeze(game.id, alias, None, true)?;
            self.toast(Level::Success, freeze_message(alias, true, game.trainer_enabled));
        }
        self.refresh_cheats();
        Ok(())
    }

    fn scanner_toggle_freeze(&mut self, alias: &str) -> Result<()> {
        let game = self.require_game()?;
        let enabled = self
            .scanner
            .cheats
            .iter()
            .find(|c| c.entry.alias == alias)
            .map(|c| c.entry.freeze_enabled)
            .ok_or_else(|| anyhow!("`{alias}` is not in the cheat table"))?;
        self.engine.freeze(game.id, alias, None, !enabled)?;
        self.toast(Level::Success, freeze_message(alias, !enabled, game.trainer_enabled));
        self.refresh_cheats();
        Ok(())
    }

    pub(crate) fn scanner_edit_location(&mut self, value: &str, location: Location) -> Result<()> {
        let game = self.require_game()?;
        let rel = self.require_file()?;
        let value = ScalarValue::parse(value.trim()).with_context(|| format!("`{}` is not a number", value.trim()))?;
        let allow_raw = matches!(location, Location::Raw { .. });
        let rep = self.engine.edit_value(game.id, &rel, &EditTarget::Location { location, value }, allow_raw)?;
        self.report_edit(&rep);
        Ok(())
    }

    pub(crate) fn scanner_edit_cheat(&mut self, alias: &str, value: &str) -> Result<()> {
        let game = self.require_game()?;
        let value = ScalarValue::parse(value.trim()).with_context(|| format!("`{}` is not a number", value.trim()))?;
        let rep = self.engine.set_cheat(game.id, alias, value)?;
        self.report_edit(&rep);
        Ok(())
    }

    /// Toast + refresh after an edit report.
    pub(crate) fn report_edit(&mut self, rep: &savelens_watcher::EditReport) {
        if rep.no_op {
            self.toast(Level::Info, format!("{}: value already set, nothing written", rep.rel_path));
        } else {
            let what = if rep.changes.is_empty() { "written".to_string() } else { rep.changes.join("; ") };
            self.toast(
                Level::Success,
                format!(
                    "{}: {what} [{:?}] backup {}",
                    rep.rel_path,
                    rep.mode,
                    rep.safety_tag.as_deref().unwrap_or("-")
                ),
            );
        }
        self.refresh();
        self.ast_reload_if_working();
    }

    pub(crate) fn scanner_delete_cheat(&mut self, alias: &str) -> Result<()> {
        let game = self.require_game()?;
        self.engine.store().delete_cheat(game.id, alias)?;
        self.toast(Level::Info, format!("Removed {alias} from the cheat table"));
        self.refresh_cheats();
        Ok(())
    }

    pub(crate) fn key_scan_options(&mut self, sel: usize, key: KeyEvent) {
        let mut sel = sel;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => sel = widgets::step(sel, OPTION_COUNT, 1),
            KeyCode::Char('k') | KeyCode::Up => sel = widgets::step(sel, OPTION_COUNT, -1),
            KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Char('l') | KeyCode::Char('h') => {
                self.toggle_option(sel);
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('o') => return,
            _ => {}
        }
        self.mode = Mode::ScanOptions { sel };
    }

    fn toggle_option(&mut self, sel: usize) {
        let s = &mut self.scanner;
        let n = SCAN_TYPES.len();
        if sel < n {
            let ty = SCAN_TYPES[sel];
            if s.config.types.contains(&ty) {
                s.config.types.retain(|t| *t != ty);
            } else {
                s.config.types.push(ty);
                s.config.types.sort();
            }
        } else {
            match sel - n {
                0 => {
                    s.endian = match s.endian {
                        EndianChoice::Little => EndianChoice::Big,
                        EndianChoice::Big => EndianChoice::Both,
                        EndianChoice::Both => EndianChoice::Little,
                    };
                    s.apply_endian();
                }
                1 => {
                    s.config.alignment = match s.config.alignment {
                        ScanAlign::Unaligned => ScanAlign::Natural,
                        ScanAlign::Natural => ScanAlign::Fixed(4),
                        ScanAlign::Fixed(_) => ScanAlign::Unaligned,
                    }
                }
                2 => s.config.scan_ast = !s.config.scan_ast,
                3 => s.config.scan_numeric_strings = !s.config.scan_numeric_strings,
                4 => s.config.scan_raw = !s.config.scan_raw,
                _ => s.raw_payload = !s.raw_payload,
            }
        }
        let was_fresh = s.session.is_fresh();
        s.reset();
        if !was_fresh {
            self.toast(Level::Info, "Scan options changed: scan reset");
        }
    }

    fn option_rows(&self) -> Vec<(String, bool, String)> {
        let s = &self.scanner;
        let mut rows: Vec<(String, bool, String)> = SCAN_TYPES
            .iter()
            .map(|t| (format!("type {}", t.name()), s.config.types.contains(t), String::new()))
            .collect();
        let endian = match s.endian {
            EndianChoice::Little => "little endian",
            EndianChoice::Big => "big endian",
            EndianChoice::Both => "little + big endian",
        };
        rows.push(("byte order".into(), true, endian.into()));
        rows.push(("alignment".into(), true, align_label(s.config.alignment)));
        rows.push(("scan AST numeric leaves".into(), s.config.scan_ast, String::new()));
        rows.push(("scan numeric strings".into(), s.config.scan_numeric_strings, String::new()));
        rows.push(("scan bytes / raw regions".into(), s.config.scan_raw, String::new()));
        rows.push(("whole-file raw mode (payload bytes)".into(), s.raw_payload, String::new()));
        rows
    }

    pub(crate) fn draw_scan_options(&self, f: &mut Frame, area: Rect, sel: usize) {
        let rows = self.option_rows();
        let rect = widgets::centered(area, 60, rows.len() as u16 + 2);
        f.render_widget(Clear, rect);
        let items: Vec<ListItem> = rows
            .into_iter()
            .map(|(label, on, value)| {
                let mark = if value.is_empty() {
                    Span::styled(
                        if on { "[x] " } else { "[ ] " },
                        Style::default().fg(if on { theme::GREEN } else { theme::MUTED }),
                    )
                } else {
                    Span::styled(" ⇄  ", theme::accent())
                };
                ListItem::new(Line::from(vec![
                    mark,
                    Span::styled(label, Style::default().fg(theme::TEXT)),
                    Span::styled(
                        if value.is_empty() { String::new() } else { format!(": {value}") },
                        Style::default().fg(theme::YELLOW),
                    ),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(theme::modal("Scan options (Space toggles)", theme::ACCENT))
            .highlight_style(theme::selected())
            .highlight_symbol("▶ ");
        let mut state = ListState::default().with_selected(Some(sel));
        f.render_stateful_widget(list, rect, &mut state);
    }

    pub(crate) fn draw_scanner(&mut self, f: &mut Frame, area: Rect) {
        let cheat_h = (self.scanner.cheats.len() as u16).saturating_add(3).clamp(5, 10);
        let [top, middle, bottom] =
            Layout::vertical([Constraint::Length(6), Constraint::Min(4), Constraint::Length(cheat_h)]).areas(area);
        self.draw_scan_header(f, top);
        let [cands, history] = Layout::horizontal([Constraint::Min(30), Constraint::Length(36)]).areas(middle);
        self.draw_candidates(f, cands);
        self.draw_scan_history(f, history);
        self.draw_cheats(f, bottom);
    }

    fn draw_scan_header(&self, f: &mut Frame, area: Rect) {
        let s = &self.scanner;
        let game = self.game_rec().map(|g| g.name.clone()).unwrap_or_else(|| "no game".into());
        let file = self.file.clone().unwrap_or_else(|| "no file (f)".into());
        let mut type_spans = vec![Span::styled("Types     ", theme::muted())];
        for t in SCAN_TYPES {
            let on = s.config.types.contains(&t);
            type_spans.push(Span::styled(
                format!("{} ", t.name()),
                if on {
                    Style::default().fg(theme::GREEN)
                } else {
                    Style::default().fg(theme::MUTED).add_modifier(Modifier::CROSSED_OUT)
                },
            ));
        }
        let endian = match s.endian {
            EndianChoice::Little => "LE",
            EndianChoice::Big => "BE",
            EndianChoice::Both => "LE+BE",
        };
        type_spans.push(Span::styled(
            format!(
                "│ {endian} · {} · AST {} · raw {}{}",
                align_label(s.config.alignment),
                on_off(s.config.scan_ast),
                on_off(s.config.scan_raw),
                if s.raw_payload { " · WHOLE-FILE RAW" } else { "" }
            ),
            theme::subtle(),
        ));
        let filter_text = if s.filter.is_empty() { "press i to type a filter".to_string() } else { s.filter.clone() };
        let step = if s.session.is_fresh() { "first scan" } else { "next scan" };
        let status = if self.jobs.is_running(JobKind::Scan) {
            Line::from(Span::styled(
                format!("{} scanning…", widgets::spinner(self.frame)),
                Style::default().fg(theme::YELLOW),
            ))
        } else if let Some(e) = &s.last_error {
            Line::from(Span::styled(format!("✖ {e}"), Style::default().fg(theme::RED)))
        } else {
            Line::from(vec![
                Span::styled("Matches   ", theme::muted()),
                Span::styled(
                    s.session.candidate_count().to_string(),
                    Style::default().fg(theme::ACCENT2).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" candidate(s) after {} step(s)", s.session.steps().len()), theme::subtle()),
            ])
        };
        let lines = vec![
            Line::from(vec![
                Span::styled("Target    ", theme::muted()),
                Span::styled(game, theme::bold_accent()),
                Span::styled(" / ", theme::muted()),
                Span::styled(file, Style::default().fg(theme::TEXT)),
                Span::styled("   source ", theme::muted()),
                Span::styled(s.source.clone(), Style::default().fg(theme::ACCENT2)),
            ]),
            Line::from(vec![
                Span::styled("Filter    ", theme::muted()),
                Span::styled(format!(" ❯ {filter_text} "), Style::default().fg(theme::TEXT).bg(theme::SURFACE)),
                Span::styled(format!("  n = {step}"), theme::muted()),
            ]),
            Line::from(type_spans),
            status,
        ];
        f.render_widget(Paragraph::new(lines).block(theme::block("Save-State Scanner", false)), area);
    }

    fn draw_candidates(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.scanner.pane == ScanPane::Candidates;
        let page = area.height.saturating_sub(3).max(1) as usize;
        self.scanner_page = page;
        let explicit = self.scanner.session.explicit_count();
        let total = self.scanner.session.candidate_count();
        if self.scanner.session.is_fresh() || explicit == 0 {
            let msg: Vec<Line> = if self.scanner.session.is_fresh() {
                vec![
                    Line::from(Span::styled("How to find a value:", theme::title())),
                    Line::from("  1. i → type the value you see in game (e.g. 1200), Enter runs the first scan"),
                    Line::from("  2. change it in game and save (or pick another snapshot source with s)"),
                    Line::from("  3. i → `1200 -> 1500`, `+300`, `inc`, `changed`… then n narrows the list"),
                    Line::from("  4. a pins the hit under an alias, e writes a new value, F freezes it"),
                ]
            } else {
                vec![Line::from(Span::styled(
                    if total > 0 {
                        format!("{total} implicit candidate(s) (unknown initial value): run a next scan (inc, dec, changed…) to narrow them down")
                    } else {
                        "No candidate left: press R to start over".to_string()
                    },
                    theme::subtle(),
                ))]
            };
            f.render_widget(
                Paragraph::new(msg).wrap(Wrap { trim: false }).block(theme::block("Candidates", focused)),
                area,
            );
            return;
        }
        let sel = self.scanner.cand_sel.min(explicit - 1);
        let start = (sel / page) * page;
        let cands = self.scanner.session.candidates(start, page);
        let root = self.scanner.snapshot.as_ref();
        let rows: Vec<Row> = cands
            .iter()
            .map(|c| {
                let alias = self.alias_of(&c.location);
                let changed = c.previous != c.current;
                Row::new(vec![
                    Cell::from(Span::styled(
                        alias.map(|a| format!("★ {a}")).unwrap_or_default(),
                        Style::default().fg(theme::YELLOW),
                    )),
                    Cell::from(Span::styled(c.location.to_string(), Style::default().fg(theme::TEXT))),
                    Cell::from(Span::styled(c.location.type_label(root), theme::subtle())),
                    Cell::from(Span::styled(c.previous.to_string(), theme::muted())),
                    Cell::from(Span::styled(
                        c.current.to_string(),
                        Style::default()
                            .fg(if changed { theme::GREEN } else { theme::TEXT })
                            .add_modifier(Modifier::BOLD),
                    )),
                ])
            })
            .collect();
        let pages = explicit.div_ceil(page);
        let title = format!("Candidates {explicit} · page {}/{pages}", start / page + 1);
        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Min(18),
                Constraint::Length(14),
                Constraint::Length(12),
                Constraint::Length(12),
            ],
        )
        .header(Row::new(["Pin", "Location", "Type", "Previous", "Current"]).style(theme::header_row()))
        .block(theme::block(&title, focused))
        .row_highlight_style(if focused { theme::selected() } else { theme::selected_dim() })
        .highlight_symbol("▶");
        let mut state = TableState::default().with_selected(Some(sel - start));
        f.render_stateful_widget(table, area, &mut state);
    }

    fn draw_scan_history(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .scanner
            .session
            .steps()
            .iter()
            .enumerate()
            .map(|(i, s)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{}. ", i + 1), theme::muted()),
                    Span::styled(s.filter.clone(), Style::default().fg(theme::TEXT)),
                    Span::styled(format!(" @{}", s.snapshot_label), theme::muted()),
                    Span::styled(format!(" → {}", s.remaining), Style::default().fg(theme::ACCENT2)),
                ]))
            })
            .collect();
        f.render_widget(List::new(items).block(theme::block("Scan history", false)), area);
    }

    fn draw_cheats(&self, f: &mut Frame, area: Rect) {
        let focused = self.scanner.pane == ScanPane::Cheats;
        let trainer = self.game_rec().is_some_and(|g| g.trainer_enabled);
        let title = format!(
            "Cheat table · trainer {} (v focus · Enter edit · F freeze · x delete)",
            if trainer { "ON" } else { "off" }
        );
        let rows: Vec<Row> = self
            .scanner
            .cheats
            .iter()
            .map(|c| {
                let value = match (&c.current, &c.error) {
                    (Some(v), _) => {
                        Span::styled(v.to_string(), Style::default().fg(theme::GREEN).add_modifier(Modifier::BOLD))
                    }
                    (None, Some(e)) => Span::styled(widgets::ellipsize(e, 30), Style::default().fg(theme::RED)),
                    (None, None) => Span::styled("—", theme::muted()),
                };
                Row::new(vec![
                    Cell::from(Span::styled(
                        if c.entry.freeze_enabled { "❄" } else { " " },
                        Style::default().fg(theme::TEAL),
                    )),
                    Cell::from(Span::styled(
                        c.entry.alias.clone(),
                        Style::default().fg(theme::YELLOW).add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(c.entry.rel_path.clone(), theme::subtle())),
                    Cell::from(Span::styled(c.entry.location.to_string(), Style::default().fg(theme::TEXT))),
                    Cell::from(value),
                    Cell::from(Span::styled(
                        c.entry.frozen_value.map(|v| v.to_string()).unwrap_or_else(|| "—".into()),
                        Style::default().fg(if c.entry.freeze_enabled { theme::TEAL } else { theme::MUTED }),
                    )),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(2),
                Constraint::Length(14),
                Constraint::Length(16),
                Constraint::Min(16),
                Constraint::Length(14),
                Constraint::Length(12),
            ],
        )
        .header(Row::new(["", "Alias", "File", "Location", "Live value", "Frozen"]).style(theme::header_row()))
        .block(theme::block(&title, focused))
        .row_highlight_style(if focused { theme::selected() } else { theme::selected_dim() });
        let sel = (!self.scanner.cheats.is_empty()).then_some(self.scanner.cheat_sel);
        let mut state = TableState::default().with_selected(sel);
        f.render_stateful_widget(table, area, &mut state);
    }
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

fn freeze_message(alias: &str, enabled: bool, trainer: bool) -> String {
    match (enabled, trainer) {
        (true, true) => format!("{alias} frozen: re-applied after every save"),
        (true, false) => format!("{alias} frozen (enable the disk trainer with t on the Games tab to apply it)"),
        (false, _) => format!("{alias} unfrozen"),
    }
}
