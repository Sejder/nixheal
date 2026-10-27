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
        eprintln!("nixheal: {msg}");
    }
}

/// A pin, unpin, hold or release, or why nixheal gave up; always shown.
pub fn change(msg: &str) {
    eprintln!("nixheal: {msg}");
}

/// A build or download nix starts, forwarded off a terminal so the journal
/// shows progress as plain nix would; hidden with --quiet.
pub fn nix_progress(msg: &str) {
    if !is_quiet() {
        eprintln!("{msg}");
    }
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
        eprintln!("<4>nixheal: warning: {msg}");
    } else {
        eprintln!("nixheal: warning: {msg}");
    }
}
