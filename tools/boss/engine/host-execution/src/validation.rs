use anyhow::{Result, bail, ensure};
use boss_protocol_spoke::v1::{Bookmark, Run, WorkspaceKind as Kind, WorkspaceOp};

fn id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 256
            && value != "."
            && value != ".."
            && !value.starts_with('-')
            && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid opaque identifier"
    );
    Ok(())
}

pub(crate) fn store_operation(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::DiffBookmark
            | Kind::DiscardBaseline
            | Kind::CheckpointBaseline
            | Kind::ReconcileBookmark
            | Kind::ReleaseHeads
    )
}

fn reference(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 512
            && !value.starts_with(['-', '/'])
            && !value.contains("..")
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-@".contains(&b)),
        "invalid revision reference"
    );
    Ok(())
}

fn commit(value: &str) -> Result<()> {
    ensure!(
        matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected full commit identity"
    );
    Ok(())
}

fn bookmark(value: &Bookmark, repo: &str) -> Result<()> {
    id(&value.execution_id)?;
    ensure!(
        value
            .execution_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "invalid recovery execution id"
    );
    ensure!(value.repo_id == repo, "bookmark belongs to another repository");
    ensure!(
        value.head == format!("boss/{}", value.execution_id)
            && value.recovery == format!("boss-recovery/{}", value.execution_id)
            && value.base == format!("boss-base/{}", value.execution_id),
        "unexpected recovery bookmark names"
    );
    if !value.anchor_commit.is_empty() {
        commit(&value.anchor_commit)?;
    }
    Ok(())
}

/// Each kind has an explicit field allowlist. Unused fields are rejected, so a
/// workspace command cannot become an argv/path tunnel as the schema evolves.
pub fn validate_workspace(op: &WorkspaceOp, run: Option<&Run>) -> Result<Kind> {
    let kind = Kind::try_from(op.kind)?;
    let mut allowed = WorkspaceOp {
        kind: op.kind,
        ..Default::default()
    };
    if !matches!(kind, Kind::EnsureRepo | Kind::ListRepos | Kind::ListWorkspaces) {
        let run = run.ok_or_else(|| anyhow::anyhow!("workspace mutation requires assignment"))?;
        id(&run.execution_id)?;
        ensure!(
            run.execution_id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "invalid recovery execution id"
        );
        id(&run.run_id)?;
        id(&run.spawn_token)?;
        ensure!(run.assignment_generation > 0, "missing assignment generation");
        id(&op.repo_id)?;
        allowed.repo_id.clone_from(&op.repo_id);
    }
    if !store_operation(kind)
        && !matches!(
            kind,
            Kind::EnsureRepo | Kind::ListRepos | Kind::ListWorkspaces | Kind::Lease
        )
    {
        id(&op.workspace_id)?;
        id(&op.lease_id)?;
        allowed.workspace_id.clone_from(&op.workspace_id);
        allowed.lease_id.clone_from(&op.lease_id);
    }
    match kind {
        Kind::WorkspaceUnspecified => bail!("unspecified workspace operation"),
        Kind::EnsureRepo => {
            let origin = &op.origin;
            ensure!(
                !origin.is_empty()
                    && origin.len() <= 2048
                    && !origin.starts_with(['-', '/'])
                    && !origin.contains("..")
                    && origin
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b":/._-@".contains(&b))
                    && (origin.starts_with("https://")
                        || origin.starts_with("ssh://")
                        || (origin.contains('@') && origin.contains(':'))
                        || !origin.contains(':')),
                "unsupported repository origin"
            );
            if !origin.contains(':') {
                // The shared CLI builder passes non-slugs as --origin.
                // A slash-only reference would therefore be a local path.
                id(origin)?;
            }
            allowed.origin.clone_from(origin);
        }
        Kind::Lease => {
            if !op.workspace_id.is_empty() {
                id(&op.workspace_id)?;
            }
            ensure!(
                !op.allow_dirty || !op.workspace_id.is_empty(),
                "dirty reclaim requires a workspace"
            );
            ensure!(
                op.task_label.len() <= 1024 && !op.task_label.contains('\0'),
                "invalid task label"
            );
            ensure!(op.exclude_workspace_ids.len() <= 256, "too many workspace exclusions");
            for value in &op.exclude_workspace_ids {
                id(value)?;
            }
            ensure!(
                !op.exclude_workspace_ids.contains(&op.workspace_id),
                "preferred workspace is excluded"
            );
            allowed.workspace_id.clone_from(&op.workspace_id);
            allowed.task_label.clone_from(&op.task_label);
            allowed.allow_dirty = op.allow_dirty;
            allowed.exclude_workspace_ids.clone_from(&op.exclude_workspace_ids);
        }
        Kind::Heartbeat => {
            ensure!(
                op.ttl_seconds <= i64::MAX as u64,
                "lease TTL exceeds host integer range"
            );
            allowed.ttl_seconds = op.ttl_seconds;
        }
        Kind::ForceRelease => {
            ensure!(
                !op.reason.is_empty() && op.reason.len() <= 4096 && !op.reason.contains('\0'),
                "force release requires a bounded reason"
            );
            allowed.reason.clone_from(&op.reason);
        }
        Kind::CreateChange => {
            ensure!(
                !op.title.is_empty() && op.title.len() <= 4096 && !op.title.contains('\0'),
                "change requires a bounded title"
            );
            allowed.title.clone_from(&op.title);
        }
        Kind::GotoPr | Kind::PrepareRecoveryPr => {
            ensure!(op.pr_number > 0, "missing PR number");
            allowed.pr_number = op.pr_number;
        }
        Kind::GotoRevision => {
            commit(&op.revision)?;
            allowed.revision.clone_from(&op.revision);
        }
        Kind::CreateBookmark => {
            if let Some(predecessor) = &op.predecessor {
                bookmark(predecessor, &op.repo_id)?;
            }
            if !op.inherited_base.is_empty() {
                reference(&op.inherited_base)?;
            }
            allowed.predecessor.clone_from(&op.predecessor);
            allowed.inherited_base.clone_from(&op.inherited_base);
        }
        Kind::RestoreBookmark => {
            allowed.self_retry = op.self_retry;
        }
        Kind::RestoreRebased | Kind::ReconcileBookmark => {
            reference(&op.trunk)?;
            if !op.pr_bookmark.is_empty() {
                reference(&op.pr_bookmark)?;
            }
            allowed.trunk.clone_from(&op.trunk);
            allowed.pr_bookmark.clone_from(&op.pr_bookmark);
        }
        Kind::Release
        | Kind::Status
        | Kind::ListWorkspaces
        | Kind::ListRepos
        | Kind::DiffBookmark
        | Kind::DiscardBaseline
        | Kind::CheckpointBaseline
        | Kind::ReleaseHeads => {}
    }
    if matches!(
        kind,
        Kind::DiffBookmark
            | Kind::RestoreBookmark
            | Kind::DiscardBaseline
            | Kind::RestoreRebased
            | Kind::CheckpointBaseline
            | Kind::PrepareRecoveryPr
            | Kind::ReconcileBookmark
            | Kind::ReleaseHeads
    ) {
        let value = op
            .bookmark
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing recovery bookmark"))?;
        bookmark(value, &op.repo_id)?;
        ensure!(
            value.execution_id == run.expect("validated assignment").execution_id,
            "bookmark belongs to another execution"
        );
        allowed.bookmark.clone_from(&op.bookmark);
    }
    ensure!(*op == allowed, "fields not permitted for workspace operation");
    Ok(kind)
}
