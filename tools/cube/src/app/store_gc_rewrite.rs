//! Prepare expensive operation rewrites without excluding readers.
use super::*;
use std::fs;

/// Refresh a primary left stale by a legacy in-place collection.
///
/// Callers must not hold the maintenance gate shared: this takes exclusivity
/// the same way staged GC does, so `workspace update-stale` cannot merge a
/// racing operation head.
pub(crate) fn recover_stale_primary(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    source: &Path,
) -> Result<()> {
    let gate = jj_maintenance_gate(database_path, MAINTENANCE_CMD_TIMEOUT)?.ok_or(CubeError::MaintenanceBusy)?;
    gate.recover_quarantine(runner)?;
    runner.run_with_timeout(
        &gated_jj_invocation(source, &["workspace", "update-stale"]),
        MAINTENANCE_CMD_TIMEOUT,
    )?;
    Ok(())
}

/// Repair primaries left stale by the legacy in-place collector before
/// preparing a staged rewrite. Release the reader before taking exclusivity.
pub(super) fn prepare_with_recovery(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    source: &Path,
    cut: &str,
) -> Result<Option<PreparedRewrite>> {
    let reader = JjGate::shared_for_database(database_path)?;
    let snapshot = snapshot_primary(runner, source, MAINTENANCE_CMD_TIMEOUT);
    if let Err(error) = snapshot {
        if crate::app::jj::jj_update_stale_recovery_kind(&error).is_none() {
            return Err(error);
        }
        drop(reader);
        recover_stale_primary(runner, database_path, source)?;
    } else {
        drop(reader);
    }
    let _reader = JjGate::shared_for_database(database_path)?;
    PreparedRewrite::prepare(runner, source, cut)
}

/// Only operation heads are private. jj writes content-addressed operation
/// files into the shared store, but cannot publish them to live readers.
/// The GC lock prevents a collector from deleting those files meanwhile.
pub(super) struct PreparedRewrite {
    _scratch: tempfile::TempDir,
    original: String,
    rewritten: String,
}

impl PreparedRewrite {
    pub(super) fn prepare(runner: &dyn CommandRunner, source: &Path, cut: &str) -> Result<Option<Self>> {
        // Flush edits the primary working copy holds, so the head recorded
        // below already contains them. Refreshing a working copy that still
        // has unsnapshotted edits snapshots it at its pre-rewrite operation
        // and merges that history back into the rewritten one.
        snapshot_primary(runner, source, MAINTENANCE_CMD_TIMEOUT)?;
        let Some(original) = single_op_head_before_rewrite(source)? else {
            return Ok(None);
        };
        let scratch = tempfile::Builder::new()
            .prefix("cube-rewrite-")
            .tempdir_in(source.join(".jj"))
            .map_err(CubeError::Io)?;
        let repo = scratch.path().join(".jj/repo");
        fs::create_dir_all(repo.join("op_heads/heads")).map_err(CubeError::Io)?;
        fs::copy(source.join(".jj/repo/op_heads/type"), repo.join("op_heads/type")).map_err(CubeError::Io)?;
        fs::write(repo.join("op_heads/heads").join(&original), b"").map_err(CubeError::Io)?;
        for entry in fs::read_dir(source.join(".jj/repo")).map_err(CubeError::Io)? {
            let entry = entry.map_err(CubeError::Io)?;
            if entry.file_name() != "op_heads" {
                std::os::unix::fs::symlink(entry.path(), repo.join(entry.file_name())).map_err(CubeError::Io)?;
            }
        }
        std::os::unix::fs::symlink(source.join(".jj/working_copy"), scratch.path().join(".jj/working_copy"))
            .map_err(CubeError::Io)?;
        // jj may allocate a new config id for this copied repo. Keep its
        // configuration inside the disposable directory too.
        let mut abandon = gated_jj_invocation(
            scratch.path(),
            &["--ignore-working-copy", "op", "abandon", &format!("..{cut}")],
        );
        for name in ["HOME", "XDG_CONFIG_HOME"] {
            abandon.env.push((name.into(), scratch.path().display().to_string()));
        }
        runner.run_with_timeout(&abandon, MAINTENANCE_CMD_TIMEOUT)?;
        let heads = op_heads(scratch.path())?;
        let [rewritten] = heads.as_slice() else {
            return Err(CubeError::InvalidArgument(
                "prepared rewrite did not produce exactly one head".into(),
            ));
        };
        // Build the rewritten head's commit index before excluding readers.
        runner.run_with_timeout(
            &gated_jj_invocation(
                source,
                &[
                    "--ignore-working-copy",
                    "--at-operation",
                    rewritten,
                    "log",
                    "-r",
                    "root()",
                    "--no-graph",
                    "-T",
                    "commit_id",
                ],
            ),
            MAINTENANCE_CMD_TIMEOUT,
        )?;
        Ok(Some(Self {
            _scratch: scratch,
            original,
            rewritten: rewritten.clone(),
        }))
    }

