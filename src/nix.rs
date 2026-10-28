use crate::graph::Graph;
use crate::log;
use crate::logparse;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};

pub struct Nix {
    flake: String,
}

pub struct BuildOutcome {
    pub success: bool,
    pub errors: Vec<String>,
}

fn command() -> Command {
    let mut c = Command::new("nix");
    c.args(["--extra-experimental-features", "nix-command flakes"]);
    c.args(["--option", "warn-dirty", "false"]);
    c
}

fn tail(stderr: &[u8], lines: usize) -> String {
    let text = logparse::strip_ansi(&String::from_utf8_lossy(stderr));
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn checked(what: &str, out: Output) -> Result<Vec<u8>> {
    if !out.status.success() {
        bail!("{what} failed:\n{}", tail(&out.stderr, 20));
    }
    Ok(out.stdout)
}

/// One nix-output-monitor view on a terminal, fed by every nix command of a
/// round, so evaluation and build show as one session the way nh shows them.
/// Off a terminal, or with --quiet, there is none, and error messages go to
/// stderr instead, where the journal picks them up.
pub struct Monitor {
    nom: Option<Child>,
    pipe: Option<ChildStdin>,
}

impl Monitor {
    pub fn start() -> Self {
        let nom = (std::io::stderr().is_terminal() && !log::is_quiet())
            .then(|| {
                Command::new("nom")
                    .arg("--json")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .ok()
            })
            .flatten();
        let mut m = Self { nom, pipe: None };
        m.pipe = m.nom.as_mut().and_then(|n| n.stdin.take());
        m
    }

    fn active(&self) -> bool {
        self.pipe.is_some()
    }

    /// Run `cmd` with internal-json logging, feeding its stderr to nom and
    /// collecting its error messages and stdout.
    fn run(&mut self, mut cmd: Command) -> Result<(ExitStatus, Vec<u8>, Vec<String>)> {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning nix")?;
        crate::guard::track(child.id());

        let mut stdout = child.stdout.take().expect("piped stdout");
        let collect = std::thread::spawn(move || {
            let mut out = Vec::new();
            stdout.read_to_end(&mut out).map(|_| out)
        });

        let mut reader = BufReader::new(child.stderr.take().expect("piped stderr"));
        let mut errors = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if reader.read_until(b'\n', &mut buf)? == 0 {
                break;
            }
            if let Some(pipe) = &mut self.pipe
                && pipe.write_all(&buf).is_err()
            {
                self.pipe = None;
            }
            let line = String::from_utf8_lossy(&buf);
            if !self.active()
                && let Some(text) = logparse::activity_text(line.trim_end())
            {
                log::nix_progress(&text);
            }
            if let Some(msg) = logparse::error_message(line.trim_end()) {
                if !self.active() {
                    log::nix_error(&msg);
                }
                errors.push(msg);
            }
        }

        let status = child.wait().context("waiting for nix");
        crate::guard::untrack(child.id());
        let status = status?;
        let out = collect.join().expect("stdout reader")?;
        Ok((status, out, errors))
    }

    pub fn finish(mut self) {
        self.pipe = None;
        if let Some(mut n) = self.nom.take() {
            let _ = n.wait();
        }
    }
}

impl Nix {
    pub fn new(flake: &Path) -> Self {
        Self {
            flake: flake.display().to_string(),
        }
    }

    fn installable(&self, attr: &str) -> String {
        format!("{}#{}", self.flake, attr)
    }

    pub fn flake_update(&self, inputs: &[String]) -> Result<()> {
        let status = command()
            .args(["flake", "update", "--flake", &self.flake])
            .args(inputs)
            .stdin(Stdio::null())
            .status()
            .context("spawning nix flake update")?;
        if !status.success() {
            bail!("nix flake update failed");
        }
        Ok(())
    }

    /// The drvPath of `attr`, or the evaluation error messages.
    pub fn drv_path(
        &self,
        attr: &str,
        monitor: &mut Monitor,
    ) -> Result<Result<String, Vec<String>>> {
        let mut cmd = command();
        cmd.args(["eval", "--raw", "--log-format", "internal-json"])
            .arg(self.installable(&format!("{attr}.drvPath")));
        let (status, out, errors) = monitor.run(cmd)?;
        if status.success() {
            Ok(Ok(String::from_utf8_lossy(&out).trim().to_string()))
        } else if errors.is_empty() {
            Ok(Err(vec![format!("nix eval {attr} failed")]))
        } else {
            Ok(Err(errors))
        }
    }

