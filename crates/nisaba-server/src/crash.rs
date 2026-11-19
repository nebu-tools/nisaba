//! Injected crashes, for the tests that stop the server in the middle of a
//! write.
//!
//! When the environment variable `NISABA_CRASH_AT` names a point, the
//! process kills itself with `SIGKILL` when it reaches that point, as if the
//! power went off there: no destructor runs, nothing is flushed. Without the
//! variable every point does nothing. The points are listed in [`POINTS`],
//! and the test `a_crash_at_any_point_of_a_write_loses_nothing_acknowledged`
//! stops the server at each one.

use std::sync::OnceLock;

/// Every crash point, in the order a write passes them.
pub const POINTS: &[&str] = &[
    "append:before-commit",
    "append:after-commit",
    "compact:snapshot-written",
    "compact:before-commit",
    "compact:after-commit",
    "asset:file-written",
    "asset:renamed",
    "asset:after-commit",
    "project:created",
    "project:deleted",
    "backup:database-copied",
];

fn armed() -> Option<&'static str> {
    static AT: OnceLock<Option<String>> = OnceLock::new();
    AT.get_or_init(|| std::env::var("NISABA_CRASH_AT").ok())
        .as_deref()
}

/// Stops the process here if `NISABA_CRASH_AT` names this point.
pub fn point(name: &'static str) {
    debug_assert!(POINTS.contains(&name), "unknown crash point {name}");
    if armed() == Some(name) {
        die();
    }
}

#[cfg(target_os = "linux")]
fn die() -> ! {
    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
    std::process::abort()
}

#[cfg(not(target_os = "linux"))]
fn die() -> ! {
    std::process::abort()
}
