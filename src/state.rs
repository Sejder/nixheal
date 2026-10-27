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

/// Where the last lock that built for `flake` is kept, one file per flake.
fn last_good_path(flake: &Path) -> Option<PathBuf> {
    let key: String = Sha256::digest(flake.to_string_lossy().as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(state_dir()?.join(format!("last-good-{key}.lock")))
}

pub fn load_last_good(flake: &Path) -> Option<Lock> {
    Lock::load(&last_good_path(flake)?).ok()
}

pub fn save_last_good(flake: &Path, lock: &Lock) -> Result<()> {
    let Some(path) = last_good_path(flake) else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    write_atomic(&path, lock.bytes())
}
