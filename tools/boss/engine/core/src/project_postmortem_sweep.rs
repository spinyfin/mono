//! Periodic reconciler that auto-schedules a `design_postmortem` task when
//! a project's implementation work drains to zero.
//!
//! ## Trigger
//!
//! On each pass, for every project with a design doc set
//! (`design_doc_path.is_some()`), the sweep checks whether every task that
//! counts toward the project (`project_task` / `design` / `investigation` —
//! deliberately excludes `design_postmortem` itself, both by kind and
//! because [`boss_engine::work::WorkDb::list_tasks`] never returns that
//! kind for a project) is terminal. If so, and at least one
//! `project_task`/`investigation` completed since the last postmortem (or
//! ever, if there has been none), the engine auto-creates a new
//! `design_postmortem` task whose remit is to review the merged PRs since
//! the last postmortem against the design doc and update it to reflect
//! what actually shipped.
//!
//! ## Edge-triggered and re-armable, without a cursor column
//!
//! The "since the last postmortem" cutoff is derived, not stored: it is
//! the most recent (non-deleted) `design_postmortem` task's `completed_at`.
//! This makes the trigger self-limiting without any extra schema:
//!
//! - Right after a postmortem is created, the next pass's dedup gate
//!   (below) skips the project entirely — the postmortem is still open.
//! - Once it completes, subsequent passes see the trigger count at zero
//!   again (nothing new happened), but zero `project_task`/`investigation`
//!   completions postdate the postmortem's own `completed_at` — so the
//!   "at least one completion since last postmortem" precondition fails
//!   and no new postmortem is scheduled.
//! - Only when a *new* wave of tasks is added to the project, worked, and
//!   drained to zero again does the cutoff comparison find fresh
//!   completions and re-fire — satisfying the "re-armable" requirement
//!   without the sweep needing to remember anything between passes.
//!
//! ## Dedup gate
//!
//! Never schedule a new postmortem while the project's most recent one is
//! still open. "Open" is `!status.is_terminal()` (matches the vocabulary
//! `TaskStatus::is_terminal()` already uses everywhere else) rather than a
//! bespoke status list, so a `blocked` postmortem also blocks a duplicate
//! rather than falling through a gap.
//!
//! ## Cadence
//!
//! This is a low-frequency, low-cardinality reconciliation (products and
//! projects are small in number for a dev tool), so a straightforward
//! per-product/per-project scan every pass is cheap enough; there is no
//! need for a single denormalised SQL query the way the higher-frequency
//! sweeps use.

use std::sync::Arc;
use std::time::Duration;

use crate::work::{Product, Project, TaskKind, TaskStatus, TriggerTaskSnapshot, WorkDb};

/// Interval between sweep passes. Postmortem scheduling is not latency
/// sensitive (it fires once a whole wave of implementation work has
/// finished), so this runs far less often than the dependency/dispatch
/// safety-net sweeps.
pub const PROJECT_POSTMORTEM_SWEEP_INTERVAL_SECS: u64 = 300;

/// Counters from one sweep pass.
#[derive(Debug, Default)]
pub struct ProjectPostmortemSweepOutcome {
    /// Number of projects evaluated this pass (design doc set, live tasks
    /// exist).
    pub projects_evaluated: usize,
    /// Number of `design_postmortem` tasks created this pass.
    pub postmortems_created: usize,
}

impl crate::sweep_loop::SweepOutcome for ProjectPostmortemSweepOutcome {
    fn has_activity(&self) -> bool {
        self.postmortems_created > 0
    }

    fn log(&self) {
        tracing::info!(
            projects_evaluated = self.projects_evaluated,
            postmortems_created = self.postmortems_created,
            "project-postmortem sweep: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so a wave that finished while the engine was
/// down is reconciled at boot.
///
/// `kick_fn` is called whenever the pass creates at least one postmortem
/// task, so the coordinator scheduler picks up its (autostart) execution
/// immediately rather than waiting for the next dispatch-triggering event.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    interval: Duration,
    kick_fn: Arc<dyn Fn() + Send + Sync>,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let kick_fn = Arc::clone(&kick_fn);
        async move {
            let outcome = run_one_pass(work_db.as_ref()).await;
            if outcome.postmortems_created > 0 {
                kick_fn();
            }
            outcome
        }
    })
}

