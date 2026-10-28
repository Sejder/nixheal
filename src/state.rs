use crate::config::Target;
use crate::lock::Lock;
use crate::pins::write_atomic;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|d| d.join("nixheal"))
}

pub fn key(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Where the last lock that built is kept: one file per flake and target, and
/// one per flake for targets that have not built yet.
fn last_good_path(flake: &Path, target: Option<&Target>) -> Option<PathBuf> {
    let flake = flake.to_string_lossy();
    let name = match target {
        Some(t) => format!("last-good-{}.lock", key(&format!("{flake}\0{t}"))),
        None => format!("last-good-{}.lock", key(&flake)),
    };
    Some(state_dir()?.join(name))
}

/// The run lock for a flake that is not in a git repository.
pub fn lock_path(flake: &Path) -> Result<PathBuf> {
    let dir = state_dir().context("neither XDG_STATE_HOME nor HOME is set")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(format!("run-{}.lock", key(&flake.to_string_lossy()))))
}

pub fn load_last_good(flake: &Path, target: Option<&Target>) -> Option<Lock> {
    Lock::load(&last_good_path(flake, target)?).ok()
}

pub fn save_last_good(flake: &Path, target: Option<&Target>, lock: &Lock) -> Result<()> {
    let Some(path) = last_good_path(flake, target) else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    write_atomic(&path, lock.bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pins::Kind;

    #[test]
    fn one_file_per_target() {
        let flake = Path::new("/home/alice/nixos");
        let a = last_good_path(flake, Some(&Target::new(Kind::Nixos, "a")));
        let b = last_good_path(flake, Some(&Target::new(Kind::Nixos, "b")));
        let home = last_good_path(flake, Some(&Target::new(Kind::Home, "a")));
        let any = last_good_path(flake, None);
        assert_ne!(a, b);
        assert_ne!(a, home);
        assert_ne!(a, any);
    }
}
