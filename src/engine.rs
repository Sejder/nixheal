use crate::config::Target;
use crate::date;
use crate::graph::{Ascent, Graph};
use crate::guard;
use crate::lock::Lock;
use crate::log;
use crate::logparse::{self, Failure};
use crate::nix::{Monitor, Nix};
use crate::pins::{Entry, Held, Kind, Pin, PinsFile, Reason};
use crate::resolve::{self, Matcher};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

const ASCENT_CAP: usize = 20_000;

/// A package more packages than this depend on is never pinned: the overlay
/// would swap it in for all of them, and every one would rebuild from source.
pub const MAX_DEPENDENTS: usize = 25;

pub struct Engine {
    pub nix: Nix,
    pub pins_path: PathBuf,
    pub new_lock: Lock,
    /// The last good lock for targets without one of their own.
    pub old_lock: Lock,
    /// The lock each target last built with, where it is known.
    pub last_good: HashMap<Target, Lock>,
    pub targets: Vec<Target>,
    /// The machine nixheal runs on, when it is one of the targets and must not
    /// be held: its failure aborts the run instead of holding it.
    pub guarded: Option<Target>,
    /// The machine nixheal runs on, when it is one of the targets.
    pub local: Option<Target>,
    /// Where pins.json sits in the flake's source tree: "pins.json", or
    /// "dir/pins.json" for a flake in a subdirectory of its repository.
    pub pins_rel: String,
    pub deny: Vec<String>,
    pub system: String,
    pub max_rounds: u32,
    pub max_new_pins: usize,
    pub warn_after_days: i64,
    pub today: i64,
}

#[derive(Debug, Default)]
pub struct Report {
    /// One line per pin, unpin, hold and release, for logs and commit messages.
    pub changes: Vec<String>,
    pub local_failure: Option<String>,
    /// The local machine's built system, for switching without evaluating it again.
    pub local_drv: Option<String>,
    /// Every target that built, with its system's drv.
    pub built: Vec<(Target, String)>,
    /// The machine's own target was held (only possible with --hold-local).
    pub local_held: bool,
}

/// What a target's own programs.nixheal says, read before anything builds.
#[derive(Debug, serde::Deserialize)]
struct Probe {
    enable: bool,
    name: String,
    pins: Option<String>,
    sets: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum Status {
    Active,
    Done,
    Failed(String),
}

struct Wanted {
    input: String,
    attr: String,
    reason: Reason,
    failed_drv: Option<String>,
    trail: Vec<String>,
}

struct State {
    target: Target,
    base: Option<Entry>,
    entry: Entry,
    held_before: Option<Held>,
    held_lock: Option<Lock>,
    old_lock: Lock,
    retrying: BTreeMap<(String, String), Pin>,
    inputs: Vec<String>,
    drv: Option<String>,
    /// The pins file the builds saw in the round this target built in.
    built_with: Option<Vec<u8>>,
    system: std::cell::OnceCell<String>,
    status: Status,
    new_pins: usize,
}

impl State {
    fn key(&self, input: &str, attr: &str) -> String {
        format!("{}: {input}.{attr}", self.target)
    }
}

impl Engine {
    pub fn run(&self) -> Result<Report> {
        self.check_targets()?;
        let original = PinsFile::load(&self.pins_path)?;
        let mut states: Vec<State> = self
            .targets
            .iter()
            .map(|t| self.init(&original, t))
            .collect::<Result<_>>()?;

        for round in 1..=self.max_rounds {
            let active: Vec<usize> = (0..states.len())
                .filter(|&i| states[i].status == Status::Active)
                .collect();
            if active.is_empty() {
                break;
            }
            let written = self.write(&original, &states)?;
            let names: Vec<String> = active
                .iter()
                .map(|&i| states[i].target.to_string())
                .collect();
            let pins: usize = active.iter().map(|&i| states[i].entry.len()).sum();
            log::info(&format!(
                "round {round}: evaluating and building {} with {pins} pin(s)",
                names.join(", ")
            ));
            let started = std::time::Instant::now();
            let pins_before = pins;
            self.round(&mut states, &active)?;
            for &i in &active {
                if states[i].status == Status::Done {
                    states[i].built_with = Some(written.clone());
                }
            }
            if active
                .iter()
                .any(|&i| matches!(states[i].status, Status::Failed(_)))
            {
                log::flush_nix_errors();
            } else {
                log::discard_nix_errors();
            }
            let pins_after: usize = active.iter().map(|&i| states[i].entry.len()).sum();
            if pins_after > pins_before {
                log::info(&format!(
                    "the errors above were handled: pinned {} package(s), building again",
                    pins_after - pins_before
                ));
            }
            let took = started.elapsed().as_secs();
            if took >= 5 {
                log::info(&format!("round {round} took {took}s"));
            }
        }

        for st in &mut states {
            if st.status == Status::Active {
                st.status =
                    Status::Failed(format!("still failing after {} rounds", self.max_rounds));
            }
        }
        self.finish(original, states)
    }

