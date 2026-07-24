//! Progress reporting for long-running stages of a command.

use std::time::{Duration, Instant};

use crate::app::errors::Result;

/// Minimum stage duration before a completion line is printed. Fast stages
/// stay silent so normal runs aren't spammed; slow stages (the whole point
/// of this instrumentation — see `run_stage`'s doc comment) get an explicit
/// "done (Ns)" line so a multi-minute gate run doesn't read as a hang.
const STAGE_ELAPSED_REPORT_THRESHOLD: Duration = Duration::from_secs(3);

/// Run `f`, announcing `label` on stderr before it starts and, if it takes
/// longer than [`STAGE_ELAPSED_REPORT_THRESHOLD`], announcing completion
/// with elapsed time afterward. Human-readable progress only — stdout stays
/// reserved for parseable output (the PR URL), per repobin/cube convention.
pub(super) fn run_stage<T>(label: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    eprintln!("cube: {label}…");
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed();
    if elapsed >= STAGE_ELAPSED_REPORT_THRESHOLD {
        eprintln!("cube: {label} done ({:.1}s)", elapsed.as_secs_f64());
    }
    result
}
