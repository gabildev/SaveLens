//! Command handlers.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use savelens_cas::{format_ts, CommitKind, GameRecord, PanicScope};
use savelens_core::{
    Alignment, CherryPickMode, CherryPickOptions, Endianness, Location, Node, NodePath, ScalarValue, ScanConfig,
    ScanFilter, ScanSession, ScanType,
};
use savelens_watcher::{Daemon, DaemonEvent, DaemonOptions, EditTarget, Engine, FileStatus, SaveLensConfig, WORKING};

use crate::cli::{AlignArg, Cli, Command, EndianArg, ScanArgs};
use crate::style::*;

fn open_engine(cli: &Cli) -> Result<Engine> {
    let engine = match &cli.home {
        Some(h) => Engine::open_at(h),
        None => Engine::open_default(),
    };
    engine.context("cannot open the SaveLens store")
}

fn game(engine: &Engine, query: &str) -> Result<GameRecord> {
    Ok(engine.store().find_game(query)?)
}

fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Parses a node path (`Player.Gold`) or a raw location (`Region@0x1a4:i32le`,
/// `$@420:u8`).
pub fn parse_location(s: &str) -> Result<Location> {
    let Some((region, rest)) = s.rsplit_once('@') else {
        return Ok(Location::Ast { path: NodePath::parse(s)? });
    };
    let (off, ty) =
        rest.split_once(':').ok_or_else(|| anyhow!("raw location `{s}` needs a type, e.g. `$@0x1a4:i32le`"))?;
    let offset = if let Some(h) = off.strip_prefix("0x").or_else(|| off.strip_prefix("0X")) {
        u64::from_str_radix(h, 16)?
    } else {
        off.parse::<u64>()?
    };
    let lower = ty.to_ascii_lowercase();
    let (name, endian) = if let Some(n) = lower.strip_suffix("le") {
        (n.to_string(), Endianness::Little)
    } else if let Some(n) = lower.strip_suffix("be") {
        (n.to_string(), Endianness::Big)
    } else {
        (lower.clone(), Endianness::Little)
    };
    let ty: ScanType = name.parse()?;
    Ok(Location::Raw { region: NodePath::parse(region)?, offset, ty, endian })
}

fn kind_badge(k: CommitKind) -> String {
    let s = format!("{:<11}", k.as_str());
    match k {
        CommitKind::Auto => cyan(&s),
        CommitKind::Manual | CommitKind::Import => bold(&s),
        CommitKind::Trainer => magenta(&s),
        CommitKind::Edit | CommitKind::CherryPick => yellow(&s),
        CommitKind::Restore => green(&s),
    }
}

fn print_tree(node: &Node, label: &str, depth: usize, max_depth: usize, indent: usize) {
    let pad = "  ".repeat(indent);
    match node {
        Node::Map(m) => {
            println!("{pad}{} {}", bold(label), dim(&format!("{{{} fields}}", m.len())));
            if depth >= max_depth {
                return;
            }
            for (i, (k, v)) in m.iter().enumerate() {
                if i >= 60 {
                    println!("{pad}  {}", dim(&format!("… {} more", m.len() - i)));
                    break;
                }
                print_tree(v, k, depth + 1, max_depth, indent + 1);
            }
        }
        Node::Array(a) => {
            println!("{pad}{} {}", bold(label), dim(&format!("[{} items]", a.len())));
            if depth >= max_depth {
                return;
            }
            for (i, v) in a.iter().enumerate() {
                if i >= 30 {
                    println!("{pad}  {}", dim(&format!("… {} more", a.len() - i)));
                    break;
                }
                print_tree(v, &format!("[{i}]"), depth + 1, max_depth, indent + 1);
            }
        }
        Node::RawRegion(r) => {
            println!(
                "{pad}{} {}",
                bold(label),
                dim(&format!("<raw region {} bytes, {} strings>", r.len(), r.strings.len()))
            );
            for s in r.strings.iter().take(10) {
                println!("{pad}  {} {:?}", dim(&format!("@{:#06x}", s.offset)), s.text);
            }
        }
        leaf => println!("{pad}{label}: {} {}", cyan(&leaf.preview(80)), dim(leaf.kind().name())),
    }
}

fn scan_config(a: &ScanArgs) -> Result<ScanConfig> {
    let types = a
        .types
        .split(',')
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.parse::<ScanType>())
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ScanConfig {
        types,
        endians: match a.endian {
            EndianArg::Le => vec![Endianness::Little],
            EndianArg::Be => vec![Endianness::Big],
            EndianArg::Both => vec![Endianness::Little, Endianness::Big],
        },
        alignment: match a.align {
            AlignArg::Unaligned => Alignment::Unaligned,
            AlignArg::Natural => Alignment::Natural,
            AlignArg::Four => Alignment::Fixed(4),
        },
        ..ScanConfig::default()
    })
}

