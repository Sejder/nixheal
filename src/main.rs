mod cmd;
mod config;
mod date;
mod engine;
mod git;
mod graph;
mod guard;
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

const EXIT_CODES: &str = "Exit status: 0 when everything built or was held, 1 when nixheal gave up on this machine's own host, 2 on errors, 3 with --fail-on-hold when something is held, 130 when interrupted; after a successful build, switch and boot return the switcher's.";

/// Update a flake and keep it building: packages that fail are pinned to the
/// nixpkgs they last built with, and unpinned once they build again.
#[derive(Parser)]
#[command(
    version,
    after_help = "Start with 'nixheal switch --update'. See what is pinned with 'nixheal status', \
                  why with 'nixheal why ATTR', and undo a pin with 'nixheal clear ATTR'."
)]
struct Cli {
    /// Only report pins, unpins, holds, warnings and errors.
    #[arg(short, long, global = true, help_heading = "Global options")]
    quiet: bool,
    /// Report progress even when programs.nixheal.quiet is set.
    #[arg(
        short,
        long,
        global = true,
        conflicts_with = "quiet",
        help_heading = "Global options"
    )]
    verbose: bool,
    #[command(flatten)]
    at: cmd::Where,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pin and unpin until every configured host and home builds.
    #[command(after_help = EXIT_CODES)]
    Build {
        #[command(flatten)]
        build: cmd::BuildArgs,
    },
    /// Build, then switch this machine's own host or home.
    #[command(after_help = EXIT_CODES)]
    Switch {
        #[command(flatten)]
        build: cmd::BuildArgs,
    },
    /// Build, then make this machine's own host boot into it.
    #[command(after_help = EXIT_CODES)]
    Boot {
        #[command(flatten)]
        build: cmd::BuildArgs,
    },
    /// List the pins and held hosts and homes, with their age.
    Status {
        /// Print a JSON summary for dashboards and scripts instead.
        #[arg(long)]
        json: bool,
    },
    /// Show why a package is pinned: the build that failed, how it leads to
    /// the package, and the end of its build log.
    Why {
        #[command(flatten)]
        pick: cmd::Pick,
        /// A top-level package attribute, e.g. firefox.
        #[arg(value_name = "ATTR")]
        attr: String,
    },
    /// Pin a package by hand at the current lock; it stays until cleared.
    /// Applied by the next build, switch or boot.
    Pin {
        #[command(flatten)]
        pick: cmd::Pick,
        /// A top-level package attribute, e.g. firefox.
        #[arg(value_name = "ATTR")]
        attr: String,
        /// The input to pin it in; defaults to programs.nixheal.input.
        #[arg(long, value_name = "INPUT")]
        input: Option<String>,
        /// Pin it at the input's revision in this flake.lock instead of the
        /// current one, e.g. one from git history, for a package that builds
        /// but misbehaves.
        #[arg(long, value_name = "FILE")]
        old_lock: Option<std::path::PathBuf>,
    },
    /// Remove a pin made by hand, have the next run retry one nixheal made
    /// (or remove it with --drop), or release a held host or home with --held.
    #[command(
        visible_alias = "unpin",
        group = clap::ArgGroup::new("what").required(true).args(["attr", "held"])
    )]
    Clear {
        #[command(flatten)]
        pick: cmd::Pick,
        /// The pinned attribute, e.g. firefox.
        #[arg(value_name = "ATTR")]
        attr: Option<String>,
        /// Only clear the pin in this input; by default every input pinning ATTR.
        #[arg(long, value_name = "INPUT")]
        input: Option<String>,
        /// Release the held host or home instead, so the next run builds it
        /// with the current lock.
        #[arg(long, conflicts_with_all = ["attr", "input", "drop"])]
        held: bool,
        /// Remove a pin nixheal made instead of retrying it. If the package
        /// still fails, its host is held until its package set moves.
        #[arg(long)]
        drop: bool,
    },
    /// Fold another pins.json into the flake's; per host or home, the entry
    /// from the newer lock wins.
    Merge {
        /// Another pins.json, e.g. one published by a build host.
        #[arg(value_name = "FILE")]
        file: std::path::PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    log::set_verbose(cli.verbose);
    log::set_quiet(cli.quiet);
    let at = &cli.at;
    let result = match &cli.command {
        Command::Build { build } => cmd::reconcile(at, build, cmd::Activation::None),
        Command::Switch { build } => cmd::reconcile(at, build, cmd::Activation::Switch),
        Command::Boot { build } => cmd::reconcile(at, build, cmd::Activation::Boot),
        Command::Merge { file } => cmd::merge(at, file),
        Command::Status { json } => cmd::status(at, *json),
        Command::Why { pick, attr } => cmd::why(at, pick, attr),
        Command::Pin {
            pick,
            attr,
            input,
            old_lock,
        } => cmd::pin(at, pick, attr, input.as_deref(), old_lock.as_deref()),
        Command::Clear {
            pick,
            attr,
            input,
            held,
            drop,
        } => cmd::clear(at, pick, attr.as_deref(), input.as_deref(), *held, *drop),
    };
    result.unwrap_or_else(|e| {
        log::flush_nix_errors();
        eprintln!("nixheal: error: {e:#}");
        if let Err(e) = guard::restore() {
            eprintln!("nixheal: error: restoring flake.lock and pins.json failed: {e:#}");
        }
        ExitCode::from(2)
    })
}
