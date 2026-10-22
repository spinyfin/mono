//! Lease-time reclaim of cached-unhealthy free workspaces (see
//! `crate::app::lease_reclaim`), and the provisioning event that explains a
//! mint when the pool still had free rows.

use super::support::{
    ExpectedCommand, FakeRunner, audit_events, head_status_command, head_status_output, jj_status_conflicted,
    jj_status_dirty, seed_mono_repo, unpushed_probe_command, with_database_path,
};
use clap::Parser;
use tempfile::TempDir;

use crate::cli::Cli;
use crate::metadata::{WorkspaceCandidate, WorkspaceHealth, WorkspaceRecord, WorkspaceState};
use crate::store::{Store, WorkspaceListFilter};

use crate::app::dispatch::run_with_dependencies;
use crate::app::lease_reclaim::{order_reclaim_candidates, unhealthy_breakdown};
use crate::app::salvage::{SALVAGE_LOG_TEMPLATE, list_salvage_records, salvage_log_limit_arg, salvage_revset};

const PRESERVED: &str = "unpushed_work_preserved";

/// Register `ids` as free workspaces of `mono` and mark each with `health`
/// and a release reason, the shape a release-time preservation or an expired
/// lease leaves behind.
fn seed_unhealthy_pool(
    tempdir: &TempDir,
    database_path: &std::path::Path,
    ids: &[&str],
    health: WorkspaceHealth,
    release_reason: &str,
) -> (std::path::PathBuf, Vec<std::path::PathBuf>) {
    let workspace_root = tempdir.path().join("workspaces");
    seed_mono_repo(&workspace_root, database_path);

    let mut store = Store::open_at(database_path).unwrap();
    let mut paths = Vec::new();
    let mut candidates = Vec::new();
    for id in ids {
        let path = workspace_root.join(id);
        std::fs::create_dir_all(path.join(".jj")).expect("workspace dir");
        candidates.push(WorkspaceCandidate {
            workspace_id: (*id).to_string(),
            workspace_path: path.clone(),
        });
        paths.push(path);
    }
    store.sync_workspaces("mono", &candidates).unwrap();
    for id in ids {
        store.update_workspace_health("mono", id, health).unwrap();
        let conn = rusqlite::Connection::open(database_path).expect("sqlite open");
        conn.execute(
            "UPDATE workspaces SET last_release_reason = ?1 WHERE workspace_id = ?2",
            rusqlite::params![release_reason, id],
        )
        .expect("release reason");
    }
    (workspace_root, paths)
}

fn scan_status_command(ws_path: &std::path::Path, output: &str) -> ExpectedCommand {
    ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["status", "--no-pager"], output)
}

/// The salvage sequence: one `jj log` over the salvage revset, then one
/// `jj diff` per captured commit.
fn salvage_commands_for(ws_path: &std::path::Path, log_output: &str, diffs: &[(&str, &str)]) -> Vec<ExpectedCommand> {
    let revset = salvage_revset("main");
    let limit = salvage_log_limit_arg();
    let mut v = vec![ExpectedCommand::ok(
        ws_path.to_path_buf(),
        "jj",
        &[
            "log",
            "--no-graph",
            "-n",
            &limit,
            "-r",
            &revset,
            "-T",
            SALVAGE_LOG_TEMPLATE,
        ],
        log_output,
    )];
    for (commit_id, diff) in diffs {
        v.push(ExpectedCommand::ok(
            ws_path.to_path_buf(),
            "jj",
            &["diff", "--no-pager", "--git", "-r", commit_id],
            diff,
        ));
    }
    v
}

