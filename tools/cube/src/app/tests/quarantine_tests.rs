use super::support::{
    ExpectedCommand, FakeRunner, audit_events, force_lease_expiry, head_status_command, head_status_output,
    lease_runner_for, seed_mono_repo, unpushed_probe_command, with_database_path,
};
use clap::Parser;
use tempfile::TempDir;

use crate::cli::Cli;
use crate::metadata::WorkspaceState;
use crate::store::{Store, WorkspaceListFilter};

use crate::app::dispatch::run_with_dependencies;

/// Regression for the 2026-07-16 "dirty-reclaim guard hard-fails the
/// lease call, then the next lease destroys the 'protected' work
/// anyway" incident. Setup mimics the original 2026-05-12 race:
///   1. A worker leases a workspace and starts editing — `@` ends
///      up off main on an unbookmarked change (the worker's WIP).
///   2. The worker's lease ages past its TTL (engine forgot to
///      heartbeat — the orthogonal bug the engine-side fix
///      addresses).
///   3. A new lease request arrives. `expire_stale_leases` reclaims
///      the slot, health-check sees no uncommitted *file* changes so
///      claims it as `Clean`, and only then does the guard's stricter
///      `@`-empty-and-on-main check catch the still-there WIP.
///
/// Old (buggy) behavior: the guard refused the destructive reset but
/// that refusal hard-failed the WHOLE lease call, and the release it
/// performed to unwind the claim left the workspace plain `free` with
/// no trace of the refusal — so the very next lease call could select
/// the same workspace and destructively reset it, destroying the work
/// the guard had just "protected" three seconds earlier.
///
/// The fix this test pins down: the guard's refusal never hard-fails
/// the call. Instead the workspace is durably quarantined (health
/// status survives the release, unlike the one-shot `prior_expired`
/// bookkeeping) and the lease call transparently falls back to a
/// freshly auto-created workspace, so the caller gets a normal
/// successful lease and the prior worker's `@` is never touched.
#[test]
fn second_lease_quarantines_dirty_reclaim_and_falls_back_to_fresh_workspace() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // First lease — normal happy path.
    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let first = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "wip"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("first lease");
    let prior_lease_id = first.payload["workspace"]["lease_id"]
        .as_str()
        .expect("lease id")
        .to_string();
    lease_runner.assert_exhausted();

    // Worker has been editing — `@` is off main, has uncommitted
    // content. Force expiry so `expire_stale_leases` reclaims it
    // on the next lease call.
    force_lease_expiry(&database_path, &prior_lease_id, 1);

    // The second lease's reset path should run `jj status --no-pager`
    // (health check), then `jj git fetch`, then the head-status probe.
    // Stub that probe to return a non-empty `@` on an unpushed
    // feature bookmark — exactly the shape a still-active worker's WIP
    // looks like — so the guard falls through to the orphan probe, which
    // reports a real commit that no remote holds. That is the 2-in-400
    // case the guard exists for, and it must still refuse. Falling back
    // to a freshly auto-created workspace (`mono-agent-002`) then runs
    // the standard add/fetch/remote-list/bookmark-set/new/log sequence.
    let probe_output = head_status_output("abcd1234", false, "feature-bookmark", "feature-bookmark", "");
    let new_path = workspace_root.join("mono-agent-002");
    let staging = workspace_root.join(".incoming-mono-agent-002");
    let second_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        head_status_command(&workspace_path, &probe_output),
        unpushed_probe_command(&workspace_path, "abcd1234\t6e6b90bc\n"),
        ExpectedCommand::workspace_add_mono(&workspace_root, &staging),
        ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "def5678",
        ),
    ]);

    let second = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "incoming"]),
        Some(&database_path),
        &second_runner,
    )
    .expect("second lease must succeed via fresh-workspace fallback, not hard-fail");
    second_runner.assert_exhausted();

    assert_eq!(second.payload["workspace"]["workspace_id"], "mono-agent-002");

    // The crucial regression-pin: `jj new main` was NEVER invoked on
    // the quarantined workspace, so the prior worker's `@` is
    // untouched. The two read-only probes are the only post-fetch jj
    // calls that ran against `mono-agent-001`; every remaining command
    // in the script targets `mono-agent-002`, and the runner's exhausted
    // assertion above proves nothing extra was issued.
    let events = audit_events(&tempdir);
    let refused: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.reset_refused_dirty")
        .collect();
    assert_eq!(refused.len(), 1, "expected one workspace.reset_refused_dirty event");
    assert_eq!(refused[0]["prior_lease_id"], prior_lease_id);
    assert_eq!(refused[0]["workspace_path"], workspace_path.display().to_string());

    // `lease.expired_reclaimed` must also have been audited so the
    // timeline reads end-to-end ("we swept this lease, then we
    // refused to destructively reset its workspace, then we
    // quarantined it and fell back to a fresh one").
    let reclaimed: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "lease.expired_reclaimed")
        .collect();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0]["prior_lease_id"], prior_lease_id);

    let quarantined: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.dirty_reclaim_quarantined")
        .collect();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0]["workspace_id"], "mono-agent-001");
    assert_eq!(quarantined[0]["prior_lease_id"], prior_lease_id);

    // mono-agent-001 is durably quarantined (free, not plain free —
    // this is the fix for the "next lease destroys the 'protected'
    // work anyway" half of the bug: unlike the old one-shot
    // `prior_expired` guard, this state survives the release and is
    // visible to `cube workspace list`). mono-agent-002 is the fresh
    // workspace, leased.
    use crate::store::{EffectiveState, Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let rows = store.list_workspaces_filtered(&WorkspaceListFilter::default()).unwrap();
    assert_eq!(rows.len(), 2);

    let quarantined_row = rows
        .iter()
        .find(|r| r.workspace_id == "mono-agent-001")
        .expect("mono-agent-001 row");
    assert_eq!(quarantined_row.state, crate::metadata::WorkspaceState::Free);
    assert_eq!(
        quarantined_row.health_status,
        Some(crate::metadata::WorkspaceHealth::Quarantined)
    );
    assert_eq!(
        quarantined_row.last_release_reason.as_deref(),
        Some(crate::app::workspace::DIRTY_RECLAIM_QUARANTINE_REASON)
    );

    let fresh_row = rows
        .iter()
        .find(|r| r.workspace_id == "mono-agent-002")
        .expect("mono-agent-002 row");
    assert_eq!(fresh_row.state, crate::metadata::WorkspaceState::Leased);

    // The quarantined row is excluded from every `Free*` selection
    // surface a subsequent lease call would use — proving the guard is
    // now durable instead of one-shot.
    for effective in [
        EffectiveState::Free,
        EffectiveState::FreeDirty,
        EffectiveState::FreeConflicted,
    ] {
        let matches = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                effective_state: Some(effective),
                ..Default::default()
            })
            .unwrap();
        assert!(
            matches.iter().all(|r| r.workspace_id != "mono-agent-001"),
            "quarantined workspace must not appear under {effective:?}"
        );
    }
    let quarantined_filter = store
        .list_workspaces_filtered(&WorkspaceListFilter {
            effective_state: Some(EffectiveState::FreeQuarantined),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(quarantined_filter.len(), 1);
    assert_eq!(quarantined_filter[0].workspace_id, "mono-agent-001");
}

/// Seed a repo whose only workspace is quarantined.
fn seed_quarantined_workspace(
    tempdir: &TempDir,
    database_path: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, database_path);

    let mut store = crate::store::Store::open_at(database_path).unwrap();
    store
        .sync_workspaces(
            "mono",
            &[crate::metadata::WorkspaceCandidate {
                workspace_id: "mono-agent-001".to_string(),
                workspace_path: workspace_path.clone(),
            }],
        )
        .unwrap();
    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Quarantined)
        .unwrap();

    (workspace_root, workspace_path)
}