fn run_scan(engine: &Engine, a: &ScanArgs, json: bool) -> Result<()> {
    let g = game(engine, &a.game)?;
    let mut session = ScanSession::new(scan_config(a)?);
    for (i, step) in a.steps.iter().enumerate() {
        let (rev, filter_s) =
            step.split_once('=').ok_or_else(|| anyhow!("scan step `{step}` must be REV=FILTER, e.g. HEAD~1=1500"))?;
        let filter = ScanFilter::parse(filter_s)?;
        let opened = engine.open_version(g.id, rev, &a.file)?;
        let root = if a.raw { opened.into_raw_mode().root() } else { opened.root() };
        let n =
            if i == 0 { session.first_scan(&root, &filter, rev)? } else { session.next_scan(&root, &filter, rev)? };
        if !json {
            println!(
                "{} {:<10} {:<22} → {} candidate(s)",
                dim(&format!("#{i}")),
                rev,
                filter.to_string(),
                bold(&n.to_string())
            );
        }
    }
    let cands = session.candidates(0, a.limit);
    if json {
        return print_json(&serde_json::json!({
            "count": session.candidate_count(),
            "candidates": cands,
        }));
    }
    if session.implicit_count() > 0 {
        println!(
            "{}",
            yellow(&format!(
                "{} raw candidates still implicit: add a relational step (inc/dec/changed/+N) to narrow them",
                session.implicit_count()
            ))
        );
    }
    for c in &cands {
        println!("  {:<48} {:>16} → {}", c.location.to_string(), c.previous.to_string(), green(&c.current.to_string()));
    }
    if let Some(alias) = &a.pin {
        if session.explicit_count() != 1 {
            bail!("--pin needs exactly one candidate, {} remain", session.explicit_count());
        }
        let loc = session.locations()[0].clone();
        engine.pin(g.id, &a.file, alias, &loc)?;
        println!("{} pinned {loc} as `{alias}`", green("✔"));
    }
    Ok(())
}

