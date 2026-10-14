//! `savelens what …`: "what changed?" questions.

use anyhow::{anyhow, Result};
use savelens_cas::{format_ts, WatchQuestion};
use savelens_core::{CategoryGroup, VariableCategory};
use savelens_watcher::{Engine, IgnoreScope};

use crate::cli::{ScopeArg, WhatCmd};
use crate::style::*;

fn category(input: &str) -> Result<VariableCategory> {
    VariableCategory::parse(input).ok_or_else(|| anyhow!("unknown category `{input}` (see `savelens what categories`)"))
}

fn print_question(engine: &Engine, q: &WatchQuestion) {
    let c = q.candidate();
    let game = engine.store().game(q.game_id).map(|g| g.name).unwrap_or_else(|_| format!("#{}", q.game_id));
    let status = match q.status {
        savelens_cas::QuestionStatus::Pending => yellow("?"),
        savelens_cas::QuestionStatus::Answered => green("✔"),
        savelens_cas::QuestionStatus::Ignored => dim("muted"),
        savelens_cas::QuestionStatus::Snoozed => dim("later"),
    };
    println!(
        "{status} {:>4}  {}  {}  {}  {}",
        bold(&format!("#{}", q.id)),
        bold(&c.label()),
        cyan(&c.change_text()),
        dim(&format!("{game} · {}", q.rel_path)),
        dim(&format_ts(q.updated_at))
    );
    match (&q.category, &q.alias) {
        (Some(cat), Some(alias)) => println!("        {} {cat} → `{alias}`", green("answered")),
        _ => {
            if let Some(g) = &q.guess {
                println!("        looks like {} (\"{}\")", green(g.category.label()), g.keyword);
            }
            let choices: Vec<String> = c.quick_choices().iter().map(|c| c.id().to_string()).collect();
            println!("        {}", dim(&format!("savelens what answer {} <{}|…>", q.id, choices.join("|"))));
        }
    }
}

pub fn run(engine: &Engine, action: Option<&WhatCmd>, json: bool, find: impl Fn(&str) -> Result<i64>) -> Result<()> {
    let list = WhatCmd::List { game: None, all: false };
    match action.unwrap_or(&list) {
        WhatCmd::List { game, all } => {
            let id = game.as_deref().map(&find).transpose()?;
            let qs = engine.questions(id, *all)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&qs)?);
                return Ok(());
            }
            if qs.is_empty() {
                println!(
                    "{}",
                    dim("no pending questions: play and save, SaveLens asks when a stat-like value changes")
                );
            }
            for q in &qs {
                print_question(engine, q);
            }
        }
        WhatCmd::Answer { id, category: cat, alias } => {
            let cat = category(cat)?;
            let (q, entry) = engine.answer_question(*id, cat, alias.as_deref())?;
            println!(
                "{} {} is {cat}: pinned as `{}` in {}",
                green("✔"),
                q.candidate().label(),
                bold(&entry.alias),
                q.rel_path
            );
        }
        WhatCmd::Ignore { id, scope } => {
            let scope = match scope {
                ScopeArg::This => IgnoreScope::This,
                ScopeArg::Similar => IgnoreScope::Similar,
                ScopeArg::Game => IgnoreScope::Game,
            };
            let (_, mute) = engine.ignore_question(*id, scope)?;
            let wh = mute.rel_path.unwrap_or_else(|| "every file".into());
            println!("{} muted `{}` ({wh})", green("✔"), mute.pattern);
        }
        WhatCmd::Later { id, minutes } => {
            let q = engine.snooze_question(*id, *minutes)?;
            println!("{} asking again after {}", green("✔"), format_ts(q.snooze_until.unwrap_or(q.updated_at)));
        }
        WhatCmd::Mute { game, pattern, file } => {
            let m = engine.mute_pattern(find(game)?, file.as_deref(), pattern)?;
            println!("{} #{} muted `{}`", green("✔"), m.id, m.pattern);
        }
        WhatCmd::Mutes { game } => {
            let mutes = engine.store().watch_mutes(find(game)?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&mutes)?);
                return Ok(());
            }
            for m in mutes {
                let wh = m.rel_path.unwrap_or_else(|| "*".into());
                println!("{:>4}  {}  {}", m.id, bold(&m.pattern.0), dim(&wh));
            }
        }
        WhatCmd::Unmute { game, id } => {
            engine.store().watch_unmute(find(game)?, *id)?;
            println!("{} unmuted #{id}", green("✔"));
        }
        WhatCmd::Categories => {
            for group in CategoryGroup::ALL {
                println!("{}", bold(group.label()));
                for c in group.categories() {
                    println!("  {:>2}  {:<16} {}", c.number(), cyan(c.id()), c.label());
                }
            }
        }
    }
    Ok(())
}
