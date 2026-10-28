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
    Build {
        #[command(flatten)]
        build: cmd::BuildArgs,
    },
    /// Build, then switch this machine's own host or home.
    Switch {
        #[command(flatten)]
        build: cmd::BuildArgs,
    },
    /// Build, then make this machine's own host boot into it.
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
    },
    /// Remove a pin so the next run builds the package from the current lock,
    /// or release a held host or home with --held.
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
        #[arg(long, conflicts_with_all = ["attr", "input"])]
        held: bool,
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
        Command::Pin { pick, attr, input } => cmd::pin(at, pick, attr, input.as_deref()),
        Command::Clear {
            pick,
            attr,
            input,
            held,
        } => cmd::clear(at, pick, attr.as_deref(), input.as_deref(), *held),
    };
    result.unwrap_or_else(|e| {
        log::flush_nix_errors();
        eprintln!("nixheal: error: {e:#}");
        ExitCode::from(2)
    })
}
