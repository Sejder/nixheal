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
    /// The run added pins.json to the git index, which it was not in.
    pub untrack: bool,
    pub flake: PathBuf,
}

impl Restore {
    /// Put the files back; returns whether anything had changed.
    pub fn apply(&self) -> Result<bool> {
        let mut changed = false;
        if std::fs::read(&self.lock_path).ok().as_deref() != Some(self.lock.as_slice()) {
            write_atomic(&self.lock_path, &self.lock)?;
            changed = true;
        }
        match &self.pins {
            Some(bytes) => {
                if std::fs::read(&self.pins_path).ok().as_deref() != Some(bytes.as_slice()) {
                    write_atomic(&self.pins_path, bytes)?;
                    changed = true;
                }
            }
            None => match std::fs::remove_file(&self.pins_path) {
                Ok(()) => changed = true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("removing {}", self.pins_path.display()));
                }
            },
        }
        if (self.pins.is_none() || self.untrack)
            && let Some(git) = Git::open(&self.flake)
        {
            git.forget(&self.pins_path)?;
        }
        Ok(changed)
    }
}

static ARMED: Mutex<Option<Restore>> = Mutex::new(None);
static HANDLER: Once = Once::new();
static CHILDREN: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Remember a running nix, so an interrupt stops it too: a SIGTERM sent to
/// nixheal alone (kill, timeout) does not reach it otherwise.
pub fn track(pid: u32) {
    if let Ok(mut c) = CHILDREN.lock() {
        c.push(pid);
    }
}

pub fn untrack(pid: u32) {
    if let Ok(mut c) = CHILDREN.lock() {
        c.retain(|p| *p != pid);
    }
}

fn stop_children() {
    if let Ok(c) = CHILDREN.lock() {
        for pid in c.iter() {
            if let Ok(pid) = i32::try_from(*pid) {
                // SAFETY: kill(2) only sends a signal; a stale pid is harmless here.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
    }
}

/// Put the files back on SIGINT or SIGTERM while `restore` is armed, so an
/// interrupted run, a Ctrl-C or a shutdown during the timer, leaves the
/// checkout as it found it.
pub fn arm(restore: Restore) {
    HANDLER.call_once(|| {
        let _ = ctrlc::set_handler(|| {
            stop_children();
            // Exit while still holding the lock, so a write the main thread
            // is waiting to make in `critical` never lands after the restore.
            let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(r) = armed.take() {
                match r.apply() {
                    Ok(_) => log::change("interrupted; restored flake.lock and pins.json"),
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

/// Put the files back now, and stop guarding them. Says so when anything
/// had changed.
pub fn restore() -> Result<()> {
    let taken = ARMED.lock().ok().and_then(|mut a| a.take());
    if let Some(r) = taken
        && r.apply()?
    {
        log::change("restored flake.lock and pins.json");
    }
    Ok(())
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
