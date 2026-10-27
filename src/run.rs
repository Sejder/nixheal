use crate::date;
use crate::failures::{Failures, Pin, Reason, write_atomic};
use crate::graph::Ascent;
use crate::lock::Lock;
use crate::logparse::{self, Failure};
use crate::nix::Nix;
use crate::resolve::{self, Matcher};
use anyhow::{Context, Result, anyhow};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const ASCENT_CAP: usize = 20_000;

fn key_value(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .ok_or_else(|| format!("expected NAME=VALUE, got {s:?}"))
}

fn default_system() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

#[derive(clap::Args)]
pub struct Args {
    /// Flake directory whose flake.lock has just been updated.
    #[arg(long, default_value = ".")]
    pub flake: PathBuf,
    /// The last lock that built, i.e. the published one.
    #[arg(long)]
    pub old_lock: PathBuf,
    /// Defaults to failures.json in the flake directory.
    #[arg(long)]
    pub failures: Option<PathBuf>,
    /// NAME=ATTR of an output that must build; repeatable.
    #[arg(long = "output", value_parser = key_value, required = true)]
    pub outputs: Vec<(String, String)>,
    /// INPUT=ATTR of a package set built from that flake input; repeatable.
    #[arg(long = "pkgs", value_parser = key_value, required = true)]
    pub pkgs: Vec<(String, String)>,
    #[arg(long, default_value_t = 8)]
    pub max_rounds: u32,
    /// Give up rather than add more than this many new pins in one run.
    #[arg(long, default_value_t = 10)]
    pub max_new_pins: usize,
    /// Extra attrs that must never be pinned; a trailing * matches a prefix.
    #[arg(long = "deny")]
    pub deny: Vec<String>,
    /// System used to evaluate package sets when checking eval failures.
    #[arg(long, default_value_t = default_system())]
    pub system: String,
}

struct Ctx<'a> {
    args: &'a Args,
    nix: Nix,
    path: PathBuf,
    new_lock: Lock,
    old_lock: Lock,
    inputs: Vec<String>,
    today: i64,
}

enum Outcome {
    Done,
    GaveUp(String),
}

pub fn run(args: &Args) -> Result<ExitCode> {
    let flake = args
        .flake
        .canonicalize()
        .with_context(|| format!("resolving {}", args.flake.display()))?;
    let path = args.failures.clone().unwrap_or_else(|| flake.join("failures.json"));
    let original = std::fs::read(&path).ok();

    let mut inputs = Vec::new();
    for (input, _) in &args.pkgs {
        if !inputs.contains(input) {
            inputs.push(input.clone());
        }
    }

    let ctx = Ctx {
        args,
        nix: Nix::new(&flake),
        new_lock: Lock::load(&flake.join("flake.lock"))?,
        old_lock: Lock::load(&args.old_lock)?,
        path: path.clone(),
        inputs,
        today: date::today(),
    };

    let result = rounds(&ctx);
    if !matches!(result, Ok(Outcome::Done)) {
        restore(&path, original.as_deref())?;
    }
    match result? {
        Outcome::Done => Ok(ExitCode::SUCCESS),
        Outcome::GaveUp(why) => {
            eprintln!("autopin: giving up: {why}");
            eprintln!("autopin: left {} unchanged", path.display());
            Ok(ExitCode::from(1))
        }
    }
}

fn restore(path: &Path, original: Option<&[u8]>) -> Result<()> {
    match original {
        Some(bytes) => write_atomic(path, bytes),
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        },
    }
}