    /// The head the rewrite replaces.
    pub(super) fn original(&self) -> &str {
        &self.original
    }

    /// The prepared head: the trusted anchor for every later ancestry check.
    pub(super) fn rewritten(&self) -> &str {
        &self.rewritten
    }

    pub(super) fn is_current(&self, source: &Path) -> Result<bool> {
        Ok(op_heads(source)? == [self.original.clone()])
    }

    /// Caller holds exclusivity, has observed a quiet host, checked the old
    /// head, and installed the durable quarantine marker. A crash between
    /// publishing the new head and removing the old leaves two heads and
    /// therefore fails closed in quarantine recovery.
    pub(super) fn publish(&self, source: &Path) -> Result<()> {
        if !self.is_current(source)? {
            return Err(CubeError::InvalidArgument(
                "operation heads advanced after rewrite preparation; nothing published".into(),
            ));
        }
        let heads = source.join(".jj/repo/op_heads/heads");
        if self.original != self.rewritten {
            fs::File::create(heads.join(&self.rewritten))
                .map_err(CubeError::Io)?
                .sync_all()
                .map_err(CubeError::Io)?;
            fs::File::open(&heads)
                .map_err(CubeError::Io)?
                .sync_all()
                .map_err(CubeError::Io)?;
            fs::remove_file(heads.join(&self.original)).map_err(CubeError::Io)?;
            fs::File::open(&heads)
                .map_err(CubeError::Io)?
                .sync_all()
                .map_err(CubeError::Io)?;
        }
        Ok(())
    }
}

/// Snapshot the store's primary working copy now. A no-op when it holds no
/// edits; otherwise it records one operation, which a prepared rewrite sees as
/// progress. Any jj command snapshots first, so `status` is portable across
/// the supported versions.
pub(super) fn snapshot_primary(runner: &dyn CommandRunner, source: &Path, timeout: Duration) -> Result<()> {
    runner
        .run_with_timeout(&gated_jj_invocation(source, &["status"]), timeout)
        .map(drop)
}

/// The store's only operation head. `workspace update-stale` cannot be pinned
/// to an operation, and jj merges multiple heads on load, so a refresh is
/// never run over several.
fn single_head(source: &Path, rewritten: &str, when: &str) -> Result<String> {
    let heads = op_heads(source)?;
    let [head] = heads.as_slice() else {
        return Err(CubeError::InvalidArgument(format!(
            "the store at {} holds {} operation heads {when} (expected only {rewritten}); not refreshing the \
             working copy, which would merge them",
            source.display(),
            heads.len()
        )));
    };
    Ok(head.clone())
}

/// Require exactly one operation head, and that it is `rewritten` or safely
/// descends from it. The head set is verified both before and after
/// `workspace update-stale` rather than trusted. Every caller holds the
/// maintenance gate exclusively, so the ancestry walk runs as a holder.
fn require_head_on(source: &Path, runner: &dyn CommandRunner, rewritten: &str, when: &str) -> Result<()> {
    let head = single_head(source, rewritten, when)?;
    if head != rewritten {
        crate::app::store_gc_integrity::require_safe_progress_gate_held(runner, source, rewritten, &head)?;
    }
    Ok(())
}

/// Refresh the primary working copy onto the published rewrite and verify the
/// result. Used right after publication, with the gate held exclusively.
pub(super) fn complete_refresh(
    runner: &dyn CommandRunner,
    source: &Path,
    rewritten: &str,
    timeout: Duration,
) -> Result<()> {
    require_head_on(source, runner, rewritten, "before the working-copy refresh")?;
    runner.run_with_timeout(&gated_jj_invocation(source, &["workspace", "update-stale"]), timeout)?;
    require_head_on(source, runner, rewritten, "after the working-copy refresh")
}

