mod cmd;
mod config;
mod date;
mod engine;
mod git;
mod graph;
mod lock;
mod log;
mod logparse;
mod nix;
mod pins;
mod resolve;
mod state;
mod summary;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

/// Update a flake and keep it building: packages that fail are pinned to the
/// nixpkgs they last built with, and unpinned once they build again.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Only report pins, unpins, holds, warnings and errors.
    #[arg(short, long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pin and unpin until every configured host builds, then commit.
    Build {
        #[command(flatten)]
        build: cmd::BuildArgs,
    },
    /// Build, commit, then switch the machine's own host or home.
    Switch {
        #[command(flatten)]
        build: cmd::BuildArgs,
    },
    /// Build, commit, then make the machine's own host boot into it.
    Boot {
        #[command(flatten)]
        build: cmd::BuildArgs,
    },
    /// Merge another pins.json into the flake's; per host, the newer lock wins.
    Merge {
        #[command(flatten)]
        at: cmd::Where,
        file: std::path::PathBuf,
    },
    /// List the pins and held hosts.
    Status {
        #[command(flatten)]
        at: cmd::Where,
        /// Print a flat JSON summary for dashboards instead of the list.
        #[arg(long)]
        json: bool,
    },
    /// Show why a package is pinned: the failed drv, the path up to the
    /// package, and the end of the build log.
    Why {
        #[command(flatten)]
        pick: cmd::Pick,
        attr: String,
    },
    /// Pin a package by hand at the current lock; it stays until cleared.
    Pin {
        #[command(flatten)]
        pick: cmd::Pick,
        attr: String,
        #[arg(long, default_value = "nixpkgs")]
        input: String,
    },
    /// Remove a pin, or release a held host with --held.
    Clear {
        #[command(flatten)]
        pick: cmd::Pick,
        attr: Option<String>,
        #[arg(long)]
        input: Option<String>,
        #[arg(long)]
        held: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    log::set_quiet(cli.quiet);
    let result = match &cli.command {
        Command::Build { build } => cmd::reconcile(build, cmd::Activation::None),
        Command::Switch { build } => cmd::reconcile(build, cmd::Activation::Switch),
        Command::Boot { build } => cmd::reconcile(build, cmd::Activation::Boot),
        Command::Merge { at, file } => cmd::merge(at, file),
        Command::Status { at, json } => cmd::status(at, *json),
        Command::Why { pick, attr } => cmd::why(pick, attr),
        Command::Pin { pick, attr, input } => cmd::pin(pick, attr, input),
        Command::Clear {
            pick,
            attr,
            input,
            held,
        } => cmd::clear(pick, attr.as_deref(), input.as_deref(), *held),
    };
    result.unwrap_or_else(|e| {
        eprintln!("nixheal: error: {e:#}");
        ExitCode::from(2)
    })
}
