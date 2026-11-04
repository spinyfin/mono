//! Release a cube workspace lease when a run has provably ended.
//!
//! Cube's 24-hour lease TTL is the backstop for two cases this module must
//! still honour: a recovery bookmark that did not write, and an inferred
//! death whose pane or process occupancy cannot be confirmed. Every other
//! terminal path releases the lease as soon as those gates pass, so a
//! finished run does not hold a 10–20 GiB workspace until TTL.
//!
//! Resume prefers `preserved/<execution>` bookmarks over legacy recovery names
//! in the shared jj store, never from the originating working copy.
//!
//! This is the release policy shared by completion's teardown and the engine's
//! lease sweeps. Scanning the host for occupying processes, and the sweep that
//! drives it, stay with the engine host layer.

use std::future::Future;
use std::path::Path;

use anyhow::Result;
use boss_protocol::{CreateAttentionItemInput, WorkExecution};

use crate::work::WorkDb;

pub use boss_engine_storage_values::attention_kinds::LEASE_UNRELEASED_ATTENTION_KIND;

pub use boss_engine_storage_values::attention_kinds::LEASE_RELEASE_FAILED_ATTENTION_KIND;

pub use boss_execution_contracts::{
    BookmarkStatus, KeepReason, LeaseReleaseDecision, LeaseReleaseOutcome, Occupancy, PaneStatus, PreStartStage,
    TerminalCause,
};

/// Workspace occupancy probe. Production walks process cwds; tests inject.
pub trait WorkspaceOccupancy: Send + Sync {
    fn occupancy(&self, workspace_path: &Path) -> Occupancy;

    /// True when the verdict was established on the host that owns a
    /// non-local row's workspace. A probe that only sees the engine host
    /// must not vouch for a remote workspace.
    fn probes_owning_host(&self) -> bool {
        false
    }
}

/// A verdict already established on a remote row's owning host.
pub struct OwningHostOccupancy(pub Occupancy);

impl WorkspaceOccupancy for OwningHostOccupancy {
    fn occupancy(&self, _: &Path) -> Occupancy {
        self.0
    }

    fn probes_owning_host(&self) -> bool {
        true
    }
}

/// Teardown preservation on the execution's owning host.
#[async_trait::async_trait]
pub trait BookmarkGate: Send + Sync {
    async fn status(&self, execution: &WorkExecution) -> BookmarkStatus;
}

/// Pure policy. Callers still have to perform the cube release and DB clear.
pub fn decide(
    cause: TerminalCause,
    bookmark: &BookmarkStatus,
    pane: PaneStatus,
    occupancy: Occupancy,
) -> LeaseReleaseDecision {
    if matches!(bookmark, BookmarkStatus::Unconfirmed(_)) {
        return LeaseReleaseDecision::Keep(KeepReason::BookmarkUnconfirmed);
    }
    if pane == PaneStatus::Present {
        return LeaseReleaseDecision::Keep(KeepReason::PaneStillPresent);
    }
    match cause {
        TerminalCause::ProvenCompleted | TerminalCause::ProvenNeverStarted | TerminalCause::ProvenStopped => {
            LeaseReleaseDecision::Release
        }
        TerminalCause::Inferred => match occupancy {
            Occupancy::Empty => LeaseReleaseDecision::Release,
            Occupancy::Occupied => LeaseReleaseDecision::Keep(KeepReason::ProcessInWorkspace),
            Occupancy::Unknown => LeaseReleaseDecision::Keep(KeepReason::OccupancyUnconfirmed),
        },
    }
}

/// Preserve unpublished work through jj on the owning host. A missing record
/// is never evidence of an empty workspace.
pub struct DbBookmarkGate<'a> {
    pub db: &'a WorkDb,
    pub adapter: Option<&'a dyn boss_execution_contracts::HostAdapter>,
}

