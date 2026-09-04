//! `bossctl work proposals list` / `bossctl work proposals show` — the
//! coordinator-facing read surface for the `worker_proposals` ledger.
//!
//! Both open `state.db` directly (same resolution `metrics`/`hosts`/`work
//! executions` use — see [`crate::open_state_db`]), so they work even when
//! the engine is wedged. Per the proposal-API design's §"UI visibility and
//! provenance", proposals get no app-side listing surface; this is the full
//! ledger, including `rejected`/`expired`/`superseded` history.

use std::path::PathBuf;

use anyhow::{Context, Result};
use boss_protocol::{ProposalKind, ProposalState, WorkerProposal};

use crate::open_state_db;

/// `bossctl work proposals list` — optionally filtered by
/// execution/work-item/kind/state, newest first, bounded by `limit` (`0` =
/// unlimited, matching the `--tail` convention elsewhere in this CLI).
///
/// `state` is left unfiltered by default rather than defaulting to
/// `proposed` only — the `rejected`/`expired` history is the point of this
/// verb.
pub(crate) fn work_proposals_list(
    json: bool,
    state_root: Option<PathBuf>,
    execution_id: Option<String>,
    work_item_id: Option<String>,
    kind: Option<String>,
    state: Option<String>,
    limit: usize,
) -> Result<()> {
    let kind = kind
        .map(|k| k.parse::<ProposalKind>())
        .transpose()
        .map_err(|err| anyhow::anyhow!(err))
        .context("parsing --kind")?;
    let state = state
        .map(|s| s.parse::<ProposalState>())
        .transpose()
        .map_err(|err| anyhow::anyhow!(err))
        .context("parsing --state")?;
    let limit = if limit == 0 { None } else { Some(limit) };

    let db = open_state_db(state_root)?;
    // Shared choke point: short ids resolve (or hard-error) before the
    // proposals filter so a bare T-form never silently matches nothing.
    let work_item_id = work_item_id
        .map(|id| db.resolve_work_item_ref_strict(&id))
        .transpose()
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    let proposals = db
        .list_worker_proposals(execution_id.as_deref(), work_item_id.as_deref(), kind, state, limit)
        .context("listing worker proposals")?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "proposals": proposals,
            })
        );
    } else if proposals.is_empty() {
        println!("no proposals match the given filters");
    } else {
        for proposal in &proposals {
            print_proposal_row(proposal);
        }
    }
    Ok(())
}

/// `bossctl work proposals show <prp_id>` — the full stored record for one
/// proposal, including its payload and disposition. Opens `state.db`
/// directly, the same as [`work_proposals_list`].
pub(crate) fn work_proposals_show(json: bool, state_root: Option<PathBuf>, id: &str) -> Result<()> {
    let db = open_state_db(state_root)?;
    let proposal = db.get_worker_proposal(id).context("reading worker proposal")?;

    if json {
        println!("{}", serde_json::json!({ "proposal": proposal }));
    } else {
        print_proposal_detail(&proposal);
    }
    Ok(())
}

fn print_proposal_row(proposal: &WorkerProposal) {
    let work_item = proposal.work_item_id.as_deref().unwrap_or("-");
    println!(
        "{}  [{}]  kind={}  execution={}  work_item={}  created={}",
        proposal.id, proposal.state, proposal.kind, proposal.execution_id, work_item, proposal.created_at,
    );
    if let Some(applied_ref) = &proposal.applied_ref {
        println!("  applied_ref: {applied_ref}");
    }
    if let Some(decision_reason) = &proposal.decision_reason {
        let decided_by = proposal
            .decided_by
            .map(|d| d.to_string())
            .unwrap_or_else(|| "-".to_owned());
        println!("  decision ({decided_by}): {decision_reason}");
    }
}

/// Human-readable rendering for `bossctl work proposals show`: every stored
/// field, including the payload pretty-printed (falling back to the raw
/// string if it somehow isn't valid JSON — the column has no `CHECK`
/// constraint, matching every sibling enum-as-TEXT column in this table).
fn print_proposal_detail(proposal: &WorkerProposal) {
    println!("id:            {}", proposal.id);
    println!("kind:          {}", proposal.kind);
    println!("state:         {}", proposal.state);
    println!("execution_id:  {}", proposal.execution_id);
    println!("work_item_id:  {}", proposal.work_item_id.as_deref().unwrap_or("-"));
    println!("created_at:    {}", proposal.created_at);
    println!("idempotency:   {}", proposal.idempotency_key);
    println!(
        "decided_by:    {}",
        proposal
            .decided_by
            .map(|d| d.to_string())
            .unwrap_or_else(|| "-".to_owned())
    );
    println!("decided_at:    {}", proposal.decided_at.as_deref().unwrap_or("-"));
    println!(
        "decision_reason: {}",
        proposal.decision_reason.as_deref().unwrap_or("-")
    );
    println!("applied_ref:   {}", proposal.applied_ref.as_deref().unwrap_or("-"));
    println!("payload:");
    match serde_json::from_str::<serde_json::Value>(&proposal.payload_json) {
        Ok(value) => println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| proposal.payload_json.clone())
        ),
        Err(_) => println!("{}", proposal.payload_json),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use boss_engine::work::{
        CreateChoreInput, CreateExecutionInput, CreateProductInput, ExecutionKind, ExecutionStatus,
        SubmitWorkerProposalInput, WorkDb,
    };
    use boss_protocol::ProposalKind;

    use super::{work_proposals_list, work_proposals_show};

    /// Minimal RAII scratch-directory guard — this crate has no `tempfile`
    /// dev-dependency, so this is the cheapest way to clean up a real,
    /// file-backed `WorkDb` without adding one just for a smoke test.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("bossctl-proposals-test-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// End-to-end smoke test against a real, file-backed `state.db`: seeds a
    /// product/chore/execution, submits a proposal through the same
    /// `WorkDb::submit_worker_proposal` write path `boss propose` uses, then
    /// calls the exact `work_proposals_list`/`work_proposals_show` functions
    /// `bossctl`'s CLI dispatches to — the closest exercise of the real code
    /// path available in-repo (the worker sandbox that wrote this surface
    /// cannot invoke the compiled `bossctl` binary itself; see the PR
    /// description).
    #[test]
    fn list_and_show_round_trip_against_a_real_db() {
        let scratch = ScratchDir::new("round-trip");
        let db = WorkDb::open(scratch.0.join("state.db")).unwrap();

        let product = db
            .create_product(
                CreateProductInput::builder()
                    .name("Proposals Smoke")
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id)
                    .name("Smoke chore")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        let outcome = db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &execution.id,
                work_item_id: &chore.id,
                kind: ProposalKind::Blocked,
                payload_json: r#"{"reason":"smoke test"}"#,
                idempotency_key: "smoke-1",
            })
            .unwrap()
            .unwrap();

        println!(
            "=== bossctl work proposals list --execution-id {} --json ===",
            execution.id
        );
        work_proposals_list(
            true,
            Some(scratch.0.clone()),
            Some(execution.id.clone()),
            None,
            None,
            None,
            50,
        )
        .unwrap();

        println!("=== bossctl work proposals show {} --json ===", outcome.proposal.id);
        work_proposals_show(true, Some(scratch.0.clone()), &outcome.proposal.id).unwrap();
    }
}
