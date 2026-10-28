use crate::config::{self, Config, ROOT_PREFIX, Target};
use crate::date;
use crate::engine::{self, Engine};
use crate::git::Git;
use crate::guard;
use crate::lock::Lock;
use crate::log;
use crate::nix::{self, Nix};
use crate::pins::{Kind, Pin, PinsFile, Reason};
use crate::state;
use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const PINS: &str = "pins.json";

fn default_system() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

#[derive(clap::Args)]
pub struct Where {
    /// The config file; by default $NIXHEAL_CONFIG, then
    /// ~/.config/nixheal.json, then /etc/nixheal.json.
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        help_heading = "Global options"
    )]
    pub config: Option<PathBuf>,
    /// The flake directory; by default the config's flake, then the current
    /// directory.
    #[arg(
        long,
        global = true,
        value_name = "DIR",
        help_heading = "Global options"
    )]
    pub flake: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct BuildArgs {
    /// Run nix flake update first; only the named inputs when given.
    #[arg(short, long, num_args = 0.., value_name = "INPUT")]
    pub update: Option<Vec<String>>,
    /// Evaluate and build as usual and report what would be pinned and
    /// unpinned, then restore flake.lock and pins.json.
    #[arg(long)]
    pub dry_run: bool,
    /// Commit flake.lock and pins.json afterwards (default:
    /// programs.nixheal.commit, which is off).
    #[arg(long, overrides_with = "no_commit")]
    pub commit: bool,
    /// Don't commit, overriding programs.nixheal.commit.
    #[arg(long, overrides_with = "commit")]
    pub no_commit: bool,

    /// Only these hosts or homes from the config; repeatable, or
    /// comma-separated.
    #[arg(
        long = "only",
        value_name = "NAME",
        value_delimiter = ',',
        help_heading = "Fleet options"
    )]
    pub only: Vec<String>,
    /// Hold this machine's own host like any other when it cannot be fixed,
    /// instead of giving up; for a machine that builds a fleet.
    #[arg(long, help_heading = "Fleet options")]
    pub hold_local: bool,
    /// Point this symlink at this machine's own system once it built, and
    /// remove it otherwise, so a separate step can activate it.
    #[arg(long, value_name = "PATH", help_heading = "Fleet options")]
    pub built_link: Option<PathBuf>,
    /// Keep every system that built alive with a GC root in this directory,
    /// e.g. for a binary cache to serve them. Roots this flake's runs made
    /// there for targets no longer configured are removed; nothing else is.
    #[arg(long, value_name = "DIR", help_heading = "Fleet options")]
    pub gc_roots: Option<PathBuf>,
    /// Exit with 3 when a host or home is held, e.g. so a service's
    /// OnFailure reports it.
    #[arg(long, help_heading = "Fleet options")]
    pub fail_on_hold: bool,

    /// The lock to pin back to; by default the one each target last built with.
    #[arg(long, value_name = "FILE", help_heading = "Tuning")]
    pub old_lock: Option<PathBuf>,
    /// Give up after this many evaluate-and-build rounds.
    #[arg(
        long,
        value_name = "N",
        default_value_t = 8,
        value_parser = clap::value_parser!(u32).range(1..),
        help_heading = "Tuning"
    )]
    pub max_rounds: u32,
    /// Hold a host, or give up on this machine's own, rather than pin more
    /// than this many new packages for it in one run.
    #[arg(long, value_name = "N", default_value_t = 10, help_heading = "Tuning")]
    pub max_new_pins: usize,
    /// The system for eval checks when a target's own cannot be evaluated.
    #[arg(long, value_name = "SYSTEM", default_value_t = default_system(), help_heading = "Tuning")]
    pub system: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    None,
    Switch,
    Boot,
}

#[derive(clap::Args)]
pub struct Pick {
    /// A NixOS host; by default this machine's own host or home.
    #[arg(long, value_name = "NAME", conflicts_with = "home")]
    pub host: Option<String>,
    /// A Home Manager configuration.
    #[arg(long, value_name = "NAME")]
    pub home: Option<String>,
}

struct Ctx {
    config: Option<Config>,
    config_path: Option<PathBuf>,
    flake: PathBuf,
    /// Where the flake directory came from, for error messages.
    source: String,
}

