fn journal() -> bool {
    std::env::var_os("JOURNAL_STREAM").is_some()
}

pub fn info(msg: &str) {
    eprintln!("autopin: {msg}");
}

/// Logged at warning priority when stderr goes to the journal.
pub fn warn(msg: &str) {
    if journal() {
        eprintln!("<4>autopin: warning: {msg}");
    } else {
        eprintln!("autopin: warning: {msg}");
    }
}
