use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::IgnoredAny;
use std::collections::{BTreeSet, HashMap, HashSet};

const STORE: &str = "/nix/store/";

#[derive(Debug, Default)]
pub struct Node {
    pub name: String,
    pub pname: Option<String>,
    pub inputs: Vec<String>,
    pub parents: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Graph {
    pub nodes: HashMap<String, Node>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Ascent {
    Roots(BTreeSet<String>),
    Unpinnable { via: String },
    TooLarge,
}

#[derive(Deserialize)]
struct NewDoc {
    derivations: HashMap<String, DrvJson>,
}

#[derive(Deserialize)]
struct DrvJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    env: EnvJson,
    #[serde(default)]
    inputs: Option<InputsJson>,
    #[serde(default, rename = "inputDrvs")]
    input_drvs: Option<HashMap<String, IgnoredAny>>,
}

#[derive(Deserialize, Default)]
struct EnvJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    pname: Option<String>,
}

#[derive(Deserialize)]
struct InputsJson {
    #[serde(default)]
    drvs: HashMap<String, IgnoredAny>,
}

fn full_path(p: String) -> String {
    if p.starts_with('/') { p } else { format!("{STORE}{p}") }
}

impl Graph {
    /// Accepts both `nix derivation show` layouts: the versioned one with
    /// `derivations` and store-relative keys, and the older full-path map.
    pub fn from_show_json(bytes: &[u8]) -> Result<Self> {
        let drvs = match serde_json::from_slice::<NewDoc>(bytes) {
            Ok(doc) => doc.derivations,
            Err(_) => serde_json::from_slice::<HashMap<String, DrvJson>>(bytes)
                .context("parsing nix derivation show output")?,
        };

        let mut graph = Graph::default();
        for (path, drv) in drvs {
            let inputs = match (drv.inputs, drv.input_drvs) {
                (Some(i), _) => i.drvs.into_keys().collect(),
                (None, Some(i)) => i.into_keys().collect(),
                (None, None) => Vec::new(),
            };
            let node = Node {
                name: drv.name.or(drv.env.name).unwrap_or_default(),
                pname: drv.env.pname,
                inputs: inputs.into_iter().map(full_path).collect(),
                parents: Vec::new(),
            };
            graph.nodes.insert(full_path(path), node);
        }
        graph.link();
        Ok(graph)
    }

    fn link(&mut self) {
        let edges: Vec<(String, String)> = self
            .nodes
            .iter()
            .flat_map(|(p, n)| n.inputs.iter().map(move |i| (i.clone(), p.clone())))
            .collect();
        for (child, parent) in edges {
            if let Some(n) = self.nodes.get_mut(&child) {
                n.parents.push(parent);
            }
        }
        for n in self.nodes.values_mut() {
            n.parents.sort();
            n.parents.dedup();
        }
    }