/// Run a single sweep pass over every product's projects. Returns per-pass
/// counters for the caller to log or assert in tests.
pub async fn run_one_pass(work_db: &WorkDb) -> ProjectPostmortemSweepOutcome {
    let mut outcome = ProjectPostmortemSweepOutcome::default();

    let products = match work_db.list_products() {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(?err, "project-postmortem sweep: failed to list products; skipping pass");
            return outcome;
        }
    };

    for product in &products {
        let projects = match work_db.list_projects(&product.id, None) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(
                    product_id = %product.id,
                    ?err,
                    "project-postmortem sweep: failed to list projects; skipping product",
                );
                continue;
            }
        };
        for project in &projects {
            if project.design_doc_path.as_deref().unwrap_or_default().is_empty() {
                // No design doc yet — precondition #4: skip rather than error.
                continue;
            }
            match evaluate_project(work_db, product, project).await {
                Ok(EvalOutcome::Skipped) => {}
                Ok(EvalOutcome::Evaluated) => outcome.projects_evaluated += 1,
                Ok(EvalOutcome::Scheduled) => {
                    outcome.projects_evaluated += 1;
                    outcome.postmortems_created += 1;
                }
                Err(err) => {
                    tracing::warn!(
                        project_id = %project.id,
                        ?err,
                        "project-postmortem sweep: failed to evaluate project; skipping",
                    );
                }
            }
        }
    }

    outcome
}

enum EvalOutcome {
    /// Design doc missing (already filtered before this point), trigger
    /// count non-zero, dedup gate hit, or zero net completions — no action
    /// taken, not counted as an evaluated candidate.
    Skipped,
    /// Trigger count was zero and the project was a genuine candidate, but
    /// the precondition (dedup gate / zero net completions) held off
    /// scheduling.
    Evaluated,
    /// A new `design_postmortem` task was created.
    Scheduled,
}

async fn evaluate_project(work_db: &WorkDb, product: &Product, project: &Project) -> anyhow::Result<EvalOutcome> {
    let tasks = work_db.list_project_trigger_tasks(&product.id, &project.id)?;
    if tasks.is_empty() {
        return Ok(EvalOutcome::Skipped);
    }
    if tasks.iter().any(|t| !t.status.is_terminal()) {
        // Trigger count (non-terminal project_task/design/investigation
        // tasks) is still non-zero.
        return Ok(EvalOutcome::Skipped);
    }

    let last_postmortem = work_db.last_design_postmortem_for_project(&project.id)?;
    if let Some(ref lp) = last_postmortem
        && !lp.status.is_terminal()
    {
        // Dedup gate: a postmortem for this project is still open.
        return Ok(EvalOutcome::Evaluated);
    }
    let cutoff = last_postmortem
        .as_ref()
        .and_then(|lp| epoch_secs(lp.completed_at.as_deref()));

    let newly_completed: Vec<&TriggerTaskSnapshot> = tasks
        .iter()
        .filter(|t| matches!(t.kind, TaskKind::ProjectTask | TaskKind::Investigation))
        .filter(|t| t.status == TaskStatus::Done)
        .filter(|t| match (cutoff, epoch_secs(t.completed_at.as_deref())) {
            // `completed_at` has one-second resolution, so a task that
            // completes in the same wall-clock second as the last
            // postmortem's own completion is genuinely ambiguous — it could
            // be a task the postmortem already reviewed (real production
            // causality: the postmortem cannot complete before the work it
            // reviews does, so ties are only possible with already-reviewed
            // work, never with new work racing ahead of it) or, in a
            // synthetic same-second test, a fresh task. Strict `>` resolves
            // the tie toward "already reviewed" — a missed re-fire is
            // recovered by the next wave's completion (this sweep is a
            // reconciler, not a one-shot), whereas the other direction would
            // let a project_task the last postmortem already covered
            // re-trigger a duplicate, which rule 2 (no self-retrigger)
            // exists specifically to prevent.
            (Some(cutoff), Some(completed)) => completed > cutoff,
            (None, Some(_)) => true,
            _ => false,
        })
        .collect();
    if newly_completed.is_empty() {
        // Precondition #4 (second half): zero net implementation work
        // since the last postmortem (or ever) — nothing to review.
        return Ok(EvalOutcome::Evaluated);
    }

    let merged_prs: Vec<(&str, &str)> = newly_completed
        .iter()
        .filter_map(|t| t.pr_url.as_deref().map(|url| (t.name.as_str(), url)))
        .collect();

    let description = compose_postmortem_brief(project, &merged_prs);
    let created = work_db.create_design_postmortem(&product.id, &project.id, &project.name, description)?;
    tracing::info!(
        project_id = %project.id,
        task_id = %created.id,
        merged_prs = merged_prs.len(),
        "project-postmortem sweep: scheduled design postmortem",
    );

    // The new task is `autostart = true, status = todo`; give it an
    // execution row now rather than waiting for the next unrelated
    // invalidation to trigger `reconcile_product_executions`.
    if let Err(err) = work_db.reconcile_product_executions(&product.id) {
        tracing::warn!(
            product_id = %product.id,
            task_id = %created.id,
            ?err,
            "project-postmortem sweep: failed to reconcile executions after scheduling postmortem",
        );
    }

    Ok(EvalOutcome::Scheduled)
}

