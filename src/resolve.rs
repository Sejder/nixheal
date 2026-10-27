use crate::graph::{Graph, Node};
use crate::logparse::strip_version;
use crate::nix::{self, Nix};
use anyhow::{Result, bail};
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

pub const DEFAULT_DENY: &[&str] = &[
    "linux*",
    "linuxPackages*",
    "linuxKernel*",
    "glibc*",
    "systemd*",
    "nix",
    "nixVersions*",
    "stdenv*",
    "gcc*",
    "bash",
    "bashInteractive",
    "coreutils*",
    "nixos-*",
];

static ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_+-]*$").unwrap());

pub fn is_attr_name(s: &str) -> bool {
    ATTR.is_match(s)
}

fn glob(pattern: &str, s: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => s.starts_with(prefix),
        None => s == pattern,
    }
}

pub fn denied(attr: &str, extra: &[String]) -> bool {
    DEFAULT_DENY
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
        .any(|p| glob(p, attr))
}

pub fn candidate_names(node: &Node) -> Vec<String> {
    let mut out = Vec::new();
    for name in [node.pname.as_deref(), Some(strip_version(&node.name))].into_iter().flatten() {
        if is_attr_name(name) && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

fn nix_list(names: &BTreeSet<String>) -> String {
    let items: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    format!("[ {} ]", items.join(" "))
}

#[derive(Default)]
pub struct Matcher {
    by_drv: HashMap<String, BTreeSet<(String, String)>>,
}

impl Matcher {
    /// Match every node in `nodes` against the top-level attrs of each package
    /// set, by guessing the attr name from the drv and confirming the drvPath.
    pub fn build(
        nix: &Nix,
        graph: &Graph,
        nodes: &HashSet<String>,
        sets: &[(String, String)],
    ) -> Result<Self> {
        let names: BTreeSet<String> = nodes
            .iter()
            .filter_map(|p| graph.nodes.get(p))
            .flat_map(candidate_names)
            .collect();
        let mut m = Matcher::default();
        if names.is_empty() {
            return Ok(m);
        }

        let apply = format!(
            r#"set: builtins.listToAttrs (map (n: {{ name = n; value = let r = builtins.tryEval (if builtins.hasAttr n set then (let v = builtins.getAttr n set; in if builtins.isAttrs v && (v.type or null) == "derivation" then v.drvPath else null) else null); in if r.success then r.value else null; }}) {})"#,
            nix_list(&names)
        );

        for (input, attr) in sets {
            let result = nix.eval_apply(attr, &apply)?;
            for (name, drv) in result.as_object().into_iter().flatten() {
                if let Some(drv) = drv.as_str().filter(|d| nodes.contains(*d)) {
                    m.by_drv
                        .entry(drv.to_string())
                        .or_default()
                        .insert((input.clone(), name.clone()));
                }
            }
        }
        Ok(m)
    }

    pub fn matches(&self, drv: &str) -> bool {
        self.by_drv.contains_key(drv)
    }

    pub fn attrs(&self, drv: &str) -> impl Iterator<Item = &(String, String)> {
        self.by_drv.get(drv).into_iter().flatten()
    }
}

/// Whether `attr` evaluates to a derivation in the nixpkgs at `locked`.
pub fn evaluates_at(locked: &Value, system: &str, attr: &str) -> Result<bool> {
    if !is_attr_name(attr) || !system.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        bail!("refusing to evaluate unexpected name {attr:?} for {system:?}");
    }
    let json = serde_json::to_string(locked)?;
    if json.contains("''") || json.contains("${") {
        bail!("locked node cannot be quoted safely");
    }
    let expr = format!(
        r#"let p = import (builtins.fetchTree (builtins.fromJSON ''{json}'')) {{ system = "{system}"; config.allowUnfree = true; }}; r = builtins.tryEval (if builtins.hasAttr "{attr}" p then (builtins.getAttr "{attr}" p).drvPath else null); in r.success && r.value != null"#
    );
    Ok(nix::eval_expr(&expr)?.as_bool().unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denylist() {
        assert!(denied("linux_6_12", &[]));
        assert!(denied("linuxPackages_latest", &[]));
        assert!(denied("systemd", &[]));
        assert!(denied("nix", &[]));
        assert!(!denied("nix-output-monitor", &[]));
        assert!(!denied("obs-studio", &[]));
        assert!(denied("obs-studio", &["obs-*".to_string()]));
    }

    #[test]
    fn candidates_from_pname_and_name() {
        let node = Node {
            name: "obs-studio-31.0.1".into(),
            pname: Some("obs-studio".into()),
            ..Default::default()
        };
        assert_eq!(candidate_names(&node), vec!["obs-studio"]);

        let node = Node { name: "hello-2.12".into(), pname: None, ..Default::default() };
        assert_eq!(candidate_names(&node), vec!["hello"]);

        let node = Node { name: "python3.13-requests-2.32".into(), pname: Some("requests".into()), ..Default::default() };
        assert_eq!(candidate_names(&node), vec!["requests"]);

        let node = Node { name: "unit-foo.service".into(), pname: None, ..Default::default() };
        assert!(candidate_names(&node).is_empty());
    }

    #[test]
    fn nix_list_quotes() {
        let names: BTreeSet<String> = ["b".to_string(), "a-1".to_string()].into();
        assert_eq!(nix_list(&names), r#"[ "a-1" "b" ]"#);
    }
}
