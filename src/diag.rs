//! Tiny diagnostic sink. Background threads (narrator, synth, spine) emit
//! `[stage] ...` progress/notice lines. In headless mode those go to stderr; in
//! TUI mode they would corrupt the alternate screen, so the TUI silences them
//! up front. Real errors do not go through here — they are returned as `Err` and
//! printed by `main` only after the terminal is restored (§5.4).

use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);

/// Silence (true) or allow (false) diagnostic output.
pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

/// Whether diagnostics should print.
pub fn enabled() -> bool {
    !QUIET.load(Ordering::Relaxed)
}

/// `eprintln!`-style diagnostic line, suppressed when quiet (TUI mode).
#[macro_export]
macro_rules! diag {
    ($($arg:tt)*) => {{
        if $crate::diag::enabled() {
            eprintln!($($arg)*);
        }
    }};
}