impl Ctx {
    /// The config and flake a command acts on. A config named by --config or
    /// $NIXHEAL_CONFIG must load; one found by itself is skipped with a
    /// warning when the command can do without.
    fn open(at: &Where, need_config: bool) -> Result<Self> {
        let (config, config_path) = match config::locate(at.config.as_deref()) {
            None if need_config => return Err(config::missing()),
            None => (None, None),
            Some(found) => match config::read(&found.path) {
                Ok(c) => {
                    log::set_quiet(c.quiet);
                    (Some(c), Some(found.path))
                }
                Err(e) if need_config || found.explicit => {
                    return Err(e.context(format!("the config file from {}", found.from)));
                }
                Err(e) => {
                    log::warn(&format!("ignoring {}: {e:#}", found.path.display()));
                    (None, None)
                }
            },
        };
        let (flake, source) = match (&at.flake, config.as_ref().and_then(|c| c.flake.clone())) {
            (Some(f), _) => (f.clone(), "from --flake".to_string()),
            (None, Some(f)) => (
                PathBuf::from(f),
                format!(
                    "programs.nixheal.flake in {}",
                    config_path.as_deref().unwrap_or(Path::new("?")).display()
                ),
            ),
            (None, None) => (std::env::current_dir()?, "the current directory".into()),
        };
        let flake = match flake.canonicalize() {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                "flake directory {} does not exist ({source}); pass --flake DIR",
                flake.display()
            ),
            Err(e) => {
                return Err(anyhow!(e).context(format!(
                    "resolving the flake directory {} ({source})",
                    flake.display()
                )));
            }
        };
        Ok(Self {
            config,
            config_path,
            flake,
            source,
        })
    }

    /// Which flake and config a command acts on, said first, since both can
    /// come from far away (/etc/nixheal.json from any directory).
    fn announce(&self) {
        match &self.config_path {
            Some(c) => log::info(&format!(
                "flake {} (config {})",
                self.flake.display(),
                c.display()
            )),
            None => log::info(&format!("flake {}", self.flake.display())),
        }
    }

    fn pins_path(&self) -> PathBuf {
        self.flake.join(PINS)
    }

    /// The target --host or --home names, or this machine's own. A name the
    /// config does not list (and `file` has no entry for) is a typo: it would
    /// write an entry nothing ever reads.
    fn target(&self, pick: &Pick, file: Option<&PinsFile>) -> Result<Target> {
        let target = match (&pick.host, &pick.home, &self.config) {
            (Some(h), _, _) => Target::new(Kind::Nixos, h),
            (_, Some(h), _) => Target::new(Kind::Home, h),
            (None, None, Some(c)) => return Ok(c.local()),
            (None, None, None) => bail!("no config to take the host from; pass --host or --home"),
        };
        let Some(cfg) = &self.config else {
            return Ok(target);
        };
        let mut known = cfg.targets();
        known.push(cfg.local());
        if known.contains(&target)
            || file.is_some_and(|f| f.entry(target.kind, &target.name).is_some())
        {
            return Ok(target);
        }
        let names: Vec<String> = known
            .iter()
            .filter(|t| t.kind == target.kind)
            .map(|t| t.name.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        bail!(
            "{target} is not a configured {} (configured: {}); check --{}",
            target.kind.noun(),
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            },
            target.kind.noun()
        )
    }

    /// The flags that name `target` on the command line, empty for this
    /// machine's own entry.
    fn flag_for(&self, target: &Target) -> String {
        if self.config.as_ref().map(Config::local).as_ref() == Some(target) {
            return String::new();
        }
        match target.kind {
            Kind::Nixos => format!(" --host {}", target.name),
            Kind::Home => format!(" --home {}", target.name),
        }
    }

    /// For an attr that is not pinned on `target`: where it is pinned instead.
    fn pinned_elsewhere(&self, file: &PinsFile, target: &Target, attr: &str) -> String {
        let others: Vec<String> = file
            .entries()
            .filter(|(k, n, e)| {
                !(*k == target.kind && *n == target.name) && !e.inputs_pinning(attr).is_empty()
            })
            .map(|(k, n, _)| {
                let t = Target::new(k, n);
                format!("{t} (pass{})", self.flag_for(&t))
            })
            .collect();
        if others.is_empty() {
            String::new()
        } else {
            format!("; it is pinned on {}", others.join(", "))
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
    log::info(&format!(
        "{} {target} with {program}",
        if verb == "boot" {
            "setting up boot of"
        } else {
            "switching"
        }
    ));
    let status = cmd
        .status()
        .with_context(|| format!("running {program}; is it installed?"))?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(status.code().unwrap_or(1).clamp(1, 255) as u8)
    })
}