#[async_trait::async_trait]
impl BookmarkGate for DbBookmarkGate<'_> {
    async fn status(&self, execution: &WorkExecution) -> BookmarkStatus {
        // Read-only reviews never preserve or inherit implementation work.
        if execution.kind.is_read_only_review() {
            return BookmarkStatus::NotRequired;
        }
        let result: Result<()> = async {
            // An open legacy creation/backup failure must not skip preserve().
            // The row's source is what teardown needs; the failure item is
            // about a previous attempt's bookmark write, not this run's work.
            let record = self
                .db
                .preservation_source(&execution.id)?
                .ok_or_else(|| anyhow::anyhow!("workspace recovery store location is unavailable for preservation"))?;
            let workspace = execution
                .workspace_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("workspace path unavailable for preservation"))?;
            let recorded = self.db.preserved_tip(&execution.id)?;
            let since_unix = execution.started_epoch().or_else(|| execution.created_epoch());
            let outcome = if let Some(adapter) = self.adapter {
                adapter
                    .confirm_workspace_recovery(&record, Path::new(workspace), recorded.as_ref(), since_unix)
                    .await?
            } else if record.host_id != "local" {
                anyhow::bail!("owning host adapter is unavailable for remote preservation");
            } else {
                boss_engine_recovery::execution_bookmark::preserve(
                    &boss_engine_recovery::execution_bookmark::LocalJj,
                    &record,
                    Path::new(workspace),
                    recorded.as_ref(),
                    since_unix,
                )
                .await?
            };
            match &outcome.tip {
                Some(tip) => self.db.record_preserved_tip(&execution.id, tip)?,
                None => self.db.clear_preserved_tip(&execution.id)?,
            }
            if let Some(inspect) = outcome.inspect {
                raise_inspect_attention(self.db, execution, inspect);
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => BookmarkStatus::Confirmed,
            Err(err) => BookmarkStatus::Unconfirmed(format!("{err:#}")),
        }
    }
}

/// An open recovery creation or backup failure for this execution. Dispatch
/// resolves it once a later attempt writes its own bookmark, so an open one
/// always belongs to the attempt whose lease is being released. Unreadable
/// attention state counts as a failure: it is never evidence of a clean run.
pub fn has_open_bookmark_failure(db: &WorkDb, execution_id: &str) -> bool {
    db.list_attention_items(execution_id).map_or(true, |items| {
        items.iter().any(|item| {
            item.status == "open"
                && matches!(
                    item.kind.as_str(),
                    boss_engine_storage_values::attention_kinds::EXECUTION_BOOKMARK_RECOVERY_FAILED_ATTENTION_KIND
                        | boss_engine_storage_values::attention_kinds::EXECUTION_BOOKMARK_CREATE_FAILED_ATTENTION_KIND
                )
        })
    })
}

/// The directory is positively known not to exist (not merely unreadable).
pub fn workspace_provably_absent(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(err) if err.kind() == std::io::ErrorKind::NotFound)
}

/// Evidence supplied by the caller, separately from lease identity.
pub struct ReleaseEvidence<'a> {
    pub cause: TerminalCause,
    pub pane: PaneStatus,
    pub occupancy: &'a dyn WorkspaceOccupancy,
    pub bookmark: &'a dyn BookmarkGate,
}

/// Always-empty occupancy, for proven-death paths that do not scan.
pub struct EmptyOccupancy;

impl WorkspaceOccupancy for EmptyOccupancy {
    fn occupancy(&self, _: &Path) -> Occupancy {
        Occupancy::Empty
    }
}