    /// Build already evaluated derivations in one `--keep-going` run.
    pub fn build_drvs(&self, drvs: &[String], monitor: &mut Monitor) -> Result<BuildOutcome> {
        let mut cmd = command();
        cmd.args([
            "build",
            "--no-link",
            "--keep-going",
            "--log-format",
            "internal-json",
        ]);
        if monitor.active() {
            cmd.arg("-v");
        }
        cmd.args(drvs.iter().map(|d| format!("{d}^*")));
        let (status, _, errors) = monitor.run(cmd)?;
        Ok(BuildOutcome {
            success: status.success(),
            errors,
        })
    }

    pub fn show_drvs(&self, drvs: &[String]) -> Result<Graph> {
        let out = command()
            .args(["derivation", "show", "--recursive"])
            .args(drvs)
            .stdin(Stdio::null())
            .output()
            .context("spawning nix derivation show")?;
        let store = crate::graph::store_dir_of(drvs.first().map_or("", String::as_str));
        Graph::from_show_json(&checked("nix derivation show", out)?, &store)
    }

    pub fn eval_apply(&self, attr: &str, apply: &str) -> Result<Value> {
        let out = command()
            .args(["eval", "--json"])
            .arg(self.installable(attr))
            .args(["--apply", apply])
            .stdin(Stdio::null())
            .output()
            .context("spawning nix eval")?;
        let bytes = checked(&format!("nix eval {attr}"), out)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// The store path of the flake's source as nix copies it now.
    pub fn source_path(&self) -> Result<String> {
        let out = command()
            .args(["flake", "metadata", "--json", &self.flake])
            .stdin(Stdio::null())
            .output()
            .context("spawning nix flake metadata")?;
        let json: Value = serde_json::from_slice(&checked("nix flake metadata", out)?)?;
        json["path"]
            .as_str()
            .map(str::to_string)
            .context("nix flake metadata has no path")
    }

    /// The full error of evaluating `attr`, with every frame of its trace:
    /// what nix shows by default is cut short before the packages an error
    /// passed through. Not shown to the user.
    pub fn eval_trace(&self, attr: &str) -> Result<String> {
        let out = command()
            .args(["eval", "--raw", "--show-trace"])
            .arg(self.installable(attr))
            .stdin(Stdio::null())
            .output()
            .context("spawning nix eval")?;
        Ok(logparse::strip_ansi(&String::from_utf8_lossy(&out.stderr)))
    }

    pub fn eval_raw(&self, attr: &str) -> Result<String> {
        let out = command()
            .args(["eval", "--raw"])
            .arg(self.installable(attr))
            .stdin(Stdio::null())
            .output()
            .context("spawning nix eval")?;
        let bytes = checked(&format!("nix eval {attr}"), out)?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_string())
    }
}

/// Keep a built derivation alive with a GC root at `link`.
pub fn add_root(drv: &str, link: &Path) -> Result<()> {
    let out = command()
        .args(["build", "--out-link"])
        .arg(link)
        .arg(format!("{drv}^out"))
        .stdin(Stdio::null())
        .output()
        .context("spawning nix build")?;
    checked(&format!("rooting {drv}"), out).map(|_| ())
}

/// Whether every output of `drv` is in the store, i.e. it really built.
pub fn is_built(drv: &str) -> Result<bool> {
    let out = command()
        .args(["path-info", "--offline"])
        .arg(format!("{drv}^*"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("spawning nix path-info")?;
    Ok(out.success())
}

/// The `out` path of a built derivation, read from the drv itself.
pub fn out_path(drv: &str) -> Result<String> {
    let out = command()
        .args(["derivation", "show", drv])
        .stdin(Stdio::null())
        .output()
        .context("spawning nix derivation show")?;
    let json: Value = serde_json::from_slice(&checked("nix derivation show", out)?)?;
    let drvs = json.get("derivations").unwrap_or(&json);
    let path = drvs
        .as_object()
        .and_then(|o| o.values().next())
        .and_then(|d| d["outputs"]["out"]["path"].as_str())
        .with_context(|| format!("{drv} has no out path"))?;
    Ok(if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{path}", crate::graph::store_dir_of(drv))
    })
}

pub fn eval_expr(expr: &str) -> Result<Value> {
    let out = command()
        .args(["eval", "--json", "--expr", expr])
        .stdin(Stdio::null())
        .output()
        .context("spawning nix eval")?;
    let bytes = checked("nix eval --expr", out)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// The last lines of a derivation's build log, if nix kept one.
pub fn log_tail(drv: &str, lines: usize) -> Option<String> {
    let out = command()
        .args(["log", drv])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = logparse::strip_ansi(&String::from_utf8_lossy(&out.stdout));
    let kept: Vec<&str> = text.lines().filter(|l| !l.starts_with("@nix ")).collect();
    Some(kept[kept.len().saturating_sub(lines)..].join("\n"))
}