/// The file in a --gc-roots directory listing the roots this flake's runs
/// made, so two flakes sharing the directory leave each other's alone.
fn roots_manifest(dir: &Path, flake: &Path) -> PathBuf {
    dir.join(format!(
        ".{ROOT_PREFIX}roots-{}",
        state::key(&flake.to_string_lossy())
    ))
}

/// Root the systems that built, and remove the roots this flake's runs made
/// for targets that are no longer configured (only when `prune`), so their
/// systems can be collected. Roots nixheal did not make are left alone.
fn update_roots(
    dir: &Path,
    flake: &Path,
    built: &[(Target, String)],
    targets: &[Target],
    prune: bool,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (target, drv) in built {
        nix::add_root(drv, &dir.join(target.root_name()))?;
    }
    let manifest = roots_manifest(dir, flake);
    let ours: Vec<String> = std::fs::read_to_string(&manifest)
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let keep: Vec<String> = targets.iter().map(Target::root_name).collect();
    let mut listed: Vec<String> = if prune {
        for name in ours.iter().filter(|n| !keep.contains(n)) {
            let path = dir.join(name);
            if path
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                log::info(&format!(
                    "removed the root {name}: its target is no longer configured"
                ));
            }
        }
        Vec::new()
    } else {
        ours
    };
    for (target, _) in built {
        let name = target.root_name();
        if !listed.contains(&name) {
            listed.push(name);
        }
    }
    listed.sort();
    let mut text = listed.join("\n");
    text.push('\n');
    crate::pins::write_atomic(&manifest, text.as_bytes())
}

fn remove_link(link: &Path) -> Result<()> {
    match std::fs::remove_file(link) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", link.display()))
        }
        _ => Ok(()),
    }
}

/// Refuse setups where a command that writes would do damage or write to the
/// wrong place: a directory that is not a locked flake, running as root in a
/// checkout someone else owns, or a flake that is a store path.
fn check_flake(ctx: &Ctx) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let flake = ctx.flake.as_path();
    if !flake.join("flake.lock").is_file() {
        bail!(
            "no flake.lock in {} ({}); nixheal needs a locked flake: pass --flake DIR, or run nix flake lock there",
            flake.display(),
            ctx.source
        );
    }
    let text = flake.to_string_lossy();
    if text.contains('#') || text.contains('?') {
        bail!(
            "{} contains '#' or '?', which nix reads as part of a flake reference; move the checkout",
            flake.display()
        );
    }
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