/// Release `lease_id` for `execution` when [`decide`] says so. Cube is
/// claimed with a compare-and-clear before cube is called. Only the claim
/// owner may restore the lease after a genuine release failure.
pub async fn apply_release(
    db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    evidence: ReleaseEvidence<'_>,
    do_release: impl Future<Output = Result<()>>,
) -> LeaseReleaseOutcome {
    let current = match db.get_execution(&execution.id) {
        Ok(row) if row.cube_lease_id.as_deref() == Some(lease_id) => row,
        Ok(row) => {
            if row.cube_lease_id.is_none() {
                resolve_unreleased_attention(db, &row.id);
            }
            return LeaseReleaseOutcome::NoLease;
        }
        Err(err) => {
            return LeaseReleaseOutcome::CubeFailed {
                error: format!("{err:#}"),
            };
        }
    };
    let execution = &current;
    let ReleaseEvidence {
        cause,
        pane,
        occupancy,
        bookmark,
    } = evidence;
    let local = matches!(db.execution_host_id(&execution.id), Ok(None))
        || matches!(db.execution_host_id(&execution.id), Ok(Some(host)) if host == "local");
    let occupancy_status = match cause {
        TerminalCause::Inferred if !local && !occupancy.probes_owning_host() => Occupancy::Unknown,
        TerminalCause::Inferred => match execution.workspace_path.as_deref() {
            Some(path) => occupancy.occupancy(Path::new(path)),
            None => Occupancy::Unknown,
        },
        _ => Occupancy::Empty,
    };
    // Pane and occupancy verdicts come first. Confirming the bookmark
    // snapshots the working copy and may create a preserved bookmark in the
    // shared store, which must not race a worker that may still be running
    // jj in this workspace.
    let bookmark_status = if matches!(
        decide(cause, &BookmarkStatus::NotRequired, pane, occupancy_status),
        LeaseReleaseDecision::Release
    ) {
        bookmark.status(execution).await
    } else {
        BookmarkStatus::NotRequired
    };
    match decide(cause, &bookmark_status, pane, occupancy_status) {
        LeaseReleaseDecision::Keep(reason) => {
            if matches!(reason, KeepReason::BookmarkUnconfirmed) {
                let detail = match &bookmark_status {
                    BookmarkStatus::Unconfirmed(msg) => msg.clone(),
                    _ => reason.label().to_owned(),
                };
                raise_unreleased_attention(
                    db,
                    execution,
                    "Workspace lease held: recovery bookmark unconfirmed",
                    &format!(
                        "The cube workspace lease `{lease_id}` was not released because the recovery bookmark \
                         for this run could not be confirmed. The lease stays held until cube's TTL. {detail}"
                    ),
                );
            }
            tracing::info!(
                execution_id = %execution.id,
                lease_id,
                reason = reason.label(),
                "workspace lease kept",
            );
            LeaseReleaseOutcome::Kept { reason }
        }
        LeaseReleaseDecision::Release => {
            match db.claim_execution_workspace_release(&execution.id, lease_id) {
                Ok(true) => {}
                Ok(false) => return LeaseReleaseOutcome::NoLease,
                Err(err) => {
                    return LeaseReleaseOutcome::CubeFailed {
                        error: format!("{err:#}"),
                    };
                }
            }
            let result = match do_release.await {
                Ok(()) => {
                    tracing::info!(
                        execution_id = %execution.id,
                        lease_id,
                        cause = cause.label(),
                        "workspace lease released",
                    );
                    LeaseReleaseOutcome::Released
                }
                Err(err)
                    if err.chain().any(|source| {
                        source
                            .downcast_ref::<boss_host_execution::cube::CubeCliError>()
                            .is_some_and(|error| error.exit_code == Some(5))
                    }) =>
                {
                    LeaseReleaseOutcome::Released
                }
                Err(err) => {
                    let error = format!("{err:#}");
                    if let Err(restore_err) = db.restore_claimed_execution_workspace(
                        &execution.id,
                        lease_id,
                        execution.cube_workspace_id.as_deref(),
                        execution.workspace_path.as_deref(),
                    ) {
                        tracing::error!(
                            execution_id = %execution.id,
                            lease_id,
                            error = %format!("{restore_err:#}"),
                            "failed to restore lease columns after cube release failure",
                        );
                    }
                    raise_attention(
                        db,
                        execution,
                        LEASE_RELEASE_FAILED_ATTENTION_KIND,
                        "Workspace lease held: cube release failed",
                        &format!(
                            "The cube workspace lease `{lease_id}` was not released: {error}. \
                             Every release gate had passed, so the lease stays recorded and the \
                             leftover-lease sweep retries it; cube's TTL remains the backstop."
                        ),
                    );
                    tracing::error!(
                        execution_id = %execution.id,
                        lease_id,
                        error = %error,
                        "workspace lease release failed; keeping the lease",
                    );
                    LeaseReleaseOutcome::CubeFailed { error }
                }
            };
            if result == LeaseReleaseOutcome::Released {
                if let Err(err) = db.finish_execution_workspace_release(execution) {
                    tracing::error!(error = %err, "failed to clear released workspace metadata");
                }
                resolve_unreleased_attention(db, &execution.id);
            }
            result
        }
    }
}

/// The refused lease has not been positioned by this execution. Registry-only
/// release preserves all of the occupant's files and recovery pointers.
pub async fn release_refused_workspace(
    db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    adapter: &dyn boss_execution_contracts::HostAdapter,
) -> LeaseReleaseOutcome {
    apply_release(
        db,
        execution,
        lease_id,
        ReleaseEvidence {
            cause: TerminalCause::ProvenNeverStarted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark: &NeverPositioned,
        },
        adapter.force_release_lease(lease_id, Some("refused occupied workspace; preserve checkout")),
    )
    .await
}