/// Dispatches a parsed command line.
pub fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    match &cli.command {
        Command::Tui => {
            let engine = open_engine(&cli)?;
            run_tui(engine)
        }
        Command::Gui => {
            let engine = open_engine(&cli)?;
            run_gui(engine)
        }
        Command::Overlay { game: g, opacity, no_daemon, no_hotkeys } => {
            let engine = open_engine(&cli)?;
            let game_id = match g {
                Some(q) => Some(game(&engine, q)?.id),
                None => None,
            };
            run_overlay(engine, game_id, *opacity, !*no_daemon, !*no_hotkeys)
        }
        Command::Watch { games, no_hotkeys } => {
            let engine = open_engine(&cli)?;
            let ids = if games.is_empty() {
                None
            } else {
                Some(games.iter().map(|q| game(&engine, q).map(|g| g.id)).collect::<Result<Vec<_>>>()?)
            };
            watch(engine, ids, !*no_hotkeys, json)
        }
        Command::Discover { track_all } => {
            let engine = open_engine(&cli)?;
            let report = engine.discover(&savelens_discovery::DiscoveryConfig::from_env())?;
            if json && !*track_all {
                return print_json(&report);
            }
            println!("{} Steam root(s), {} librar(ies)", report.steam_roots.len(), report.libraries.len());
            for w in &report.warnings {
                println!("  {}", yellow(&format!("! {w}")));
            }
            for g in &report.games {
                let rt = match &g.runtime {
                    savelens_discovery::GameRuntime::Native => "native".to_string(),
                    savelens_discovery::GameRuntime::Proton { .. } => "proton".to_string(),
                    savelens_discovery::GameRuntime::Unknown => "unknown".to_string(),
                };
                println!(
                    "{} {} {} {}",
                    bold(&g.name),
                    dim(&g.app_id.map(|a| format!("[{a}]")).unwrap_or_default()),
                    cyan(&rt),
                    dim(&g.engine.map(|e| format!("{e:?}")).unwrap_or_default())
                );
                for (i, l) in g.save_locations.iter().enumerate() {
                    println!(
                        "   {} {} ({} files) {}",
                        dim(&format!("#{i}")),
                        l.root.display(),
                        l.files.len(),
                        dim(&format!("{:?}", l.source))
                    );
                    if *track_all {
                        match engine.track_discovered(g, i) {
                            Ok((rec, _)) => println!("     {} tracked as game #{}", green("✔"), rec.id),
                            Err(e) => println!("     {} {e}", red("✘")),
                        }
                    }
                }
            }
            Ok(())
        }
        Command::Hunt { seconds, pid, roots } => {
            let engine = open_engine(&cli)?;
            let cfg = savelens_discovery::DiscoveryConfig::from_env();
            let report = engine.discover(&cfg).ok();
            println!("{}", dim(&format!("hunting for {seconds}s — play the game and save now…")));
            let opts = savelens_discovery::hunter::HuntOptions {
                duration: Duration::from_secs(*seconds),
                poll_interval: Duration::from_millis(250),
                roots: roots.clone(),
                pid_filter: *pid,
            };
            let rep = savelens_discovery::hunter::hunt(&cfg, report.as_ref(), &opts)?;
            if json {
                return print_json(&rep);
            }
            for p in &rep.processes {
                println!("{} pid {} {} {}", cyan("game process"), p.pid, bold(&p.name), dim(&p.reason));
            }
            for c in rep.candidates.iter().take(20) {
                println!("{:>6.1}  {}  {}", c.score, bold(&c.path.display().to_string()), dim(&c.reasons.join(", ")));
            }
            if rep.candidates.is_empty() {
                println!("{}", yellow("no candidate save file was written during the hunt"));
            }
            Ok(())
        }
        Command::Track { name, dir, patterns, app_id } => {
            let engine = open_engine(&cli)?;
            let (g, c) = engine.track(name, *app_id, dir, patterns, "manual")?;
            let files = engine.tracked_files(&g)?;
            println!(
                "{} tracking {} (#{}) at {} — {} file(s)",
                green("✔"),
                bold(&g.name),
                g.id,
                g.save_root.display(),
                files.len()
            );
            if let Some(c) = c {
                println!("  import commit #{}: {}", c.id, c.summary);
            }
            Ok(())
        }
        Command::Untrack { game: q } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            engine.store().remove_game(g.id)?;
            println!("{} {} is no longer tracked (run `savelens gc` to reclaim space)", green("✔"), g.name);
            Ok(())
        }
        Command::Games => {
            let engine = open_engine(&cli)?;
            let games = engine.store().games()?;
            if json {
                return print_json(&games);
            }
            if games.is_empty() {
                println!("no games tracked yet — try `savelens discover` or `savelens track <name> <dir>`");
            }
            for g in games {
                println!(
                    "{:>3}  {}  {}  {} {} {}",
                    g.id,
                    bold(&g.name),
                    dim(&g.save_root.display().to_string()),
                    cyan(&g.current_branch),
                    if g.watch_enabled { green("watch") } else { dim("no-watch") },
                    if g.trainer_enabled { magenta("trainer") } else { String::new() }
                );
            }
            Ok(())
        }
        Command::Status { game: q } => {
            let engine = open_engine(&cli)?;
            let stats = engine.store().stats()?;
            let daemon = engine.store().daemon_status(10_000)?;
            match q {
                None => {
                    if json {
                        return print_json(&serde_json::json!({ "stats": stats, "daemon": daemon }));
                    }
                    println!("store      {}", engine.store().root().display());
                    println!(
                        "storage    {} logical, {} on disk ({:.1}x dedup), {} commits, {} safety backups",
                        bytes(stats.logical_bytes),
                        bytes(stats.physical_bytes),
                        stats.dedup_ratio(),
                        stats.commits,
                        stats.safety_backups
                    );
                    match daemon {
                        Some(d) => println!("daemon     {} (pid {}, {})", green("running"), d.pid, d.info),
                        None => println!("daemon     {}", dim("not running")),
                    }
                    Ok(())
                }
                Some(q) => {
                    let g = game(&engine, q)?;
                    let files = engine.tracked_files(&g)?;
                    if json {
                        return print_json(&serde_json::json!({ "game": g, "files": files }));
                    }
                    println!("{} (#{}) branch {}", bold(&g.name), g.id, cyan(&g.current_branch));
                    println!("{}", dim(&g.save_root.display().to_string()));
                    let head = engine.store().branch_head(g.id, &g.current_branch)?;
                    for f in files {
                        let opened = engine.open_version(g.id, WORKING, &f.rel_path);
                        let committed = match head {
                            Some(h) => engine.store().read_commit_file(h, &f.rel_path).ok(),
                            None => None,
                        };
                        let state = match (&committed, std::fs::read(&f.abs_path).ok()) {
                            (Some(c), Some(d)) if *c == d => green("clean"),
                            (Some(_), Some(_)) => yellow("modified"),
                            (None, _) => yellow("untracked"),
                            (_, None) => red("missing"),
                        };
                        println!("  {:<40} {:>10} {state}", f.rel_path, bytes(f.size));
                        match opened {
                            Ok(o) => println!("    {}", dim(&o.envelope().badges().join(" · "))),
                            Err(e) => println!("    {}", red(&e.to_string())),
                        }
                    }
                    Ok(())
                }
            }
        }
        Command::Snapshot { game: q, message } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            match engine.snapshot(g.id, message, CommitKind::Manual)? {
                Some(c) => println!("{} commit #{} on {}: {}", green("✔"), c.id, c.branch, c.summary),
                None => println!("nothing changed since the last commit"),
            }
            Ok(())
        }
        Command::Log { game: q, branch, limit, full } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let log = engine.log(g.id, branch.as_deref(), *limit)?;
            if json {
                return print_json(&log);
            }
            for c in log {
                println!(
                    "{} {} {} {} {}",
                    yellow(&format!("#{:<5}", c.id)),
                    dim(&format_ts(c.created_at)),
                    kind_badge(c.kind),
                    c.message,
                    if c.summary.is_empty() { String::new() } else { cyan(&format!("— {}", c.summary)) }
                );
                if *full {
                    for cl in engine.store().changelogs(c.id)? {
                        println!("        {}", bold(&cl.rel_path));
                        for l in cl.lines {
                            println!("          {l}");
                        }
                    }
                }
            }
            Ok(())
        }
        Command::Diff { game: q, from, to, file } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let diffs = engine.diff(g.id, from, to, file.as_deref())?;
            if json {
                return print_json(&diffs);
            }
            for d in diffs {
                let status = match d.status {
                    FileStatus::Added => green("added"),
                    FileStatus::Removed => red("removed"),
                    FileStatus::Modified => yellow("modified"),
                    FileStatus::Unchanged => dim("unchanged"),
                };
                println!("{} {status} {}", bold(&d.rel_path), dim(&d.summary));
                for l in d.lines {
                    println!("    {l}");
                }
            }
            Ok(())
        }
        Command::Inspect { path, depth } => {
            let engine = open_engine(&cli)?;
            let opened = engine.inspect_path(path)?;
            if json {
                return print_json(&opened.ast());
            }
            let env = opened.envelope();
            println!("{}  {} bytes", bold(&path.display().to_string()), opened.original().len());
            println!("codec      {}", cyan(&env.codec));
            for l in &env.layers {
                println!("layer      {l}");
            }
            println!("round-trip {:?}", env.round_trip);
            println!("edit mode  {:?}", opened.edit_mode());
            for n in &env.notes {
                println!("note       {}", dim(n));
            }
            println!();
            print_tree(&opened.root(), "$", 0, *depth, 0);
            Ok(())
        }
        Command::Get { game: q, file, path, rev } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let root = engine.root_at(g.id, rev, file)?;
            let p = NodePath::parse(path)?;
            let node = root.resolve(&p)?;
            if json {
                return print_json(node);
            }
            print_tree(node, &p.to_string(), 0, 4, 0);
            Ok(())
        }
        Command::Edit { game: q, file, target, value, raw } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let loc = parse_location(target)?;
            let t = match loc {
                Location::Ast { path } => EditTarget::Path { path, value: value.clone() },
                raw_loc @ Location::Raw { .. } => {
                    EditTarget::Location { location: raw_loc, value: ScalarValue::parse(value)? }
                }
            };
            let allow_raw = *raw || matches!(t, EditTarget::Location { .. });
            let rep = engine.edit_value(g.id, file, &t, allow_raw)?;
            report_edit(&rep);
            Ok(())
        }
        Command::Scan(a) => {
            let engine = open_engine(&cli)?;
            run_scan(&engine, a, json)
        }
        Command::Pin { game: q, file, alias, location } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let loc = parse_location(location)?;
            let e = engine.pin(g.id, file, alias, &loc)?;
            println!("{} `{}` → {} in {}", green("✔"), e.alias, e.location, e.rel_path);
            Ok(())
        }
        Command::What { action } => {
            let engine = open_engine(&cli)?;
            crate::what::run(&engine, action.as_ref(), json, |q| game(&engine, q).map(|g| g.id))
        }
        Command::Cheats { game: q } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let vals = engine.cheat_values(g.id)?;
            if json {
                return print_json(&vals);
            }
            println!("{} trainer {}", bold(&g.name), if g.trainer_enabled { magenta("ON") } else { dim("off") });
            for v in vals {
                let cur = v.current.map(|c| c.to_string()).unwrap_or_else(|| red("?"));
                let frozen = match (v.entry.freeze_enabled, v.entry.frozen_value) {
                    (true, Some(f)) => magenta(&format!("frozen at {f}")),
                    (false, Some(f)) => dim(&format!("freeze off ({f})")),
                    _ => String::new(),
                };
                println!(
                    "  {:<16} {:>14}  {}  {} {}",
                    bold(&v.entry.alias),
                    green(&cur),
                    frozen,
                    dim(&v.entry.location.to_string()),
                    dim(&v.entry.rel_path)
                );
                if let Some(e) = v.error {
                    println!("    {}", red(&e));
                }
            }
            Ok(())
        }
        Command::Set { game: q, alias, value } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let rep = engine.set_cheat(g.id, alias, ScalarValue::parse(value)?)?;
            report_edit(&rep);
            Ok(())
        }
        Command::Freeze { game: q, alias, value, off } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let v = value.as_deref().map(ScalarValue::parse).transpose()?;
            engine.freeze(g.id, alias, v, !*off)?;
            let e = engine.store().cheat_by_alias(g.id, alias)?;
            println!(
                "{} `{alias}` {}",
                green("✔"),
                if *off {
                    "unfrozen".to_string()
                } else {
                    format!("frozen at {}", e.frozen_value.map(|v| v.to_string()).unwrap_or_default())
                }
            );
            if !g.trainer_enabled && !*off {
                println!("  {}", yellow(&format!("the trainer is off: `savelens trainer {} on`", g.id)));
            }
            Ok(())
        }
        Command::Trainer { game: q, state, apply } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let on = state == "on";
            engine.store().set_trainer_enabled(g.id, on)?;
            println!("{} disk trainer {} for {}", green("✔"), if on { magenta("ON") } else { dim("off") }, g.name);
            if *apply && on {
                for f in engine.tracked_files(&g)? {
                    if let Some(rep) = engine.inject_trainer(g.id, &f.rel_path)? {
                        println!(
                            "  {} {}: {} in {:.1} ms",
                            green("✔"),
                            rep.rel_path,
                            rep.applied.join(", "),
                            rep.elapsed_ms
                        );
                    }
                }
            }
            Ok(())
        }
        Command::CherryPick { game: q, rev, file, paths, merge, strict } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let paths = paths.iter().map(|p| NodePath::parse(p)).collect::<Result<Vec<_>, _>>()?;
            let opts = CherryPickOptions {
                mode: if *merge { CherryPickMode::DeepMerge } else { CherryPickMode::Replace },
                allow_structure_change: !*strict,
                ..CherryPickOptions::default()
            };
            let rep = engine.cherry_pick(g.id, rev, file, &paths, &opts)?;
            report_edit(&rep);
            Ok(())
        }
        Command::Checkout { game: q, rev, files } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let sel = if files.is_empty() { None } else { Some(files.as_slice()) };
            let rep = engine.checkout(g.id, rev, sel)?;
            println!(
                "{} restored #{}: {} written, {} unchanged",
                green("✔"),
                rep.target,
                rep.written.len(),
                rep.unchanged.len()
            );
            for (f, t) in rep.written.iter().zip(&rep.safety_tags) {
                println!("  {f}  {}", dim(&format!("backup {t}")));
            }
            Ok(())
        }
        Command::Branch { game: q, name, from, switch, delete } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            match name {
                None => {
                    for b in engine.store().branches(g.id)? {
                        let mark = if b.name == g.current_branch { green("*") } else { " ".into() };
                        println!(
                            "{mark} {} {}",
                            bold(&b.name),
                            dim(&b.head.map(|h| format!("#{h}")).unwrap_or_else(|| "(empty)".into()))
                        );
                    }
                }
                Some(n) if *delete => {
                    engine.store().delete_branch(g.id, n)?;
                    println!("{} deleted branch {n}", green("✔"));
                }
                Some(n) => {
                    let b = engine.create_branch(g.id, n, from.as_deref())?;
                    println!(
                        "{} branch {} at {}",
                        green("✔"),
                        b.name,
                        b.head.map(|h| format!("#{h}")).unwrap_or_default()
                    );
                    if *switch {
                        let rep = engine.switch_branch(g.id, n)?;
                        println!("  switched ({} file(s) written)", rep.written.len());
                    }
                }
            }
            Ok(())
        }
        Command::Switch { game: q, branch } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let rep = engine.switch_branch(g.id, branch)?;
            println!("{} on branch {} ({} file(s) written)", green("✔"), bold(branch), rep.written.len());
            Ok(())
        }
        Command::Tag { game: q, name, rev } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            match name {
                None => {
                    for t in engine.store().tags(g.id)? {
                        println!("{} → #{} {}", bold(&t.name), t.commit_id, dim(&format_ts(t.created_at)));
                    }
                }
                Some(n) => {
                    let id = engine.store().resolve_rev(g.id, rev)?;
                    engine.store().create_tag(g.id, n, id)?;
                    println!("{} tag {n} → #{id}", green("✔"));
                }
            }
            Ok(())
        }
        Command::PanicRestore { game: q, via_daemon } => {
            let engine = open_engine(&cli)?;
            let gid = match q {
                Some(q) => Some(game(&engine, q)?.id),
                None => None,
            };
            if *via_daemon {
                if engine.store().daemon_status(10_000)?.is_none() {
                    bail!("no running daemon; run without --via-daemon");
                }
                engine.store().push_request("panic-restore", &serde_json::json!({ "game": gid }))?;
                println!("{} panic restore requested from the daemon", green("✔"));
                return Ok(());
            }
            let scope = match gid {
                Some(id) => PanicScope::Game(id),
                None => PanicScope::Latest,
            };
            let rep = engine.panic_restore(&scope)?;
            if rep.removed {
                println!(
                    "{} {} removed (it did not exist before SaveLens touched it)",
                    red("PANIC RESTORE"),
                    rep.path.display()
                );
            } else {
                println!(
                    "{} {} restored to {} (blake3 {})",
                    red("PANIC RESTORE"),
                    bold(&rep.path.display().to_string()),
                    rep.restored_tag,
                    dim(&rep.restored_hash[..16.min(rep.restored_hash.len())])
                );
            }
            println!("  {}", dim(&format!("the replaced state is kept as {}", rep.replaced_state_tag)));
            Ok(())
        }
        Command::Rollback { game: q } => {
            let engine = open_engine(&cli)?;
            let g = game(&engine, q)?;
            let rep = engine.quick_rollback(g.id)?;
            println!("{} rolled back to #{} ({} file(s))", green("✔"), rep.target, rep.written.len());
            Ok(())
        }
        Command::Backups { game: q, limit } => {
            let engine = open_engine(&cli)?;
            let gid = match q {
                Some(q) => Some(game(&engine, q)?.id),
                None => None,
            };
            let list = engine.store().safety_backups(gid, *limit)?;
            if json {
                return print_json(&list);
            }
            for b in list {
                println!(
                    "{}  {}  {}  {}",
                    yellow(&b.tag),
                    dim(&format_ts(b.created_at)),
                    b.abs_path.display(),
                    dim(&format!("{}{}", b.reason, if b.verified { " ✔" } else { "" }))
                );
            }
            Ok(())
        }
        Command::RestoreBackup { tag } => {
            let engine = open_engine(&cli)?;
            let m = engine.store().restore_safety_tag(tag)?;
            println!(
                "{} {} restored from {tag} (previous state kept as {})",
                green("✔"),
                m.abs_path.display(),
                m.safety.tag
            );
            Ok(())
        }
        Command::Gc => {
            let engine = open_engine(&cli)?;
            let (b, c) = engine.store().gc()?;
            println!("{} removed {b} blob(s) and {c} chunk(s)", green("✔"));
            Ok(())
        }
        Command::Verify => {
            let engine = open_engine(&cli)?;
            let bad = engine.store().verify_all()?;
            if bad.is_empty() {
                println!("{} every stored blob verified", green("✔"));
                Ok(())
            } else {
                for (h, e) in &bad {
                    println!("{} {h}: {e}", red("✘"));
                }
                bail!("{} corrupted blob(s)", bad.len())
            }
        }
        Command::Config { init } => {
            let engine = open_engine(&cli)?;
            let dir = engine.store().root();
            if *init {
                if dir.join(SaveLensConfig::FILE).exists() {
                    bail!("{} already exists", dir.join(SaveLensConfig::FILE).display());
                }
                SaveLensConfig::default().save(dir)?;
                println!("{} wrote {}", green("✔"), dir.join(SaveLensConfig::FILE).display());
            }
            println!("{}", toml_like(engine.config())?);
            Ok(())
        }
        Command::Keys => {
            let engine = open_engine(&cli)?;
            let keys = engine.pipeline().keys();
            for s in keys.sources() {
                println!("{}", dim(&format!("source: {s}")));
            }
            for p in keys.profiles() {
                println!(
                    "{:<20} {:<14} apps {:?} files {:?}{} {}",
                    bold(&p.name),
                    format!("{:?}", p.scheme),
                    p.app_ids,
                    p.file_patterns,
                    if p.generic { " generic" } else { "" },
                    dim(&p.description)
                );
            }
            Ok(())
        }
        Command::Codecs => {
            let engine = open_engine(&cli)?;
            for (id, name) in engine.pipeline().registry().describe() {
                println!("{:<12} {}", bold(id), name);
            }
            Ok(())
        }
    }
}