/// The counts of a run's changes, as the closing line reports them.
fn tally(changes: &[String]) -> String {
    let count = |prefix: &str| changes.iter().filter(|c| c.starts_with(prefix)).count();
    let mut parts = Vec::new();
    for (prefix, word) in [
        ("pinned ", "pinned"),
        ("unpinned ", "unpinned"),
        ("held ", "held"),
        ("released ", "released"),
    ] {
        let n = count(prefix);
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    if parts.is_empty() {
        "no pins changed".into()
    } else {
        parts.join(", ")
    }
}

pub fn reconcile(at: &Where, args: &BuildArgs, how: Activation) -> Result<ExitCode> {
    let update = args.update.as_deref();
    let ctx = Ctx::open(at, true)?;
    let cfg = ctx.config.clone().expect("config required");
    ctx.announce();
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

    check_flake(&ctx)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;

    let git = Git::open(&ctx.flake);
    let pins_path = ctx.pins_path();
    let lock_path = ctx.flake.join("flake.lock");
    let orig_lock =
        std::fs::read(&lock_path).with_context(|| format!("reading {}", lock_path.display()))?;
    if let Some(names) = update {
        let known = Lock::from_bytes(orig_lock.clone())
            .with_context(|| format!("parsing {}", lock_path.display()))?
            .inputs();
        let unknown: Vec<&str> = names
            .iter()
            .filter(|n| !known.contains(n))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            let mut known = known;
            known.sort();
            bail!(
                "--update {}: not an input of flake.lock (its inputs: {})",
                unknown.join(", "),
                known.join(", ")
            );
        }
    }
    if !args.dry_run
        && let Some(link) = &args.built_link
    {
        remove_link(link)?;
    }
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
        return Err(e.context("restored flake.lock and pins.json"));
    }

    let engine = Engine {
        nix,
        pins_path: pins_path.clone(),
        new_lock: Lock::load(&lock_path)?,
        old_lock,
        last_good,
        targets,
        guarded,
        local: local.clone(),
        pins_rel: format!(
            "{}{PINS}",
            git.as_ref().map(|g| g.prefix()).unwrap_or_default()
        ),
        deny: cfg.deny.clone(),
        system: args.system.clone(),
        max_rounds: args.max_rounds,
        max_new_pins: args.max_new_pins,
        warn_after_days: cfg.warn_after_days,
        today: date::today(),
    };
    log::set_dry_run(args.dry_run);
    let report = engine.run();
    log::set_dry_run(false);
    let report = match report {
        Ok(r) => r,
        Err(e) => {
            guard::restore()?;
            return Err(e.context("restored flake.lock and pins.json"));
        }
    };

    if report.local_failure.is_some() {
        guard::restore()?;
        log::change(
            "restored flake.lock and pins.json; nothing was committed or activated. Fix the failure, or on a machine that builds a fleet, pass --hold-local to hold this host and update the rest",
        );
        return Ok(ExitCode::from(1));
    }

    if args.dry_run {
        guard::restore()?;
        log::change(&format!(
            "dry run: {}; restored flake.lock and pins.json, nothing was committed or activated",
            tally(&report.changes)
        ));
        return Ok(ExitCode::SUCCESS);
    }

    // From here the run's results stand or fall together: any error before
    // the commit puts flake.lock and pins.json back.
    let settled = (|| -> Result<String> {
        if let Some(dir) = &args.gc_roots {
            update_roots(
                dir,
                &ctx.flake,
                &report.built,
                &cfg.targets(),
                args.only.is_empty(),
            )?;
            log::info(&format!(
                "rooted {} system(s) in {}",
                report.built.len(),
                dir.display()
            ));
        }
        guard::settle(|| {
            let commit = (cfg.commit || args.commit) && !args.no_commit;
            let Some(git) = &git else {
                return Ok(if commit {
                    "the flake is not in a git repository, so nothing was committed".into()
                } else {
                    "flake.lock and pins.json are not in a git repository".into()
                });
            };
            if !git.changed(&[&lock_path, &pins_path])? {
                return Ok("flake.lock and pins.json unchanged".into());
            }
            if !commit {
                return Ok(
                    "flake.lock and pins.json changed but not committed (commit them yourself, pass --commit, or set programs.nixheal.commit = true)"
                        .into(),
                );
            }
            {
                let title = if git.changed(&[&lock_path])? {
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
                    return Ok(match git.head() {
                        Some(h) => format!("committed {h} {title:?}"),
                        None => format!("committed {title:?}"),
                    });
                }
                Ok("nothing to commit".into())
            }
        })
    })();
    let committed = match settled {
        Ok(c) => c,
        Err(e) => {
            guard::restore()?;
            return Err(e.context("restored flake.lock and pins.json"));
        }
    };
    log::info(&format!(
        "done: {} built, {}; {}",
        report.built.len(),
        tally(&report.changes),
        committed
    ));

    // Only now, with the results kept, are these locks known to build.
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

    let code = match &local {
        Some(target) if report.local_held => {
            if how != Activation::None {
                log::warn(&format!(
                    "not activating: {target} is held; see nixheal status"
                ));
            }
            ExitCode::SUCCESS
        }
        Some(target) => activate(target, &ctx.flake, built.as_deref(), how, &cfg.switcher)?,
        None if how != Activation::None => {
            log::info(&format!(
                "not activating: {} was not built in this run",
                cfg.local()
            ));
            ExitCode::SUCCESS
        }
        None => ExitCode::SUCCESS,
    };
    if args.fail_on_hold && report.held > 0 && code == ExitCode::SUCCESS {
        log::warn(&format!(
            "{} target(s) held; exiting with 3 (--fail-on-hold)",
            report.held
        ));
        return Ok(ExitCode::from(3));
    }
    Ok(code)
}