    /// Every configured target must exist in the flake: a typo in hosts or
    /// homes would otherwise be held as a configuration that does not
    /// evaluate, and recorded in pins.json.
    fn check_targets(&self) -> Result<()> {
        for (kind, output, option) in [
            (Kind::Nixos, "nixosConfigurations", "hosts"),
            (Kind::Home, "homeConfigurations", "homes"),
        ] {
            let wanted: Vec<&Target> = self.targets.iter().filter(|t| t.kind == kind).collect();
            if wanted.is_empty() {
                continue;
            }
            let names: Vec<String> = match self.nix.eval_apply(output, "builtins.attrNames") {
                Ok(v) => serde_json::from_value(v)?,
                Err(e) if e.to_string().contains("does not provide attribute") => {
                    bail!(
                        "programs.nixheal.{option} lists {}, but the flake has no {output}",
                        wanted
                            .iter()
                            .map(|t| t.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
                // The flake's outputs do not evaluate at all: each target is
                // held for that by its probe.
                Err(_) => continue,
            };
            let missing: Vec<&str> = wanted
                .iter()
                .filter(|t| !names.contains(&t.name))
                .map(|t| t.name.as_str())
                .collect();
            if !missing.is_empty() {
                bail!(
                    "programs.nixheal.{option} lists {}, which {output} does not have (it has: {})",
                    missing.join(", "),
                    names.join(", ")
                );
            }
        }
        Ok(())
    }

    fn init(&self, original: &PinsFile, target: &Target) -> Result<State> {
        let base = original.entry(target.kind, &target.name).cloned();
        let mut entry = base.clone().unwrap_or_default();
        let held_before = entry.held.take();
        let held_lock = held_before
            .as_ref()
            .map(|h| Lock::from_json(&h.lock))
            .transpose()?;

        let mut retrying = BTreeMap::new();
        let pinned: Vec<(String, String, Pin)> = entry
            .iter()
            .filter(|(_, _, p)| p.reason != Reason::Manual)
            .map(|(i, a, p)| (i.to_string(), a.to_string(), p.clone()))
            .collect();
        for (input, attr, pin) in pinned {
            if self.new_lock.nar_hash(&input).as_deref() != Some(pin.checked_nar_hash.as_str()) {
                log::info(&format!(
                    "retrying {target}: {input}.{attr}: {input} moved since it was pinned"
                ));
                entry.remove(&input, &attr);
                retrying.insert((input, attr), pin);
            }
        }
        entry.lock_sha256 = self.new_lock.sha256();
        entry.lock_last_modified = self.new_lock.last_modified();

        let (inputs, status) = match self.probe(target)? {
            Ok(sets) => (sets, Status::Active),
            Err(why) => (Vec::new(), Status::Failed(why)),
        };
        let old_lock = self.last_good.get(target).unwrap_or(&self.old_lock).clone();

        Ok(State {
            target: target.clone(),
            base,
            entry,
            held_before,
            held_lock,
            old_lock,
            retrying,
            inputs,
            drv: None,
            built_with: None,
            system: std::cell::OnceCell::new(),
            status,
            new_pins: 0,
        })
    }

    /// Check that the target applies the pins nixheal writes for it, and return
    /// its package sets. A nixheal setting that would make every pin silently
    /// do nothing stops the run. A configuration that does not evaluate at all,
    /// such as one setting an option the update removed, is that target's
    /// problem: it comes back as the reason to hold it.
    fn probe(&self, target: &Target) -> Result<Result<Vec<String>, String>> {
        let apply = "c: { enable = c.enable or false; name = c.name or \"\"; \
                     pins = if (c.pinsFile or null) == null then null else toString c.pinsFile; \
                     sets = builtins.attrNames (c.packageSets or { }); }";
        let probe: Probe = match self.nix.eval_apply(&target.nixheal_config(), apply) {
            Ok(v) => serde_json::from_value(v)?,
            Err(e) => {
                let text = e.to_string();
                if text.contains("attribute 'nixheal' missing")
                    || text.contains("attribute 'programs' missing")
                {
                    bail!("{target} has no programs.nixheal; import the nixheal module there");
                }
                return Ok(Err(format!(
                    "its configuration does not evaluate: {}",
                    logparse::headline(&text)
                )));
            }
        };
        if !probe.enable {
            bail!("programs.nixheal.enable is off in {target}, so its pins would not be applied");
        }
        if probe.name != target.name {
            bail!(
                "{target} has programs.nixheal.name = \"{}\"; it must be \"{}\", its configuration's name, or its pins are never applied",
                probe.name,
                target.name
            );
        }
        match probe.pins.as_deref().map(flake_relative) {
            None => bail!("programs.nixheal.pinsFile is not set in {target}"),
            Some(Some(rel)) if rel == self.pins_rel => {}
            Some(rel) => bail!(
                "{target} has programs.nixheal.pinsFile = {}; it must be {}, the pins.json next to flake.lock, where nixheal writes it",
                rel.unwrap_or_else(|| probe.pins.clone().unwrap_or_default()),
                self.pins_rel
            ),
        }
        for set in &probe.sets {
            if self.new_lock.locked(set).is_none() {
                log::warn(&format!(
                    "{target}: package set {set} is not an input of flake.lock, so its packages cannot be pinned"
                ));
            }
        }
        Ok(Ok(probe.sets))
    }

    /// The pins file as the builds should see it: working entries for active
    /// and finished targets, the original entry for failed ones. Returns what
    /// was written.
    fn write(&self, original: &PinsFile, states: &[State]) -> Result<Vec<u8>> {
        let mut file = original.clone();
        for st in states {
            match (&st.status, &st.base) {
                (Status::Failed(_), Some(e)) => {
                    file.set(st.target.kind, &st.target.name, e.clone())
                }
                (Status::Failed(_), None) => {}
                _ => file.set(st.target.kind, &st.target.name, st.entry.clone()),
            }
        }
        guard::critical(|| file.save(&self.pins_path))?;
        Ok(file.to_bytes())
    }

    fn round(&self, states: &mut [State], active: &[usize]) -> Result<()> {
        // Evaluate each host once and build what evaluated, in one nom view.
        // The drvs are kept, so a failure is traced without evaluating again.
        let mut monitor = Monitor::start();
        let mut built: Vec<(usize, String)> = Vec::new();
        let mut eval_failed: Vec<(usize, Vec<String>)> = Vec::new();
        for &i in active {
            match self
                .nix
                .drv_path(&states[i].target.output(), &mut monitor)?
            {
                Ok(drv) => built.push((i, drv)),
                Err(msgs) => eval_failed.push((i, msgs)),
            }
        }
        let drvs: Vec<String> = built.iter().map(|(_, d)| d.clone()).collect();
        let mut outcome = if drvs.is_empty() {
            None
        } else {
            Some(self.nix.build_drvs(&drvs, &mut monitor)?)
        };
        monitor.finish();
        let graph = match &outcome {
            Some(o) if !o.success => Some(self.nix.show_drvs(&drvs)?),
            _ => None,
        };

        // A download or a builder killed by a signal may have failed for the
        // moment only (the network, memory); one retry tells that apart from
        // a real breakage before anything is pinned or the run stops.
        if let (Some(first), Some(graph)) = (&outcome, &graph)
            && self.looks_transient(&first.errors, graph)
        {
            log::info("a download or a killed builder failed; building once more before deciding");
            let mut monitor = Monitor::start();
            outcome = Some(self.nix.build_drvs(&drvs, &mut monitor)?);
            monitor.finish();
        }

        if let Some(outcome) = outcome {
            if outcome.success {
                if graph.is_some() {
                    log::discard_nix_errors();
                }
                for (i, drv) in &built {
                    states[*i].status = Status::Done;
                    states[*i].drv = Some(drv.clone());
                }
            } else {
                let parsed = logparse::parse(&outcome.errors);
                let infra: Vec<String> = parsed
                    .iter()
                    .filter_map(|f| match f {
                        Failure::Infra { drv, reason } => Some(format!("{drv}: {reason}")),
                        _ => None,
                    })
                    .collect();
                if !infra.is_empty() {
                    bail!(
                        "some builds could not run on this machine, which a pin cannot fix; nothing was pinned or held:\n  {}",
                        infra.join("\n  ")
                    );
                }
                let graph = graph.expect("shown for a failed build");
                let failed = build_failures(&outcome.errors);
                let not_built = logparse::failed_drvs(&outcome.errors);
                let why = outcome
                    .errors
                    .last()
                    .map(|m| logparse::headline(m))
                    .unwrap_or_else(|| "no error message".into());
                // nix failed without naming a derivation (the daemon went away,
                // the store is full): nothing can be said about any host, so
                // none is held for it.
                if not_built.is_empty() {
                    bail!(
                        "the build failed without naming a derivation, so nixheal cannot tell which host it affects; nothing was pinned or held: {why}"
                    );
                }
                for (i, top) in &built {
                    let closure = graph.closure(top);
                    let mine: Vec<&String> =
                        failed.iter().filter(|d| closure.contains(*d)).collect();
                    let st = &mut states[*i];
                    let own_build_failed = closure.iter().any(|d| not_built.contains(d));
                    if mine.is_empty() && (own_build_failed || !crate::nix::is_built(top)?) {
                        st.status = Status::Failed(format!(
                            "its system did not build, and nix reported no failure nixheal can pin: {why}"
                        ));
                    } else if mine.is_empty() {
                        st.status = Status::Done;
                        st.drv = Some(top.clone());
                    } else {
                        let wanted = self.build_wanted(&graph, &closure, &mine, st)?;
                        self.apply(st, wanted);
                    }
                }
            }
        }

        for (i, msgs) in eval_failed {
            let st = &mut states[i];
            let parsed = logparse::parse(&msgs);
            if let Some((attr, reason)) = parsed.iter().find_map(|f| match f {
                Failure::Refused { attr, reason } => Some((attr, reason)),
                _ => None,
            }) {
                st.status = Status::Failed(format!(
                    "nixpkgs refuses {attr} because {reason}; nixheal does not pin around that, so allow it or stop using it"
                ));
                continue;
            }
            let attrs: Vec<String> = parsed
                .into_iter()
                .filter_map(|f| match f {
                    Failure::Eval { attr } => Some(attr),
                    _ => None,
                })
                .collect();
            if attrs.is_empty() {
                let why = msgs
                    .first()
                    .map_or_else(String::new, |m| logparse::headline(m));
                st.status = Status::Failed(format!(
                    "evaluation fails in a way nixheal cannot pin: {why}"
                ));
                continue;
            }
            let mut context: Option<Vec<String>> = None;
            let mut wanted = Vec::new();
            for attr in attrs {
                let mut found = self
                    .eval_input(st, &attr)?
                    .map(|input| (input, attr.clone()));
                if found.is_none() {
                    let context = match &context {
                        Some(c) => c,
                        None => {
                            let trace = self
                                .nix
                                .eval_trace(&format!("{}.drvPath", st.target.output()))?;
                            let full = logparse::eval_context(&[trace]);
                            context.insert(if full.is_empty() {
                                logparse::eval_context(&msgs)
                            } else {
                                full
                            })
                        }
                    };
                    for outer in context {
                        if let Some(input) = self.eval_input(st, outer)? {
                            log::info(&format!(
                                "{}: {attr} is not pinnable itself; {outer}, which evaluation fails inside, is",
                                st.target
                            ));
                            found = Some((input, outer.clone()));
                            break;
                        }
                    }
                }
                match found {
                    Some((input, attr)) => wanted.push(Wanted {
                        input,
                        attr,
                        reason: Reason::Eval,
                        failed_drv: None,
                        trail: Vec::new(),
                    }),
                    None => {
                        st.status = Status::Failed(format!(
                            "evaluation fails on {attr:?}, and no older nixpkgs that built has it, so pinning cannot help"
                        ));
                        break;
                    }
                }
            }
            if st.status == Status::Active {
                self.apply(st, Ok(wanted));
            }
        }
        Ok(())
    }

    /// Whether a failed build may have failed only for the moment: a builder
    /// killed by a signal, or a download (a fixed-output derivation) whose
    /// builder failed.
    fn looks_transient(&self, errors: &[String], graph: &Graph) -> bool {
        let parsed = logparse::parse(errors);
        if parsed
            .iter()
            .any(|f| matches!(f, Failure::Infra { reason, .. } if reason.contains("due to signal")))
        {
            return true;
        }
        let failed: Vec<&String> = parsed
            .iter()
            .filter_map(|f| match f {
                Failure::Build { drv } => Some(drv),
                _ => None,
            })
            .collect();
        if failed.is_empty() || errors.iter().any(|m| m.contains("hash mismatch")) {
            return false;
        }
        failed
            .iter()
            .any(|d| graph.nodes.get(d.as_str()).is_some_and(|n| n.fixed_output))
    }

    fn build_wanted(
        &self,
        graph: &Graph,
        closure: &HashSet<String>,
        failed: &[&String],
        st: &State,
    ) -> Result<Result<Vec<Wanted>, String>> {
        let mut nodes = HashSet::new();
        for drv in failed {
            match graph.ancestors(drv, closure, ASCENT_CAP) {
                Some(a) => nodes.extend(a),
                None => {
                    return Ok(Err(format!(
                        "{} is a dependency of nearly everything",
                        graph.display_name(drv)
                    )));
                }
            }
        }
        let matcher = match Matcher::build(
            &self.nix,
            graph,
            &nodes,
            &st.target.package_sets(),
            &st.inputs,
        ) {
            Ok(m) => m,
            Err(e) => return Ok(Err(format!("matching packages failed: {e:#}"))),
        };

        let mut wanted: Vec<Wanted> = Vec::new();
        for drv in failed {
            let roots = match graph.find_roots(drv, closure, ASCENT_CAP, |p| matcher.matches(p)) {
                Ascent::Roots(r) => r,
                Ascent::Unpinnable { via } => {
                    log::info(&format!(
                        "unpinnable on {}: {drv} reaches {} without passing a package",
                        st.target,
                        graph.display_name(&via)
                    ));
                    return Ok(Err(format!(
                        "{} is not inside any pinnable package",
                        graph.display_name(drv)
                    )));
                }
                Ascent::TooLarge => {
                    return Ok(Err(format!(
                        "{} is a dependency of nearly everything",
                        graph.display_name(drv)
                    )));
                }
            };
            for root in &roots {
                // Everything above the root rebuilds once it is swapped, the
                // packages that are not top-level attributes (python3Packages.*)
                // as much as those that are.
                let dependents =
                    graph
                        .ancestors(root, closure, ASCENT_CAP)
                        .map_or(usize::MAX, |a| {
                            a.iter()
                                .filter(|d| {
                                    *d != root && (matcher.matches(d) || graph.is_package(d))
                                })
                                .count()
                        });
                if dependents > MAX_DEPENDENTS {
                    return Ok(Err(format!(
                        "{} is used by {} other packages; pinning it would rebuild them all from source",
                        graph.display_name(root),
                        if dependents == usize::MAX {
                            "too many".to_string()
                        } else {
                            dependents.to_string()
                        }
                    )));
                }
                let attrs: BTreeSet<&(String, String)> = matcher.attrs(root).collect();
                for (input, attr) in attrs {
                    if !wanted.iter().any(|w| &w.input == input && &w.attr == attr) {
                        wanted.push(Wanted {
                            input: input.clone(),
                            attr: attr.clone(),
                            reason: Reason::Build,
                            failed_drv: Some(drv.to_string()),
                            trail: graph.trail(drv, root, closure),
                        });
                    }
                }
            }
        }
        Ok(Ok(wanted))
    }

    /// The input whose last good set provides `attr` while the new one does not.
    /// A pin under retry counts as last good: the last lock only built because
    /// of it.
    fn eval_input(&self, st: &State, attr: &str) -> Result<Option<String>> {
        if !resolve::is_attr_name(attr) {
            return Ok(None);
        }
        if let Some((input, _)) = st.retrying.keys().find(|(_, a)| a == attr) {
            return Ok(Some(input.clone()));
        }
        let old_lock = st.held_lock.as_ref().unwrap_or(&st.old_lock);
        let system = st.system.get_or_init(|| {
            self.nix
                .eval_raw(&st.target.system_attr())
                .unwrap_or_else(|_| self.system.clone())
        });
        for input in &st.inputs {
            let (Some(old), Some(new)) = (old_lock.locked(input), self.new_lock.locked(input))
            else {
                continue;
            };
            if old["narHash"] == new["narHash"] {
                continue;
            }
            if resolve::evaluates_at(&old, system, attr)?
                && !resolve::evaluates_at(&new, system, attr)?
            {
                return Ok(Some(input.clone()));
            }
        }
        Ok(None)
    }

    fn apply(&self, st: &mut State, wanted: Result<Vec<Wanted>, String>) {
        let wanted = match wanted {
            Ok(w) if w.is_empty() => {
                st.status = Status::Failed("it fails, but nothing to pin was found".into());
                return;
            }
            Ok(w) => w,
            Err(why) => {
                st.status = Status::Failed(why);
                return;
            }
        };
        if let Err(why) = self.add_pins(st, wanted) {
            st.status = Status::Failed(why);
        }
    }

    fn add_pins(&self, st: &mut State, wanted: Vec<Wanted>) -> Result<(), String> {
        for w in wanted {
            let key = st.key(&w.input, &w.attr);
            if resolve::denied(&w.attr, &self.deny) {
                return Err(format!(
                    "{}.{} fails and is on the never-pin list (the built-in core packages, or programs.nixheal.deny)",
                    w.input, w.attr
                ));
            }
            if st.entry.get(&w.input, &w.attr).is_some() {
                return Err(format!("{}.{} is pinned and still fails", w.input, w.attr));
            }
            let checked = self
                .new_lock
                .nar_hash(&w.input)
                .ok_or_else(|| format!("{} is not an input of the new lock", w.input))?;

            let pin = match st.retrying.remove(&(w.input.clone(), w.attr.clone())) {
                Some(mut pin) => {
                    log::info(&format!(
                        "still failing {key}, keeping the pin from {}",
                        pin.since
                    ));
                    pin.checked_nar_hash = checked;
                    pin
                }
                None => {
                    let old_lock = st.held_lock.as_ref().unwrap_or(&st.old_lock);
                    let locked = old_lock.locked(&w.input).ok_or_else(|| {
                        format!("{} is not an input of the last good lock", w.input)
                    })?;
                    if locked["narHash"].as_str() == Some(checked.as_str()) {
                        return Err(format!(
                            "{}.{} fails at the last good {} too, so pinning cannot help",
                            w.input, w.attr, w.input
                        ));
                    }
                    st.new_pins += 1;
                    if st.new_pins > self.max_new_pins {
                        return Err(format!(
                            "it needs more than {} new pins in one run (--max-new-pins)",
                            self.max_new_pins
                        ));
                    }
                    log::change(&format!(
                        "pinned {key} at {}: {}",
                        describe(&locked),
                        w.failed_drv.as_deref().unwrap_or("evaluation failed")
                    ));
                    Pin {
                        locked,
                        since: date::format(self.today),
                        reason: w.reason,
                        failed_drv: w.failed_drv,
                        trail: w.trail,
                        checked_nar_hash: checked,
                    }
                }
            };
            st.entry.insert(&w.input, &w.attr, pin);
        }
        Ok(())
    }

    fn finish(&self, original: PinsFile, states: Vec<State>) -> Result<Report> {
        let mut report = Report::default();
        let local_failure = states.iter().find_map(|st| match &st.status {
            Status::Failed(reason) if Some(&st.target) == self.guarded.as_ref() => {
                Some(reason.clone())
            }
            _ => None,
        });
        if let Some(reason) = local_failure {
            log::flush_nix_errors();
            log::change(&format!(
                "giving up on {}: {reason}",
                st_name(self.guarded.as_ref())
            ));
            report.local_failure = Some(reason);
            return Ok(report);
        }
        let mut file = original;
        let mut done_with: Vec<(Target, Vec<u8>)> = Vec::new();
        for st in states {
            let target = st.target.clone();
            match st.status {
                Status::Done => {
                    if let Some(bytes) = &st.built_with {
                        done_with.push((target.clone(), bytes.clone()));
                    }
                    if Some(&target) == self.local.as_ref() {
                        report.local_drv = st.drv.clone();
                    }
                    if let Some(drv) = &st.drv {
                        report.built.push((target.clone(), drv.clone()));
                    }
                    for (input, attr) in st.retrying.keys() {
                        log::change(&format!("unpinned {target}: {input}.{attr}: builds again"));
                    }
                    report.changes.extend(
                        st.retrying
                            .keys()
                            .map(|(i, a)| format!("unpinned {target}: {i}.{a}")),
                    );
                    if st.held_before.is_some() {
                        log::change(&format!("released {target}: it builds with the new lock"));
                        report.changes.push(format!("released {target}"));
                    }
                    let previous = st.base.as_ref();
                    for (input, attr, pin) in st.entry.iter() {
                        if previous.and_then(|b| b.get(input, attr)).is_none() {
                            report
                                .changes
                                .push(format!("pinned {target}: {input}.{attr}"));
                        }
                        self.log_age(&target, input, attr, pin);
                    }
                    file.set(target.kind, &target.name, st.entry);
                    log::info(&format!("{target} builds"));
                }
                Status::Failed(reason) => {
                    if Some(&target) == self.local.as_ref() {
                        report.local_held = true;
                    }
                    let mut entry = st.base.unwrap_or_else(|| Entry {
                        lock_sha256: st.old_lock.sha256(),
                        lock_last_modified: st.old_lock.last_modified(),
                        ..Default::default()
                    });
                    let held = match st.held_before {
                        Some(mut h) => {
                            log::change(&format!(
                                "still holding {target} on its last good lock (since {}): {reason}",
                                h.since
                            ));
                            h.reason = reason.clone();
                            h
                        }
                        None => {
                            report.changes.push(format!("held {target}: {reason}"));
                            log::change(&format!(
                                "held {target} on its last good lock: {reason}. It is retried on every run; see nixheal status"
                            ));
                            Held {
                                since: date::format(self.today),
                                reason: reason.clone(),
                                lock: st.old_lock.json().clone(),
                            }
                        }
                    };
                    entry.held = Some(held);
                    file.set(target.kind, &target.name, entry);
                }
                Status::Active => unreachable!("active targets are failed before finishing"),
            }
        }
        guard::critical(|| file.save(&self.pins_path))?;
        self.refresh_built(&file.to_bytes(), &done_with, &mut report)?;
        Ok(report)
    }

    /// A target that built in an early round saw a pins file other rounds
    /// changed since. Its system only depends on that file when the
    /// configuration embeds the flake's source, but then what it built is not
    /// what gets committed: evaluate it again and build what changed.
    fn refresh_built(
        &self,
        final_bytes: &[u8],
        done_with: &[(Target, Vec<u8>)],
        report: &mut Report,
    ) -> Result<()> {
        let stale: Vec<&Target> = done_with
            .iter()
            .filter(|(_, b)| b.as_slice() != final_bytes)
            .map(|(t, _)| t)
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        let mut monitor = Monitor::start();
        let mut rebuild = Vec::new();
        for target in stale {
            let drv = match self.nix.drv_path(&target.output(), &mut monitor)? {
                Ok(d) => d,
                Err(msgs) => bail!(
                    "{target} built, but no longer evaluates with the final pins.json: {}",
                    msgs.first()
                        .map(|m| logparse::headline(m))
                        .unwrap_or_default()
                ),
            };
            if let Some(slot) = report.built.iter_mut().find(|(t, _)| t == target)
                && slot.1 != drv
            {
                slot.1 = drv.clone();
                if Some(target) == self.local.as_ref() {
                    report.local_drv = Some(drv.clone());
                }
                rebuild.push(drv);
            }
        }
        if !rebuild.is_empty() {
            log::info(&format!(
                "building {} system(s) again with the final pins.json",
                rebuild.len()
            ));
            let outcome = self.nix.build_drvs(&rebuild, &mut monitor)?;
            if !outcome.success {
                log::flush_nix_errors();
                bail!("a system that built failed to build again with the final pins.json");
            }
        }
        monitor.finish();
        Ok(())
    }

    fn log_age(&self, target: &Target, input: &str, attr: &str, pin: &Pin) {
        match date::parse(&pin.since).map(|d| self.today - d) {
            Some(days) if days > self.warn_after_days && pin.reason != Reason::Manual => {
                log::warn(&format!(
                    "{target}: {input}.{attr} has been pinned for {days} days (warn after {}); see nixheal why {attr}",
                    self.warn_after_days
                ))
            }
            Some(0) => log::info(&format!("{target}: {input}.{attr} pinned today")),
            Some(days) => log::info(&format!(
                "{target}: {input}.{attr} pinned for {days} day(s)"
            )),
            None => log::warn(&format!(
                "{target}: {input}.{attr} has an unreadable date {:?}",
                pin.since
            )),
        }
    }
}

fn build_failures(errors: &[String]) -> Vec<String> {
    logparse::parse(errors)
        .into_iter()
        .filter_map(|f| match f {
            Failure::Build { drv } => Some(drv),
            _ => None,
        })
        .collect()
}

fn st_name(target: Option<&Target>) -> String {
    target.map_or_else(|| "the local machine".into(), Target::to_string)
}

/// The path of a store path below its store object, e.g. "hosts/a/pins.json"
/// for /nix/store/<hash>-source/hosts/a/pins.json; None for other paths.
fn flake_relative(path: &str) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    let parts: Vec<&str> = rest.split('/').collect();
    let object = parts.iter().position(|p| {
        p.len() > 33
            && p.as_bytes()[32] == b'-'
            && p[..32].bytes().all(|b| b.is_ascii_alphanumeric())
    })?;
    let rel = parts[object + 1..].join("/");
    (!rel.is_empty()).then_some(rel)
}

pub fn describe(locked: &serde_json::Value) -> String {
    locked["rev"]
        .as_str()
        .map(|r| r.chars().take(12).collect())
        .or_else(|| locked["narHash"].as_str().map(str::to_string))
        .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_file_relative_to_the_flake() {
        let h = "0123456789abcdfghijklmnpqrsvwxyz";
        assert_eq!(
            flake_relative(&format!("/nix/store/{h}-source/pins.json")).as_deref(),
            Some("pins.json")
        );
        assert_eq!(
            flake_relative(&format!("/nix/store/{h}-source/hosts/laptop/pins.json")).as_deref(),
            Some("hosts/laptop/pins.json")
        );
        assert_eq!(
            flake_relative(&format!("/opt/store/{h}-source/pins.json")).as_deref(),
            Some("pins.json")
        );
        assert_eq!(flake_relative("/home/alice/nixos/pins.json"), None);
    }
}
