//! Collapsible rendering of a semantic save tree, shared by the tree diff
//! (read-only right side, cherry-pick left side) and the AST editor.

use std::collections::{BTreeSet, HashMap, HashSet};

use egui::collapsing_header::CollapsingState;
use egui::{Color32, RichText};
use savelens_core::{Node, NodePath};

use crate::app::Intent;
use crate::theme;
use crate::treediff::{NodeStatus, Side, TreeDiffModel};

/// Children rendered per container (the rest is summarised).
const MAX_CHILDREN: usize = 500;

/// Interaction mode of a tree.
pub(crate) enum TreeMode<'a> {
    ReadOnly,
    Pick { selected: &'a mut BTreeSet<String>, intents: &'a mut Vec<Intent> },
    Edit { edits: &'a mut HashMap<String, String>, intents: &'a mut Vec<Intent>, enabled: bool },
}

/// Rendering context.
pub(crate) struct TreeCtx<'a> {
    pub id_prefix: &'static str,
    pub diff: Option<(&'a TreeDiffModel, Side)>,
    pub visible: Option<&'a HashSet<String>>,
    pub mode: TreeMode<'a>,
}

pub(crate) fn status_color(s: NodeStatus) -> Color32 {
    match s {
        NodeStatus::Same => theme::TEXT,
        NodeStatus::Added => theme::LIME,
        NodeStatus::Removed => theme::RED,
        NodeStatus::Modified => theme::AMBER,
        NodeStatus::Contains => theme::CYAN,
    }
}

fn status_marker(s: NodeStatus) -> &'static str {
    match s {
        NodeStatus::Same => "",
        NodeStatus::Added => "+ ",
        NodeStatus::Removed => "− ",
        NodeStatus::Modified => "~ ",
        NodeStatus::Contains => "• ",
    }
}

/// Editable text of a scalar leaf.
pub(crate) fn leaf_text(node: &Node) -> Option<String> {
    Some(match node {
        Node::Bool(b) => b.to_string(),
        Node::I8(v) => v.to_string(),
        Node::I16(v) => v.to_string(),
        Node::I32(v) => v.to_string(),
        Node::I64(v) => v.to_string(),
        Node::U8(v) => v.to_string(),
        Node::U16(v) => v.to_string(),
        Node::U32(v) => v.to_string(),
        Node::U64(v) => v.to_string(),
        Node::F32(_) | Node::F64(_) => node.preview(64),
        Node::String(s) => s.clone(),
        _ => return None,
    })
}

/// Draws a whole tree.
pub(crate) fn show_tree(ui: &mut egui::Ui, root: &Node, ctx: &mut TreeCtx<'_>) {
    let path = NodePath::root();
    show_node(ui, root, &path, "$", 0, ctx);
}

fn status(ctx: &TreeCtx<'_>, path: &NodePath) -> NodeStatus {
    match ctx.diff {
        Some((m, side)) => m.status(side, path),
        None => NodeStatus::Same,
    }
}

