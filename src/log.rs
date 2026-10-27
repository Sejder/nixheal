use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);

pub fn set_quiet(quiet: bool) {
    if quiet {
        QUIET.store(true, Ordering::Relaxed);
    }
}

pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

fn journal() -> bool {
    std::env::var_os("JOURNAL_STREAM").is_some()
}

/// Progress; hidden with --quiet.
pub fn info(msg: &str) {
    if !is_quiet() {
        eprintln!("autopin: {msg}");
    }
}

/// A pin, unpin, hold or release, or why autopin gave up; always shown.
pub fn change(msg: &str) {
    eprintln!("autopin: {msg}");
}

/// An error message forwarded from nix; hidden with --quiet.
pub fn nix_error(msg: &str) {
    if !is_quiet() {
        eprintln!("{msg}");
    }
}

/// Logged at warning priority when stderr goes to the journal.
pub fn warn(msg: &str) {
    if journal() {
        eprintln!("<4>autopin: warning: {msg}");
    } else {
        eprintln!("autopin: warning: {msg}");
    }
}