/// A quarantined workspace that still holds the prior holder's unpushed
/// work must never be selected by a later lease — the durability half of
/// the 2026-07-16 fix. Without it, the bug reproduces exactly as
/// reported: the guard "protects" the workspace in one call, and the very
/// next call destroys it anyway. The reclaim path added for the workspace
/// leak probes it (read-only) and must come away refusing.
#[test]
fn quarantined_workspace_with_unpushed_work_is_not_reclaimed_by_a_later_lease() {
    let (tempdir, database_path) = with_database_path();
    let (workspace_root, workspace_path) = seed_quarantined_workspace(&tempdir, &database_path);

    // The lease finds no clean candidate, so it probes the quarantined
    // workspace before growing the pool. The probe reports committed WIP
    // that no remote holds, so the quarantine stands and a fresh
    // workspace is provisioned after all. Note the probe is strictly
    // read-only: no `jj new` is ever issued against mono-agent-001.
    let new_path = workspace_root.join("mono-agent-002");
    let staging = workspace_root.join(".incoming-mono-agent-002");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        head_status_command(
            &workspace_path,
            &head_status_output("abcd1234", true, "boss/exec_local", "boss/exec_local", ""),
        ),
        unpushed_probe_command(&workspace_path, "abcd1234\t6e6b90bc\n"),
        ExpectedCommand::workspace_add_mono(&workspace_root, &staging),
        ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "def5678",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "incoming"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease must succeed by skipping the quarantined workspace");
    runner.assert_exhausted();
    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-002");

    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let quarantined_row = store
        .list_workspaces_filtered(&WorkspaceListFilter {
            workspace_id: Some("mono-agent-001"),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .next()
        .expect("mono-agent-001 row still present");
    assert_eq!(quarantined_row.state, crate::metadata::WorkspaceState::Free);
    assert_eq!(
        quarantined_row.health_status,
        Some(crate::metadata::WorkspaceHealth::Quarantined),
        "quarantine must survive a lease call that could not verify it"
    );

    let events = audit_events(&tempdir);
    let refused: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.quarantine_reclaim_refused")
        .collect();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["workspace_id"], "mono-agent-001");
    assert_eq!(refused[0]["unpushed_commits"], "abcd1234:6e6b90bc");
}

/// The other half of the leak fix: a quarantined workspace whose prior
/// holder's work has since reached a remote is real, usable capacity.
/// Leasing must reclaim it rather than mint yet another workspace —
/// before this, `health_status='quarantined'` was a one-way door and
/// every refusal permanently shrank the pool.
#[test]
fn quarantined_workspace_verified_clean_is_reclaimed_instead_of_minting() {
    let (tempdir, database_path) = with_database_path();
    let (_workspace_root, workspace_path) = seed_quarantined_workspace(&tempdir, &database_path);

    // Probe: `@` is empty and its parent carries the pushed boss branch
    // plus its PR bookmark, so the orphan probe comes back empty. The
    // quarantine is cleared and the *same* workspace is reset and leased
    // — note the absence of any `jj workspace add` in this script.
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        head_status_command(
            &workspace_path,
            &head_status_output(
                "abcd1234",
                true,
                "boss/exec_deadbeef,pr/2196",
                "boss/exec_deadbeef,pr/2196",
                "boss/exec_deadbeef",
            ),
        ),
        unpushed_probe_command(&workspace_path, ""),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "def5678",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "incoming"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease must succeed by reclaiming the quarantined workspace");
    runner.assert_exhausted();
    assert_eq!(
        result.payload["workspace"]["workspace_id"], "mono-agent-001",
        "the reclaimed workspace must be reused, not a freshly minted one"
    );

    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let rows = store.list_workspaces_filtered(&WorkspaceListFilter::default()).unwrap();
    assert_eq!(rows.len(), 1, "no new workspace may be minted");
    assert_eq!(rows[0].workspace_id, "mono-agent-001");
    assert_eq!(rows[0].state, crate::metadata::WorkspaceState::Leased);
    assert_eq!(rows[0].health_status, None, "quarantine must be lifted");

    let events = audit_events(&tempdir);
    let cleared: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.quarantine_cleared")
        .collect();
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0]["workspace_id"], "mono-agent-001");
    assert_eq!(cleared[0]["source"], "lease_reclaim");
    assert_eq!(cleared[0]["reuse_reason"], "nothing_orphaned");

    let scan: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.quarantine_reclaim_scan")
        .collect();
    assert_eq!(scan.len(), 1);
    assert_eq!(scan[0]["available"], 1);
    assert_eq!(scan[0]["scanned"], 1);
    assert_eq!(scan[0]["truncated"], false);
    assert_eq!(scan[0]["reclaimed"], "mono-agent-001");
}

