use crate::config::{self, Config, Target};
use crate::date;
use crate::engine::{self, Engine};
use crate::git::Git;
use crate::guard;
use crate::lock::Lock;
use crate::log;
use crate::nix::{self, Nix};
use crate::pins::{Kind, Pin, PinsFile, Reason};
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
pub struct BuildArgs {
    #[command(flatten)]
    pub at: Where,
    /// Run nix flake update first; only the named inputs when given.
    #[arg(long, num_args = 0.., value_name = "INPUT")]
    pub update: Option<Vec<String>>,
    /// Show what would be pinned and unpinned, then put every file back.
    #[arg(long)]
    pub dry_run: bool,
    /// Leave flake.lock and pins.json uncommitted.
    #[arg(long)]
    pub no_commit: bool,
    /// Hold the machine's own host like any other when it cannot be fixed,
    /// instead of giving up; for a machine that builds a fleet.
    #[arg(long)]
    pub hold_local: bool,
    /// Point this symlink at the machine's own system once it built, and
    /// remove it otherwise, so a separate step can activate it.
    #[arg(long)]
    pub built_link: Option<PathBuf>,
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    None,
    Switch,
    Boot,
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
/// Returns whether it created the file.
fn ensure_pins(path: &Path, git: Option<&Git>) -> Result<bool> {
    let created = !path.exists();
    if created {
        PinsFile::default().save(path)?;
        log::info(&format!("created {}", path.display()));
    }
    if let Some(git) = git
        && !git.tracked(path)
    {
        git.intent_to_add(path)?;
        log::info(&format!(
            "added {} to the git index so the flake can see it",
            path.display()
        ));
    }
    Ok(created)
}

fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .map(str::to_string)
        })
        .and_then(|l| l.split_whitespace().nth(1).map(|u| u == "0"))
        .unwrap_or(false)
}

/// Activate the built system. With nh it is handed the store path, so it only
/// diffs and activates; nixos-rebuild and home-manager evaluate the flake again.
fn activate(
    target: &Target,
    flake: &Path,
    built: Option<&str>,
    how: Activation,
    switcher: &str,
) -> Result<ExitCode> {
    let verb = match (how, target.kind) {
        (Activation::None, _) => return Ok(ExitCode::SUCCESS),
        (Activation::Switch, _) => "switch",
        (Activation::Boot, Kind::Nixos) => "boot",
        (Activation::Boot, Kind::Home) => {
            bail!("Home Manager has no boot activation; use nixheal switch")
        }
    };
    let installable = format!("{}#{}", flake.display(), target.name);
    let mut cmd = match (switcher, target.kind) {
        ("native", Kind::Nixos) => {
            let mut c = Command::new("nixos-rebuild");
            c.args([verb, "--flake", &installable]);
            if !is_root() {
                c.arg("--sudo");
            }
            c
        }
        ("native", Kind::Home) => {
            let mut c = Command::new("home-manager");
            c.args([verb, "--flake", &installable]);
            c
        }
        (_, kind) => {
            let mut c = Command::new("nh");
            c.args([if kind == Kind::Nixos { "os" } else { "home" }, verb]);
            match (built, kind) {
                (Some(path), _) => c.arg(path),
                (None, Kind::Nixos) => c.arg(flake).args(["-H", &target.name]),
                (None, Kind::Home) => c.arg(flake).args(["-c", &target.name]),
            };
            c
        }
    };
    let program = cmd.get_program().to_string_lossy().into_owned();
    log::info(&format!("{verb} {target} with {program}"));
    let status = cmd
        .status()
        .with_context(|| format!("running {program}; is it installed?"))?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(status.code().unwrap_or(1).clamp(1, 255) as u8)
    })
}

fn remove_link(link: &Path) -> Result<()> {
    match std::fs::remove_file(link) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", link.display()))
        }
        _ => Ok(()),
    }
}