fn rounds(ctx: &Ctx) -> Result<Outcome> {
    let mut state = Failures::load(&ctx.path)?;
    state.lock_sha256 = ctx.new_lock.sha256();

    let mut retrying: BTreeMap<(String, String), Pin> = BTreeMap::new();
    let pinned: Vec<(String, String, Pin)> = state
        .iter()
        .map(|(i, a, p)| (i.to_string(), a.to_string(), p.clone()))
        .collect();
    for (input, attr, pin) in pinned {
        let current = ctx.new_lock.nar_hash(&input);
        if current.as_deref() != Some(pin.checked_nar_hash.as_str()) {
            eprintln!("autopin: retrying {input}.{attr}: {input} moved since it was pinned");
            state.remove(&input, &attr);
            retrying.insert((input, attr), pin);
        }
    }

    let outputs: Vec<String> = ctx.args.outputs.iter().map(|(_, a)| a.clone()).collect();
    let names: Vec<&str> = ctx.args.outputs.iter().map(|(n, _)| n.as_str()).collect();
    let mut new_pins = 0;

    for round in 1..=ctx.args.max_rounds {
        state.save(&ctx.path)?;
        eprintln!("autopin: round {round}: building {} with {} pin(s)", names.join(", "), state.len());

        let outcome = ctx.nix.build(&outputs)?;
        if outcome.success {
            finish(ctx, &state, &retrying);
            state.save(&ctx.path)?;
            return Ok(Outcome::Done);
        }

        let failures = logparse::parse(&outcome.errors);
        if failures.is_empty() {
            return Ok(Outcome::GaveUp("the build failed without a failure autopin recognises".into()));
        }

        let wanted = match targets(ctx, &outputs, &failures, &retrying)? {
            Ok(t) => t,
            Err(why) => return Ok(Outcome::GaveUp(why)),
        };

        for (input, attr, reason, failed_drv) in wanted {
            if resolve::denied(&attr, &ctx.args.deny) {
                return Ok(Outcome::GaveUp(format!("{input}.{attr} fails but is never pinned")));
            }
            if state.get(&input, &attr).is_some() {
                return Ok(Outcome::GaveUp(format!("{input}.{attr} is pinned and still fails")));
            }
            let checked = ctx
                .new_lock
                .nar_hash(&input)
                .ok_or_else(|| anyhow!("{input} is not an input of the new lock"))?;

            let pin = match retrying.remove(&(input.clone(), attr.clone())) {
                Some(mut pin) => {
                    eprintln!("autopin: still failing {input}.{attr}, keeping the pin from {}", pin.since);
                    pin.checked_nar_hash = checked;
                    pin
                }
                None => {
                    let Some(locked) = ctx.old_lock.locked(&input) else {
                        return Ok(Outcome::GaveUp(format!("{input} is not an input of the old lock")));
                    };
                    if locked["narHash"].as_str() == Some(checked.as_str()) {
                        return Ok(Outcome::GaveUp(format!(
                            "{input}.{attr} fails at the last good {input} too, so pinning cannot help"
                        )));
                    }
                    new_pins += 1;
                    if new_pins > ctx.args.max_new_pins {
                        return Ok(Outcome::GaveUp(format!(
                            "more than {} new pins needed in one run",
                            ctx.args.max_new_pins
                        )));
                    }
                    eprintln!(
                        "autopin: pinned {input}.{attr} at {}: {}",
                        describe(&locked),
                        failed_drv.as_deref().unwrap_or("evaluation failed")
                    );
                    Pin {
                        locked,
                        since: date::format(ctx.today),
                        reason,
                        failed_drv,
                        checked_nar_hash: checked,
                    }
                }
            };
            state.insert(&input, &attr, pin);
        }
    }

    Ok(Outcome::GaveUp(format!("still failing after {} rounds", ctx.args.max_rounds)))
}

type Target = (String, String, Reason, Option<String>);

