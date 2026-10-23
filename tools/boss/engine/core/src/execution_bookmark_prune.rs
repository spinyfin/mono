//! Retire the shared-store pointers of executions whose PR is finished.
//!
//! Every execution gets three bookmarks in the shared jj store (`boss-base/`,
//! `boss-recovery/`, `boss/`), and jj writes every bookmark into the view of
//! every operation in that store. Nothing used to delete them, so the view a
//! `jj new`, a snapshot or a `jj bookmark set` writes grew with the number of
//! executions ever run (2.6 MB each, at ~5,000 operations a day, on the dev
//! Mac before this sweep existed). The pointers only matter while the work
//! might still have to be recovered, and that ends when the PR that carried
//! it is merged or closed.
//!
//! Each pass examines a bounded batch of terminal, local executions whose
//! records are still live ([`crate::work::WorkDb::list_prune_candidates`]).
//! A record whose chain-owning task is still open is stamped and left alone.
//! For the rest the recovery crate is asked to prove the preserved work
//! published ([`boss_engine_recovery::execution_bookmark::prune`]): an empty
//! unpublished diff, or containment in a voucher the engine recorded — the
//! PR head the merge poller last observed, the heads the chain's executions
//! recorded around their pushes, the pushed branch's remote-tracking
//! bookmark, the trunk's. Only when a voucher is missing does the sweep
//! spend a bounded number of `gh pr view` calls per pass on the PR's live
//! head. Proven work loses all three bookmarks and its record is retired;
//! unproven work keeps its recovery head, is re-examined daily, and is
//! surfaced as an attention item naming the changes nothing vouches for.
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use boss_engine_recovery::execution_bookmark::{PruneOutcome, Voucher};

use crate::coordinator::ExecutionCoordinator;
use crate::work::{CreateAttentionItemInput, PrStateChecker, PruneCandidate, WorkDb, prune_outcome};

/// Attention kind raised when a finished PR's execution left preserved work
/// that nothing the engine recorded can vouch for.
pub(crate) const UNPROVEN: &str = "execution_bookmark_prune_unproven";

/// Records examined per pass. The backlog on first deployment is every
/// execution ever run; at the sweep's five-minute cadence this drains a few
/// thousand records in a few hours without starving the pass it shares.
pub(crate) const BATCH_SIZE: usize = 100;
/// `gh pr view` calls one pass may spend on PRs with no recorded head.
pub(crate) const GH_LOOKUPS_PER_PASS: usize = 20;
/// How soon a record whose chain is still open is looked at again.
pub(crate) const RECHECK_OPEN_SECS: i64 = 6 * 60 * 60;
/// How soon unproven (or errored) records are examined again.
pub(crate) const RECHECK_UNPROVEN_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Default, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct PruneReport {
    pub examined: usize,
    pub published: usize,
    pub already_gone: usize,
    pub unproven: usize,
    pub pr_open: usize,
    pub errors: usize,
    pub gh_lookups: usize,
}

/// One pass over the due candidates. Never fails the loop for a single
/// record: each record's failure is logged, stamped and reported, so one
/// unreadable store cannot stop the rest from being retired.
pub(crate) async fn run_one_pass(
    db: &WorkDb,
    coordinator: &ExecutionCoordinator,
    pr_state: Arc<dyn PrStateChecker>,
) -> Result<PruneReport> {
    let candidates = db.list_prune_candidates(
        crate::abandoned_branch_pr_sweep::TERMINATION_GRACE_SECS,
        RECHECK_OPEN_SECS,
        RECHECK_UNPROVEN_SECS,
        BATCH_SIZE,
    )?;
    let mut report = PruneReport::default();
    let mut trunks: HashMap<String, String> = HashMap::new();
    for candidate in candidates {
        report.examined += 1;
        let execution_id = candidate.execution.id.clone();
        if !candidate.chain_closed {
            report.pr_open += 1;
            db.record_execution_bookmark_prune_check(&execution_id, prune_outcome::PR_OPEN)?;
            continue;
        }
        let result = prune_candidate(db, coordinator, &pr_state, &candidate, &mut trunks, &mut report).await;
        match result {
            Ok(outcome) => {
                db.record_execution_bookmark_prune_check(&execution_id, outcome)?;
            }
            Err(err) => {
                report.errors += 1;
                tracing::error!(execution_id = %execution_id, error = %format!("{err:#}"), "execution bookmark prune failed");
                crate::execution_bookmark_recovery::report_failure(db, &candidate.execution, &format!("{err:#}"));
                db.record_execution_bookmark_prune_check(&execution_id, prune_outcome::ERROR)?;
            }
        }
    }
    Ok(report)
}

/// Everything the engine recorded that may carry the candidate's work,
/// strongest first.
fn vouchers_for(candidate: &PruneCandidate, publication: &str, trunk: &str) -> Vec<Voucher> {
    let pr = candidate
        .pr_url
        .as_deref()
        .and_then(boss_github::pr_url::pr_number_from_url)
        .map(|n| format!("PR #{n}"))
        .unwrap_or_else(|| "the PR".to_owned());
    let mut vouchers = Vec::new();
    if let Some(sha) = &candidate.observed_pr_head
        && let Some(voucher) = Voucher::commit(format!("{pr} head (merge poller)"), sha)
    {
        vouchers.push(voucher);
    }
    for sha in candidate.recorded_pr_heads.iter().rev() {
        if let Some(voucher) = Voucher::commit(format!("{pr} head recorded by an execution"), sha) {
            vouchers.push(voucher);
        }
    }
    vouchers.push(Voucher::remote_bookmark(publication, "origin"));
    vouchers.push(Voucher::remote_bookmark(trunk, "origin"));
    vouchers
}

