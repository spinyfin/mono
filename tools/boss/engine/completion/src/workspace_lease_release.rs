//! Release a cube workspace lease when a run has provably ended.
//!
//! Cube's 24-hour lease TTL is the backstop for an inferred death whose pane
//! or process occupancy cannot be confirmed. Every other terminal path
//! releases the lease as soon as those gates pass, so a finished run does not
//! hold a 10–20 GiB workspace until TTL.
//!
//! Teardown preserves the workspace's unpublished edits as
//! `preserved/<execution>` in the shared jj store immediately before the
//! release. That step is best effort: a failure raises an inspection warning
//! naming the lease and never keeps the lease held, so a recovery problem
//! cannot strand a workspace. Resume reads those bookmarks, never the
//! originating working copy.
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

use boss_engine_recovery::execution_bookmark::{BoundaryOwnership, WorkspaceTipSource};
use boss_execution_contracts::{CubeClient, HostAdapter};

pub use boss_execution_contracts::{
    KeepReason, LeaseReleaseDecision, LeaseReleaseOutcome, Occupancy, PaneStatus, TerminalCause,
};

/// Workspace occupancy probe. Production walks process cwds; tests inject.
pub trait WorkspaceOccupancy: Send + Sync {
    fn occupancy(&self, workspace_path: &Path) -> Occupancy;

    /// Clean detached processes only after pane death is proven. A cwd alone
    /// never authorizes signalling a process.
    fn cleanup<'a>(
        &'a self,
        _db: &'a WorkDb,
        _execution: &'a WorkExecution,
    ) -> Option<std::pin::Pin<Box<dyn Future<Output = Result<Occupancy>> + Send + 'a>>> {
        None
    }

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

/// Teardown preservation on the execution's owning host. Its outcome never
/// decides whether the lease is released; a failure raises a
/// `preserved_work_needs_inspection` attention item.
#[async_trait::async_trait]
pub trait Preservation: Send + Sync {
    async fn preserve(&self, execution: &WorkExecution, cause: TerminalCause) -> Result<()>;
}

/// Pure policy. Callers still have to perform the cube release and DB clear.
pub fn decide(cause: TerminalCause, pane: PaneStatus, occupancy: Occupancy) -> LeaseReleaseDecision {
    if pane == PaneStatus::Present {
        return LeaseReleaseDecision::Keep(KeepReason::PaneStillPresent);
    }
    match cause {
        TerminalCause::ProvenNeverStarted => LeaseReleaseDecision::Release,
        TerminalCause::Inferred | TerminalCause::ProvenCompleted | TerminalCause::ProvenStopped => match occupancy {
            Occupancy::Empty => LeaseReleaseDecision::Release,
            Occupancy::Occupied => LeaseReleaseDecision::Keep(KeepReason::ProcessInWorkspace),
            Occupancy::Unknown => LeaseReleaseDecision::Keep(KeepReason::OccupancyUnconfirmed),
        },
    }
}

/// Preserve unpublished work through jj on the owning host. A missing record
/// is never evidence of an empty workspace, except for an execution that never
/// started and was never positioned: cube placed everything in that workspace.
pub struct DbPreservation<'a> {
    pub db: &'a WorkDb,
    pub adapter: Option<&'a dyn HostAdapter>,
}

/// A [`DbPreservation`] that asks the owning cube's registry whether this
/// execution still holds its workspace before inferred-death teardown reads
/// the working copy; proven releases skip the registry check.
pub struct CubePreservation<'a> {
    inner: DbPreservation<'a>,
    cube: &'a dyn CubeClient,
}

impl<'a> DbPreservation<'a> {
    pub fn new(db: &'a WorkDb, adapter: Option<&'a dyn HostAdapter>) -> Self {
        Self { db, adapter }
    }

    pub fn with_cube(self, cube: &'a dyn CubeClient) -> CubePreservation<'a> {
        CubePreservation { inner: self, cube }
    }
}

#[async_trait::async_trait]
impl Preservation for DbPreservation<'_> {
    async fn preserve(&self, execution: &WorkExecution, cause: TerminalCause) -> Result<()> {
        self.preserve_workspace(execution, cause, None).await
    }
}

#[async_trait::async_trait]
impl Preservation for CubePreservation<'_> {
    async fn preserve(&self, execution: &WorkExecution, cause: TerminalCause) -> Result<()> {
        self.inner.preserve_workspace(execution, cause, Some(self.cube)).await
    }
}