/// `cube workspace force-release` is the salvage path for a
/// `free-quarantined` workspace: it has no active lease to target (the
/// guard already released it), so force-release must resolve it by
/// workspace id and clear the quarantine instead of erroring
/// "not currently leased".
#[test]
fn force_release_clears_quarantine_on_free_workspace() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    {
        let mut store = crate::store::Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[crate::metadata::WorkspaceCandidate {
                    workspace_id: "mono-agent-001".to_string(),
                    workspace_path: workspace_path.clone(),
                }],
            )
            .unwrap();
        store
            .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Quarantined)
            .unwrap();
    }

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "force-release", "mono-agent-001"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("force-release must clear the quarantine");

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
    assert_eq!(result.payload["workspace"]["health_status"], serde_json::Value::Null);

    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let row = store
        .list_workspaces_filtered(&WorkspaceListFilter {
            workspace_id: Some("mono-agent-001"),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .next()
        .expect("row present");
    assert_eq!(row.state, crate::metadata::WorkspaceState::Free);
    assert_eq!(row.health_status, None);
    assert_eq!(row.last_release_reason.as_deref(), Some("quarantine-cleared"));
}

/// The dirty guard must NOT fire on the steady-state happy path:
/// when `@` is empty and its parent is on `main`, the workspace is
/// safe to reset and lease acquisition proceeds normally.
#[test]
fn second_lease_resets_normally_when_at_is_clean_on_main() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let first = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "wip"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("first lease");
    let prior_lease_id = first.payload["workspace"]["lease_id"]
        .as_str()
        .expect("lease id")
        .to_string();
    lease_runner.assert_exhausted();

    force_lease_expiry(&database_path, &prior_lease_id, 1);

    // Clean @: empty, parent on main → safe to reset, and cheap enough
    // that the orphan probe is never issued.
    let probe_output = head_status_output("abcd1234", true, "main", "main", "main");
    let second_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        head_status_command(&workspace_path, &probe_output),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "def5678",
        ),
    ]);

    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "fresh"]),
        Some(&database_path),
        &second_runner,
    )
    .expect("second lease must succeed when the workspace is clean on main");
    second_runner.assert_exhausted();
}