/// Parse a `completed_at`-shaped column (decimal epoch-seconds string) into
/// an `i64` for numeric (not lexical) comparison.
fn epoch_secs(value: Option<&str>) -> Option<i64> {
    value.and_then(|s| s.parse::<i64>().ok())
}

/// Compose the postmortem task's `description` — the remit brief that
/// surfaces to the worker via `runner::work_item_details`'s `- details:`
/// block. Lists every PR the review must cover so the worker doesn't have
/// to rediscover the wave from scratch.
fn compose_postmortem_brief(project: &Project, merged_prs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Design postmortem for project \"{}\": review the PRs merged since the last postmortem (or since the project began, if this is the first) and update the project's design doc to reflect what actually shipped — decisions that diverged, scope added or dropped, and contracts that evolved during implementation. Also flag any uncompleted work the review surfaces (see the required structured-output section in your instructions below) so the engine can schedule it.\n\n",
        project.name
    ));
    if let Some(path) = project.design_doc_path.as_deref().filter(|p| !p.is_empty()) {
        out.push_str(&format!("Design doc: `{path}`\n"));
    }
    out.push_str("Merged PRs to review:\n");
    for (name, url) in merged_prs {
        out.push_str(&format!("- {name}: {url}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_support::*;
    use crate::work::{CreateProjectInput, CreateTaskInput, SetProjectDesignDocInput, Task, WorkItemPatch};

    fn set_design_doc(db: &WorkDb, project_id: &str) {
        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project_id.to_owned(),
            design_doc_path: Some("docs/designs/alpha.md".to_owned()),
            ..Default::default()
        })
        .unwrap();
    }

    fn create_project_no_seed(db: &WorkDb, product_id: &str, name: &str) -> boss_protocol::Project {
        db.create_project(
            CreateProjectInput::builder()
                .product_id(product_id)
                .name(name)
                .no_design_task(true)
                .build(),
        )
        .unwrap()
    }

    fn create_done_project_task(db: &WorkDb, product_id: &str, project_id: &str, name: &str, pr_url: &str) -> Task {
        let task = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(product_id)
                    .project_id(project_id)
                    .name(name)
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                pr_url: Some(pr_url.to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        match db.get_work_item(&task.id).unwrap() {
            boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t,
            other => panic!("expected a task, got {other:?}"),
        }
    }

    /// No design doc set → skipped even though every task is terminal.
    #[tokio::test]
    async fn skips_project_without_design_doc() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        create_done_project_task(&db, &product_id, &project.id, "impl", "https://github.com/o/r/pull/1");

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.projects_evaluated, 0);
        assert_eq!(outcome.postmortems_created, 0);
    }

    /// First wave: design doc set, one project_task done with a PR, no
    /// prior postmortem → schedules one.
    #[tokio::test]
    async fn schedules_postmortem_on_first_completed_wave() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        create_done_project_task(&db, &product_id, &project.id, "impl", "https://github.com/o/r/pull/1");

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 1);

        let tasks = db.list_tasks(&product_id, Some(&project.id), None, false).unwrap();
        // list_tasks only returns project_task/design/investigation —
        // the postmortem itself must never appear here (rule 2).
        assert!(tasks.iter().all(|t| t.kind != TaskKind::DesignPostmortem));

        let last = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();
        assert_eq!(last.status, TaskStatus::Todo);
        assert!(last.description.contains("pull/1"));
    }

    /// Non-terminal task present → trigger count is non-zero, no schedule.
    #[tokio::test]
    async fn does_not_schedule_while_tasks_are_open() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        db.create_task(
            CreateTaskInput::builder()
                .product_id(&product_id)
                .project_id(&project.id)
                .name("still open")
                .build(),
        )
        .unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 0);
    }

    /// Dedup gate: an open postmortem already exists → skip even though
    /// the trigger count is zero.
    #[tokio::test]
    async fn dedup_gate_skips_while_postmortem_open() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        create_done_project_task(&db, &product_id, &project.id, "impl", "https://github.com/o/r/pull/1");

        db.create_design_postmortem(&product_id, &project.id, &project.name, "existing".to_owned())
            .unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(
            outcome.postmortems_created, 0,
            "must not double-schedule while one is open"
        );
    }

    /// Re-armable: once the existing postmortem completes AND a fresh wave
    /// of implementation work lands and drains, a second postmortem is
    /// scheduled.
    #[tokio::test]
    async fn reschedules_after_new_wave_following_completed_postmortem() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        let task1 = create_done_project_task(&db, &product_id, &project.id, "impl 1", "https://github.com/o/r/pull/1");
        // `completed_at` is epoch-seconds resolution, and this test's two
        // task completions and one postmortem completion all happen
        // in-process with no real elapsed time between them — force
        // strictly increasing timestamps rather than relying on real
        // wall-clock ordering, which would make the "since last postmortem"
        // cutoff comparison racy (see `force_completed_at_for_test`'s doc
        // comment).
        db.force_completed_at_for_test(&task1.id, 1_000).unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 1);
        let first_pm = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();

        // Complete the postmortem itself, strictly after task1.
        db.update_work_item(
            &first_pm.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                pr_url: Some("https://github.com/o/r/pull/2".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        db.force_completed_at_for_test(&first_pm.id, 2_000).unwrap();

        // No new work: must not re-fire.
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 0, "zero net new work must not re-trigger");

        // A fresh wave lands and drains, strictly after the postmortem.
        let task2 = create_done_project_task(&db, &product_id, &project.id, "impl 2", "https://github.com/o/r/pull/3");
        db.force_completed_at_for_test(&task2.id, 3_000).unwrap();
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(
            outcome.postmortems_created, 1,
            "a fresh completed wave must re-arm the trigger"
        );

        let second_pm = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();
        assert_ne!(second_pm.id, first_pm.id);
        assert!(second_pm.description.contains("pull/3"));
        assert!(
            !second_pm.description.contains("pull/1"),
            "the already-reviewed PR must not be re-listed in the new postmortem's brief"
        );
    }

    /// The postmortem task's own completion must never count as
    /// "implementation work" that re-arms the trigger, and the seed
    /// `design` task's completion must not either (rule 2 / precondition).
    #[tokio::test]
    async fn design_and_postmortem_kinds_never_feed_the_precondition() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        // Real design task this time (not the no-seed helper) so its
        // completion is on the record as `kind = design`.
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(&product_id)
                    .name("Alpha")
                    .build(),
            )
            .unwrap();
        set_design_doc(&db, &project.id);
        let design_task = db
            .list_tasks(&product_id, Some(&project.id), None, false)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == TaskKind::Design)
            .unwrap();
        db.update_work_item(
            &design_task.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                pr_url: Some("https://github.com/o/r/pull/1".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(
            outcome.postmortems_created, 0,
            "design task completing alone (no project_task/investigation work) must not trigger a postmortem"
        );
    }
}