fn show_node(ui: &mut egui::Ui, node: &Node, path: &NodePath, name: &str, depth: usize, ctx: &mut TreeCtx<'_>) {
    let key = path.to_string();
    if let Some(v) = ctx.visible {
        if !v.contains(&key) && !path.is_root() {
            return;
        }
    }
    let st = status(ctx, path);
    let color = status_color(st);
    if node.is_container() {
        let id = ui.make_persistent_id((ctx.id_prefix, key.as_str()));
        let default_open = depth < 2 || st != NodeStatus::Same || ctx.visible.is_some();
        let state = CollapsingState::load_with_default_open(ui.ctx(), id, default_open);
        state
            .show_header(ui, |ui| {
                ui.label(RichText::new(format!("{}{name}", status_marker(st))).color(color).strong());
                ui.label(RichText::new(node.preview(40)).small().color(theme::TEXT_DIM));
                if let TreeMode::Pick { selected, intents } = &mut ctx.mode {
                    if !path.is_root() {
                        let mut on = selected.contains(&key);
                        if ui.checkbox(&mut on, "").on_hover_text("Select for a multi-section cherry-pick").changed() {
                            if on {
                                selected.insert(key.clone());
                            } else {
                                selected.remove(&key);
                            }
                        }
                        if ui
                            .add(theme::ghost_button("Cherry-Pick this section", theme::MAGENTA).small())
                            .on_hover_text(format!("Restore {key} from this revision into the working save"))
                            .clicked()
                        {
                            intents.push(Intent::RequestPick(vec![path.clone()]));
                        }
                    }
                }
            })
            .body(|ui| match node {
                Node::Map(m) => {
                    for (k, child) in m.iter().take(MAX_CHILDREN) {
                        show_node(ui, child, &path.key(k.clone()), k, depth + 1, ctx);
                    }
                    if m.len() > MAX_CHILDREN {
                        ui.label(
                            RichText::new(format!("… {} more field(s)", m.len() - MAX_CHILDREN))
                                .small()
                                .color(theme::TEXT_FAINT),
                        );
                    }
                }
                Node::Array(a) => {
                    for (i, child) in a.iter().enumerate().take(MAX_CHILDREN) {
                        show_node(ui, child, &path.index(i), &format!("[{i}]"), depth + 1, ctx);
                    }
                    if a.len() > MAX_CHILDREN {
                        ui.label(
                            RichText::new(format!("… {} more item(s)", a.len() - MAX_CHILDREN))
                                .small()
                                .color(theme::TEXT_FAINT),
                        );
                    }
                }
                _ => {}
            });
        return;
    }

    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{}{name}", status_marker(st))).color(color));
        match &mut ctx.mode {
            TreeMode::ReadOnly => {
                ui.label(theme::mono(node.preview(120)).color(if st == NodeStatus::Same {
                    theme::TEXT
                } else {
                    color
                }));
            }
            TreeMode::Pick { selected, .. } => {
                ui.label(theme::mono(node.preview(120)).color(if st == NodeStatus::Same {
                    theme::TEXT
                } else {
                    color
                }));
                if !path.is_root() {
                    let mut on = selected.contains(&key);
                    if ui.checkbox(&mut on, "").on_hover_text("Select for a multi-section cherry-pick").changed() {
                        if on {
                            selected.insert(key.clone());
                        } else {
                            selected.remove(&key);
                        }
                    }
                }
            }
            TreeMode::Edit { edits, intents, enabled } => match leaf_text(node) {
                Some(orig) if *enabled => {
                    let mut text = edits.get(&key).cloned().unwrap_or_else(|| orig.clone());
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut text).font(egui::TextStyle::Monospace).desired_width(220.0),
                    );
                    if r.changed() {
                        edits.insert(key.clone(), text.clone());
                    }
                    let dirty = edits.get(&key).is_some_and(|t| *t != orig);
                    ui.label(RichText::new(node.kind().name()).small().color(theme::TEXT_FAINT));
                    if dirty {
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui.add(theme::accent_button("Set", theme::LIME).small()).clicked() || enter {
                            intents.push(Intent::EditValue { path: path.clone(), value: text });
                        }
                        if ui.small_button("↺").on_hover_text("Discard this edit").clicked() {
                            edits.remove(&key);
                        }
                    }
                }
                _ => {
                    ui.label(theme::mono(node.preview(120)));
                    ui.label(RichText::new(node.kind().name()).small().color(theme::TEXT_FAINT));
                }
            },
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_texts() {
        assert_eq!(leaf_text(&Node::I64(5)).as_deref(), Some("5"));
        assert_eq!(leaf_text(&Node::String("x y".into())).as_deref(), Some("x y"));
        assert_eq!(leaf_text(&Node::Bool(true)).as_deref(), Some("true"));
        assert!(leaf_text(&Node::Bytes(vec![1])).is_none());
        assert!(leaf_text(&Node::Array(vec![])).is_none());
    }
}
