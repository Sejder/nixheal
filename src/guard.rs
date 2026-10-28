use crate::git::Git;
use crate::log;
use crate::pins::write_atomic;
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

/// The files a run must put back if it does not finish: flake.lock and
/// pins.json as they were, or pins.json removed when the run created it.
pub struct Restore {
    pub lock_path: PathBuf,
    pub lock: Vec<u8>,
    pub pins_path: PathBuf,
    /// None when this run created pins.json.
    pub pins: Option<Vec<u8>>,
    pub flake: PathBuf,
}

impl Restore {
    pub fn apply(&self) -> Result<()> {
        write_atomic(&self.lock_path, &self.lock)?;
        match &self.pins {
            Some(bytes) => write_atomic(&self.pins_path, bytes),
            None => {
                match std::fs::remove_file(&self.pins_path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        return Err(e)
                            .with_context(|| format!("removing {}", self.pins_path.display()));
                    }
                    _ => {}
                }
                if let Some(git) = Git::open(&self.flake) {
                    git.forget(&self.pins_path)?;
                }
                Ok(())
            }
        }
    }
}

static ARMED: Mutex<Option<Restore>> = Mutex::new(None);
static HANDLER: Once = Once::new();

/// Put the files back on SIGINT or SIGTERM while `restore` is armed, so an
/// interrupted run, a Ctrl-C or a shutdown during the timer, leaves the
/// checkout as it found it.
pub fn arm(restore: Restore) {
    HANDLER.call_once(|| {
        let _ = ctrlc::set_handler(|| {
            if let Ok(mut armed) = ARMED.lock()
                && let Some(r) = armed.take()
            {
                match r.apply() {
                    Ok(()) => log::change("interrupted; restored flake.lock and pins.json"),
                    Err(e) => log::warn(&format!("interrupted; restoring failed: {e:#}")),
                }
            }
            std::process::exit(130);
        });
    });
    if let Ok(mut armed) = ARMED.lock() {
        *armed = Some(restore);
    }
}

/// Run `f` so a signal cannot interrupt it halfway: the handler waits for it,
/// then restores. For writes of the guarded files during a run.
pub fn critical<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _held = ARMED.lock();
    f()
}

/// Run `f`, the step that makes a run's results stand (its commit), and stop
/// guarding once it succeeded, in one step a signal cannot split: either the
/// files are restored or they are committed, never reverted under a commit.
pub fn settle<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
    let result = f();
    if result.is_ok() {
        *armed = None;
    }
    result
}

/// Put the files back now, and stop guarding them.
pub fn restore() -> Result<()> {
    let taken = ARMED.lock().ok().and_then(|mut a| a.take());
    match taken {
        Some(r) => r.apply(),
        None => Ok(()),
    }
}

/// An exclusive lock on the flake for the length of a run, in its git
/// directory when it has one, so a timer run and a manual run cannot write
/// the same files at once.
pub struct RunLock {
    _file: File,
}

pub fn lock_flake(flake: &Path) -> Result<RunLock> {
    let path = match Git::open(flake).and_then(|g| g.git_dir()) {
        Some(dir) => dir.join("nixheal.lock"),
        None => crate::state::lock_path(flake)?,
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(RunLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!(
                "another nixheal run is using {}; try again when it is done",
                flake.display()
            )
        }
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", path.display()))
        }
    }
}
