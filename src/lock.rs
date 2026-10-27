use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

pub struct Lock {
    bytes: Vec<u8>,
    json: Value,
}

impl Lock {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_bytes(bytes).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let json: Value = serde_json::from_slice(&bytes)?;
        if !json["nodes"].is_object() {
            bail!("not a flake.lock: no nodes");
        }
        Ok(Self { bytes, json })
    }

    pub fn sha256(&self) -> String {
        Sha256::digest(&self.bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn root(&self) -> &str {
        self.json["root"].as_str().unwrap_or("root")
    }

    fn resolve(&self, node: &str, input: &str, depth: u32) -> Option<String> {
        if depth > 32 {
            return None;
        }
        match &self.json["nodes"][node]["inputs"][input] {
            Value::String(s) => Some(s.clone()),
            Value::Array(path) => {
                let mut cur = self.root().to_string();
                for seg in path {
                    cur = self.resolve(&cur, seg.as_str()?, depth + 1)?;
                }
                Some(cur)
            }
            _ => None,
        }
    }

    pub fn locked(&self, input: &str) -> Option<Value> {
        let node = self.resolve(self.root(), input, 0)?;
        let locked = &self.json["nodes"][node]["locked"];
        locked.is_object().then(|| locked.clone())
    }

    pub fn nar_hash(&self, input: &str) -> Option<String> {
        self.locked(input)?["narHash"].as_str().map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"{
      "nodes": {
        "nixpkgs_4": { "locked": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "aaa", "narHash": "sha256-A" } },
        "nixpkgs-unstable": { "locked": { "type": "tarball", "url": "https://releases.nixos.org/x/nixexprs.tar.zst", "narHash": "sha256-U" } },
        "hm": { "inputs": { "nixpkgs": ["nixpkgs"] } },
        "root": { "inputs": { "nixpkgs": "nixpkgs_4", "nixpkgs-unstable": "nixpkgs-unstable", "home-manager": "hm", "followed": ["home-manager", "nixpkgs"] } }
      },
      "root": "root",
      "version": 7
    }"#;

    fn lock() -> Lock {
        Lock::from_bytes(LOCK.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn resolves_root_inputs_through_node_names() {
        assert_eq!(lock().nar_hash("nixpkgs").as_deref(), Some("sha256-A"));
        assert_eq!(lock().nar_hash("nixpkgs-unstable").as_deref(), Some("sha256-U"));
        assert_eq!(lock().locked("nixpkgs").unwrap()["rev"], "aaa");
    }

    #[test]
    fn follows_paths() {
        assert_eq!(lock().nar_hash("followed").as_deref(), Some("sha256-A"));
    }

    #[test]
    fn missing_input() {
        assert!(lock().locked("nope").is_none());
        assert!(lock().locked("home-manager").is_none());
    }

    #[test]
    fn hash_is_hex_of_bytes() {
        let l = Lock::from_bytes(br#"{"nodes":{}}"#.to_vec()).unwrap();
        assert_eq!(l.sha256().len(), 64);
        assert_ne!(l.sha256(), lock().sha256());
    }
}
