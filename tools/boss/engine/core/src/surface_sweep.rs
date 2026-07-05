//! Periodic post-merge surface sweep (incident-002 remediation P6).
//!
//! Incident 002 (`tools/boss/docs/postmortems/incident-002-merge-conflict-\
//! deletion-blessed-by-review.md`): a design-specified planner surface was
//! deleted during a forward-port, every gate passed it, and the loss reached
//! `main` silently. It was only caught **days later** by an ad-hoc gap-analysis
//! that compared the design's §Surfacing inventory against shipped code.
//!
//! P6 is the backstop that assumes A–D (the preservation brief, the deletion
//! tripwire, the citation check, the removal-forward comment) will sometimes
//! all fail. It does not *prevent* a loss; it **bounds detection latency** from
//! "days later, by luck" to "next sweep". Per the postmortem, a minimal
//! prompt-driven implementation is acceptable — a periodic sweep task the
//! engine schedules that instructs a worker to compare the design's §Surfacing
//! inventory against shipped code — rather than heavy bespoke per-project
//! infrastructure.
//!
//! ## What this loop does
//!
//! On a conservative cadence ([`SURFACE_SWEEP_INTERVAL_SECS`], daily), for
//! every project that has a design-doc pointer (`design_doc_path`), it **stages**
//! (autostart = false — no surprise worker spend) one surface-sweep
//! `investigation` task carrying the sweep brief as its description. The
//! investigation runs through the ordinary investigation dispatch path (no new
//! execution kind). Dedup is stable and status-based via
//! [`WorkDb::has_open_surface_sweep_for_project`] keyed on the
//! `surface-sweep:` `created_via` provenance marker, so at most one open sweep
//! exists per project regardless of cadence.

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{CREATED_VIA_SURFACE_SWEEP_PREFIX, CreateInvestigationInput};

use crate::work::WorkDb;

/// Interval between surface-sweep passes. Daily: the sweep is a
/// detection-latency backstop, not a fast path, and staging one investigation
/// per design-doc project more often than that is needless churn.
pub const SURFACE_SWEEP_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Counters from one sweep pass.
#[derive(Debug, Default)]
pub struct SurfaceSweepOutcome {
    /// Projects with a design-doc pointer examined this pass.
    pub projects_with_design_doc: usize,
    /// Surface-sweep investigations staged this pass (a project with an already
    /// open sweep is skipped and not counted).
    pub sweeps_staged: usize,
}

impl crate::sweep_loop::SweepOutcome for SurfaceSweepOutcome {
    fn has_activity(&self) -> bool {
        self.sweeps_staged > 0
    }

    fn log(&self) {
        tracing::info!(
            projects_with_design_doc = self.projects_with_design_doc,
            sweeps_staged = self.sweeps_staged,
            "surface sweep: staged design-surface verification investigation(s)",
        );
    }
}

