//! `savelens` — the unified SaveLens binary.

mod cli;
mod commands;
mod style;

use clap::Parser;

fn main() {
    let args = cli::Cli::parse();
    let filter = match args.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SAVELENS_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_writer(std::io::stderr)
        .try_init();
    if let Err(e) = commands::run(args) {
        eprintln!("{} {e:#}", style::red("error:"));
        std::process::exit(1);
    }
}