fn toml_like(cfg: &SaveLensConfig) -> Result<String> {
    Ok(serde_json::to_string_pretty(cfg)?)
}

fn report_edit(rep: &savelens_watcher::EditReport) {
    if rep.no_op {
        println!("nothing to do: the file already holds these values");
        return;
    }
    println!(
        "{} {} updated ({:?} mode){}",
        green("✔"),
        bold(&rep.rel_path),
        rep.mode,
        rep.commit.map(|c| format!(", commit #{c}")).unwrap_or_default()
    );
    for c in &rep.changes {
        println!("    {c}");
    }
    if let Some(t) = &rep.safety_tag {
        println!("  {}", dim(&format!("pre-mutation backup: {t}  (undo: savelens panic-restore)")));
    }
}

fn watch(engine: Engine, games: Option<Vec<i64>>, hotkeys: bool, json: bool) -> Result<()> {
    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    ctrlc::set_handler(move || {
        let _ = tx.try_send(());
    })
    .context("cannot install the Ctrl+C handler")?;
    let handle = Daemon::spawn(engine, DaemonOptions { hotkeys, games, ..DaemonOptions::default() })?;
    loop {
        crossbeam_channel::select! {
            recv(rx) -> _ => break,
            recv(handle.events()) -> ev => {
                let Ok(ev) = ev else { break };
                if json {
                    println!("{}", serde_json::to_string(&ev)?);
                    continue;
                }
                print_event(&ev);
                if matches!(ev, DaemonEvent::Stopped) { break; }
            }
        }
    }
    handle.stop()?;
    Ok(())
}