/// Only a failure that precedes any bookmark row may skip confirmation: a row
/// means pointers exist that the release must not strand, and an open
/// creation or backup failure means the bookmark write itself may have left
/// work behind that no row records. A lease retained from a previous attempt
/// therefore keeps its bookmark gate whatever stage the caller assumes.
pub fn never_positioned(db: &WorkDb, execution_id: &str, stage: PreStartStage) -> bool {
    stage == PreStartStage::BeforeRecoveryBookmark
        && matches!(db.execution_bookmark_optional(execution_id), Ok(None))
        && !has_open_bookmark_failure(db, execution_id)
}

pub struct NeverPositioned;

#[async_trait::async_trait]
impl BookmarkGate for NeverPositioned {
    async fn status(&self, _: &WorkExecution) -> BookmarkStatus {
        BookmarkStatus::NotRequired
    }
}

/// Pre-start helper: the worker never started, so occupancy is not scanned.
pub async fn release_lease_after_proven_never_started(
    work_db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    adapter: &dyn boss_execution_contracts::HostAdapter,
    stage: PreStartStage,
) -> LeaseReleaseOutcome {
    let current = match work_db.get_execution(&execution.id) {
        Ok(row) if row.cube_lease_id.as_deref() == Some(lease_id) => row,
        Ok(_) => return LeaseReleaseOutcome::NoLease,
        Err(err) => {
            return LeaseReleaseOutcome::CubeFailed {
                error: format!("{err:#}"),
            };
        }
    };
    let execution = &current;
    // A previous attempt's lease may have expired or completed release after
    // a timeout. Registry absence requires no destructive workspace operation.
    if let Ok(workspaces) = adapter.list_workspaces().await
        && !workspaces.iter().any(|ws| ws.lease_id.as_deref() == Some(lease_id))
    {
        return match work_db.claim_execution_workspace_release(&execution.id, lease_id) {
            Ok(true) => {
                if let Err(err) = work_db.finish_execution_workspace_release(execution) {
                    tracing::error!(
                        execution_id = %execution.id,
                        lease_id,
                        error = %format!("{err:#}"),
                        "failed to clear released workspace metadata",
                    );
                }
                resolve_unreleased_attention(work_db, &execution.id);
                LeaseReleaseOutcome::Released
            }
            Ok(false) => LeaseReleaseOutcome::NoLease,
            Err(err) => LeaseReleaseOutcome::CubeFailed {
                error: format!("{err:#}"),
            },
        };
    }
    let db_gate = DbBookmarkGate {
        db: work_db,
        adapter: Some(adapter),
    };
    let bookmark: &dyn BookmarkGate = if never_positioned(work_db, &execution.id, stage) {
        &NeverPositioned
    } else {
        &db_gate
    };
    apply_release(
        work_db,
        execution,
        lease_id,
        ReleaseEvidence {
            cause: TerminalCause::ProvenNeverStarted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            bookmark,
        },
        adapter.release_workspace(lease_id),
    )
    .await
}

fn inspect_workspace_line(inspect: &boss_engine_recovery::execution_bookmark::PreserveInspect) -> Option<String> {
    match (
        inspect.workspace_change_id.as_deref(),
        inspect.workspace_commit_id.as_deref(),
        inspect.workspace_count,
    ) {
        (Some(change), Some(commit), Some(count)) => Some(format!(
            "- workspace tip: change `{change}`, commit `{commit}`, {count} unpublished commits"
        )),
        _ => None,
    }
}

fn inspect_body(inspect: &boss_engine_recovery::execution_bookmark::PreserveInspect) -> String {
    let existing = inspect.existing_commit_id.as_deref().unwrap_or("none");
    let mut body = format!(
        "{}. Existing preserved commit: `{existing}`. Nothing was deleted.",
        inspect.reason
    );
    if let Some(line) = inspect_workspace_line(inspect) {
        body.push('\n');
        body.push_str(&line);
    } else {
        body.push_str(" New workspace tip: unavailable.");
    }
    body
}

