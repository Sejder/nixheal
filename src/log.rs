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

/// Whether stderr is the journal: systemd sets JOURNAL_STREAM to the
/// device and inode of the stream it connected, which a terminal or a pipe a
/// child of the service opened does not match.
fn journal() -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(stream) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let Some((dev, ino)) = stream
        .to_string_lossy()
        .split_once(':')
        .map(|(d, i)| (d.to_string(), i.to_string()))
    else {
        return false;
    };
    std::fs::metadata("/proc/self/fd/2")
        .map(|m| m.dev().to_string() == dev && m.ino().to_string() == ino)
        .unwrap_or(false)
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

/// An error message forwarded from nix; shown with --quiet too.
pub fn nix_error(msg: &str) {
    eprintln!("{msg}");
}

/// Logged at warning priority when stderr goes to the journal.
pub fn warn(msg: &str) {
    if journal() {
        eprintln!("<4>nixheal: warning: {msg}");
    } else {
        eprintln!("nixheal: warning: {msg}");
    }
}