pub fn merge(at: &Where, other: &Path) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    ctx.announce();
    check_flake(&ctx)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;
    let bytes = std::fs::read(other).with_context(|| format!("reading {}", other.display()))?;
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", other.display()))?;
    if raw.get("version").is_none() {
        bail!(
            "{} is not a nixheal pins file (no version)",
            other.display()
        );
    }
    let incoming =
        PinsFile::parse(&bytes).with_context(|| format!("parsing {}", other.display()))?;
    let pins_path = ctx.pins_path();
    ensure_pins(&pins_path, Git::open(&ctx.flake).as_ref())?;
    let mut file = PinsFile::load(&pins_path)?;
    let mut taken = 0;
    for (kind, name, entry) in incoming.entries() {
        let target = Target::new(kind, name);
        if file.entry(kind, name) == Some(entry) {
            continue;
        }
        if file.merge(kind, name, entry.clone()) {
            taken += 1;
            log::info(&format!("took {target} from {}", other.display()));
        } else {
            log::info(&format!(
                "kept {target}: the entry here comes from a newer lock"
            ));
        }
    }
    file.save(&pins_path)?;
    log::change(&format!(
        "merged {} entr{} from {} into {}{}",
        taken,
        if taken == 1 { "y" } else { "ies" },
        other.display(),
        pins_path.display(),
        if taken > 0 {
            "; the next build applies it"
        } else {
            ""
        }
    ));
    Ok(ExitCode::SUCCESS)
}

fn reason_words(reason: Reason) -> &'static str {
    match reason {
        Reason::Build => "failed to build",
        Reason::Eval => "failed to evaluate",
        Reason::Manual => "pinned by hand",
    }
}

fn age_text(days: Option<i64>) -> String {
    match days {
        Some(0) => "today".into(),
        Some(d) => format!("{d}d"),
        None => "?".into(),
    }
}

