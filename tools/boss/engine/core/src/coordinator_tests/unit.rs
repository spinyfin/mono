//! Unit tests for the module's free helper functions (conflict-descriptor
//! stripping, rebase-payload parsing, live-worker occupancy, failing-check
//! ranking).
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;

/// Reproduces the live incident (flunge, PR brianduff/flunge#906,
/// 2026-07-16): rung 1's `conflicted_files` come straight from `jj
/// resolve --list`, whose entries carry a trailing conflict-type
/// descriptor glued onto the path (e.g. `"MODULE.bazel.lock    2-sided
/// conflict"`). Left unstripped, rung 0's resolver lookup does a
/// `file_name()` match against the whole annotated string and never
/// matches, so the batch gets declined and escalated to an agent even
/// though a resolver for the bare filename exists.
#[test]
fn strip_jj_conflict_descriptor_removes_trailing_annotation() {
    assert_eq!(
        strip_jj_conflict_descriptor("MODULE.bazel.lock    2-sided conflict"),
        "MODULE.bazel.lock",
    );
    // Descriptor can carry extra detail past the sided-conflict marker.
    assert_eq!(
        strip_jj_conflict_descriptor("f.txt    2-sided conflict including 1 deletion"),
        "f.txt",
    );
    // Paths with internal spaces must not be truncated at the first space.
    assert_eq!(
        strip_jj_conflict_descriptor("a b long name.txt    2-sided conflict"),
        "a b long name.txt",
    );
    // Already-bare paths (or an unrecognized future format) pass through.
    assert_eq!(strip_jj_conflict_descriptor("MODULE.bazel.lock"), "MODULE.bazel.lock");
    assert_eq!(strip_jj_conflict_descriptor("  spaced.txt  "), "spaced.txt");
}

#[test]
fn parse_rebase_payload_strips_conflict_descriptors_from_conflicted_files() {
    let payload = serde_json::json!({
        "status": "conflicts",
        "pushed": false,
        "conflicted_files": [
            "MODULE.bazel.lock    2-sided conflict",
            "a b long name.txt    3-sided conflict including 1 deletion",
        ],
    });
    let outcome = parse_rebase_payload(payload).expect("payload parses");
    assert!(!outcome.clean);
    assert!(!outcome.pushed);
    assert_eq!(
        outcome.conflicted_files,
        vec!["MODULE.bazel.lock".to_owned(), "a b long name.txt".to_owned()],
    );
}

/// Lease-time occupancy guard (defect 3, regression test c). The
/// pure decision: a workspace is "occupied" only by a tracked worker
/// with a *live* process and non-terminal activity on that workspace;
/// a dead-pid occupant (the orphan-resume case) is re-leasable, and
/// the dispatching execution never blocks itself.
#[test]
fn occupying_live_worker_blocks_only_a_live_tracked_occupant() {
    use boss_protocol::{LiveWorkerState, WorkerActivity};

    // exec-live: alive process; exec-dead: process gone. Both are
    // recorded against the SAME workspace cube just handed us.
    let mut live = LiveWorkerState::new_spawning(1, "exec-live", "opus", 4242, None);
    live.activity = WorkerActivity::Working;
    let mut dead = LiveWorkerState::new_spawning(2, "exec-dead", "opus", 5151, None);
    dead.activity = WorkerActivity::Working;

    let workspace_of = |eid: &str| match eid {
        "exec-live" | "exec-dead" | "exec-new" => Some("mono-agent-021".to_owned()),
        _ => None,
    };
    let pid_alive = |pid: i32| pid == 4242; // only exec-live is alive

    // A redispatch CANNOT lease a workspace occupied by a live process.
    assert_eq!(
        occupying_live_worker("mono-agent-021", "exec-new", &[live.clone()], workspace_of, pid_alive),
        Some("exec-live".to_owned()),
        "a live occupant must block the lease",
    );

    // A dead-pid occupant does NOT block — the workspace is genuinely
    // free (normal orphan-resume).
    assert_eq!(
        occupying_live_worker("mono-agent-021", "exec-new", &[dead], workspace_of, pid_alive),
        None,
        "a dead occupant must not block the lease",
    );

    // The dispatching execution never blocks itself.
    let mut myself = LiveWorkerState::new_spawning(3, "exec-new", "opus", 4242, None);
    myself.activity = WorkerActivity::Working;
    assert_eq!(
        occupying_live_worker("mono-agent-021", "exec-new", &[myself], workspace_of, pid_alive),
        None,
        "the dispatching execution must never block itself",
    );

    // A terminal-activity occupant has released its slot — not occupying.
    let mut terminated = LiveWorkerState::new_spawning(4, "exec-live", "opus", 4242, None);
    terminated.activity = WorkerActivity::Terminated;
    assert_eq!(
        occupying_live_worker("mono-agent-021", "exec-new", &[terminated], workspace_of, pid_alive),
        None,
        "a terminated worker no longer occupies its workspace",
    );

    // Occupancy is workspace-scoped: a live worker on a DIFFERENT
    // workspace doesn't block this lease.
    assert_eq!(
        occupying_live_worker("mono-agent-099", "exec-new", &[live], workspace_of, pid_alive),
        None,
        "occupancy must be scoped to the leased workspace",
    );
}

#[test]
fn pick_worst_failing_check_prefers_failure() {
    let json = serde_json::json!([
            {"name": "infra", "conclusion": "CANCELLED", "target_url": "https://buildkite.com/o/p/builds/2#j", "provider": "buildkite", "provider_job_id": "j"},
            {"name": "tests", "conclusion": "FAILURE", "target_url": "https://buildkite.com/o/p/builds/3#k", "provider": "buildkite", "provider_job_id": "k"},
            {"name": "x", "conclusion": "TIMED_OUT", "target_url": "https://buildkite.com/o/p/builds/4#l", "provider": "buildkite", "provider_job_id": "l"},
        ])
        .to_string();
    let picked = pick_worst_failing_check(&json).expect("expected one entry");
    assert_eq!(picked.conclusion, "FAILURE");
    assert_eq!(picked.provider, "buildkite");
    assert_eq!(picked.provider_job_id.as_deref(), Some("k"));
}

#[test]
fn pick_worst_failing_check_handles_malformed_json() {
    assert!(pick_worst_failing_check("{not json}").is_none());
    assert!(pick_worst_failing_check("[]").is_none());
}

#[test]
fn pick_worst_failing_check_falls_back_to_only_entry() {
    let json = serde_json::json!([
            {"name": "n", "conclusion": "STARTUP_FAILURE", "target_url": "u", "provider": "github_actions", "provider_job_id": "1"},
        ])
        .to_string();
    let picked = pick_worst_failing_check(&json).expect("entry");
    assert_eq!(picked.conclusion, "STARTUP_FAILURE");
}