fn print_event(ev: &DaemonEvent) {
    let line = match ev {
        DaemonEvent::Started { games, hotkeys } => {
            let mut s = format!("{} watching {games} game(s)", green("●"));
            for h in hotkeys {
                s.push_str(&format!("\n  {}", dim(h)));
            }
            s
        }
        DaemonEvent::Watching { game, root } => {
            format!("{} {} {}", cyan("watch"), bold(game), dim(&root.display().to_string()))
        }
        DaemonEvent::Unwatched { game } => format!("{} {game}", dim("unwatch")),
        DaemonEvent::Committed { game, rel_path, commit, summary, .. } => {
            format!("{} {} {} #{commit} {}", cyan("commit"), bold(game), rel_path, green(summary))
        }
        DaemonEvent::Unchanged { game, rel_path } => format!("{} {game} {rel_path}", dim("same")),
        DaemonEvent::TrainerApplied { game, rel_path, applied, elapsed_ms } => {
            format!("{} {} {} {} in {elapsed_ms:.1} ms", magenta("trainer"), bold(game), rel_path, applied.join(", "))
        }
        DaemonEvent::Locked { path } => {
            format!("{} {} (read-only snapshot, no injection)", yellow("locked"), path.display())
        }
        DaemonEvent::PanicRestored { path, tag } => format!("{} {} → {tag}", red("PANIC RESTORE"), path.display()),
        DaemonEvent::Snapshot { game, commit } => format!(
            "{} {game} {}",
            cyan("snapshot"),
            commit.map(|c| format!("#{c}")).unwrap_or_else(|| "nothing changed".into())
        ),
        DaemonEvent::Rollback { game, files } => format!("{} {game} {}", yellow("rollback"), files.join(", ")),
        DaemonEvent::TrainerToggled { game, enabled } => {
            format!("{} {game} {}", magenta("trainer"), if *enabled { "ON" } else { "off" })
        }
        DaemonEvent::Questions { game, rel_path, questions, .. } => {
            let mut s = format!("{} {} {rel_path}", yellow("what?"), bold(game));
            for q in questions {
                s.push_str(&format!(
                    "\n  #{} {}  {}",
                    q.id,
                    q.describe(),
                    dim(&format!("savelens what answer {} <category>", q.id))
                ));
            }
            s
        }
        DaemonEvent::OverlayToggleRequested => dim("overlay toggle requested"),
        DaemonEvent::Warning { message } => yellow(&format!("! {message}")),
        DaemonEvent::Error { message } => red(&format!("✘ {message}")),
        DaemonEvent::Stopped => dim("daemon stopped"),
    };
    println!("{line}");
}

