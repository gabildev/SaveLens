//! Tab 4: AST explorer with integrity badges, fuzzy search, leaf editing and
//! surgical cherry-pick.

use anyhow::{anyhow, bail, Result};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use savelens_core::{cherry_pick, CherryPickOptions, NoAliases, Node, NodeKind, NodePath};
use savelens_watcher::{EditTarget, WORKING};

use crate::app::{Action, App, InputPurpose, Mode, PickPurpose, Picker};
use crate::jobs::{JobKind, JobOutput, LoadedTree};
use crate::tabs::timeline::change_color;
use crate::theme;
use crate::tree::{self, TreeRow};
use crate::widgets::{self, Level};

/// Maximum preview lines shown in the cherry-pick confirmation.
const PREVIEW_LINES: usize = 18;

fn value_color(kind: NodeKind) -> Color {
    match kind {
        NodeKind::String => theme::GREEN,
        NodeKind::Bool => theme::ACCENT2,
        NodeKind::Null => theme::MUTED,
        NodeKind::Bytes | NodeKind::RawRegion => theme::TEAL,
        k if k.is_numeric() => theme::PEACH,
        _ => theme::SUBTEXT,
    }
}

impl App {
    fn ast_key(&self) -> Option<(i64, String, String)> {
        Some((self.game?, self.file.clone()?, self.ast.rev.clone()))
    }

    /// Loads the tree of the selected file/revision on a worker if needed.
    pub(crate) fn ast_ensure_loaded(&mut self) {
        let Some(key) = self.ast_key() else {
            return;
        };
        if self.ast.loaded.as_ref() == Some(&key) || self.jobs.is_running(JobKind::AstLoad) {
            return;
        }
        let engine = self.engine.clone();
        let label = format!("Parsing {} @ {}", key.1, key.2);
        let job_key = key.clone();
        self.jobs.spawn(JobKind::AstLoad, label, move || {
            let (game, rel, rev) = &job_key;
            let result = engine
                .open_version(*game, rev, rel)
                .map(|o| LoadedTree { root: o.root(), badges: o.envelope().badges(), codec: o.codec_id().to_string() })
                .map_err(|e| e.to_string());
            JobOutput::AstLoad { key: job_key, result }
        });
    }

    /// Forces a reload when the explorer shows the files on disk.
    pub(crate) fn ast_reload_if_working(&mut self) {
        if self.ast.rev.eq_ignore_ascii_case(WORKING) && self.ast.loaded.is_some() {
            self.ast.loaded = None;
            self.ast_ensure_loaded();
        }
    }

    pub(crate) fn on_ast_loaded(&mut self, key: (i64, String, String), result: Result<LoadedTree, String>) {
        if self.ast_key().as_ref() != Some(&key) {
            // Stale result (file or revision changed meanwhile).
            self.ast_ensure_loaded();
            return;
        }
        self.ast.loaded = Some(key);
        match result {
            Ok(t) => {
                if self.ast.expanded.is_empty() {
                    self.ast.expanded.insert(tree::key(&NodePath::root()));
                    let mut first = Vec::new();
                    if let Node::Map(m) = &t.root {
                        for k in m.keys() {
                            first.push(NodePath::root().key(k.clone()));
                        }
                    }
                    for p in first {
                        self.ast.expanded.insert(tree::key(&p));
                    }
                }
                let root = &t.root;
                self.ast.marked.retain(|p| root.get(p).is_some());
                self.ast.tree = Some(t);
                self.ast.error = None;
                self.ast_rebuild();
            }
            Err(e) => {
                self.ast.tree = None;
                self.ast.rows.clear();
                self.ast.error = Some(e.clone());
                self.toast(Level::Error, format!("Cannot open file: {e}"));
            }
        }
    }

    fn ast_rebuild(&mut self) {
        let sel_path = self.ast.rows.get(self.ast.sel).map(|r| r.path.clone());
        self.ast.rows = match &self.ast.tree {
            Some(t) => tree::flatten(&t.root, &self.ast.expanded),
            None => Vec::new(),
        };
        if let Some(p) = sel_path {
            if let Some(i) = self.ast.rows.iter().position(|r| r.path == p) {
                self.ast.sel = i;
            }
        }
        self.ast.sel = self.ast.sel.min(self.ast.rows.len().saturating_sub(1));
    }

