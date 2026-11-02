//! Unit tests for the coordinator's free helper functions (ineligibility
//! summary, live-worker occupancy, failing-check ranking). The cube payload
//! parsing helpers are tested beside `CommandCubeClient` in the engine.

use super::{occupying_live_worker, pick_worst_failing_check, summarize_ineligibility};
use boss_execution_contracts::{Eligibility, IneligibilityReason};

fn ineligible_host(id: &str, reasons: Vec<IneligibilityReason>) -> Eligibility {
    Eligibility {
        host_id: id.to_owned(),
        eligible: false,
        reasons,
        free_slots: 0,
        had_prior_run_on_branch: false,
    }
}

#[test]
fn summarize_ineligibility_names_driver_only_when_every_host_lacks_it() {
    let all_missing = vec![
        ineligible_host(
            "local",
            vec![IneligibilityReason::MissingCapabilities(vec!["driver=codex".into()])],
        ),
        ineligible_host(
            "remote",
            vec![IneligibilityReason::MissingCapabilities(vec!["driver=codex".into()])],
        ),
    ];
    let summary = summarize_ineligibility(&all_missing, Some("codex"));
    assert!(summary.starts_with("no enabled host has driver codex;"));
    assert!(summary.contains("local: missing driver codex"));

    let mixed = vec![
        ineligible_host(
            "local",
            vec![IneligibilityReason::MissingCapabilities(vec!["driver=codex".into()])],
        ),
        ineligible_host("remote", vec![IneligibilityReason::NoFreeSlots]),
    ];
    let summary = summarize_ineligibility(&mixed, Some("codex"));
    assert!(!summary.contains("no enabled host has driver"));
    assert_eq!(summary, "local: missing driver codex; remote: no free slots");
}

#[test]
fn summarize_ineligibility_directs_an_unprobed_remote_host_to_discovery() {
    let report = vec![ineligible_host(
        "anaplian",
        vec![IneligibilityReason::DriverProbeNotRun],
    )];

    assert_eq!(
        summarize_ineligibility(&report, Some("claude")),
        "anaplian: driver discovery has not run; run `bossctl hosts probe anaplian`"
    );
}

#[test]
fn summarize_ineligibility_does_not_recommend_probing_a_disabled_host() {
    let report = vec![ineligible_host(
        "anaplian",
        vec![IneligibilityReason::Disabled, IneligibilityReason::DriverProbeNotRun],
    )];

    assert_eq!(summarize_ineligibility(&report, Some("claude")), "anaplian: disabled");
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