/// The ordinary post-claim fetch-and-reset plus the head read every lease
/// finishes with.
fn claim_reset_commands_for(ws_path: &std::path::Path, head: &str) -> Vec<ExpectedCommand> {
    vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            ws_path.to_path_buf(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            ws_path.to_path_buf(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            ws_path.to_path_buf(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            head,
        ),
    ]
}

/// What minting a fresh `mono-agent-002` looks like: the shared-store
/// `workspace add` followed by the same fetch-and-reset as any claim.
fn mint_commands_for(workspace_root: &std::path::Path, new_id: &str) -> Vec<ExpectedCommand> {
    let new_path = workspace_root.join(new_id);
    let staging = workspace_root.join(format!(".incoming-{new_id}"));
    let mut v = vec![ExpectedCommand::workspace_add_mono(workspace_root, &staging)];
    v.extend(claim_reset_commands_for(&new_path, "def5678"));
    v
}

fn lease_mono(database_path: &std::path::Path, runner: &FakeRunner, extra: &[&str]) -> crate::app::errors::RunResult {
    let mut argv = vec!["cube", "workspace", "lease", "mono", "--task", "incoming"];
    argv.extend_from_slice(extra);
    run_with_dependencies(Cli::parse_from(argv), Some(database_path), runner).expect("lease must succeed")
}

fn row(database_path: &std::path::Path, id: &str) -> WorkspaceRecord {
    Store::open_at(database_path)
        .unwrap()
        .list_workspaces_filtered(&WorkspaceListFilter {
            workspace_id: Some(id),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("{id} row present"))
}

fn pool_size(database_path: &std::path::Path) -> usize {
    Store::open_at(database_path)
        .unwrap()
        .list_workspaces_filtered(&WorkspaceListFilter::default())
        .unwrap()
        .len()
}

/// The 2026-10-20/21 shape: the only free workspace was released with its
/// unpushed `@` preserved, so every lease used to step around it and mint.
/// Now the lease salvages that work and reuses the workspace — the pool does
/// not grow.
#[test]
fn lease_salvages_and_reuses_a_dirty_free_workspace_instead_of_minting() {
    let (tempdir, database_path) = with_database_path();
    let (_root, paths) = seed_unhealthy_pool(
        &tempdir,
        &database_path,
        &["mono-agent-001"],
        WorkspaceHealth::Dirty,
        PRESERVED,
    );
    let ws_path = &paths[0];

    let mut script = vec![
        // The scan's revalidation window re-probes the cached-dirty row and
        // confirms it: still dirty, still skipped.
        scan_status_command(ws_path, jj_status_dirty()),
        // Reclaim: fetch, then the reuse guard. `@` is non-empty on an
        // unpushed exec bookmark, so the guard refuses and salvage runs.
        ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "fetch"], ""),
        head_status_command(
            ws_path,
            &head_status_output("abcd1234", false, "boss/exec_local", "boss/exec_local", ""),
        ),
        unpushed_probe_command(ws_path, "abcd1234\t6e6b90bc\n"),
    ];
    script.extend(salvage_commands_for(
        ws_path,
        "abcd1234\t6e6b90bc\thalf-finished refactor\n",
        &[("6e6b90bc", "diff --git a/x b/x\n+one\n")],
    ));
    // Then the claim resets it like any other candidate.
    script.extend(claim_reset_commands_for(ws_path, "def5678"));
    let runner = FakeRunner::new(script);

    let result = lease_mono(&database_path, &runner, &[]);
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
    assert_eq!(result.payload["unhealthy_reclaim"]["reclaimed"], "mono-agent-001");
    assert_eq!(result.payload["unhealthy_reclaim"]["available"], 1);
    assert_eq!(pool_size(&database_path), 1, "no workspace may be minted");
    let leased = row(&database_path, "mono-agent-001");
    assert_eq!(leased.state, WorkspaceState::Leased);
    assert_eq!(leased.health_status, None);

    // The preserved work is retrievable without jj archaeology.
    let records = list_salvage_records(Some(&database_path), None, None).expect("salvage records");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].manifest.workspace_id, "mono-agent-001");
    assert_eq!(records[0].manifest.last_release_reason.as_deref(), Some(PRESERVED));
    let patch = std::fs::read_to_string(records[0].path.join(&records[0].manifest.commits[0].patch)).expect("patch");
    assert!(patch.contains("+one"), "the patch holds the actual work: {patch}");

    let events = audit_events(&tempdir);
    let salvaged: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.lease_reclaim_salvaged")
        .collect();
    assert_eq!(salvaged.len(), 1);
    assert_eq!(salvaged[0]["unpushed_commits"], "abcd1234:6e6b90bc");
    let reclaimed: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.lease_reclaimed")
        .collect();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0]["workspace_id"], "mono-agent-001");
    assert_eq!(reclaimed[0]["prior_health"], "dirty");
    assert_eq!(reclaimed[0]["prior_release_reason"], PRESERVED);
    assert!(reclaimed[0]["salvaged_to"].is_string());
    let scan: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.lease_reclaim_scan")
        .collect();
    assert_eq!(scan.len(), 1);
    assert_eq!(scan[0]["reclaimed"], "mono-agent-001");
    assert_eq!(scan[0]["truncated"], false);
    assert!(
        !events
            .iter()
            .any(|e| e["event"] == "workspace.provisioned_no_eligible_free"),
        "a reused workspace is not a provisioning"
    );
    assert!(
        !events.iter().any(|e| e["event"] == "workspace.created"),
        "no workspace may be created"
    );
}