/// Compose the surface-sweep investigation brief (the task description). The
/// investigation directive wraps this with the standard "produce a doc, PR
/// only, no code" framing; this text is the sweep-specific instruction.
pub fn compose_surface_sweep_brief(project_name: &str, design_doc_path: &str) -> String {
    format!(
        "Surface sweep (incident-002 P6 backstop) for project **{project_name}**.\n\
         \n\
         Goal: verify that every user-facing surface the design specifies still \
         exists in shipped code — catch silent regressions where a surface was \
         deleted or downgraded to a placeholder. (In incident-002 a merged \
         planner badge was deleted during a forward-port and stayed gone for \
         days before an ad-hoc analysis noticed.)\n\
         \n\
         Steps:\n\
         1. Open the project's design doc: `{design_doc_path}` (if the path is \
         stale, resolve the current one via `boss project` / the repo).\n\
         2. Find its **§Surfacing** section — the heading that enumerates the \
         concrete surfaces (pages, components, endpoints, badges, flags) the \
         design ships.\n\
         3. For EACH surface it enumerates, check shipped code on `main` and \
         classify it:\n   \
         - **PRESENT** — the surface exists and matches the design's \
         description.\n   \
         - **REGRESSED** — it exists but was downgraded to a placeholder / \
         hardcoded stub / lost its described behaviour (e.g. a bucket badge \
         replaced by a static \"Insights\" chip).\n   \
         - **ABSENT** — it is gone entirely (file 404s / no call site).\n\
         4. For every REGRESSED or ABSENT surface, cite the exact file(s)/\
         line(s), the design section that specifies it, and — if you can find \
         it — the commit/PR that removed it.\n\
         \n\
         Deliverable: a findings doc with a table (surface | design §ref | \
         status | evidence). If every surface is PRESENT, say so explicitly. Do \
         NOT modify product code — this is an investigation; restoring a \
         regressed surface is a separate task the operator files from your \
         findings.",
    )
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`. Fires
/// immediately on spawn so a design-doc project that gained a regression while
/// the engine was down gets a sweep staged at boot.
pub fn spawn_loop(work_db: Arc<WorkDb>, interval: Duration) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        async move { run_one_pass(work_db.as_ref()) }
    })
}

/// Run a single surface-sweep pass: for every design-doc project lacking an
/// open surface-sweep investigation, stage one. Returns per-pass counters.
///
/// Every failure mode (list error, create error, duplicate) is logged and
/// skipped — a backstop sweep must never crash the engine, and a project it
/// misses this pass is retried next pass.
pub fn run_one_pass(work_db: &WorkDb) -> SurfaceSweepOutcome {
    let mut outcome = SurfaceSweepOutcome::default();

    let products = match work_db.list_products() {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(?err, "surface sweep: failed to list products; skipping pass");
            return outcome;
        }
    };

    for product in products {
        let projects = match work_db.list_projects(&product.id, None) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(product_id = %product.id, ?err, "surface sweep: failed to list projects; skipping product");
                continue;
            }
        };
        for project in projects {
            let Some(design_doc_path) = project
                .design_doc_path
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
            else {
                continue; // no design doc → nothing to sweep against
            };
            outcome.projects_with_design_doc += 1;

            match work_db.has_open_surface_sweep_for_project(&project.id) {
                Ok(true) => continue, // one open sweep already staged
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(project_id = %project.id, ?err, "surface sweep: dedup check failed; skipping project");
                    continue;
                }
            }

            let brief = compose_surface_sweep_brief(&project.name, design_doc_path);
            let name = format!("Surface sweep: {}", project.name);
            let created_via = format!("{CREATED_VIA_SURFACE_SWEEP_PREFIX}{}", project.id);
            match work_db.create_investigation(
                CreateInvestigationInput::builder()
                    .product_id(product.id.clone())
                    .project_id(project.id.clone())
                    .name(name)
                    .description(brief)
                    .created_via(created_via)
                    // Staged, not auto-dispatched: the operator gates the spend.
                    .autostart(false)
                    .build(),
            ) {
                Ok(_) => {
                    outcome.sweeps_staged += 1;
                    tracing::info!(
                        project_id = %project.id,
                        product_id = %product.id,
                        "surface sweep: staged design-surface verification investigation",
                    );
                }
                Err(err) => {
                    // Most likely a same-name recent duplicate slipped past the
                    // status-based dedup (a benign race); log and move on.
                    tracing::debug!(project_id = %project.id, ?err, "surface sweep: skipped staging (likely duplicate)");
                }
            }
        }
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::work::TaskKind;
    use boss_protocol::{CreateProjectInput, SetProjectDesignDocInput};
    use tempfile::TempDir;

    fn open_db() -> (TempDir, WorkDb) {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    fn project_with_design_doc(db: &WorkDb, product_id: &str, name: &str, path: &str) -> String {
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product_id)
                    .name(name)
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();
        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            unset: false,
            design_doc_branch: None,
            design_doc_path: Some(path.to_owned()),
            design_doc_repo_remote_url: None,
        })
        .unwrap();
        project.id
    }

    #[test]
    fn brief_names_project_and_surfacing_section() {
        let brief = compose_surface_sweep_brief("Tournament Recs", "tools/boss/docs/designs/tre.md");
        assert!(brief.contains("Tournament Recs"));
        assert!(brief.contains("§Surfacing"));
        assert!(brief.contains("tools/boss/docs/designs/tre.md"));
        assert!(brief.contains("REGRESSED"));
        assert!(brief.contains("ABSENT"));
        // Must be an investigation (no code changes).
        assert!(brief.contains("Do NOT modify product code"));
    }

    #[test]
    fn stages_one_sweep_per_design_doc_project_and_dedups() {
        let (_dir, db) = open_db();
        let product = create_test_product_with_repo(&db, "P", Some("https://github.com/o/r"));
        let project_id = project_with_design_doc(&db, &product.id, "TRE", "docs/designs/tre.md");

        // First pass stages exactly one sweep.
        let out = run_one_pass(&db);
        assert_eq!(out.projects_with_design_doc, 1);
        assert_eq!(out.sweeps_staged, 1);

        // The staged task is an investigation scoped to the project, carrying
        // the surface-sweep provenance marker and staged (not autostarted).
        let staged: Vec<_> = db
            .list_tasks(&product.id, Some(&project_id), None, false)
            .unwrap()
            .into_iter()
            .filter(|t| t.kind == TaskKind::Investigation)
            .collect();
        assert_eq!(staged.len(), 1, "exactly one sweep investigation staged");
        assert!(staged[0].created_via.starts_with("surface-sweep:"));
        assert!(!staged[0].autostart, "sweep must be staged, not auto-dispatched");

        // Second pass stages nothing — the open sweep dedups.
        let out2 = run_one_pass(&db);
        assert_eq!(out2.projects_with_design_doc, 1);
        assert_eq!(out2.sweeps_staged, 0, "an open sweep must not be re-staged");
    }

    #[test]
    fn skips_projects_without_a_design_doc() {
        let (_dir, db) = open_db();
        let product = create_test_product_with_repo(&db, "P", Some("https://github.com/o/r"));
        db.create_project(
            CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name("no-doc")
                .no_design_task(true)
                .build(),
        )
        .unwrap();

        let out = run_one_pass(&db);
        assert_eq!(
            out.projects_with_design_doc, 0,
            "a project with no design doc is not swept"
        );
        assert_eq!(out.sweeps_staged, 0);
    }

    #[test]
    fn dedup_helper_tracks_open_sweep() {
        let (_dir, db) = open_db();
        let product = create_test_product_with_repo(&db, "P", Some("https://github.com/o/r"));
        let project_id = project_with_design_doc(&db, &product.id, "TRE", "docs/designs/tre.md");
        assert!(!db.has_open_surface_sweep_for_project(&project_id).unwrap());
        run_one_pass(&db);
        assert!(db.has_open_surface_sweep_for_project(&project_id).unwrap());
    }
}
