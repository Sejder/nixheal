use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    Build,
    Eval,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pin {
    pub locked: Value,
    pub since: String,
    pub reason: Reason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_drv: Option<String>,
    pub checked_nar_hash: String,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Failures {
    #[serde(default)]
    pub lock_sha256: String,
    #[serde(default)]
    pub pins: BTreeMap<String, BTreeMap<String, Pin>>,
}

impl Failures {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(self).expect("failures serialize");
        out.push(b'\n');
        out
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &self.to_bytes())
    }

    pub fn get(&self, input: &str, attr: &str) -> Option<&Pin> {
        self.pins.get(input)?.get(attr)
    }

    pub fn insert(&mut self, input: &str, attr: &str, pin: Pin) {
        self.pins
            .entry(input.to_string())
            .or_default()
            .insert(attr.to_string(), pin);
    }

    pub fn remove(&mut self, input: &str, attr: &str) -> Option<Pin> {
        let set = self.pins.get_mut(input)?;
        let pin = set.remove(attr);
        if set.is_empty() {
            self.pins.remove(input);
        }
        pin
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, &Pin)> {
        self.pins.iter().flat_map(|(input, set)| {
            set.iter()
                .map(move |(attr, pin)| (input.as_str(), attr.as_str(), pin))
        })
    }

    pub fn len(&self) -> usize {
        self.pins.values().map(BTreeMap::len).sum()
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pin(since: &str) -> Pin {
        Pin {
            locked: json!({ "type": "github", "rev": "abc", "narHash": "sha256-X" }),
            since: since.into(),
            reason: Reason::Build,
            failed_drv: Some("/nix/store/aaa-libfoo-1.2.drv".into()),
            checked_nar_hash: "sha256-Y".into(),
        }
    }

    #[test]
    fn round_trip_and_stable_order() {
        let mut f = Failures::default();
        f.lock_sha256 = "00".into();
        f.insert("nixpkgs-unstable", "zed-editor", pin("2026-10-20"));
        f.insert("nixpkgs-unstable", "obs-studio", pin("2026-10-21"));
        f.insert("nixpkgs", "hello", pin("2026-10-22"));
        let bytes = f.to_bytes();
        let back: Failures = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, f);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.find("\"nixpkgs\"").unwrap() < text.find("\"nixpkgs-unstable\"").unwrap());
        assert!(text.find("obs-studio").unwrap() < text.find("zed-editor").unwrap());
        assert!(text.contains("\"checkedNarHash\""));
        assert!(text.contains("\"lockSha256\""));
        assert_eq!(f.len(), 3);
    }

    #[test]
    fn remove_drops_empty_inputs() {
        let mut f = Failures::default();
        f.insert("nixpkgs", "hello", pin("2026-10-22"));
        assert!(f.remove("nixpkgs", "hello").is_some());
        assert!(f.pins.is_empty());
        assert!(f.remove("nixpkgs", "hello").is_none());
    }

    #[test]
    fn empty_file_shape() {
        let f: Failures = serde_json::from_str("{}").unwrap();
        assert_eq!(f.len(), 0);
    }
}