pub fn status(at: &Where, json: bool) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    let pins_path = ctx.pins_path();
    let file = PinsFile::load(&pins_path)?;
    let today = date::today();
    let warn = ctx.config.as_ref().map_or(14, |c| c.warn_after_days);
    if json {
        println!("{}", crate::summary::summary(&file, today, warn));
        return Ok(ExitCode::SUCCESS);
    }
    println!("flake {}", ctx.flake.display());
    if !pins_path.exists() {
        println!(
            "no pins.json in {}; nixheal creates it on its first build",
            ctx.flake.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    struct Row {
        target: String,
        what: String,
        reason: String,
        since: String,
        note: Option<String>,
    }
    let mut rows: Vec<Row> = Vec::new();
    let (mut pins, mut held, mut stale) = (0, 0, 0);
    let mut since = |date_text: &str, manual: bool| {
        let age = date::parse(date_text).map(|d| today - d);
        let old = !manual && age.is_none_or(|a| a > warn);
        if old {
            stale += 1;
        }
        format!(
            "{date_text} ({}){}",
            age_text(age),
            if old { " STALE" } else { "" }
        )
    };
    for (kind, name, entry) in file.entries() {
        let target = Target::new(kind, name).to_string();
        if let Some(h) = &entry.held {
            held += 1;
            rows.push(Row {
                target: target.clone(),
                what: "-".into(),
                reason: "whole host held".into(),
                since: since(&h.since, false),
                note: Some(h.reason.clone()),
            });
        }
        for (attr, sets) in entry.packages() {
            pins += 1;
            let pin = sets[0].1;
            let inputs: Vec<&str> = sets.iter().map(|(i, _)| *i).collect();
            rows.push(Row {
                target: target.clone(),
                what: format!("{attr} ({})", inputs.join(", ")),
                reason: reason_words(pin.reason).into(),
                since: since(&pin.since, pin.reason == Reason::Manual),
                note: None,
            });
        }
    }
    if rows.is_empty() {
        println!("no pins and nothing held");
        return Ok(ExitCode::SUCCESS);
    }
    let width = |f: fn(&Row) -> &str, title: &str| {
        rows.iter()
            .map(|r| f(r).len())
            .chain([title.len()])
            .max()
            .unwrap_or(0)
    };
    let (wt, ww, wr) = (
        width(|r| &r.target, "TARGET"),
        width(|r| &r.what, "PACKAGE (SETS)"),
        width(|r| &r.reason, "REASON"),
    );
    println!(
        "{:wt$}  {:ww$}  {:wr$}  SINCE",
        "TARGET", "PACKAGE (SETS)", "REASON"
    );
    for r in &rows {
        println!(
            "{:wt$}  {:ww$}  {:wr$}  {}",
            r.target, r.what, r.reason, r.since
        );
        if let Some(note) = &r.note {
            println!("    {note}");
        }
    }
    println!("{pins} pinned package(s), {held} held, {stale} stale (older than {warn} days).");
    println!(
        "'nixheal why ATTR' explains a pin, 'nixheal clear ATTR' removes one, 'nixheal clear --held --host NAME' releases a hold."
    );
    Ok(ExitCode::SUCCESS)
}

pub fn why(at: &Where, pick: &Pick, attr: &str) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    let file = PinsFile::load(&ctx.pins_path())?;
    let target = ctx.target(pick, Some(&file))?;
    let Some(sets) = file
        .entry(target.kind, &target.name)
        .and_then(|e| e.packages().remove(attr).filter(|s| !s.is_empty()))
    else {
        bail!(
            "{attr} is not pinned on {target}{}",
            ctx.pinned_elsewhere(&file, &target, attr)
        );
    };
    println!("{target}: {attr}");
    let mut failures: Vec<&Pin> = Vec::new();
    for (_, pin) in &sets {
        if !failures.iter().any(|p| p.same_failure(pin) || *p == *pin) {
            failures.push(pin);
        }
    }
    let inputs: Vec<String> = sets
        .iter()
        .map(|(i, p)| format!("{i} at {}", engine::describe_dated(&p.locked)))
        .collect();
    println!("  pinned in: {}", inputs.join(", "));
    for pin in failures {
        println!("  reason:    {}", reason_words(pin.reason));
        println!("  since:     {}", pin.since);
        if let Some(error) = &pin.error {
            println!("  error:     {error}");
        }
        if let Some(drv) = &pin.failed_drv {
            println!("  failed:    {drv}");
        }
        if !pin.trail.is_empty() {
            println!("  trail:     {}", pin.trail.join(" -> "));
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
    println!("  undo with: nixheal clear {attr}{}", ctx.flag_for(&target));
    Ok(ExitCode::SUCCESS)
}

pub fn pin(at: &Where, pick: &Pick, attr: &str, input: Option<&str>) -> Result<ExitCode> {
    if !crate::resolve::is_attr_name(attr) {
        bail!("{attr:?} is not a top-level attribute name");
    }
    let ctx = Ctx::open(at, false)?;
    ctx.announce();
    check_flake(&ctx)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;
    let target = ctx.target(pick, None)?;
    let input = input
        .map(str::to_string)
        .or_else(|| ctx.config.as_ref().and_then(|c| c.input.clone()))
        .unwrap_or_else(|| "nixpkgs".to_string());
    let input = input.as_str();
    let lock = Lock::load(&ctx.flake.join("flake.lock"))?;
    let Some(locked) = lock.locked(input) else {
        let mut known = lock.inputs();
        known.sort();
        bail!(
            "{input} is not an input of flake.lock (its inputs: {}); pass --input",
            known.join(", ")
        );
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
    let at_rev = engine::describe(&locked);
    let entry = file.entry_mut(target.kind, &target.name);
    if let Some(old) = entry.get(input, attr) {
        log::change(&format!(
            "{target}: {input}.{attr} was already pinned ({}, since {}); replacing it with a pin by hand",
            reason_words(old.reason),
            old.since
        ));
    }
    entry.insert(
        input,
        attr,
        Pin {
            locked,
            since: date::format(date::today()),
            reason: Reason::Manual,
            failed_drv: None,
            trail: Vec::new(),
            error: None,
            checked_nar_hash: checked,
        },
    );
    file.save(&pins_path)?;
    log::change(&format!(
        "pinned {target}: {input}.{attr} at {at_rev} by hand; it stays until 'nixheal clear {attr}{}'. The next build, switch or boot applies it",
        ctx.flag_for(&target)
    ));
    Ok(ExitCode::SUCCESS)
}

pub fn clear(
    at: &Where,
    pick: &Pick,
    attr: Option<&str>,
    input: Option<&str>,
    held: bool,
) -> Result<ExitCode> {
    let ctx = Ctx::open(at, false)?;
    ctx.announce();
    check_flake(&ctx)?;
    let _run_lock = guard::lock_flake(&ctx.flake)?;
    let pins_path = ctx.pins_path();
    let mut file = PinsFile::load(&pins_path)?;
    let target = ctx.target(pick, Some(&file))?;
    let elsewhere = attr.map(|a| ctx.pinned_elsewhere(&file, &target, a));
    let entry = file.entry_mut(target.kind, &target.name);
    if held {
        if entry.held.take().is_none() {
            bail!("{target} is not held");
        }
        log::change(&format!(
            "released {target}; the next build, switch or boot builds it with the current lock"
        ));
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
            bail!(
                "{attr} is not pinned on {target}{}",
                elsewhere.unwrap_or_default()
            );
        }
        for i in removed {
            log::change(&format!(
                "cleared {target}: {i}.{attr}; the next build, switch or boot builds it from the current lock, and pins it again if it still fails"
            ));
        }
    }
    file.save(&pins_path)?;
    Ok(ExitCode::SUCCESS)
}