/// Run one expire → reclaim → reuse cycle where the head-status probe
/// reports `head_status`, and assert the workspace was reused in place:
/// no `jj workspace add`, no quarantine, still exactly one workspace.
///
/// `orphan_probe` is the stubbed orphan-probe output, or `None` when the
/// fast path should make that second probe unnecessary.
fn assert_expired_workspace_is_reused(head_status: &str, orphan_probe: Option<&str>) {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let first = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "wip"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("first lease");
    let prior_lease_id = first.payload["workspace"]["lease_id"]
        .as_str()
        .expect("lease id")
        .to_string();
    lease_runner.assert_exhausted();

    force_lease_expiry(&database_path, &prior_lease_id, 1);

    let mut script = vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        head_status_command(&workspace_path, head_status),
    ];
    if let Some(output) = orphan_probe {
        script.push(unpushed_probe_command(&workspace_path, output));
    }
    script.extend([
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "def5678",
        ),
    ]);

    let runner = FakeRunner::new(script);
    let second = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "next"]),
        Some(&database_path),
        &runner,
    )
    .expect("second lease must reuse the expired workspace");
    // Exhaustion is the load-bearing assertion for "did not mint": a
    // `jj workspace add` would be an unscripted command and fail here.
    runner.assert_exhausted();
    assert_eq!(second.payload["workspace"]["workspace_id"], "mono-agent-001");

    let store = Store::open_at(&database_path).unwrap();
    let rows = store.list_workspaces_filtered(&WorkspaceListFilter::default()).unwrap();
    assert_eq!(rows.len(), 1, "reuse must not grow the pool");
    assert_eq!(rows[0].state, WorkspaceState::Leased);

    let events = audit_events(&tempdir);
    assert!(
        !events
            .iter()
            .any(|e| e["event"] == "workspace.reset_refused_dirty"
                || e["event"] == "workspace.dirty_reclaim_quarantined"),
        "a reusable workspace must not be refused or quarantined",
    );
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "lease.expired_reclaimed" && e["workspace_id"] == "mono-agent-001"),
        "the expired lease must still have been swept",
    );
}

/// The workspace leak, reproduced at the predicate level. jj renders a
/// local bookmark that has diverged from its remote as `main*`, and the
/// guard used to compare that rendered text against `"main"`. It never
/// matched, so a workspace sitting cleanly on main was refused,
/// quarantined and replaced by a freshly minted one — 50 of flunge's 105
/// recorded refusals were exactly this. It must now be reused in place.
#[test]
fn expired_workspace_on_diverged_main_is_reused_not_quarantined() {
    assert_expired_workspace_is_reused(&head_status_output("abcd1234", true, "main*", "main", "main"), None);
}

/// Same bug, the `main@git` rendering: the parent carries no local
/// bookmark at all, only the colocated-git remote one. 3 recorded
/// refusals.
#[test]
fn expired_workspace_on_remote_qualified_main_is_reused_not_quarantined() {
    assert_expired_workspace_is_reused(&head_status_output("abcd1234", true, "main@git", "", "main"), None);
}

/// Same bug, the no-bookmark rendering: `@` is empty and `main` has moved
/// past this base commit, so the parent carries no bookmark. 20 recorded
/// refusals. The orphan probe settles it — the base commit is still an
/// ancestor of `main@origin`, so nothing would be lost.
#[test]
fn expired_workspace_with_no_parent_bookmark_is_reused_not_quarantined() {
    assert_expired_workspace_is_reused(&head_status_output("abcd1234", true, "", "", ""), Some(""));
}

/// Same bug, the pushed-boss-branch rendering: the prior holder's work is
/// already on GitHub, so there is nothing to protect. 55 recorded
/// refusals — the single largest bucket.
#[test]
fn expired_workspace_on_pushed_boss_branch_is_reused_not_quarantined() {
    assert_expired_workspace_is_reused(
        &head_status_output(
            "abcd1234",
            true,
            "boss/exec_deadbeef,pr/2196",
            "boss/exec_deadbeef,pr/2196",
            "boss/exec_deadbeef",
        ),
        Some(""),
    );
}
