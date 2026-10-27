mod date;
mod failures;
mod graph;
mod lock;
mod logparse;
mod nix;
mod resolve;
mod run;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use failures::Failures;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Keep failing packages at their last good nixpkgs so the rest of the system
/// keeps updating, and drop each pin once the package builds again.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build the outputs, pinning what fails and unpinning what builds again.
    Run(run::Args),
    /// List the current pins.
    Status {
        #[arg(long, default_value = "failures.json")]
        failures: PathBuf,
    },
    /// Remove a pin by hand.
    Clear {
        #[arg(long, default_value = "failures.json")]
        failures: PathBuf,
        input: String,
        attr: String,
    },
}

fn status(path: &Path) -> Result<ExitCode> {
    let state = Failures::load(path)?;
    if state.len() == 0 {
        println!("no pins");
        return Ok(ExitCode::SUCCESS);
    }
    let today = date::today();
    for (input, attr, pin) in state.iter() {
        let age = date::parse(&pin.since)
            .map(|d| format!("{}d", today - d))
            .unwrap_or_else(|| "?".into());
        let reason = match pin.reason {
            failures::Reason::Build => "build",
            failures::Reason::Eval => "eval",
        };
        println!(
            "{input}.{attr}\tsince {} ({age})\t{reason}\t{}",
            pin.since,
            pin.failed_drv.as_deref().unwrap_or("-")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn clear(path: &Path, input: &str, attr: &str) -> Result<ExitCode> {
    let mut state = Failures::load(path)?;
    if state.remove(input, attr).is_none() {
        bail!("{input}.{attr} is not pinned");
    }
    state.save(path)?;
    println!("cleared {input}.{attr}");
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Run(args) => run::run(args),
        Command::Status { failures } => status(failures),
        Command::Clear { failures, input, attr } => clear(failures, input, attr),
    };
    result.unwrap_or_else(|e| {
        eprintln!("autopin: error: {e:#}");
        ExitCode::from(2)
    })
}
