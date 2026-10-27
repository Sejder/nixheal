use crate::config::{self, Config, Target};
use crate::date;
use crate::engine::{self, Engine};
use crate::git::Git;
use crate::lock::Lock;
use crate::log;
use crate::nix::{self, Nix};
use crate::pins::{Kind, Pin, PinsFile, Reason, write_atomic};
use crate::state;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const PINS: &str = "pins.json";

fn default_system() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

#[derive(clap::Args)]
pub struct Where {
    /// Config file; defaults to ~/.config/nixheal.json, then /etc/nixheal.json.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Flake directory; defaults to the config's flake.
    #[arg(long, global = true)]
    pub flake: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct Build {
    #[command(flatten)]
    pub at: Where,
    /// Show what would be pinned and unpinned, then put every file back.
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub no_switch: bool,
    #[arg(long)]
    pub no_commit: bool,
    /// Only these hosts or homes from the config; repeatable.
    #[arg(long = "only")]
    pub only: Vec<String>,
    /// The last lock that built; defaults to the one nixheal last saw build.
    #[arg(long)]
    pub old_lock: Option<PathBuf>,
    #[arg(long, default_value_t = 8)]
    pub max_rounds: u32,
    /// Give up on a host rather than add more than this many new pins to it.
    #[arg(long, default_value_t = 10)]
    pub max_new_pins: usize,
    /// Used for eval checks when a target's own system cannot be evaluated.
    #[arg(long, default_value_t = default_system())]
    pub system: String,
    /// Keep every system that built alive with a GC root in this directory,
    /// e.g. for a binary cache to serve them.
    #[arg(long)]
    pub gc_roots: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct Pick {
    #[command(flatten)]
    pub at: Where,
    /// A NixOS host; defaults to the machine's own entry.
    #[arg(long, conflicts_with = "home")]
    pub host: Option<String>,
    /// A Home Manager configuration.
    #[arg(long)]
    pub home: Option<String>,
}

struct Ctx {
    config: Option<Config>,
    flake: PathBuf,
}

impl Ctx {
    fn open(at: &Where, need_config: bool) -> Result<Self> {
        let config = match config::load(at.config.as_deref()) {
            Ok(c) => {
                log::set_quiet(c.quiet);
                Some(c)
            }
            Err(e) if need_config => return Err(e),
            Err(_) => None,
        };
        let flake = match (&at.flake, config.as_ref().and_then(|c| c.flake.clone())) {
            (Some(f), _) => f.clone(),
            (None, Some(f)) => PathBuf::from(f),
            (None, None) => std::env::current_dir()?,
        };
        let flake = flake
            .canonicalize()
            .with_context(|| format!("resolving {}", flake.display()))?;
        Ok(Self { config, flake })
    }

    fn pins_path(&self) -> PathBuf {
        self.flake.join(PINS)
    }

    fn target(&self, pick: &Pick) -> Result<Target> {
        match (&pick.host, &pick.home, &self.config) {
            (Some(h), _, _) => Ok(Target::new(Kind::Nixos, h)),
            (_, Some(h), _) => Ok(Target::new(Kind::Home, h)),
            (None, None, Some(c)) => Ok(c.local()),
            (None, None, None) => bail!("no config to take the host from; pass --host or --home"),
        }
    }
}

/// Create the pins file when missing and make sure a git flake can see it.
fn ensure_pins(path: &Path, git: Option<&Git>) -> Result<()> {
    if !path.exists() {
        PinsFile::default().save(path)?;
        log::info(&format!("created {}", path.display()));
    }
    if let Some(git) = git
        && !git.tracked(path)
    {
        git.intent_to_add(path)?;
        log::info(&format!("added {} to the git index so the flake can see it", path.display()));
    }
    Ok(())
}

/// Switch with nh. Given the system nixheal just built, nh only diffs and
/// activates it; otherwise it evaluates and builds the flake itself.
fn switch(target: &Target, flake: &Path, built: Option<&str>) -> Result<ExitCode> {
    let mut nh = Command::new("nh");
    match target.kind {
        Kind::Nixos => nh.args(["os", "switch"]),
        Kind::Home => nh.args(["home", "switch"]),
    };
    match built {
        Some(path) => nh.arg(path),
        None => match target.kind {
            Kind::Nixos => nh.arg(flake).args(["-H", &target.name]),
            Kind::Home => nh.arg(flake).args(["-c", &target.name]),
        },
    };
    log::info(&format!("switching {target} with nh"));
    let status = nh.status().context("running nh; is it installed?")?;
    Ok(if status.success() { ExitCode::SUCCESS } else { ExitCode::from(status.code().unwrap_or(1).clamp(1, 255) as u8) })
}

pub fn reconcile(args: &Build, update: Option<&[String]>) -> Result<ExitCode> {
    let ctx = Ctx::open(&args.at, true)?;
    let cfg = ctx.config.clone().expect("config required");
    let mut targets = cfg.targets();
    if !args.only.is_empty() {
        targets.retain(|t| args.only.contains(&t.name));
    }
    if targets.is_empty() {
        bail!("no hosts or homes to build: set programs.nixheal.hosts or homes");
    }
    let local = Some(cfg.local()).filter(|l| targets.contains(l));
    // Only a machine that is about to switch must not be held: with
    // --no-switch, e.g. a fleet build, it is held like any other target.
    let guarded = local.clone().filter(|_| !args.no_switch);

    let git = Git::open(&ctx.flake);
    let pins_path = ctx.pins_path();
    let lock_path = ctx.flake.join("flake.lock");
    ensure_pins(&pins_path, git.as_ref())?;
    let orig_lock = std::fs::read(&lock_path).with_context(|| format!("reading {}", lock_path.display()))?;
    let orig_pins = std::fs::read(&pins_path)?;
    let restore = || -> Result<()> {
        write_atomic(&lock_path, &orig_lock)?;
        write_atomic(&pins_path, &orig_pins)
    };

    let old_lock = match &args.old_lock {
        Some(p) => Lock::load(p)?,
        None => match state::load_last_good(&ctx.flake) {
            Some(l) => l,
            None => Lock::from_bytes(orig_lock.clone())?,
        },
    };

    let nix = Nix::new(&ctx.flake);
    if let Some(inputs) = update
        && let Err(e) = nix.flake_update(inputs)
    {
        restore()?;
        return Err(e);
    }

    let engine = Engine {
        nix,
        pins_path: pins_path.clone(),
        new_lock: Lock::load(&lock_path)?,
        old_lock,
        targets,
        local: guarded,
        fallback_inputs: cfg.inputs.clone(),
        deny: cfg.deny.clone(),
        system: args.system.clone(),
        max_rounds: args.max_rounds,
        max_new_pins: args.max_new_pins,
        warn_after_days: cfg.warn_after_days,
        today: date::today(),
    };
    let report = match engine.run() {
        Ok(r) => r,
        Err(e) => {
            restore()?;
            return Err(e);
        }
    };

    if let Some(why) = &report.local_failure {
        restore()?;
        log::change(&format!("not switching: {why}"));
        log::change("restored flake.lock and pins.json; nothing was committed");
        return Ok(ExitCode::from(1));
    }

    if args.dry_run {
        restore()?;
        if report.changes.is_empty() {
            log::change("dry run: no pins would change");
        }
        for c in &report.changes {
            log::change(&format!("dry run: would have {c}"));
        }
        return Ok(ExitCode::SUCCESS);
    }

    state::save_last_good(&ctx.flake, &engine.new_lock)?;

    if let Some(dir) = &args.gc_roots {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        for (target, drv) in &report.built {
            nix::add_root(drv, &dir.join(target.root_name()))?;
        }
        log::info(&format!("rooted {} system(s) in {}", report.built.len(), dir.display()));
    }

    if !args.no_commit
        && let Some(git) = &git
    {
        let title = if update.is_some() { "nixheal: update flake.lock" } else { "nixheal: update pins" };
        let mut message = title.to_string();
        if !report.changes.is_empty() {
            message.push_str("\n\n");
            message.push_str(&report.changes.join("\n"));
        }
        if git.commit(&[&lock_path, &pins_path], &message)? {
            log::info(&format!("committed {title:?}"));
        }
    }

    match &local {
        Some(target) if !args.no_switch => {
            let built = match report.local_drv.as_deref().map(nix::out_path) {
                Some(Ok(path)) => Some(path),
                Some(Err(e)) => {
                    log::info(&format!("letting nh evaluate again: {e:#}"));
                    None
                }
                None => None,
            };
            switch(target, &ctx.flake, built.as_deref())
        }
        _ => Ok(ExitCode::SUCCESS),
    }
}

/// Fold another pins file into the flake's, entry by entry; an entry from a
/// newer lock wins, as when two runs wrote the same host.
pub fn merge(at: &Where, other: &Path) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    let pins_path = ctx.pins_path();
    ensure_pins(&pins_path, Git::open(&ctx.flake).as_ref())?;
    let mut file = PinsFile::load(&pins_path)?;
    let incoming = PinsFile::load(other)?;
    let mut taken = 0;
    for (kind, name, entry) in incoming.entries() {
        if file.entry(kind, name) == Some(entry) {
            continue;
        }
        if file.merge(kind, name, entry.clone()) {
            taken += 1;
            log::info(&format!("took {} from {}", Target::new(kind, name), other.display()));
        } else {
            log::info(&format!("kept {}: it comes from a newer lock", Target::new(kind, name)));
        }
    }
    file.save(&pins_path)?;
    log::info(&format!("merged {} entr{} into {}", taken, if taken == 1 { "y" } else { "ies" }, pins_path.display()));
    Ok(ExitCode::SUCCESS)
}

pub fn status(at: &Where, json: bool) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    let file = PinsFile::load(&ctx.pins_path())?;
    let today = date::today();
    let warn = ctx.config.as_ref().map_or(14, |c| c.warn_after_days);
    if json {
        println!("{}", crate::summary::summary(&file, today, warn));
        return Ok(ExitCode::SUCCESS);
    }
    let mut any = false;
    for (kind, name, entry) in file.entries() {
        let target = Target::new(kind, name);
        if let Some(h) = &entry.held {
            any = true;
            println!("{target}\tHELD since {}: {}", h.since, h.reason);
        }
        for (input, attr, pin) in entry.iter() {
            any = true;
            let age = date::parse(&pin.since).map(|d| today - d);
            let flag = if age.is_some_and(|a| a > warn) { "  OLD" } else { "" };
            println!(
                "{target}\t{input}.{attr}\t{}\tsince {} ({}d){flag}",
                pin.reason.as_str(),
                pin.since,
                age.map_or("?".into(), |a| a.to_string())
            );
        }
    }
    if !any {
        println!("no pins");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn why(pick: &Pick, attr: &str) -> Result<ExitCode> {
    let ctx = Ctx::open(&pick.at, false)?;
    let target = ctx.target(pick)?;
    let file = PinsFile::load(&ctx.pins_path())?;
    let Some(entry) = file.entry(target.kind, &target.name) else {
        bail!("{target} has no entry in {PINS}");
    };
    let inputs = entry.inputs_pinning(attr);
    if inputs.is_empty() {
        bail!("{attr} is not pinned on {target}");
    }
    for input in inputs {
        let pin = entry.get(&input, attr).expect("listed input");
        println!("{target}: {input}.{attr}");
        println!("  reason:   {}", pin.reason.as_str());
        println!("  since:    {}", pin.since);
        println!("  pinned at {}", engine::describe(&pin.locked));
        if let Some(drv) = &pin.failed_drv {
            println!("  failed:   {drv}");
        }
        if !pin.trail.is_empty() {
            println!("  trail:    {}", pin.trail.join(" -> "));
        }
        if let Some(drv) = &pin.failed_drv
            && let Some(log) = nix::log_tail(drv, 20)
        {
            println!("  build log (last lines):");
            for line in log.lines() {
                println!("    {line}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn pin(pick: &Pick, attr: &str, input: &str) -> Result<ExitCode> {
    if !crate::resolve::is_attr_name(attr) {
        bail!("{attr:?} is not a top-level attribute name");
    }
    let ctx = Ctx::open(&pick.at, false)?;
    let target = ctx.target(pick)?;
    let lock = Lock::load(&ctx.flake.join("flake.lock"))?;
    let Some(locked) = lock.locked(input) else {
        bail!("{input} is not an input of flake.lock");
    };
    let checked = lock.nar_hash(input).unwrap_or_default();
    let pins_path = ctx.pins_path();
    ensure_pins(&pins_path, Git::open(&ctx.flake).as_ref())?;
    let mut file = PinsFile::load(&pins_path)?;
    let at = engine::describe(&locked);
    file.entry_mut(target.kind, &target.name).insert(
        input,
        attr,
        Pin {
            locked,
            since: date::format(date::today()),
            reason: Reason::Manual,
            failed_drv: None,
            trail: Vec::new(),
            checked_nar_hash: checked,
        },
    );
    file.save(&pins_path)?;
    println!("pinned {target}: {input}.{attr} at {at}; it stays until nixheal clear");
    Ok(ExitCode::SUCCESS)
}

pub fn clear(pick: &Pick, attr: Option<&str>, input: Option<&str>, held: bool) -> Result<ExitCode> {
    let ctx = Ctx::open(&pick.at, false)?;
    let target = ctx.target(pick)?;
    let pins_path = ctx.pins_path();
    let mut file = PinsFile::load(&pins_path)?;
    let entry = file.entry_mut(target.kind, &target.name);
    if held {
        if entry.held.take().is_none() {
            bail!("{target} is not held");
        }
        println!("released {target}");
    } else {
        let Some(attr) = attr else {
            bail!("name an attr to clear, or pass --held");
        };
        let inputs = match input {
            Some(i) => vec![i.to_string()],
            None => entry.inputs_pinning(attr),
        };
        let removed: Vec<String> = inputs
            .into_iter()
            .filter(|i| entry.remove(i, attr).is_some())
            .collect();
        if removed.is_empty() {
            bail!("{attr} is not pinned on {target}");
        }
        for i in removed {
            println!("cleared {target}: {i}.{attr}");
        }
    }
    file.save(&pins_path)?;
    Ok(ExitCode::SUCCESS)
}