/// A cached-dirty row whose `@` the guard finds reusable (a release whose
/// reset failed mid-way, say) needs no salvage: it is reclaimed on the
/// probe's verdict alone.
#[test]
fn lease_reclaims_a_dirty_free_workspace_whose_head_is_reusable_without_salvage() {
    let (tempdir, database_path) = with_database_path();
    let (_root, paths) = seed_unhealthy_pool(
        &tempdir,
        &database_path,
        &["mono-agent-001"],
        WorkspaceHealth::Dirty,
        "expired",
    );
    let ws_path = &paths[0];

    let mut script = vec![
        scan_status_command(ws_path, jj_status_dirty()),
        ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "fetch"], ""),
        // Empty `@` on main: settled by the fast path, no orphan probe.
        head_status_command(ws_path, &head_status_output("abcd1234", true, "main", "main", "main")),
    ];
    script.extend(claim_reset_commands_for(ws_path, "def5678"));
    let runner = FakeRunner::new(script);

    let result = lease_mono(&database_path, &runner, &[]);
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
    assert_eq!(pool_size(&database_path), 1);
    assert!(
        list_salvage_records(Some(&database_path), None, None)
            .unwrap()
            .is_empty()
    );
    let events = audit_events(&tempdir);
    let reclaimed: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.lease_reclaimed")
        .collect();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0]["prior_release_reason"], "expired");
    assert!(reclaimed[0]["salvaged_to"].is_null());
    assert!(reclaimed[0]["reuse_reason"].is_string());
}

/// When the work cannot be captured, the workspace is left exactly as it was
/// and the lease mints — and the provisioning event says which free
/// workspace it passed over, why, and what the pool's unhealthy set holds.
#[test]
fn lease_mints_and_explains_itself_when_the_salvage_fails() {
    let (tempdir, database_path) = with_database_path();
    let (root, paths) = seed_unhealthy_pool(
        &tempdir,
        &database_path,
        &["mono-agent-001"],
        WorkspaceHealth::Dirty,
        PRESERVED,
    );
    let ws_path = &paths[0];

    let mut script = vec![
        scan_status_command(ws_path, jj_status_dirty()),
        ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "fetch"], ""),
        head_status_command(
            ws_path,
            &head_status_output("abcd1234", false, "boss/exec_local", "boss/exec_local", ""),
        ),
        unpushed_probe_command(ws_path, "abcd1234\t6e6b90bc\n"),
    ];
    // An empty salvage log after a refusing probe is a failed capture.
    script.extend(salvage_commands_for(ws_path, "", &[]));
    script.extend(mint_commands_for(&root, "mono-agent-002"));
    let runner = FakeRunner::new(script);

    let result = lease_mono(&database_path, &runner, &[]);
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-002");
    assert_eq!(pool_size(&database_path), 2);
    let retained = row(&database_path, "mono-agent-001");
    assert_eq!(retained.state, WorkspaceState::Free);
    assert_eq!(
        retained.health_status,
        Some(WorkspaceHealth::Dirty),
        "a failed salvage must not reclaim"
    );
    assert!(
        list_salvage_records(Some(&database_path), None, None)
            .unwrap()
            .is_empty()
    );

    let events = audit_events(&tempdir);
    let skipped: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.lease_reclaim_skipped")
        .collect();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["workspace_id"], "mono-agent-001");
    assert_eq!(skipped[0]["reason"], "salvage_failed");

    let provisioned: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.provisioned_no_eligible_free")
        .collect();
    assert_eq!(provisioned.len(), 1, "exactly one provisioning, explained once");
    let p = provisioned[0];
    assert_eq!(p["repo"], "mono");
    assert_eq!(p["effective_free"], 0);
    assert_eq!(p["cached_unhealthy"], 1);
    assert_eq!(p["unhealthy_breakdown"]["dirty/unpushed_work_preserved"], 1);
    assert_eq!(p["rejected"][0]["workspace_id"], "mono-agent-001");
    assert_eq!(p["rejected"][0]["reason"], "dirty_working_copy");
    assert_eq!(p["reclaim"]["available"], 1);
    assert_eq!(p["reclaim"]["scanned"], 1);
    assert_eq!(p["reclaim"]["rejected"][0]["workspace_id"], "mono-agent-001");
    assert_eq!(p["reclaim"]["rejected"][0]["reason"], "salvage_failed");
    assert!(p["reclaim"]["reclaimed"].is_null());
}

/// `--exclude` keeps a workspace out of the reclaim too: the engine excludes
/// a workspace it just refused, and re-offering it through a different door
/// would loop on the same candidate.
#[test]
fn lease_reclaim_honours_exclude() {
    let (tempdir, database_path) = with_database_path();
    let (root, _paths) = seed_unhealthy_pool(
        &tempdir,
        &database_path,
        &["mono-agent-001"],
        WorkspaceHealth::Dirty,
        PRESERVED,
    );

    // No probe of any kind against mono-agent-001: straight to the mint.
    let runner = FakeRunner::new(mint_commands_for(&root, "mono-agent-002"));
    let result = lease_mono(&database_path, &runner, &["--exclude", "mono-agent-001"]);
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-002");
    assert_eq!(result.payload["unhealthy_reclaim"]["available"], 0);
    let events = audit_events(&tempdir);
    let provisioned: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.provisioned_no_eligible_free")
        .collect();
    assert_eq!(provisioned.len(), 1);
    assert_eq!(provisioned[0]["reclaim"]["available"], 0);
}