/// Inferred death requires an owning-host registry check. A failed check
/// raises an inspection attention item and does not hold the lease. The workspace is read
/// live only with positive ownership evidence: it still holds this execution's
/// lease, or it is free again and cube's claim counter equals the one recorded
/// for this lease, so no other lease was ever claimed on it. Anything else
/// resolves only at this lease's boundary and never adopts later changes; a
/// free workspace whose counter moved is never snapshotted and unrelated work
/// is never reported. Rewrites of this execution's boundary change are still
/// reported for inspection.
async fn workspace_tip_source(
    execution: &WorkExecution,
    boundary: Option<i64>,
    lease_generation: Option<i64>,
    cube: Option<&dyn CubeClient>,
    adapter: Option<&dyn HostAdapter>,
) -> Result<(WorkspaceTipSource, String)> {
    let workspaces = match cube {
        Some(cube) => cube.list_workspaces().await?,
        None => match adapter {
            Some(adapter) => adapter.list_workspaces().await?,
            None => anyhow::bail!("owning cube unavailable for inferred workspace release"),
        },
    };
    let owned = execution
        .cube_workspace_id
        .as_deref()
        .and_then(|id| workspaces.iter().find(|ws| ws.workspace_id == id))
        .is_some_and(|ws| {
            ws.state == "leased" && execution.cube_lease_id.is_some() && ws.lease_id == execution.cube_lease_id
        });
    if owned {
        return Ok((WorkspaceTipSource::Live, String::new()));
    }
    let registered = execution
        .cube_workspace_id
        .as_deref()
        .and_then(|id| workspaces.iter().find(|ws| ws.workspace_id == id));
    // A workspace cube no longer lists cannot be held by anyone either.
    let free = registered.is_none_or(|ws| ws.state == "free");
    let generations = (lease_generation, registered.and_then(|ws| ws.lease_generation));
    let registered_free = registered.is_some_and(|ws| ws.state == "free");
    let sole_holder = registered_free && matches!(generations, (Some(held), Some(now)) if held == now);
    // Cube's claim counter moved: another lease was claimed after this one, so
    // the free workspace carries that holder's leftovers, not this execution's.
    let claimed_since = registered_free && matches!(generations, (Some(held), Some(now)) if held != now);
    if sole_holder {
        return Ok((WorkspaceTipSource::Live, String::new()));
    }
    let ownership = format!(
        "workspace {:?}, original lease {:?}, recorded lease generation {:?}, current registry leases: {:?}",
        execution.cube_workspace_id,
        execution.cube_lease_id,
        lease_generation,
        workspaces
            .iter()
            .filter(|ws| execution.cube_workspace_id.as_deref() == Some(ws.workspace_id.as_str()))
            .map(|ws| (&ws.state, &ws.lease_id, ws.lease_generation))
            .collect::<Vec<_>>()
    );
    Ok((
        match boundary {
            Some(unix_millis) => WorkspaceTipSource::LeaseBoundary {
                unix_millis,
                ownership: if free && !claimed_since {
                    BoundaryOwnership::FreeUnproven
                } else {
                    BoundaryOwnership::Reassigned
                },
            },
            None => WorkspaceTipSource::UnknownBoundary,
        },
        ownership,
    ))
}

impl DbPreservation<'_> {
    async fn preserve_workspace(
        &self,
        execution: &WorkExecution,
        cause: TerminalCause,
        cube: Option<&dyn CubeClient>,
    ) -> Result<()> {
        // Read-only reviews never preserve or inherit implementation work.
        if execution.kind.never_writes_pr_branch() {
            return Ok(());
        }
        // An execution that never started and was never positioned holds only
        // what cube placed in the workspace: there is no worker work to lose.
        if never_positioned(self.db, execution) {
            return Ok(());
        }
        // The row's source is what teardown needs.
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
        let (tip_source, ownership) = if matches!(cause, TerminalCause::Inferred) {
            workspace_tip_source(
                execution,
                self.db.workspace_lease_boundary(&execution.id)?,
                self.db.workspace_lease_generation(&execution.id)?,
                cube,
                self.adapter,
            )
            .await?
        } else {
            (WorkspaceTipSource::Live, String::new())
        };
        let mut outcome = if let Some(adapter) = self.adapter {
            adapter
                .confirm_workspace_recovery_from(
                    &record,
                    Path::new(workspace),
                    recorded.as_ref(),
                    since_unix,
                    tip_source,
                )
                .await?
        } else if record.host_id != "local" {
            anyhow::bail!("owning host adapter is unavailable for remote preservation");
        } else {
            boss_engine_recovery::execution_bookmark::preserve_from(
                &boss_engine_recovery::execution_bookmark::LocalJj,
                &record,
                Path::new(workspace),
                recorded.as_ref(),
                since_unix,
                tip_source,
            )
            .await?
        };
        if let Some(inspect) = outcome.inspect.as_mut() {
            inspect.reason.push_str(&format!("; {ownership}"));
        }
        match &outcome.tip {
            Some(tip) => self.db.record_preserved_tip(&execution.id, tip)?,
            None => self.db.clear_preserved_tip(&execution.id)?,
        }
        if let Some(inspect) = outcome.inspect {
            raise_inspect_attention(self.db, execution, inspect);
        }
        Ok(())
    }
}

