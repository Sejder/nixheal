use crate::pins::Kind;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};

/// What the NixOS or Home Manager module writes to /etc/nixheal.json or
/// ~/.config/nixheal.json.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub kind: Kind,
    #[serde(rename = "self")]
    pub self_name: String,
    #[serde(default)]
    pub flake: Option<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub homes: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default = "default_warn_after_days")]
    pub warn_after_days: i64,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub quiet: bool,
}

fn default_warn_after_days() -> i64 {
    14
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub kind: Kind,
    pub name: String,
}

impl Target {
    pub fn new(kind: Kind, name: &str) -> Self {
        Self { kind, name: name.to_string() }
    }

    fn base(&self) -> String {
        match self.kind {
            Kind::Nixos => format!("nixosConfigurations.{}", self.name),
            Kind::Home => format!("homeConfigurations.\"{}\"", self.name),
        }
    }

    pub fn output(&self) -> String {
        match self.kind {
            Kind::Nixos => format!("{}.config.system.build.toplevel", self.base()),
            Kind::Home => format!("{}.activationPackage", self.base()),
        }
    }

    /// The system the configuration is built for, e.g. aarch64-linux for a Pi.
    pub fn system_attr(&self) -> String {
        format!("{}.pkgs.stdenv.hostPlatform.system", self.base())
    }

    /// The name of this target's GC root under --gc-roots.
    pub fn root_name(&self) -> String {
        match self.kind {
            Kind::Nixos => self.name.clone(),
            Kind::Home => format!("home-{}", self.name),
        }
    }

    pub fn package_sets(&self) -> String {
        format!("{}.config.programs.nixheal.packageSets", self.base())
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind.noun(), self.name)
    }
}

impl Config {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn local(&self) -> Target {
        Target::new(self.kind, &self.self_name)
    }

    pub fn targets(&self) -> Vec<Target> {
        self.hosts
            .iter()
            .map(|h| Target::new(Kind::Nixos, h))
            .chain(self.homes.iter().map(|h| Target::new(Kind::Home, h)))
            .collect()
    }
}

fn user_config() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("nixheal.json"))
}

/// The config in effect: an explicit path, then $NIXHEAL_CONFIG, then the
/// Home Manager file, then the NixOS one.
pub fn locate(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("NIXHEAL_CONFIG") {
        return Some(PathBuf::from(p));
    }
    [user_config(), Some(PathBuf::from("/etc/nixheal.json"))]
        .into_iter()
        .flatten()
        .find(|p| p.exists())
}

pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let Some(path) = locate(explicit) else {
        bail!("no nixheal config: enable programs.nixheal, or pass --config");
    };
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Config::parse(&bytes).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_module_output() {
        let c = Config::parse(
            br#"{"kind":"nixos","self":"ideapad","flake":"/home/mikke/nix","hosts":["ideapad","pi"],"homes":["mikke"],"inputs":["nixpkgs","nixpkgs-unstable"],"warnAfterDays":7,"deny":["cuda*"]}"#,
        )
        .unwrap();
        assert_eq!(c.local(), Target::new(Kind::Nixos, "ideapad"));
        assert_eq!(c.targets().len(), 3);
        assert_eq!(c.warn_after_days, 7);
        assert_eq!(c.targets()[2], Target::new(Kind::Home, "mikke"));
    }

    #[test]
    fn defaults() {
        let c = Config::parse(br#"{"kind":"home","self":"mikke"}"#).unwrap();
        assert_eq!(c.warn_after_days, 14);
        assert!(c.targets().is_empty());
        assert!(c.flake.is_none());
    }

    #[test]
    fn target_attrs() {
        let h = Target::new(Kind::Nixos, "ideapad");
        assert_eq!(h.output(), "nixosConfigurations.ideapad.config.system.build.toplevel");
        assert_eq!(h.package_sets(), "nixosConfigurations.ideapad.config.programs.nixheal.packageSets");
        assert_eq!(h.to_string(), "host ideapad");
        let m = Target::new(Kind::Home, "mikke@wsl");
        assert_eq!(m.output(), "homeConfigurations.\"mikke@wsl\".activationPackage");
    }
}
