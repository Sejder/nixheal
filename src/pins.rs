use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub const VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Nixos,
    Home,
}

impl Kind {
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Nixos => "host",
            Kind::Home => "home",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    Build,
    Eval,
    Manual,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Build => "build",
            Reason::Eval => "eval",
            Reason::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pin {
    pub locked: Value,
    pub since: String,
    pub reason: Reason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_drv: Option<String>,
    /// Derivation names from the failed drv up to the pinned package.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trail: Vec<String>,
    pub checked_nar_hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Held {
    pub since: String,
    pub reason: String,
    /// The whole flake.lock this host last built with.
    pub lock: Value,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    #[serde(default)]
    pub lock_sha256: String,
    #[serde(default)]
    pub lock_last_modified: i64,
    #[serde(default)]
    pub pins: BTreeMap<String, BTreeMap<String, Pin>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<Held>,
}

impl Entry {
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

    /// The inputs that pin `attr`, for commands that take only an attr name.
    pub fn inputs_pinning(&self, attr: &str) -> Vec<String> {
        self.pins
            .iter()
            .filter(|(_, set)| set.contains_key(attr))
            .map(|(input, _)| input.clone())
            .collect()
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinsFile {
    pub version: u32,
    #[serde(default)]
    pub hosts: BTreeMap<String, Entry>,
    #[serde(default)]
    pub homes: BTreeMap<String, Entry>,
}

impl Default for PinsFile {
    fn default() -> Self {
        Self {
            version: VERSION,
            hosts: BTreeMap::new(),
            homes: BTreeMap::new(),
        }
    }
}

impl PinsFile {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let json: Value = serde_json::from_slice(bytes)?;
        match json["version"].as_u64() {
            Some(v) if v == VERSION as u64 => Ok(serde_json::from_value(json)?),
            Some(v) => bail!("pins file version {v} is not supported (expected {VERSION})"),
            None if json.as_object().is_some_and(|o| o.is_empty()) => Ok(Self::default()),
            None => bail!("not a nixheal pins file (no version)"),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Self::parse(&bytes).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(self).expect("pins serialize");
        out.push(b'\n');
        out
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &self.to_bytes())
    }

    fn section(&self, kind: Kind) -> &BTreeMap<String, Entry> {
        match kind {
            Kind::Nixos => &self.hosts,
            Kind::Home => &self.homes,
        }
    }

    fn section_mut(&mut self, kind: Kind) -> &mut BTreeMap<String, Entry> {
        match kind {
            Kind::Nixos => &mut self.hosts,
            Kind::Home => &mut self.homes,
        }
    }

    pub fn entry(&self, kind: Kind, name: &str) -> Option<&Entry> {
        self.section(kind).get(name)
    }

    pub fn entry_mut(&mut self, kind: Kind, name: &str) -> &mut Entry {
        self.section_mut(kind).entry(name.to_string()).or_default()
    }

    pub fn set(&mut self, kind: Kind, name: &str, entry: Entry) {
        self.section_mut(kind).insert(name.to_string(), entry);
    }

    pub fn entries(&self) -> impl Iterator<Item = (Kind, &str, &Entry)> {
        self.hosts
            .iter()
            .map(|(n, e)| (Kind::Nixos, n.as_str(), e))
            .chain(self.homes.iter().map(|(n, e)| (Kind::Home, n.as_str(), e)))
    }

    /// Put `entry` in place unless the file already holds one computed from a
    /// newer lock: how `nixheal merge` combines two files, e.g. a fleet's
    /// published pins with a checkout's. Returns whether the entry was taken.
    pub fn merge(&mut self, kind: Kind, name: &str, entry: Entry) -> bool {
        let section = self.section_mut(kind);
        if let Some(existing) = section.get(name)
            && existing.lock_last_modified > entry.lock_last_modified
        {
            return false;
        }
        section.insert(name.to_string(), entry);
        true
    }
}

/// Replace `path` with `bytes` in one rename. The temporary file gets a fresh
/// name and is created exclusively, so a symlink planted in its place is never
/// followed. A replaced file keeps its owner and mode, so root rewriting a
/// user's flake.lock leaves it the user's.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let before = std::fs::metadata(path).ok();
    let mode = before
        .as_ref()
        .map_or(0o644, |m| m.permissions().mode() & 0o7777);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());

    for attempt in 0..32 {
        let tmp = path.with_file_name(format!(
            ".{name}.{}.{nanos}.{attempt}.tmp",
            std::process::id()
        ));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", tmp.display())),
        };
        let written = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("writing {}", tmp.display()));
        }
        if let Some(m) = &before {
            let _ = std::os::unix::fs::chown(&tmp, Some(m.uid()), Some(m.gid()));
        }
        return std::fs::rename(&tmp, path)
            .with_context(|| format!("replacing {}", path.display()));
    }
    anyhow::bail!(
        "could not create a temporary file next to {}",
        path.display()
    )
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
            trail: vec!["libfoo-1.2".into(), "obs-studio-31".into()],
            checked_nar_hash: "sha256-Y".into(),
        }
    }

    fn entry(lm: i64) -> Entry {
        Entry {
            lock_sha256: format!("{lm}"),
            lock_last_modified: lm,
            ..Default::default()
        }
    }

    #[test]
    fn round_trip_and_stable_order() {
        let mut f = PinsFile::default();
        let e = f.entry_mut(Kind::Nixos, "laptop");
        e.insert("nixpkgs-unstable", "zed-editor", pin("2026-10-20"));
        e.insert("nixpkgs", "hello", pin("2026-10-22"));
        f.entry_mut(Kind::Home, "alice").held = Some(Held {
            since: "2026-10-27".into(),
            reason: "linux fails".into(),
            lock: json!({ "nodes": {} }),
        });
        let bytes = f.to_bytes();
        assert_eq!(PinsFile::parse(&bytes).unwrap(), f);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"version\": 2"));
        assert!(text.find("\"nixpkgs\"").unwrap() < text.find("\"nixpkgs-unstable\"").unwrap());
        assert!(text.contains("\"checkedNarHash\""));
        assert!(text.contains("\"lockLastModified\""));
        assert_eq!(f.entry(Kind::Nixos, "laptop").unwrap().len(), 2);
    }

    #[test]
    fn rejects_other_versions() {
        assert!(PinsFile::parse(br#"{"version":3}"#).is_err());
        assert!(PinsFile::parse(br#"{"lockSha256":"x","pins":{}}"#).is_err());
        assert_eq!(PinsFile::parse(b"{}").unwrap(), PinsFile::default());
    }

    #[test]
    fn merge_keeps_the_newer_lock() {
        let mut f = PinsFile::default();
        assert!(f.merge(Kind::Nixos, "laptop", entry(10)));
        assert!(!f.merge(Kind::Nixos, "laptop", entry(5)));
        assert_eq!(
            f.entry(Kind::Nixos, "laptop").unwrap().lock_last_modified,
            10
        );
        assert!(f.merge(Kind::Nixos, "laptop", entry(10)));
        assert!(f.merge(Kind::Nixos, "laptop", entry(20)));
        assert_eq!(
            f.entry(Kind::Nixos, "laptop").unwrap().lock_last_modified,
            20
        );
    }

    #[test]
    fn merge_leaves_other_hosts_alone() {
        let mut f = PinsFile::default();
        f.set(Kind::Nixos, "pi", entry(3));
        f.set(Kind::Home, "laptop", entry(99));
        f.merge(Kind::Nixos, "laptop", entry(7));
        assert_eq!(f.entry(Kind::Nixos, "pi").unwrap().lock_last_modified, 3);
        assert_eq!(
            f.entry(Kind::Home, "laptop").unwrap().lock_last_modified,
            99
        );
        assert_eq!(
            f.entry(Kind::Nixos, "laptop").unwrap().lock_last_modified,
            7
        );
    }

    #[test]
    fn write_atomic_keeps_mode_and_ignores_planted_temp_names() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("nixheal-wa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("flake.lock");
        std::fs::write(&target, b"old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let decoy = dir.join("decoy");
        std::fs::write(&decoy, b"untouched").unwrap();
        std::os::unix::fs::symlink(&decoy, dir.join(".flake.lock.tmp")).unwrap();
        write_atomic(&target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::read(&decoy).unwrap(), b"untouched");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_drops_empty_inputs() {
        let mut e = Entry::default();
        e.insert("nixpkgs", "hello", pin("2026-10-22"));
        assert_eq!(e.inputs_pinning("hello"), vec!["nixpkgs"]);
        assert!(e.remove("nixpkgs", "hello").is_some());
        assert!(e.pins.is_empty());
        assert!(e.inputs_pinning("hello").is_empty());
    }
}