async fn prune_candidate(
    db: &WorkDb,
    coordinator: &ExecutionCoordinator,
    pr_state: &Arc<dyn PrStateChecker>,
    candidate: &PruneCandidate,
    trunks: &mut HashMap<String, String>,
    report: &mut PruneReport,
) -> Result<&'static str> {
    let execution = &candidate.execution;
    let record = db.execution_bookmark(&execution.id)?;
    let repo_id = execution.cube_repo_id.clone().unwrap_or_default();
    let trunk = match trunks.get(&repo_id) {
        Some(trunk) => trunk.clone(),
        None => {
            let trunk = coordinator
                .recovery_trunk(&record.host_id, &repo_id)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "main".to_owned());
            trunks.insert(repo_id, trunk.clone());
            trunk
        }
    };
    let mut vouchers = vouchers_for(candidate, &record.publication(), &trunk);
    let mut outcome = coordinator.prune_execution_bookmark(&record, &vouchers, &trunk).await?;
    if let PruneOutcome::Unproven { .. } = &outcome
        && let Some(pr_url) = candidate.pr_url.clone()
        && report.gh_lookups < GH_LOOKUPS_PER_PASS
    {
        // The engine recorded no head that carries the work; ask GitHub for
        // the PR's head once. `headRefOid` survives the branch's deletion.
        report.gh_lookups += 1;
        let checker = Arc::clone(pr_state);
        let inspected = tokio::task::spawn_blocking(move || checker.inspect(&pr_url)).await?;
        match inspected {
            Ok(inspect) => {
                if let Some(sha) = inspect.head_sha
                    && let Some(voucher) = Voucher::commit("PR head (GitHub)", &sha)
                    && !vouchers.contains(&voucher)
                {
                    vouchers = vec![voucher];
                    outcome = coordinator.prune_execution_bookmark(&record, &vouchers, &trunk).await?;
                }
            }
            Err(err) => {
                tracing::warn!(execution_id = %execution.id, error = %format!("{err:#}"), "prune: PR head lookup failed; keeping the pointers");
            }
        }
    }
    match outcome {
        PruneOutcome::Published {
            vouched_by,
            commit,
            forgotten,
            rewritten,
        } => {
            report.published += 1;
            let reason = format!(
                "pruned: work published; vouched for by {vouched_by} ({commit}){}; forgot {}",
                if rewritten > 0 {
                    format!(", {rewritten} change(s) carried as rewrites")
                } else {
                    String::new()
                },
                forgotten.join(", ")
            );
            tracing::info!(execution_id = %execution.id, %reason, "execution bookmarks pruned");
            db.retire_execution_bookmark(&execution.id, &reason)?;
            db.resolve_attention_kind_for_execution(&execution.id, UNPROVEN)?;
            Ok(prune_outcome::PUBLISHED)
        }
        PruneOutcome::AlreadyGone { forgotten } => {
            report.already_gone += 1;
            let reason = if forgotten.is_empty() {
                "pruned: both heads were already gone".to_owned()
            } else {
                format!("pruned: both heads were already gone; forgot {}", forgotten.join(", "))
            };
            db.retire_execution_bookmark(&execution.id, &reason)?;
            db.resolve_attention_kind_for_execution(&execution.id, UNPROVEN)?;
            Ok(prune_outcome::ALREADY_GONE)
        }
        PruneOutcome::Unproven {
            kept,
            missing,
            forgotten,
            tried,
        } => {
            report.unproven += 1;
            tracing::warn!(execution_id = %execution.id, %kept, missing = missing.len(), "execution bookmark prune: work not proven published; keeping the recovery head");
            if db.reraise_open_execution_attention(&execution.id, UNPROVEN)?.is_none() {
                let missing_lines = missing
                    .iter()
                    .map(|change| format!("- `{}` ({}): {}", change.commit, change.change_id, change.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                let tried_lines = tried.iter().map(|t| format!("- {t}")).collect::<Vec<_>>().join("\n");
                let forgotten_note = if forgotten.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n\nIts other pointers (`{}`) were forgotten; the baseline is rebuilt from the record when needed.",
                        forgotten.join("`, `")
                    )
                };
                db.create_attention_item(CreateAttentionItemInput {
                    execution_id: Some(execution.id.clone()),
                    work_item_id: Some(candidate.chain_root_id.clone()),
                    kind: UNPROVEN.to_owned(),
                    status: None,
                    title: "Finished PR left execution work nothing vouches for".to_owned(),
                    body_markdown: format!(
                        "The PR owning execution `{}` is merged or closed, but the work preserved at `{kept}` in the shared jj store is not carried by anything the engine recorded, so the pointer was kept. Changes nothing vouches for:\n{missing_lines}\n\nVouchers tried:\n{tried_lines}{forgotten_note}\n\nInspect with `jj -R {} log -r 'bookmarks(exact:\"{kept}\")'`. If the work is known to be obsolete, `jj bookmark forget {kept}` retires it; the sweep re-examines the record daily.",
                        execution.id,
                        record.repo_path.display(),
                    ),
                    resolved_at: None,
                })?;
            }
            Ok(prune_outcome::UNPROVEN)
        }
    }
}