/// Refresh the working copy of every free, healthy workspace of `repo` onto
/// the published rewrite, and return how many were refreshed and how many
/// could not be.
///
/// The rewrite gives every kept operation a new id, so each working copy's
/// recorded operation is no longer an ancestor of the head. jj newer than
/// 0.26 then refuses every command in that workspace as "a sibling of the
/// working copy's operation", and only after walking both the rewritten log
/// and the one it replaced to find that out; `workspace update-stale` walks
/// them again. On a log of tens of thousands of operations each walk costs
/// seconds, which is more than a lease allows one candidate, so a lease that
/// met such a workspace passed it over and provisioned a cold one, every
/// time, until collection had deleted the old log. Paying for the walk here,
/// once per workspace and off the lease path, leaves the pool reusable.
///
/// Only workspaces a lease would itself probe and refresh are touched: free,
/// and not recorded dirty, conflicted or quarantined. A leased workspace
/// belongs to its holder, and one holding unsnapshotted edits would have them
/// snapshotted at its pre-rewrite operation, merging that history back in.
/// The caller holds the maintenance gate shared, so leases proceed meanwhile;
/// a lease that reaches a workspace first refreshes it itself, and jj's
/// working-copy lock serialises the two. Failures are reported, not fatal:
/// the lease path still recovers such a workspace on its own.
pub(super) fn refresh_free_workspaces(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    repo: &str,
    timeout: Duration,
) -> Result<(usize, usize)> {
    use crate::store::{EffectiveState, WorkspaceListFilter};
    let store = match database_path {
        Some(path) => Store::open_at(path)?,
        None => Store::open_default()?,
    };
    let free = |workspace_id: Option<&str>| {
        store.list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some(repo),
            workspace_id,
            effective_state: Some(EffectiveState::Free),
            exclude_cleanup_marked: true,
            ..Default::default()
        })
    };
    let (mut refreshed, mut failed) = (0, 0);
    for workspace in free(None)? {
        // A lease may have claimed it since the listing.
        if free(Some(&workspace.workspace_id))?.is_empty() || !workspace.workspace_path.join(".jj").is_dir() {
            continue;
        }
        let refresh = gated_jj_invocation(&workspace.workspace_path, &["workspace", "update-stale"]);
        match runner.run_with_timeout(&refresh, timeout) {
            Ok(_) => refreshed += 1,
            Err(error) => {
                failed += 1;
                eprintln!(
                    "cube: store gc: {repo}: could not refresh the working copy of {} after the rewrite: {error}",
                    workspace.workspace_id
                );
            }
        }
    }
    Ok((refreshed, failed))
}

/// Settle a refresh record, under the exclusive gate, from the store's one
/// head. The record is written before publication, so the rewrite may never
/// have happened (the pass lost a race, or crashed first): then the head is
/// the original or a descendant of it, nothing was rewritten, and there is
/// nothing to refresh. A head on the rewrite is refreshed and verified. A head
/// on neither is unexplained and refused. The rewrite abandons the original's
/// history, so no head descends from both.
pub(crate) fn recover_refresh(
    runner: &dyn CommandRunner,
    source: &Path,
    original: &str,
    rewritten: &str,
    timeout: Duration,
) -> Result<()> {
    let head = single_head(source, rewritten, "when recovering a refresh")?;
    if head == rewritten || original == rewritten {
        return complete_refresh(runner, source, rewritten, timeout);
    }
    use crate::app::store_gc_integrity::descends_from_gate_held as descends;
    if head == original {
        return Ok(());
    }
    if descends(runner, source, rewritten, &head)? {
        return complete_refresh(runner, source, rewritten, timeout);
    }
    if descends(runner, source, original, &head)? {
        return Ok(());
    }
    Err(CubeError::InvalidArgument(format!(
        "the store at {} has head {head}, which descends from neither the rewrite {rewritten} nor the operation \
         {original} it replaced",
        source.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_heads_refuse_concurrent_progress_and_publish_only_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let heads = dir.path().join(".jj/repo/op_heads/heads");
        fs::create_dir_all(&heads).unwrap();
        fs::write(heads.join("original"), b"").unwrap();
        let prepared = PreparedRewrite {
            _scratch: tempfile::tempdir().unwrap(),
            original: "original".into(),
            rewritten: "rewritten".into(),
        };
        assert!(prepared.is_current(dir.path()).unwrap());
        fs::write(heads.join("concurrent"), b"").unwrap();
        assert!(!prepared.is_current(dir.path()).unwrap());
        fs::remove_file(heads.join("original")).unwrap();
        assert!(!prepared.is_current(dir.path()).unwrap());
        assert!(prepared.publish(dir.path()).is_err());
        assert_eq!(op_heads(dir.path()).unwrap(), ["concurrent"]);
        fs::remove_file(heads.join("concurrent")).unwrap();
        fs::write(heads.join("original"), b"").unwrap();
        prepared.publish(dir.path()).unwrap();
        assert_eq!(op_heads(dir.path()).unwrap(), ["rewritten"]);
    }
}
