use crate::graph::Graph;
use crate::logparse;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Output, Stdio};

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

/// Error messages from internal-json stderr, or the plain stderr tail when
/// nix printed none in that format.
fn error_messages(stderr: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(stderr);
    let msgs: Vec<String> = text.lines().filter_map(logparse::error_message).collect();
    if msgs.is_empty() { vec![tail(stderr, 40)] } else { msgs }
}

impl Nix {
    pub fn new(flake: &Path) -> Self {
        Self { flake: flake.display().to_string() }
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
    pub fn drv_path(&self, attr: &str) -> Result<Result<String, Vec<String>>> {
        let out = command()
            .args(["eval", "--raw", "--log-format", "internal-json"])
            .arg(self.installable(&format!("{attr}.drvPath")))
            .stdin(Stdio::null())
            .output()
            .context("spawning nix eval")?;
        if out.status.success() {
            Ok(Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()))
        } else {
            Ok(Err(error_messages(&out.stderr)))
        }
    }

    /// Build derivations in one `--keep-going` run, forwarding error messages
    /// to stderr so they land in the journal.
    pub fn build_drvs(&self, drvs: &[String]) -> Result<BuildOutcome> {
        let mut child = command()
            .args(["build", "--no-link", "--keep-going", "--log-format", "internal-json"])
            .args(drvs.iter().map(|d| format!("{d}^*")))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning nix build")?;

        let mut reader = BufReader::new(child.stderr.take().expect("piped stderr"));
        let mut errors = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if reader.read_until(b'\n', &mut buf)? == 0 {
                break;
            }
            let line = String::from_utf8_lossy(&buf);
            if let Some(msg) = logparse::error_message(line.trim_end()) {
                eprintln!("{msg}");
                errors.push(msg);
            }
        }

        let status = child.wait().context("waiting for nix build")?;
        Ok(BuildOutcome { success: status.success(), errors })
    }

    pub fn show_drvs(&self, drvs: &[String]) -> Result<Graph> {
        let out = command()
            .args(["derivation", "show", "--recursive"])
            .args(drvs)
            .stdin(Stdio::null())
            .output()
            .context("spawning nix derivation show")?;
        Graph::from_show_json(&checked("nix derivation show", out)?)
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

    pub fn attr_names(&self, attr: &str) -> Result<Vec<String>> {
        let v = self.eval_apply(attr, "builtins.attrNames")?;
        Ok(serde_json::from_value(v)?)
    }
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
    let out = command().args(["log", drv]).stdin(Stdio::null()).output().ok()?;
    out.status.success().then(|| tail(&out.stdout, lines))
}