    fn ast_row(&self) -> Option<&TreeRow> {
        self.ast.rows.get(self.ast.sel)
    }

    fn ast_jump(&mut self, path: &NodePath) {
        tree::expand_to(path, &mut self.ast.expanded);
        self.ast_rebuild();
        if let Some(i) = self.ast.rows.iter().position(|r| &r.path == path) {
            self.ast.sel = i;
        }
    }

    pub(crate) fn key_ast(&mut self, key: KeyEvent) -> Result<()> {
        let len = self.ast.rows.len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.ast.sel = widgets::step(self.ast.sel, len, 1),
            KeyCode::Char('k') | KeyCode::Up => self.ast.sel = widgets::step(self.ast.sel, len, -1),
            KeyCode::PageDown => self.ast.sel = widgets::step(self.ast.sel, len, 15),
            KeyCode::PageUp => self.ast.sel = widgets::step(self.ast.sel, len, -15),
            KeyCode::Char('g') | KeyCode::Home => self.ast.sel = 0,
            KeyCode::Char('G') | KeyCode::End => self.ast.sel = len.saturating_sub(1),
            KeyCode::Char('l') | KeyCode::Right => {
                if let Some(row) = self.ast_row().cloned() {
                    if row.container && !row.expanded {
                        self.ast.expanded.insert(tree::key(&row.path));
                        self.ast_rebuild();
                    } else if row.container && row.children > 0 {
                        self.ast.sel = widgets::step(self.ast.sel, len, 1);
                    }
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if let Some(row) = self.ast_row().cloned() {
                    if row.container && row.expanded {
                        self.ast.expanded.remove(&tree::key(&row.path));
                        self.ast_rebuild();
                    } else if let Some(parent) = row.path.parent() {
                        if let Some(i) = self.ast.rows.iter().position(|r| r.path == parent) {
                            self.ast.sel = i;
                        }
                    }
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(row) = self.ast_row().cloned() {
                    if row.container {
                        let k = tree::key(&row.path);
                        if !self.ast.expanded.remove(&k) {
                            self.ast.expanded.insert(k);
                        }
                        self.ast_rebuild();
                    }
                }
            }
            KeyCode::Char('/') => {
                let current = self.ast.search.clone();
                self.prompt(
                    InputPurpose::AstSearch,
                    "Fuzzy search (paths and values)",
                    &current,
                    "e.g. gold · inv sword · quest main",
                );
            }
            KeyCode::Char('n') => self.ast_next_match(1),
            KeyCode::Char('N') => self.ast_next_match(-1),
            KeyCode::Char('e') => {
                let row = self.ast_row().cloned().ok_or_else(|| anyhow!("nothing selected"))?;
                if row.container {
                    bail!("{} is a container: edit one of its leaves", row.path);
                }
                let node = self
                    .ast
                    .tree
                    .as_ref()
                    .and_then(|t| t.root.get(&row.path))
                    .ok_or_else(|| anyhow!("{} not found", row.path))?;
                let initial = match node {
                    Node::String(s) => s.clone(),
                    Node::Bytes(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                    other => other.preview(200),
                };
                let note = if self.ast.rev.eq_ignore_ascii_case(WORKING) {
                    "new value, parsed with the node's type (written to disk with a safety backup)".to_string()
                } else {
                    format!("writes the WORKING file (you are viewing {})", self.ast.rev)
                };
                self.prompt(
                    InputPurpose::AstEdit { path: row.path.clone() },
                    &format!("Edit {}", row.path),
                    &initial,
                    &note,
                );
            }
            KeyCode::Char('m') => {
                let row = self.ast_row().cloned().ok_or_else(|| anyhow!("nothing selected"))?;
                self.ast_toggle_mark(row.path);
            }
            KeyCode::Char('M') => {
                self.ast.marked.clear();
                self.toast(Level::Info, "Cleared cherry-pick marks");
            }
            KeyCode::Char('r') => {
                self.require_game()?;
                let items = self.revision_items();
                let sel = items.iter().position(|i| i.value == self.ast.rev).unwrap_or(0);
                self.mode = Mode::Picker(Picker {
                    title: "Revision to explore (= cherry-pick source)".into(),
                    items,
                    sel,
                    purpose: PickPurpose::AstRev,
                });
            }
            KeyCode::Char('f') => self.pick_file()?,
            KeyCode::Char('C') => self.ast_cherry_prompt()?,
            _ => {}
        }
        Ok(())
    }

    fn ast_toggle_mark(&mut self, path: NodePath) {
        if let Some(i) = self.ast.marked.iter().position(|p| p == &path) {
            self.ast.marked.remove(i);
            return;
        }
        let before = self.ast.marked.len();
        self.ast.marked.retain(|p| !p.is_prefix_of(&path) && !path.is_prefix_of(p));
        if self.ast.marked.len() != before {
            self.toast(Level::Info, format!("{path} replaces overlapping marks"));
        }
        self.ast.marked.push(path);
    }

    pub(crate) fn ast_search(&mut self, query: &str) {
        self.ast.search = query.trim().to_string();
        self.ast.matches = match &self.ast.tree {
            Some(t) => tree::search(&t.root, &self.ast.search),
            None => Vec::new(),
        };
        self.ast.match_idx = 0;
        match self.ast.matches.first().cloned() {
            Some(p) => {
                self.ast_jump(&p);
                self.toast(Level::Info, format!("{} match(es) for `{}`", self.ast.matches.len(), self.ast.search));
            }
            None if self.ast.search.is_empty() => {}
            None => self.toast(Level::Warning, format!("No match for `{}`", self.ast.search)),
        }
    }

    fn ast_next_match(&mut self, dir: isize) {
        let n = self.ast.matches.len();
        if n == 0 {
            self.toast(Level::Info, "No search results: press / to search");
            return;
        }
        self.ast.match_idx = if dir >= 0 { (self.ast.match_idx + 1) % n } else { (self.ast.match_idx + n - 1) % n };
        let p = self.ast.matches[self.ast.match_idx].clone();
        self.ast_jump(&p);
    }

    pub(crate) fn ast_edit(&mut self, path: NodePath, value: &str) -> Result<()> {
        let game = self.require_game()?;
        let rel = self.require_file()?;
        let rep = self.engine.edit_value(game.id, &rel, &EditTarget::Path { path, value: value.to_string() }, false)?;
        self.report_edit(&rep);
        Ok(())
    }

    fn ast_cherry_prompt(&mut self) -> Result<()> {
        let game = self.require_game()?;
        let rel = self.require_file()?;
        if self.ast.marked.is_empty() {
            bail!("mark sub-trees with m first");
        }
        if self.ast.rev.eq_ignore_ascii_case(WORKING) {
            bail!("you are viewing WORKING: choose an older source revision with r, then mark and cherry-pick");
        }
        let source =
            self.ast.tree.as_ref().map(|t| &t.root).ok_or_else(|| anyhow!("the source revision is not loaded yet"))?;
        let current = self.engine.root_at(game.id, WORKING, &rel)?;
        let mut preview = current.clone();
        let report = cherry_pick(&mut preview, source, &self.ast.marked, &CherryPickOptions::default())?;
        if report.applied.is_empty() {
            self.toast(Level::Info, "The marked sub-trees are already identical in WORKING");
            return Ok(());
        }
        let d = savelens_core::diff(&current, &preview);
        let changes = d.changelog(&NoAliases);
        let mut lines = vec![
            Line::from(vec![
                Span::styled("From ", theme::muted()),
                Span::styled(self.ast.rev.clone(), Style::default().fg(theme::ACCENT2).add_modifier(Modifier::BOLD)),
                Span::styled(" into ", theme::muted()),
                Span::styled(format!("WORKING {rel}"), theme::bold_accent()),
            ]),
            Line::from(Span::styled(
                format!("Sub-trees: {}", self.ast.marked.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")),
                Style::default().fg(theme::YELLOW),
            )),
            Line::from(""),
            Line::from(Span::styled(format!("Semantic preview ({} change(s)):", changes.len()), theme::title())),
        ];
        for c in changes.iter().take(PREVIEW_LINES) {
            lines.push(Line::from(Span::styled(format!("  {c}"), Style::default().fg(change_color(c)))));
        }
        if changes.len() > PREVIEW_LINES {
            lines.push(Line::from(Span::styled(format!("  … {} more", changes.len() - PREVIEW_LINES), theme::muted())));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Everything else in the save is kept; a safety backup is taken first.",
            theme::muted(),
        )));
        let action = Action::CherryPick { rev: self.ast.rev.clone(), rel, paths: self.ast.marked.clone() };
        self.confirm("Surgical cherry-pick", lines, action, false);
        Ok(())
    }

    pub(crate) fn ast_cherry_pick(&mut self, rev: &str, rel: &str, paths: &[NodePath]) -> Result<()> {
        let game = self.require_game()?;
        let rep = self.engine.cherry_pick(game.id, rev, rel, paths, &CherryPickOptions::default())?;
        self.ast.marked.clear();
        // Show the result: switch the explorer to the file on disk.
        self.ast.rev = WORKING.to_string();
        self.ast.loaded = None;
        self.report_edit(&rep);
        self.ast_ensure_loaded();
        Ok(())
    }

    pub(crate) fn draw_ast(&mut self, f: &mut Frame, area: Rect) {
        let [info, body] = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(area);
        self.draw_ast_info(f, info);
        let [tree_area, detail] =
            Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(body);
        self.draw_tree(f, tree_area);
        self.draw_ast_detail(f, detail);
    }

    fn draw_ast_info(&self, f: &mut Frame, area: Rect) {
        let mut spans = vec![
            Span::styled(
                self.file.clone().unwrap_or_else(|| "no file (f)".into()),
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" @ ", theme::muted()),
            Span::styled(self.ast.rev.clone(), Style::default().fg(theme::ACCENT2).add_modifier(Modifier::BOLD)),
            Span::raw("   "),
        ];
        if let Some(t) = &self.ast.tree {
            for b in &t.badges {
                spans.push(Span::styled(
                    format!(" {b} "),
                    Style::default().fg(theme::BG).bg(theme::badge_color(b)).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::raw(" "));
            }
        } else if self.jobs.is_running(JobKind::AstLoad) {
            spans.push(Span::styled(
                format!("{} parsing…", widgets::spinner(self.frame)),
                Style::default().fg(theme::YELLOW),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).block(theme::block("Integrity", false)), area);
    }

    fn draw_tree(&mut self, f: &mut Frame, area: Rect) {
        let title = match &self.ast.tree {
            Some(t) => format!("Tree · {} · {} nodes", t.codec, t.root.node_count()),
            None => "Tree".to_string(),
        };
        let block = theme::block(&title, true);
        let inner = block.inner(area);
        f.render_widget(block, area);
        if self.ast.tree.is_none() {
            let msg = match (&self.ast.error, self.game, &self.file) {
                (Some(e), _, _) => Line::from(Span::styled(format!("✖ {e}"), Style::default().fg(theme::RED))),
                (None, None, _) => Line::from(Span::styled("Select a game on the Games tab (1).", theme::muted())),
                (None, _, None) => Line::from(Span::styled("No tracked file: press f.", theme::muted())),
                _ => Line::from(Span::styled(format!("{} loading…", widgets::spinner(self.frame)), theme::muted())),
            };
            f.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }), inner);
            return;
        }
        let h = inner.height as usize;
        if h == 0 {
            return;
        }
        // Keep the cursor visible.
        if self.ast.sel < self.ast_offset {
            self.ast_offset = self.ast.sel;
        } else if self.ast.sel >= self.ast_offset + h {
            self.ast_offset = self.ast.sel + 1 - h;
        }
        self.ast_offset = self.ast_offset.min(self.ast.rows.len().saturating_sub(1));
        let current_match = self.ast.matches.get(self.ast.match_idx);
        let lines: Vec<Line> = self
            .ast
            .rows
            .iter()
            .enumerate()
            .skip(self.ast_offset)
            .take(h)
            .map(|(i, r)| {
                let marked = self.ast.marked.iter().any(|p| p == &r.path);
                let inside_mark = !marked && self.ast.marked.iter().any(|p| p.is_prefix_of(&r.path));
                let is_match = self.ast.matches.iter().any(|p| p == &r.path);
                let mut spans = vec![Span::styled(
                    if marked {
                        "✚ "
                    } else if inside_mark {
                        "┊ "
                    } else {
                        "  "
                    },
                    Style::default().fg(theme::YELLOW),
                )];
                spans.push(Span::raw("  ".repeat(r.depth)));
                let icon = if r.container {
                    if r.expanded {
                        "▾ "
                    } else {
                        "▸ "
                    }
                } else {
                    "• "
                };
                spans.push(Span::styled(
                    icon,
                    Style::default().fg(if r.container { theme::ACCENT } else { theme::OVERLAY }),
                ));
                let mut label_style =
                    Style::default().fg(if r.label.starts_with('[') { theme::SUBTEXT } else { theme::ACCENT });
                if is_match {
                    label_style = label_style.fg(theme::YELLOW).add_modifier(Modifier::UNDERLINED);
                }
                if current_match == Some(&r.path) {
                    label_style = label_style.add_modifier(Modifier::BOLD);
                }
                spans.push(Span::styled(r.label.clone(), label_style));
                if r.container {
                    spans.push(Span::styled(format!("  {}", r.preview), theme::muted()));
                } else {
                    spans.push(Span::styled(" = ", theme::muted()));
                    spans.push(Span::styled(r.preview.clone(), Style::default().fg(value_color(r.kind))));
                }
                let mut line = Line::from(spans);
                if i == self.ast.sel {
                    line = line.style(theme::selected());
                } else if marked {
                    line = line.style(Style::default().bg(theme::SURFACE));
                }
                line
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_ast_detail(&self, f: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        if let (Some(row), Some(t)) = (self.ast_row(), &self.ast.tree) {
            lines.push(widgets::kv("Path", row.path.to_string(), theme::ACCENT));
            lines.push(widgets::kv("Kind", row.kind.to_string(), theme::SUBTEXT));
            if row.container {
                lines.push(widgets::kv("Children", row.children.to_string(), theme::TEXT));
            } else if let Some(n) = t.root.get(&row.path) {
                lines.push(widgets::kv("Value", n.preview(400), value_color(row.kind)));
            }
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!("Marked for cherry-pick ({})", self.ast.marked.len()),
            theme::title(),
        )));
        if self.ast.marked.is_empty() {
            lines.push(Line::from(Span::styled("  m marks the sub-tree under the cursor", theme::muted())));
        }
        for p in &self.ast.marked {
            lines.push(Line::from(Span::styled(format!("  ✚ {p}"), Style::default().fg(theme::YELLOW))));
        }
        if !self.ast.marked.is_empty() {
            let hint = if self.ast.rev.eq_ignore_ascii_case(WORKING) {
                "  r: choose the source revision, then C".to_string()
            } else {
                format!("  C: copy from {} into WORKING", self.ast.rev)
            };
            lines.push(Line::from(Span::styled(hint, theme::muted())));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Search", theme::title())));
        if self.ast.search.is_empty() {
            lines.push(Line::from(Span::styled("  / fuzzy-searches paths and values", theme::muted())));
        } else {
            let pos = if self.ast.matches.is_empty() { 0 } else { self.ast.match_idx + 1 };
            lines.push(Line::from(vec![
                Span::styled(format!("  `{}` ", self.ast.search), Style::default().fg(theme::YELLOW)),
                Span::styled(format!("{pos}/{} (n/N)", self.ast.matches.len()), theme::subtle()),
            ]));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(theme::block("Node", false)), area);
    }
}