/// The pins this round's failures call for, or why they cannot be pinned.
fn targets(
    ctx: &Ctx,
    outputs: &[String],
    failures: &[Failure],
    retrying: &BTreeMap<(String, String), Pin>,
) -> Result<Result<Vec<Target>, String>> {
    let mut out: Vec<Target> = Vec::new();
    let mut push = |t: Target| {
        if !out.iter().any(|o| o.0 == t.0 && o.1 == t.1) {
            out.push(t);
        }
    };

    for f in failures {
        if let Failure::Eval { attr, .. } = f {
            match eval_input(ctx, attr, retrying)? {
                Some(input) => push((input, attr.clone(), Reason::Eval, None)),
                None => {
                    return Ok(Err(format!(
                        "evaluation fails on {attr:?}, which no last good package set provides"
                    )));
                }
            }
        }
    }

    let failed: Vec<&str> = failures
        .iter()
        .filter_map(|f| match f {
            Failure::Build { drv } => Some(drv.as_str()),
            Failure::Eval { .. } => None,
        })
        .collect();
    if failed.is_empty() {
        return Ok(Ok(out));
    }

    let graph = ctx.nix.show_derivations(outputs)?;
    let mut nodes = HashSet::new();
    for drv in &failed {
        if !graph.nodes.contains_key(*drv) {
            return Ok(Err(format!("{drv} failed but is not in the outputs' closure")));
        }
        match graph.ancestors(drv, ASCENT_CAP) {
            Some(a) => nodes.extend(a),
            None => return Ok(Err(format!("{drv} is a dependency of nearly everything"))),
        }
    }
    let matcher = Matcher::build(&ctx.nix, &graph, &nodes, &ctx.args.pkgs)?;

    for drv in failed {
        let roots = match graph.find_roots(drv, ASCENT_CAP, |p| matcher.matches(p)) {
            Ascent::Roots(r) => r,
            Ascent::Unpinnable { via } => {
                eprintln!("autopin: unpinnable {drv}: reaches {via} without passing a package");
                return Ok(Err(format!("{drv} is not inside any pinnable package")));
            }
            Ascent::TooLarge => return Ok(Err(format!("{drv} is a dependency of nearly everything"))),
        };
        let attrs: BTreeSet<&(String, String)> = roots.iter().flat_map(|r| matcher.attrs(r)).collect();
        for (input, attr) in attrs {
            push((input.clone(), attr.clone(), Reason::Build, Some(drv.to_string())));
        }
    }
    Ok(Ok(out))
}

/// The input whose last good set provides `attr` while the new one does not.
/// A pin under retry counts as last good: the published lock only built
/// because of it.
fn eval_input(ctx: &Ctx, attr: &str, retrying: &BTreeMap<(String, String), Pin>) -> Result<Option<String>> {
    if let Some((input, _)) = retrying.keys().find(|(_, a)| a == attr) {
        return Ok(Some(input.clone()));
    }
    for input in &ctx.inputs {
        let (Some(old), Some(new)) = (ctx.old_lock.locked(input), ctx.new_lock.locked(input)) else {
            continue;
        };
        if old["narHash"] == new["narHash"] {
            continue;
        }
        if resolve::evaluates_at(&old, &ctx.args.system, attr)?
            && !resolve::evaluates_at(&new, &ctx.args.system, attr)?
        {
            return Ok(Some(input.clone()));
        }
    }
    Ok(None)
}

fn describe(locked: &serde_json::Value) -> String {
    locked["rev"]
        .as_str()
        .map(|r| r.chars().take(12).collect())
        .or_else(|| locked["narHash"].as_str().map(str::to_string))
        .unwrap_or_else(|| "?".into())
}

fn finish(ctx: &Ctx, state: &Failures, retrying: &BTreeMap<(String, String), Pin>) {
    for (input, attr) in retrying.keys() {
        eprintln!("autopin: unpinned {input}.{attr}: builds again");
    }
    for (input, attr, pin) in state.iter() {
        let age = date::parse(&pin.since).map(|d| ctx.today - d);
        match age {
            Some(days) => eprintln!("autopin: pin {input}.{attr} is {days} day(s) old"),
            None => eprintln!("autopin: pin {input}.{attr} has an unreadable date {:?}", pin.since),
        }
    }
    eprintln!("autopin: all outputs build with {} pin(s)", state.len());
}