fn raise_inspect_attention(
    db: &WorkDb,
    execution: &WorkExecution,
    inspect: boss_engine_recovery::execution_bookmark::PreserveInspect,
) {
    let kind = boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND;
    let title = "Preserved work needs inspection";
    let open = db.list_attention_items(&execution.id).ok().and_then(|items| {
        items
            .into_iter()
            .find(|item| item.kind == kind && item.status == "open")
    });
    let Some(existing) = open else {
        raise_attention(db, execution, kind, title, &inspect_body(&inspect));
        return;
    };
    let mut body = existing.body_markdown;
    if let Some(commit) = inspect.existing_commit_id.as_deref()
        && !body.contains(&format!("`{commit}`"))
    {
        body.push_str(&format!("\n- existing preserved commit: `{commit}`"));
    }
    if let Some(line) = inspect_workspace_line(&inspect)
        && inspect
            .workspace_commit_id
            .as_deref()
            .is_none_or(|commit| !body.contains(&format!("commit `{commit}`")))
    {
        body.push('\n');
        body.push_str(&line);
    }
    if let Err(err) = db.upsert_execution_attention(&execution.id, kind, title, &body) {
        tracing::error!(
            execution_id = %execution.id,
            error = %format!("{err:#}"),
            "could not update preserved-work inspection attention",
        );
    }
}

pub fn raise_unreleased_attention(db: &WorkDb, execution: &WorkExecution, title: &str, body: &str) {
    raise_attention(db, execution, LEASE_UNRELEASED_ATTENTION_KIND, title, body);
}

pub fn raise_attention(db: &WorkDb, execution: &WorkExecution, kind: &str, title: &str, body: &str) {
    match db.list_attention_items(&execution.id) {
        Ok(items) if items.iter().any(|item| item.kind == kind && item.status == "open") => {
            return;
        }
        Ok(_) => {}
        Err(err) => {
            tracing::error!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "could not inspect workspace-lease attention",
            );
            return;
        }
    }
    if let Err(err) = db.create_attention_item(CreateAttentionItemInput {
        execution_id: Some(execution.id.clone()),
        work_item_id: None,
        kind: kind.to_owned(),
        status: None,
        title: title.to_owned(),
        body_markdown: body.to_owned(),
        resolved_at: None,
    }) {
        tracing::error!(
            execution_id = %execution.id,
            error = %format!("{err:#}"),
            "could not file workspace-lease attention",
        );
    }
}

/// The lease these items describe is gone, so they no longer say anything true.
pub fn resolve_unreleased_attention(db: &WorkDb, execution_id: &str) {
    for kind in [LEASE_UNRELEASED_ATTENTION_KIND, LEASE_RELEASE_FAILED_ATTENTION_KIND] {
        if let Err(err) = db.resolve_attention_kind_for_execution(execution_id, kind) {
            tracing::warn!(
                execution_id,
                kind,
                error = %format!("{err:#}"),
                "could not resolve workspace-lease attention",
            );
        }
    }
}

/// Release a remote worker's lease through the claimed release policy, for
/// every path that releases on the strength of a recorded pid or a finished
/// turn rather than a confirmed stop. The remote worker cannot be stopped by
/// this engine, so the owning host's recovery bookmark and workspace
/// occupancy (both probed through `host`) must confirm; an inconclusive
/// probe keeps the lease. `release` performs the cube call for the lease id
/// it is handed. Returns what actually happened.
pub async fn release_remote_lease<Fut: std::future::Future<Output = anyhow::Result<()>>>(
    work_db: &WorkDb,
    host: &dyn boss_execution_contracts::HostAdapter,
    execution_id: &str,
    pane: PaneStatus,
    release: impl FnOnce(String) -> Fut,
) -> LeaseReleaseOutcome {
    // Re-read: the row may have settled or lost its lease since the caller looked.
    let Ok(execution) = work_db.get_execution(execution_id) else {
        return LeaseReleaseOutcome::NoLease;
    };
    let Some(lease_id) = execution.cube_lease_id.clone() else {
        return LeaseReleaseOutcome::NoLease;
    };
    let occupancy = match execution.workspace_path.as_deref() {
        Some(path) => host
            .probe_workspace_occupancy(std::path::Path::new(path))
            .await
            .unwrap_or_else(|err| {
                tracing::debug!(
                    execution_id,
                    host_id = host.host_id(),
                    error = %format!("{err:#}"),
                    "remote lease release: workspace occupancy probe failed; keeping the lease",
                );
                Occupancy::Unknown
            }),
        None => Occupancy::Unknown,
    };
    apply_release(
        work_db,
        &execution,
        &lease_id,
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane,
            occupancy: &OwningHostOccupancy(occupancy),
            bookmark: &DbBookmarkGate {
                db: work_db,
                adapter: Some(host),
            },
        },
        release(lease_id.clone()),
    )
    .await
}