#[cfg(feature = "tui")]
fn run_tui(engine: Engine) -> Result<()> {
    savelens_tui::run(engine)
}

#[cfg(not(feature = "tui"))]
fn run_tui(_engine: Engine) -> Result<()> {
    bail!("this build of savelens was compiled without the `tui` feature")
}

#[cfg(feature = "gui")]
fn run_gui(engine: Engine) -> Result<()> {
    savelens_gui::run(engine)
}

#[cfg(not(feature = "gui"))]
fn run_gui(_engine: Engine) -> Result<()> {
    bail!("this build of savelens was compiled without the `gui` feature")
}

#[cfg(feature = "overlay")]
fn run_overlay(engine: Engine, game: Option<i64>, opacity: f32, daemon: bool, hotkeys: bool) -> Result<()> {
    savelens_overlay::run(
        engine,
        savelens_overlay::OverlayOptions { game, opacity: opacity.clamp(0.2, 1.0), embedded_daemon: daemon, hotkeys },
    )
}

#[cfg(not(feature = "overlay"))]
fn run_overlay(_engine: Engine, _game: Option<i64>, _opacity: f32, _daemon: bool, _hotkeys: bool) -> Result<()> {
    bail!("this build of savelens was compiled without the `overlay` feature")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_parse() {
        assert_eq!(
            parse_location("Player.Gold").unwrap(),
            Location::Ast { path: NodePath::parse("Player.Gold").unwrap() }
        );
        assert_eq!(
            parse_location("$@0x1a4:i32le").unwrap(),
            Location::Raw { region: NodePath::root(), offset: 0x1a4, ty: ScanType::I32, endian: Endianness::Little }
        );
        assert_eq!(
            parse_location("Blob@16:f32be").unwrap(),
            Location::Raw {
                region: NodePath::parse("Blob").unwrap(),
                offset: 16,
                ty: ScanType::F32,
                endian: Endianness::Big
            }
        );
        assert!(parse_location("$@16").is_err());
        assert!(parse_location("$@zz:i32").is_err());
        // Display form round-trips.
        let l = parse_location("$@0x10:u8").unwrap();
        assert_eq!(parse_location(&l.to_string()).unwrap(), l);
    }
}
