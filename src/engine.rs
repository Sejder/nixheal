use crate::config::Target;
use crate::date;
use crate::graph::{Ascent, Graph};
use crate::lock::Lock;
use crate::log;
use crate::logparse::{self, Failure};
use crate::nix::Nix;
use crate::pins::{Entry, Held, Pin, PinsFile, Reason};
use crate::resolve::{self, Matcher};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

const ASCENT_CAP: usize = 20_000;

pub struct Engine {
    pub nix: Nix,
    pub pins_path: PathBuf,
    pub new_lock: Lock,
    pub old_lock: Lock,
    pub targets: Vec<Target>,
    /// The machine autopin runs on, when it is one of the targets. Its failure
    /// aborts the run instead of holding it.
    pub local: Option<Target>,
    pub fallback_inputs: Vec<String>,
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
    retrying: BTreeMap<(String, String), Pin>,
    inputs: Vec<String>,
    sets: Vec<(String, String)>,
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
            self.write(&original, &states)?;
            let names: Vec<String> = active.iter().map(|&i| states[i].target.to_string()).collect();
            let pins: usize = active.iter().map(|&i| states[i].entry.len()).sum();
            log::info(&format!("round {round}: evaluating and building {} with {pins} pin(s)", names.join(", ")));
            let started = std::time::Instant::now();
            self.round(&mut states, &active)?;
            log::info(&format!("round {round} took {}s", started.elapsed().as_secs()));
        }

        for st in &mut states {
            if st.status == Status::Active {
                st.status = Status::Failed(format!("still failing after {} rounds", self.max_rounds));
            }
        }
        self.finish(original, states)
    }

    fn init(&self, original: &PinsFile, target: &Target) -> Result<State> {
        let base = original.entry(target.kind, &target.name).cloned();
        let mut entry = base.clone().unwrap_or_default();
        let held_before = entry.held.take();
        let held_lock = held_before.as_ref().map(|h| Lock::from_json(&h.lock)).transpose()?;

        let mut retrying = BTreeMap::new();
        let pinned: Vec<(String, String, Pin)> = entry
            .iter()
            .filter(|(_, _, p)| p.reason != Reason::Manual)
            .map(|(i, a, p)| (i.to_string(), a.to_string(), p.clone()))
            .collect();
        for (input, attr, pin) in pinned {
            if self.new_lock.nar_hash(&input).as_deref() != Some(pin.checked_nar_hash.as_str()) {
                log::info(&format!("retrying {target}: {input}.{attr}: {input} moved since it was pinned"));
                entry.remove(&input, &attr);
                retrying.insert((input, attr), pin);
            }
        }
        entry.lock_sha256 = self.new_lock.sha256();
        entry.lock_last_modified = self.new_lock.last_modified();

        let inputs = match self.nix.attr_names(&target.package_sets()) {
            Ok(names) => names,
            Err(e) => {
                log::info(&format!(
                    "could not list package sets of {target}, using the configured inputs: {}",
                    e.to_string().lines().last().unwrap_or_default()
                ));
                self.fallback_inputs.clone()
            }
        };
        let sets = inputs
            .iter()
            .map(|i| (i.clone(), format!("{}.{i}", target.package_sets())))
            .collect();

        Ok(State {
            target: target.clone(),
            base,
            entry,
            held_before,
            held_lock,
            retrying,
            inputs,
            sets,
            status: Status::Active,
            new_pins: 0,
        })
    }

    /// The pins file as the builds should see it: working entries for active
    /// and finished targets, the original entry for failed ones.
    fn write(&self, original: &PinsFile, states: &[State]) -> Result<()> {
        let mut file = original.clone();
        for st in states {
            match (&st.status, &st.base) {
                (Status::Failed(_), Some(e)) => file.set(st.target.kind, &st.target.name, e.clone()),
                (Status::Failed(_), None) => {}
                _ => file.set(st.target.kind, &st.target.name, st.entry.clone()),
            }
        }
        file.save(&self.pins_path)
    }

    fn round(&self, states: &mut [State], active: &[usize]) -> Result<()> {
        // The common case: everything evaluates and builds in one nix build,
        // which is also what shows the nom view from the start.
        let outputs: Vec<String> = active.iter().map(|&i| states[i].target.output()).collect();
        let fast = self.nix.build_outputs(&outputs)?;
        if fast.success {
            for &i in active {
                states[i].status = Status::Done;
            }
            return Ok(());
        }
        let fast_failed: Vec<String> = build_failures(&fast.errors);

        // Something failed: evaluate each host on its own to tell evaluation
        // failures apart and to get the drvs the failures are traced in.
        let mut built: Vec<(usize, String)> = Vec::new();
        let mut eval_failed: Vec<(usize, Vec<String>)> = Vec::new();
        for &i in active {
            log::info(&format!("evaluating {} to trace the failure", states[i].target));
            match self.nix.drv_path(&states[i].target.output())? {
                Ok(drv) => built.push((i, drv)),
                Err(msgs) => {
                    for m in &msgs {
                        log::nix_error(m);
                    }
                    eval_failed.push((i, msgs));
                }
            }
        }

        if !built.is_empty() {
            let drvs: Vec<String> = built.iter().map(|(_, d)| d.clone()).collect();
            // nix build evaluates every output before it builds anything, so
            // build failures from the first run mean nothing else stopped it.
            // Otherwise an evaluation failure kept these drvs from building.
            let outcome = if !fast_failed.is_empty() && eval_failed.is_empty() {
                fast
            } else {
                self.nix.build_drvs(&drvs)?
            };
            if outcome.success {
                for (i, _) in &built {
                    states[*i].status = Status::Done;
                }
            } else {
                let failed = build_failures(&outcome.errors);
                let graph = self.nix.show_drvs(&drvs)?;
                for (i, top) in &built {
                    let closure = graph.closure(top);
                    let mine: Vec<&String> = failed.iter().filter(|d| closure.contains(*d)).collect();
                    let st = &mut states[*i];
                    if failed.is_empty() {
                        st.status = Status::Failed("the build failed without a failure autopin recognises".into());
                    } else if mine.is_empty() {
                        st.status = Status::Done;
                    } else {
                        let wanted = self.build_wanted(&graph, &closure, &mine, st)?;
                        self.apply(st, wanted);
                    }
                }
            }
        }

        for (i, msgs) in eval_failed {
            let st = &mut states[i];
            let attrs: Vec<String> = logparse::parse(&msgs)
                .into_iter()
                .filter_map(|f| match f {
                    Failure::Eval { attr, .. } => Some(attr),
                    Failure::Build { .. } => None,
                })
                .collect();
            if attrs.is_empty() {
                let first = msgs.first().and_then(|m| m.lines().next()).unwrap_or("").to_string();
                st.status = Status::Failed(format!("evaluation fails in a way autopin cannot pin: {first}"));
                continue;
            }
            let mut wanted = Vec::new();
            for attr in attrs {
                match self.eval_input(st, &attr)? {
                    Some(input) => wanted.push(Wanted {
                        input,
                        attr,
                        reason: Reason::Eval,
                        failed_drv: None,
                        trail: Vec::new(),
                    }),
                    None => {
                        st.status = Status::Failed(format!(
                            "evaluation fails on {attr:?}, which no last good package set provides"
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
                None => return Ok(Err(format!("{drv} is a dependency of nearly everything"))),
            }
        }
        let matcher = match Matcher::build(&self.nix, graph, &nodes, &st.sets) {
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
                    return Ok(Err(format!("{} is not inside any pinnable package", graph.display_name(drv))));
                }
                Ascent::TooLarge => return Ok(Err(format!("{drv} is a dependency of nearly everything"))),
            };
            for root in &roots {
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
        if let Some((input, _)) = st.retrying.keys().find(|(_, a)| a == attr) {
            return Ok(Some(input.clone()));
        }
        let old_lock = st.held_lock.as_ref().unwrap_or(&self.old_lock);
        for input in &st.inputs {
            let (Some(old), Some(new)) = (old_lock.locked(input), self.new_lock.locked(input)) else {
                continue;
            };
            if old["narHash"] == new["narHash"] {
                continue;
            }
            if resolve::evaluates_at(&old, &self.system, attr)?
                && !resolve::evaluates_at(&new, &self.system, attr)?
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
                return Err(format!("{}.{} fails but is never pinned", w.input, w.attr));
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
                    log::info(&format!("still failing {key}, keeping the pin from {}", pin.since));
                    pin.checked_nar_hash = checked;
                    pin
                }
                None => {
                    let old_lock = st.held_lock.as_ref().unwrap_or(&self.old_lock);
                    let locked = old_lock
                        .locked(&w.input)
                        .ok_or_else(|| format!("{} is not an input of the last good lock", w.input))?;
                    if locked["narHash"].as_str() == Some(checked.as_str()) {
                        return Err(format!(
                            "{}.{} fails at the last good {} too, so pinning cannot help",
                            w.input, w.attr, w.input
                        ));
                    }
                    st.new_pins += 1;
                    if st.new_pins > self.max_new_pins {
                        return Err(format!("more than {} new pins needed in one run", self.max_new_pins));
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
            Status::Failed(reason) if Some(&st.target) == self.local.as_ref() => Some(reason.clone()),
            _ => None,
        });
        if let Some(reason) = local_failure {
            log::change(&format!("giving up on {}: {reason}", st_name(self.local.as_ref())));
            report.local_failure = Some(reason);
            return Ok(report);
        }
        let mut file = original;
        for st in states {
            let target = st.target.clone();
            match st.status {
                Status::Done => {
                    for (input, attr) in st.retrying.keys() {
                        log::change(&format!("unpinned {target}: {input}.{attr}: builds again"));
                    }
                    report.changes.extend(st.retrying.keys().map(|(i, a)| format!("unpinned {target}: {i}.{a}")));
                    if st.held_before.is_some() {
                        log::change(&format!("released {target}: it builds with the new lock"));
                        report.changes.push(format!("released {target}"));
                    }
                    let previous = st.base.as_ref();
                    for (input, attr, pin) in st.entry.iter() {
                        if previous.and_then(|b| b.get(input, attr)).is_none() {
                            report.changes.push(format!("pinned {target}: {input}.{attr}"));
                        }
                        self.log_age(&target, input, attr, pin);
                    }
                    if !file.merge(target.kind, &target.name, st.entry) {
                        log::info(&format!("kept the entry for {target} from a newer lock"));
                    }
                    log::info(&format!("{target} builds"));
                }
                Status::Failed(reason) => {
                    let mut entry = st.base.unwrap_or_default();
                    let held = match st.held_before {
                        Some(mut h) => {
                            h.reason = reason.clone();
                            h
                        }
                        None => {
                            report.changes.push(format!("held {target}: {reason}"));
                            Held {
                                since: date::format(self.today),
                                reason: reason.clone(),
                                lock: self.old_lock.json().clone(),
                            }
                        }
                    };
                    log::change(&format!("held {target} on its last good lock since {}: {reason}", held.since));
                    entry.held = Some(held);
                    file.set(target.kind, &target.name, entry);
                }
                Status::Active => unreachable!("active targets are failed before finishing"),
            }
        }
        file.save(&self.pins_path)?;
        Ok(report)
    }

    fn log_age(&self, target: &Target, input: &str, attr: &str, pin: &Pin) {
        match date::parse(&pin.since).map(|d| self.today - d) {
            Some(days) if days > self.warn_after_days => log::warn(&format!(
                "pin {target}: {input}.{attr} is {days} days old (warn after {})",
                self.warn_after_days
            )),
            Some(days) => log::info(&format!("pin {target}: {input}.{attr} is {days} day(s) old")),
            None => log::warn(&format!("pin {target}: {input}.{attr} has an unreadable date {:?}", pin.since)),
        }
    }
}

fn build_failures(errors: &[String]) -> Vec<String> {
    logparse::parse(errors)
        .into_iter()
        .filter_map(|f| match f {
            Failure::Build { drv } => Some(drv),
            Failure::Eval { .. } => None,
        })
        .collect()
}

fn st_name(target: Option<&Target>) -> String {
    target.map_or_else(|| "the local machine".into(), Target::to_string)
}

pub fn describe(locked: &serde_json::Value) -> String {
    locked["rev"]
        .as_str()
        .map(|r| r.chars().take(12).collect())
        .or_else(|| locked["narHash"].as_str().map(str::to_string))
        .unwrap_or_else(|| "?".into())
}