/// The execution never ran a worker and no recovery record was written for
/// it, so its workspace holds nothing that cube did not place there.
pub fn never_positioned(db: &WorkDb, execution: &WorkExecution) -> bool {
    execution.started_at.is_none() && matches!(db.preservation_source(&execution.id), Ok(None))
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
    pub preservation: &'a dyn Preservation,
}

/// Always-empty occupancy, for proven-death paths that do not scan.
pub struct EmptyOccupancy;

impl WorkspaceOccupancy for EmptyOccupancy {
    fn occupancy(&self, _: &Path) -> Occupancy {
        Occupancy::Empty
    }
}

/// Release `lease_id` for `execution` when [`decide`] says so, after one
/// best-effort teardown preservation. Cube is claimed with a compare-and-clear
/// before cube is called. Only the claim owner may restore the lease after a
/// genuine release failure.
pub async fn apply_release(
    db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    evidence: ReleaseEvidence<'_>,
    do_release: impl Future<Output = Result<()>>,
) -> LeaseReleaseOutcome {
    apply_release_choosing(db, execution, lease_id, evidence, |_| do_release).await
}

/// [`apply_release`] for callers that can release with cube's
/// `--force-reset`, which skips cube's own unpushed-work guard and salvage.
/// `release` receives whether teardown preservation succeeded and must use the
/// forced reset only when it did; after a failed preservation it must use the
/// plain release so cube keeps or salvages a dirty tree.
pub async fn apply_release_choosing<Fut: Future<Output = Result<()>>>(
    db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    evidence: ReleaseEvidence<'_>,
    release: impl FnOnce(bool) -> Fut,
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
        preservation,
    } = evidence;
    let local = matches!(db.execution_host_id(&execution.id), Ok(None))
        || matches!(db.execution_host_id(&execution.id), Ok(Some(host)) if host == "local");
    let occupancy_status = if pane == PaneStatus::Present || cause == TerminalCause::ProvenNeverStarted {
        Occupancy::Empty // Neither case may start cleanup; decide still gates on the pane.
    } else if !local && !occupancy.probes_owning_host() {
        Occupancy::Unknown
    } else if local && let Some(cleanup) = occupancy.cleanup(db, execution) {
        cleanup.await.unwrap_or_else(|err| {
            raise_attention(db, execution, LEASE_UNRELEASED_ATTENTION_KIND,
                "Workspace recovery needs process cleanup", &format!("Lease {lease_id}: {err:#}. Inspect the named processes on the owning host, then retry recovery. The workspace remains protected."));
            tracing::error!(execution_id = %execution.id, lease_id, error = %err, "workspace process cleanup failed");
            Occupancy::Unknown
        })
    } else {
        execution
            .workspace_path
            .as_deref()
            .map_or(Occupancy::Unknown, |path| occupancy.occupancy(Path::new(path)))
    };
    match decide(cause, pane, occupancy_status) {
        LeaseReleaseDecision::Keep(reason) => {
            if pane == PaneStatus::Gone {
                raise_attention(
                    db,
                    execution,
                    LEASE_UNRELEASED_ATTENTION_KIND,
                    "Workspace recovery could not release its lease",
                    &format!(
                        "Lease {lease_id} remains held: {}. Inspect the owning host's worker process cleanup log for PIDs and retry recovery after resolving the occupants or host connectivity.",
                        reason.label()
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
            // Pane and occupancy verdicts come first. Preserving snapshots the
            // working copy and may create a preserved bookmark in the shared
            // store, which must not race a worker that may still be running
            // jj in this workspace. A failure is reported and the lease is
            // returned regardless: recovery trouble never strands a workspace.
            // The failure is reported once the release outcome is known.
            let preserve_error = preservation.preserve(execution, cause).await.err();
            match db.claim_execution_workspace_release(&execution.id, lease_id) {
                Ok(true) => {}
                Ok(false) => {
                    let outcome = LeaseReleaseOutcome::NoLease;
                    report_preservation_result(db, execution, lease_id, preserve_error.as_ref(), &outcome);
                    return outcome;
                }
                Err(err) => {
                    let outcome = LeaseReleaseOutcome::CubeFailed {
                        error: format!("{err:#}"),
                    };
                    report_preservation_result(db, execution, lease_id, preserve_error.as_ref(), &outcome);
                    return outcome;
                }
            }
            let result = match release(preserve_error.is_none()).await {
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
            report_preservation_result(db, execution, lease_id, preserve_error.as_ref(), &result);
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
            preservation: &NoPreservation,
        },
        adapter.force_release_lease(lease_id, Some("refused occupied workspace; preserve checkout")),
    )
    .await
}

/// Preserves nothing: for a workspace this execution never positioned.
pub struct NoPreservation;

#[async_trait::async_trait]
impl Preservation for NoPreservation {
    async fn preserve(&self, _: &WorkExecution, _: TerminalCause) -> Result<()> {
        Ok(())
    }
}

/// Pre-start helper: the worker never started, so occupancy is not scanned.
pub async fn release_lease_after_proven_never_started(
    work_db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    adapter: &dyn boss_execution_contracts::HostAdapter,
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
    apply_release(
        work_db,
        execution,
        lease_id,
        ReleaseEvidence {
            cause: TerminalCause::ProvenNeverStarted,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            preservation: &DbPreservation::new(work_db, Some(adapter)),
        },
        adapter.release_workspace(lease_id),
    )
    .await
}

fn raise_inspect_attention(
    db: &WorkDb,
    execution: &WorkExecution,
    inspect: boss_engine_recovery::execution_bookmark::PreserveInspect,
) {
    if let Err(err) = db.raise_preserved_work_inspect_attention(&execution.id, &inspect) {
        tracing::error!(
            execution_id = %execution.id,
            error = %format!("{err:#}"),
            "could not update preserved-work inspection attention",
        );
    }
}

fn report_preservation_result(
    db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    error: Option<&anyhow::Error>,
    outcome: &LeaseReleaseOutcome,
) {
    if let Some(err) = error {
        report_preservation_failure(db, execution, lease_id, err, outcome);
    } else if let Err(err) = db.refresh_teardown_preservation_attention(&execution.id, None, &format!("{outcome:?}")) {
        tracing::error!(execution_id = %execution.id, %err, "could not refresh teardown preservation attention");
    }
}

/// Teardown could not preserve the workspace's edits. This is the one durable
/// pointer to the unbookmarked work, worded from what the release actually did.
fn report_preservation_failure(
    db: &WorkDb,
    execution: &WorkExecution,
    lease_id: &str,
    err: &anyhow::Error,
    outcome: &LeaseReleaseOutcome,
) {
    tracing::warn!(
        execution_id = %execution.id,
        lease_id,
        error = %format!("{err:#}"),
        "teardown could not preserve the workspace",
    );
    let what_happened = match outcome {
        LeaseReleaseOutcome::Released => {
            format!("lease `{lease_id}` was released without a forced reset; the workspace tree was not reset")
        }
        LeaseReleaseOutcome::CubeFailed { .. } => format!(
            "lease `{lease_id}` is still held because release could not complete, and the leftover-lease sweep retries it"
        ),
        _ => format!("lease `{lease_id}` is no longer held for this run"),
    };
    let reason = format!(
        "Teardown could not preserve this run's workspace edits ({err:#}); {what_happened}. The worker's last \
             working-copy commit is not bookmarked as `preserved/{id}`; locate it in the shared jj store (for \
             example with `jj op log`)",
        id = execution.id
    );
    if let Err(err) = db.refresh_teardown_preservation_attention(&execution.id, Some(&reason), &what_happened) {
        tracing::error!(execution_id = %execution.id, %err, "could not report teardown preservation failure");
    }
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
/// turn rather than a confirmed stop. The owning host first stops the
/// execution's owned processes (`cleanup_execution_processes`), then its
/// workspace occupancy (probed through `host`) must confirm; an inconclusive
/// probe keeps the lease. Teardown
/// preservation runs on that host first and never holds the lease. `release`
/// performs the cube call for the lease id it is handed. Returns what
/// actually happened.
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
        Some(path) if pane == PaneStatus::Gone => host
            .cleanup_execution_processes(execution_id, std::path::Path::new(path))
            .await
            .unwrap_or_else(|err| {
                raise_attention(work_db, &execution, LEASE_UNRELEASED_ATTENTION_KIND,
                    "Remote workspace recovery needs process cleanup", &format!("Lease {lease_id} on {}: {err:#}. Inspect the named processes or restore host connectivity, then retry recovery.", host.host_id()));
                tracing::debug!(
                    execution_id,
                    host_id = host.host_id(),
                    error = %format!("{err:#}"),
                    "remote lease release: workspace occupancy probe failed; keeping the lease",
                );
                Occupancy::Unknown
            }),
        _ => Occupancy::Unknown,
    };
    apply_release(
        work_db,
        &execution,
        &lease_id,
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane,
            occupancy: &OwningHostOccupancy(occupancy),
            preservation: &DbPreservation::new(work_db, Some(host)),
        },
        release(lease_id.clone()),
    )
    .await
}
