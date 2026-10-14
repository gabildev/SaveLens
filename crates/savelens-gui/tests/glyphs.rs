//! Every non-ASCII character used by the GUI and overlay sources must exist
//! in egui's bundled fonts (otherwise it renders as a "tofu" box).

use std::path::Path;

fn source_chars(dir: &Path, out: &mut std::collections::BTreeSet<char>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            source_chars(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&p).unwrap();
            for line in text.lines() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                out.extend(code.chars().filter(|c| !c.is_ascii()));
            }
        }
    }
}

#[test]
fn ui_glyphs_exist_in_bundled_fonts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut chars = std::collections::BTreeSet::new();
    source_chars(&root.join("src"), &mut chars);
    source_chars(&root.join("../savelens-overlay/src"), &mut chars);
    let ctx = egui::Context::default();
    let _ = ctx.run(egui::RawInput::default(), |_| {});
    let missing: Vec<String> = ctx.fonts(|f| {
        chars
            .iter()
            .filter(|c| !f.has_glyphs(&egui::FontId::proportional(14.0), &c.to_string()))
            .map(|c| format!("U+{:04X} {c}", *c as u32))
            .collect()
    });
    assert!(missing.is_empty(), "glyphs missing from egui fonts: {missing:?}");
}