/// Refuse setups where a run would do damage: running as root in a checkout
/// someone else owns, or a flake that is a store path and cannot be written.
fn check_flake(flake: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if flake.starts_with("/nix/store") {
        bail!(
            "{} is in the nix store; point programs.nixheal.flake (or --flake) at your checkout",
            flake.display()
        );
    }
    let owner = std::fs::metadata(flake)
        .with_context(|| format!("reading {}", flake.display()))?
        .uid();
    if is_root() && owner != 0 {
        bail!(
            "{} belongs to uid {owner}; run nixheal as that user, or as root it would leave root-owned files and git objects there (for the timer, set programs.nixheal.autoUpgrade.user)",
            flake.display()
        );
    }
    Ok(())
}

pub fn reconcile(args: &BuildArgs, how: Activation) -> Result<ExitCode> {
    let update = args.update.as_deref();
    let ctx = Ctx::open(&args.at, true)?;
    let cfg = ctx.config.clone().expect("config required");
    let mut targets = cfg.targets();
    if !args.only.is_empty() {
        let unknown: Vec<&String> = args
            .only
            .iter()
            .filter(|n| !targets.iter().any(|t| &t.name == *n))
            .collect();
        if !unknown.is_empty() {
            let known: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
            bail!(
                "--only {} is not among the configured hosts and homes ({})",
                unknown
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                known.join(", ")
            );
        }
        targets.retain(|t| args.only.contains(&t.name));
    }
    if targets.is_empty() {
        bail!("no hosts or homes to build: set programs.nixheal.hosts or homes");
    }
    let local = Some(cfg.local()).filter(|l| targets.contains(l));
    if how == Activation::Boot && local.as_ref().is_some_and(|l| l.kind == Kind::Home) {
        bail!("Home Manager has no boot activation; use nixheal switch");
    }
    // The machine's own host is given up on, not held, unless it builds a
    // fleet: holding it would leave it running an old system unnoticed.
    let guarded = local.clone().filter(|_| !args.hold_local);

    check_flake(&ctx.flake)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;
    if !args.dry_run
        && let Some(link) = &args.built_link
    {
        remove_link(link)?;
    }

    let git = Git::open(&ctx.flake);
    let pins_path = ctx.pins_path();
    let lock_path = ctx.flake.join("flake.lock");
    let orig_lock =
        std::fs::read(&lock_path).with_context(|| format!("reading {}", lock_path.display()))?;
    let created_pins = ensure_pins(&pins_path, git.as_ref())?;
    let orig_pins = if created_pins {
        None
    } else {
        Some(std::fs::read(&pins_path)?)
    };
    guard::arm(guard::Restore {
        lock_path: lock_path.clone(),
        lock: orig_lock.clone(),
        pins_path: pins_path.clone(),
        pins: orig_pins,
        flake: ctx.flake.clone(),
    });

    let mut last_good = std::collections::HashMap::new();
    let old_lock = match &args.old_lock {
        Some(p) => Lock::load(p)?,
        None => {
            for t in &targets {
                if let Some(l) = state::load_last_good(&ctx.flake, Some(t)) {
                    last_good.insert(t.clone(), l);
                }
            }
            match state::load_last_good(&ctx.flake, None) {
                Some(l) => l,
                None => Lock::from_bytes(orig_lock.clone())?,
            }
        }
    };

    let nix = Nix::new(&ctx.flake);
    if let Some(inputs) = update
        && let Err(e) = nix.flake_update(inputs)
    {
        guard::restore()?;
        return Err(e);
    }

    let engine = Engine {
        nix,
        pins_path: pins_path.clone(),
        new_lock: Lock::load(&lock_path)?,
        old_lock,
        last_good,
        targets,
        local: guarded,
        local_target: local.clone(),
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
            guard::restore()?;
            return Err(e);
        }
    };

    if let Some(why) = &report.local_failure {
        guard::restore()?;
        log::change(&format!("not activating: {why}"));
        log::change("restored flake.lock and pins.json; nothing was committed");
        return Ok(ExitCode::from(1));
    }

    if args.dry_run {
        guard::restore()?;
        if report.changes.is_empty() {
            log::change("dry run: no pins would change");
        }
        for c in &report.changes {
            log::change(&format!("dry run: would have {c}"));
        }
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(dir) = &args.gc_roots {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        for (target, drv) in &report.built {
            nix::add_root(drv, &dir.join(target.root_name()))?;
        }
        log::info(&format!(
            "rooted {} system(s) in {}",
            report.built.len(),
            dir.display()
        ));
    }

    if !args.no_commit
        && let Some(git) = &git
    {
        let title = if update.is_some() {
            "nixheal: update flake.lock"
        } else {
            "nixheal: update pins"
        };
        let mut message = title.to_string();
        if !report.changes.is_empty() {
            message.push_str("\n\n");
            message.push_str(&report.changes.join("\n"));
        }
        if git.commit(&[&lock_path, &pins_path], &message)? {
            log::info(&format!("committed {title:?}"));
        }
    }
    guard::disarm();

    // Only now, with the results committed, are these locks known to build.
    for (target, _) in &report.built {
        state::save_last_good(&ctx.flake, Some(target), &engine.new_lock)?;
    }
    if !report.built.is_empty() {
        state::save_last_good(&ctx.flake, None, &engine.new_lock)?;
    }

    let built = match report.local_drv.as_deref().map(nix::out_path) {
        Some(Ok(path)) => Some(path),
        Some(Err(e)) => {
            log::info(&format!("cannot find the built system: {e:#}"));
            None
        }
        None => None,
    };
    if let (Some(link), Some(drv)) = (&args.built_link, &report.local_drv) {
        nix::add_root(drv, link)?;
    }

    match &local {
        Some(target) if report.local_held => {
            if how != Activation::None {
                log::change(&format!("not activating: {target} is held"));
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(target) => activate(target, &ctx.flake, built.as_deref(), how, &cfg.switcher),
        None if how != Activation::None => {
            log::info(&format!(
                "not activating: {} is not among the configured targets",
                cfg.local()
            ));
            Ok(ExitCode::SUCCESS)
        }
        None => Ok(ExitCode::SUCCESS),
    }
}

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
            log::info(&format!(
                "took {} from {}",
                Target::new(kind, name),
                other.display()
            ));
        } else {
            log::info(&format!(
                "kept {}: it comes from a newer lock",
                Target::new(kind, name)
            ));
        }
    }
    file.save(&pins_path)?;
    log::info(&format!(
        "merged {} entr{} into {}",
        taken,
        if taken == 1 { "y" } else { "ies" },
        pins_path.display()
    ));
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
        let stale = |since: &str, manual: bool| {
            let age = date::parse(since).map(|d| today - d);
            let flag = if !manual && age.is_none_or(|a| a > warn) {
                "  STALE"
            } else {
                ""
            };
            (age.map_or("?".into(), |a| a.to_string()), flag)
        };
        if let Some(h) = &entry.held {
            any = true;
            let (age, flag) = stale(&h.since, false);
            println!(
                "{target}\theld\tsince {} ({age}d){flag}\t{}",
                h.since, h.reason
            );
        }
        for (input, attr, pin) in entry.iter() {
            any = true;
            let age = date::parse(&pin.since).map(|d| today - d);
            let (_, flag) = stale(&pin.since, pin.reason == Reason::Manual);
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
    let system = Nix::new(&ctx.flake)
        .eval_raw(&target.system_attr())
        .unwrap_or_else(|_| default_system());
    if !crate::resolve::evaluates_at(&locked, &system, attr)? {
        bail!(
            "{attr} is not a package in {input} at {}; nothing was pinned",
            engine::describe(&locked)
        );
    }
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