/// Bookmarks are store-wide, so one conflicted `pr/<n>` marks every free
/// workspace the scan probes as `conflicted`. Repairing it in the chosen
/// workspace fixes all of them; the cache must say so, or the siblings stay
/// withheld and the next lease mints around them.
#[test]
fn repairing_a_conflicted_bookmark_clears_the_cached_conflict_of_sibling_workspaces() {
    let (tempdir, database_path) = with_database_path();
    let (_root, paths) = seed_unhealthy_pool(
        &tempdir,
        &database_path,
        &["mono-agent-001", "mono-agent-002"],
        WorkspaceHealth::Conflicted,
        "",
    );
    let (ws1, ws2) = (&paths[0], &paths[1]);

    let mut script = vec![
        // The revalidation window re-probes both; both report the same
        // store-wide conflict. The first becomes the repairable candidate.
        scan_status_command(ws1, &jj_status_conflicted("pr/3226")),
        scan_status_command(ws2, &jj_status_conflicted("pr/3226")),
        ExpectedCommand::ok(ws1.clone(), "jj", &["bookmark", "forget", "pr/3226"], ""),
    ];
    script.extend(claim_reset_commands_for(ws1, "def5678"));
    let runner = FakeRunner::new(script);

    let result = lease_mono(&database_path, &runner, &[]);
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
    assert_eq!(pool_size(&database_path), 2, "no workspace may be minted");
    let sibling = row(&database_path, "mono-agent-002");
    assert_eq!(sibling.state, WorkspaceState::Free);
    assert_eq!(
        sibling.health_status, None,
        "the sibling's cached conflict must be cleared once the bookmark is repaired"
    );
    assert_eq!(sibling.unhealthy_since_epoch_s, None);

    let events = audit_events(&tempdir);
    let cleared: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.conflicted_cache_cleared")
        .collect();
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0]["repaired_in"], "mono-agent-001");
    assert_eq!(cleared[0]["cleared"], 1);
    assert_eq!(cleared[0]["bookmarks"][0], "pr/3226");
}

fn record(id: &str, health: Option<WorkspaceHealth>, since: Option<i64>) -> WorkspaceRecord {
    WorkspaceRecord {
        repo: "mono".to_string(),
        workspace_id: id.to_string(),
        workspace_path: std::path::PathBuf::from(format!("/ws/{id}")),
        state: WorkspaceState::Free,
        lease_id: None,
        holder: None,
        task: None,
        leased_at_epoch_s: None,
        lease_expires_at_epoch_s: None,
        head_commit: None,
        last_release_reason: Some("expired".to_string()),
        health_status: health,
        unhealthy_since_epoch_s: since,
        last_holder: None,
        last_task: None,
        last_activity_at_epoch_s: None,
    }
}

#[test]
fn reclaim_candidates_are_ordered_conflicted_first_then_oldest() {
    let mut records = vec![
        record("mono-agent-004", Some(WorkspaceHealth::Dirty), Some(100)),
        record("mono-agent-003", Some(WorkspaceHealth::Dirty), Some(50)),
        record("mono-agent-002", Some(WorkspaceHealth::Conflicted), Some(900)),
        record("mono-agent-001", Some(WorkspaceHealth::Dirty), None),
    ];
    order_reclaim_candidates(&mut records);
    let ids: Vec<&str> = records.iter().map(|r| r.workspace_id.as_str()).collect();
    assert_eq!(
        ids,
        ["mono-agent-002", "mono-agent-003", "mono-agent-004", "mono-agent-001"],
        "conflicted (cheapest) first, then dirty by age, unknown age last"
    );
}

#[test]
fn unhealthy_breakdown_counts_health_and_release_reason_pairs() {
    let mut preserved = record("mono-agent-001", Some(WorkspaceHealth::Dirty), Some(1));
    preserved.last_release_reason = Some(PRESERVED.to_string());
    let mut unexplained = record("mono-agent-003", Some(WorkspaceHealth::Conflicted), Some(1));
    unexplained.last_release_reason = None;
    let records = vec![
        preserved,
        record("mono-agent-002", Some(WorkspaceHealth::Dirty), Some(1)),
        unexplained,
        record("mono-agent-004", Some(WorkspaceHealth::Dirty), Some(1)),
    ];
    let breakdown = unhealthy_breakdown(&records);
    assert_eq!(breakdown["dirty/expired"], 2);
    assert_eq!(breakdown["dirty/unpushed_work_preserved"], 1);
    assert_eq!(breakdown["conflicted/-"], 1);
}
