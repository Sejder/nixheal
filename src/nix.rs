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

fn checked(what: &str, out: Output) -> Result<Vec<u8>> {
    if !out.status.success() {
        let stderr = logparse::strip_ansi(&String::from_utf8_lossy(&out.stderr));
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!("{what} failed:\n{}", tail.join("\n"));
    }
    Ok(out.stdout)
}

impl Nix {
    pub fn new(flake: &Path) -> Self {
        Self { flake: flake.display().to_string() }
    }

    fn installable(&self, attr: &str) -> String {
        format!("{}#{}", self.flake, attr)
    }

    /// Build every attr in one `--keep-going` run, forwarding error messages to
    /// stderr so they land in the journal.
    pub fn build(&self, attrs: &[String]) -> Result<BuildOutcome> {
        let mut child = command()
            .args(["build", "--no-link", "--keep-going", "--log-format", "internal-json"])
            .args(attrs.iter().map(|a| self.installable(a)))
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

    pub fn show_derivations(&self, attrs: &[String]) -> Result<Graph> {
        let out = command()
            .args(["derivation", "show", "--recursive"])
            .args(attrs.iter().map(|a| self.installable(a)))
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