    pub fn ancestors(&self, start: &str, cap: usize) -> Option<HashSet<String>> {
        let mut seen = HashSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(p) = stack.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            if seen.len() > cap {
                return None;
            }
            if let Some(n) = self.nodes.get(&p) {
                stack.extend(n.parents.iter().cloned());
            }
        }
        Some(seen)
    }

    /// Walk up from a failed derivation to the packages that should be pinned.
    ///
    /// A root is a matched derivation (one that is a top-level attribute of a
    /// package set) with at least one unmatched parent: it is consumed directly by
    /// configuration glue, not only by other packages. Pinning it swaps it in
    /// `final`, so matched parents pick up the pinned copy. Reaching the top of
    /// the graph without passing a root means the failure is in the
    /// configuration itself and cannot be pinned.
    pub fn find_roots(&self, start: &str, cap: usize, is_match: impl Fn(&str) -> bool) -> Ascent {
        let mut roots = BTreeSet::new();
        let mut seen = HashSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(p) = stack.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            if seen.len() > cap {
                return Ascent::TooLarge;
            }
            let parents: &[String] = self.nodes.get(&p).map_or(&[], |n| &n.parents);
            if is_match(&p) {
                if parents.is_empty() || parents.iter().any(|q| !is_match(q)) {
                    roots.insert(p);
                } else {
                    stack.extend(parents.iter().cloned());
                }
            } else if parents.is_empty() {
                return Ascent::Unpinnable { via: p };
            } else {
                stack.extend(parents.iter().cloned());
            }
        }
        Ascent::Roots(roots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: &str) -> String {
        format!("{STORE}{p}")
    }

    // toplevel -> system-path -> {obs, vlc}; obs -> libfoo; vlc -> libfoo;
    // toplevel -> etc -> unit -> libbar; obs-src fetch under obs.
    fn graph() -> Graph {
        let json = serde_json::json!({
            "version": 4,
            "derivations": {
                "t-toplevel.drv": { "name": "nixos-system", "env": {}, "inputs": { "drvs": { "sp-system-path.drv": {}, "e-etc.drv": {} }, "srcs": [] } },
                "sp-system-path.drv": { "name": "system-path", "env": {}, "inputs": { "drvs": { "o-obs-studio-31.drv": {}, "v-vlc-3.drv": {} } } },
                "o-obs-studio-31.drv": { "name": "obs-studio-31", "env": { "pname": "obs-studio" }, "inputs": { "drvs": { "l-libfoo-1.drv": {}, "s-obs-src.drv": {} } } },
                "v-vlc-3.drv": { "name": "vlc-3", "env": { "pname": "vlc" }, "inputs": { "drvs": { "l-libfoo-1.drv": {} } } },
                "l-libfoo-1.drv": { "name": "libfoo-1", "env": { "pname": "libfoo" }, "inputs": { "drvs": {} } },
                "s-obs-src.drv": { "name": "obs-src", "env": {}, "inputs": { "drvs": {} } },
                "e-etc.drv": { "name": "etc", "env": {}, "inputs": { "drvs": { "u-unit.drv": {} } } },
                "u-unit.drv": { "name": "unit-foo.service", "env": {}, "inputs": { "drvs": { "b-libbar-1.drv": {} } } },
                "b-libbar-1.drv": { "name": "libbar-1", "env": { "pname": "libbar" }, "inputs": { "drvs": {} } }
            }
        });
        Graph::from_show_json(&serde_json::to_vec(&json).unwrap()).unwrap()
    }

    fn matched(names: &[&str]) -> impl Fn(&str) -> bool {
        let set: HashSet<String> = names.iter().map(|n| s(n)).collect();
        move |p| set.contains(p)
    }

    #[test]
    fn parses_new_format() {
        let g = graph();
        let obs = &g.nodes[&s("o-obs-studio-31.drv")];
        assert_eq!(obs.pname.as_deref(), Some("obs-studio"));
        assert_eq!(obs.parents, vec![s("sp-system-path.drv")]);
        assert_eq!(g.nodes[&s("l-libfoo-1.drv")].parents.len(), 2);
    }

    #[test]
    fn parses_old_format() {
        let json = serde_json::json!({
            "/nix/store/a-app.drv": { "name": "app", "env": { "pname": "app" }, "inputDrvs": { "/nix/store/b-lib.drv": ["out"] } },
            "/nix/store/b-lib.drv": { "env": { "name": "lib" }, "inputDrvs": {} }
        });
        let g = Graph::from_show_json(&serde_json::to_vec(&json).unwrap()).unwrap();
        assert_eq!(g.nodes[&s("b-lib.drv")].parents, vec![s("a-app.drv")]);
        assert_eq!(g.nodes[&s("b-lib.drv")].name, "lib");
    }

    #[test]
    fn library_failure_pins_the_installed_apps() {
        let g = graph();
        let is_match = matched(&["o-obs-studio-31.drv", "v-vlc-3.drv", "l-libfoo-1.drv"]);
        assert_eq!(
            g.find_roots(&s("l-libfoo-1.drv"), 1000, is_match),
            Ascent::Roots([s("o-obs-studio-31.drv"), s("v-vlc-3.drv")].into())
        );
    }

    #[test]
    fn unnamed_dependency_resolves_upwards() {
        let g = graph();
        let is_match = matched(&["o-obs-studio-31.drv", "v-vlc-3.drv"]);
        assert_eq!(
            g.find_roots(&s("s-obs-src.drv"), 1000, is_match),
            Ascent::Roots([s("o-obs-studio-31.drv")].into())
        );
    }

    #[test]
    fn package_used_by_glue_is_its_own_root() {
        let g = graph();
        let is_match = matched(&["b-libbar-1.drv"]);
        assert_eq!(
            g.find_roots(&s("b-libbar-1.drv"), 1000, is_match),
            Ascent::Roots([s("b-libbar-1.drv")].into())
        );
    }

    #[test]
    fn glue_failure_is_unpinnable() {
        let g = graph();
        let is_match = matched(&["o-obs-studio-31.drv"]);
        assert_eq!(
            g.find_roots(&s("u-unit.drv"), 1000, is_match),
            Ascent::Unpinnable { via: s("t-toplevel.drv") }
        );
    }

    #[test]
    fn caps_large_ascents() {
        let g = graph();
        assert_eq!(g.find_roots(&s("l-libfoo-1.drv"), 2, |_| false), Ascent::TooLarge);
        assert!(g.ancestors(&s("l-libfoo-1.drv"), 2).is_none());
        assert_eq!(g.ancestors(&s("l-libfoo-1.drv"), 100).unwrap().len(), 5);
    }
}
