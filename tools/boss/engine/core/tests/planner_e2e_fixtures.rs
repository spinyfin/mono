//! End-to-end auto-populate fixtures (design task 11 of
//! `auto-populate-project-tasks-on-design-pr-merge.md`, project P783).
//!
//! These tests drive the *deterministic* half of the auto-populate pipeline —
//! `Populator::run` → validation → `Materializer::apply` → audit/surface — over
//! **real, merged design docs** used as ground-truth fixtures, and assert the
//! materialized task set and dependency edges match what a human coordinator
//! would (and did) produce from those docs.
//!
//! ## Why the Planner (the LLM step) is not called here
//!
//! The Planner (`Planner::plan`) is a live Anthropic API call — it cannot run
//! hermetically in a test, and its output is non-deterministic. The design
//! separates *infer* (LLM) from *apply* (engine) precisely so the apply half is
//! testable; that is what these fixtures exercise. Each fixture therefore
//! supplies the **ground-truth `PlannerOutput`** a coordinator would extract
//! from the doc's own "implementation task breakdown" section (the docs even
//! state their per-task effort and `Depends on:` edges), and the test asserts
//! the engine reproduces exactly that graph — every task with its kind, effort,
//! staged state, and provenance tag, and every dependency edge, no more and no
//! fewer. The one hermetically-testable slice of the Planner itself — building
//! the request/prompt from a `PlannerInput` — is covered against real doc
//! content in [`planner_prompt_embeds_real_doc_and_context`].
//!
//! ## Fixtures
//!
//! - **P707** = `unify-pr-remediation-on-revisions.md` (a confirmed mapping via
//!   the design-doc cross-references) — a mostly-linear phase chain.
//! - `auto-populate-project-tasks-on-design-pr-merge.md` (this project, P783) —
//!   an 11-task fan-out+integration DAG (the doc notes the Planner-generated
//!   version of this very project "would wire these same edges").
//! - `notification-dedup-scoring.md` — a 10-task rich DAG with parallel depths.
//!
//! The design names P707/P757/P754 as the docs "populated this way this week".
//! P757 and P754 are Boss-DB project ids that do not appear anywhere in the
//! repository, so two representative real design docs with the same breakdown
//! shape stand in for them; P707 is used verbatim.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;

use boss_protocol::{
    Confidence, CreateProductInput, CreateProjectInput, CreateTaskInput, DependencyDirection, EffortLevel,
    ListDependenciesInput, PLANNER_OUTCOME_STAGED, PlannerInput, PlannerOutput, ProductContext, ProjectContext,
    ProposedEdge, ProposedTask, SetProjectDesignDocInput, TaskBrief, TaskKind,
};

use boss_engine::doc_fetcher::DocFetchOutcome;
use boss_engine::materializer::Materializer;
use boss_engine::planner::{PLANNER_MODEL, PlannerOutcome, build_request_body, build_user_prompt};
use boss_engine::planner_validation::{ValidationResult, validate};
use boss_engine::populator::{DEFAULT_MAX_TASKS, PopulateContext, PopulateOutcome, Populator, PopulatorSteps};
use boss_engine::work::WorkDb;

// ---------------------------------------------------------------------------
// Ground-truth fixture docs (verbatim breakdown excerpts, see tests/fixtures)
// ---------------------------------------------------------------------------

const DOC_P707: &str = include_str!("fixtures/planner/p707_unify_pr_remediation.md");
const DOC_AUTO_POPULATE: &str = include_str!("fixtures/planner/p783_auto_populate.md");
const DOC_NOTIF_DEDUP: &str = include_str!("fixtures/planner/notification_dedup_scoring.md");
const DOC_NO_BREAKDOWN: &str = include_str!("fixtures/planner/pure_rationale_no_breakdown.md");

// ---------------------------------------------------------------------------
// Fixture model: a hand-authored task graph transcribed from a real doc
// ---------------------------------------------------------------------------

/// One proposed task in a fixture's ground-truth graph.
#[derive(Clone)]
struct T {
    handle: &'static str,
    name: &'static str,
    kind: TaskKind,
    effort: EffortLevel,
}

/// One dependency edge in a fixture's ground-truth graph, by handle.
#[derive(Clone)]
struct E {
    dependent: &'static str,
    prerequisite: &'static str,
}

/// A complete fixture: a real doc plus the task graph a coordinator extracts.
struct Fixture {
    label: &'static str,
    doc: &'static str,
    tasks: Vec<T>,
    edges: Vec<E>,
}

fn t(handle: &'static str, name: &'static str, kind: TaskKind, effort: EffortLevel) -> T {
    T {
        handle,
        name,
        kind,
        effort,
    }
}

fn e(dependent: &'static str, prerequisite: &'static str) -> E {
    E {
        dependent,
        prerequisite,
    }
}

/// The `[effort-classification]` audit line the Planner appends per task, in
/// the format the coordinator/engine emit. The fixtures embed it in each task's
/// description so the test can assert the Materializer preserves it verbatim.
fn effort_line(effort: EffortLevel) -> String {
    let level = match effort {
        EffortLevel::Trivial => "trivial",
        EffortLevel::Small => "small",
        EffortLevel::Medium => "medium",
        EffortLevel::Large => "large",
        EffortLevel::Max => "max",
    };
    format!("[effort-classification] level=`{level}` matched-rule=`fixture` reasons=\"ground-truth fixture\"")
}

/// Build a `PlannerOutput` from a fixture graph. Each task's description carries
/// its `[effort-classification]` line (as the real Planner emits), and
/// `effort_audit` collects one line per task.
fn output_from(tasks: &[T], edges: &[E], breakdown_found: bool, confidence: Confidence) -> PlannerOutput {
    let proposed_tasks: Vec<ProposedTask> = tasks
        .iter()
        .enumerate()
        .map(|(i, spec)| ProposedTask {
            handle: spec.handle.to_owned(),
            name: spec.name.to_owned(),
            description: format!("Implement: {}.\n\n{}", spec.name, effort_line(spec.effort)),
            kind: spec.kind.clone(),
            effort: spec.effort,
            ordinal: i as i64,
        })
        .collect();
    let proposed_edges: Vec<ProposedEdge> = edges
        .iter()
        .map(|edge| ProposedEdge {
            dependent: edge.dependent.to_owned(),
            prerequisite: edge.prerequisite.to_owned(),
        })
        .collect();
    let effort_audit: Vec<String> = tasks.iter().map(|s| effort_line(s.effort)).collect();
    PlannerOutput {
        tasks: proposed_tasks,
        edges: proposed_edges,
        confidence,
        breakdown_found,
        notes: "Ground-truth fixture transcribed from the merged design doc.".to_owned(),
        effort_audit,
    }
}

impl Fixture {
    fn output(&self) -> PlannerOutput {
        output_from(&self.tasks, &self.edges, true, Confidence::High)
    }

    /// handle → task name, for translating handle-keyed edges to DB-name checks.
    fn handle_names(&self) -> HashMap<&'static str, &'static str> {
        self.tasks.iter().map(|task| (task.handle, task.name)).collect()
    }
}

// ---------------------------------------------------------------------------
// The three real-doc ground-truth graphs
// ---------------------------------------------------------------------------

/// P707 — `unify-pr-remediation-on-revisions.md`, "Implementation phases".
/// Six phases: provenance (1) and directive fragments (2) are independent
/// roots; the conflict (3) and CI (4) producer cutovers each need both; the
/// dormant-path removal (5) needs 3+4; the stretch auto-rebase producer (6) is
/// a separate effort with no edge. The doc states no per-task effort, so these
/// are the effort estimates the coordinator/heuristic would assign.
fn fixture_p707() -> Fixture {
    use EffortLevel::*;
    use TaskKind::ProjectTask;
    Fixture {
        label: "P707 unify-pr-remediation",
        doc: DOC_P707,
        tasks: vec![
            t("provenance", "Provenance + reverse link", ProjectTask, Medium),
            t("fragments", "Injected directive fragments", ProjectTask, Small),
            t("conflict", "Conflict producer cutover", ProjectTask, Large),
            t("ci", "CI producer cutover", ProjectTask, Large),
            t("remove", "Remove the dormant bespoke paths", ProjectTask, Medium),
            t(
                "autorebase",
                "Fold auto-rebase in as a fourth producer",
                ProjectTask,
                Medium,
            ),
        ],
        edges: vec![
            e("conflict", "provenance"),
            e("conflict", "fragments"),
            e("ci", "provenance"),
            e("ci", "fragments"),
            e("remove", "conflict"),
            e("remove", "ci"),
        ],
    }
}

/// auto-populate (P783) — the 11-task breakdown with the doc's explicit efforts
/// and `Depends on:` edges. The shared contract (1) is the root; Planner (3),
/// Materializer (5), and validation (6) fan out; the Populator (7) integrates
/// them; CLI (9), app (10), and these very tests (11) layer on top.
fn fixture_auto_populate() -> Fixture {
    use EffortLevel::*;
    use TaskKind::ProjectTask;
    Fixture {
        label: "P783 auto-populate",
        doc: DOC_AUTO_POPULATE,
        tasks: vec![
            t("contract", "Protocol: Planner contract types", ProjectTask, Small),
            t(
                "runs_table",
                "Engine: planner_runs table + migration",
                ProjectTask,
                Medium,
            ),
            t("planner", "Engine: the Planner", ProjectTask, Large),
            t("doc_fetch", "Engine: live doc fetch", ProjectTask, Small),
            t("materializer", "Engine: deterministic Materializer", ProjectTask, Large),
            t("validation", "Engine: validation layer", ProjectTask, Medium),
            t("populator", "Engine: the Populator + trigger hook", ProjectTask, Large),
            t(
                "surfacing",
                "Engine: attention-item + event surfacing",
                ProjectTask,
                Medium,
            ),
            t("cli", "CLI: operator entry points", ProjectTask, Medium),
            t("app", "macOS app: review/release/undo surface", ProjectTask, Medium),
            t("tests", "Tests: end-to-end fixtures", ProjectTask, Large),
        ],
        edges: vec![
            e("planner", "contract"),
            e("materializer", "contract"),
            e("materializer", "runs_table"),
            e("validation", "contract"),
            e("populator", "runs_table"),
            e("populator", "planner"),
            e("populator", "doc_fetch"),
            e("populator", "materializer"),
            e("populator", "validation"),
            e("surfacing", "populator"),
            e("cli", "planner"),
            e("cli", "materializer"),
            e("cli", "validation"),
            e("cli", "populator"),
            e("app", "surfacing"),
            e("app", "cli"),
            e("tests", "materializer"),
            e("tests", "validation"),
            e("tests", "populator"),
        ],
    }
}

/// notification-dedup-scoring — the 10-item breakdown (note the split `4`/`4a`)
/// with the doc's explicit efforts and `Depends on:` edges. Schema (1), flags
/// (2), and the dedup-decision substrate (3) are roots; the two prefilters
/// (4, 4a) sit at depth 1; the creation path (5), sweep (7), edits (6) and UI
/// (8) fan out; the e2e tests (9) integrate.
fn fixture_notif_dedup() -> Fixture {
    use EffortLevel::*;
    use TaskKind::ProjectTask;
    Fixture {
        label: "notification-dedup-scoring",
        doc: DOC_NOTIF_DEDUP,
        tasks: vec![
            t(
                "schema",
                "Schema + score field + provenance ledger",
                ProjectTask,
                Medium,
            ),
            t("flags", "Feature-flag plumbing", ProjectTask, Trivial),
            t(
                "substrate",
                "Structured-output dedup-decision substrate + contract",
                ProjectTask,
                Large,
            ),
            t(
                "prefilter",
                "Comparison-set prefilter + rendering helpers",
                ProjectTask,
                Medium,
            ),
            t(
                "taxonomy",
                "Taxonomy prefilter + WorkItemBrief rendering",
                ProjectTask,
                Small,
            ),
            t("creation", "Dedup-at-creation path", ProjectTask, Large),
            t(
                "edits",
                "Canonical-edit-on-merge (bounded + recorded)",
                ProjectTask,
                Medium,
            ),
            t("sweep", "Startup sweep", ProjectTask, Large),
            t("ui", "UI priority surfacing", ProjectTask, Medium),
            t("tests", "Tests: end-to-end dedup + idempotency", ProjectTask, Large),
        ],
        edges: vec![
            e("prefilter", "substrate"),
            e("taxonomy", "substrate"),
            e("taxonomy", "flags"),
            e("creation", "schema"),
            e("creation", "flags"),
            e("creation", "substrate"),
            e("creation", "prefilter"),
            e("creation", "taxonomy"),
            e("edits", "schema"),
            e("edits", "creation"),
            e("sweep", "schema"),
            e("sweep", "flags"),
            e("sweep", "substrate"),
            e("sweep", "prefilter"),
            e("sweep", "taxonomy"),
            e("ui", "schema"),
            e("tests", "creation"),
            e("tests", "sweep"),
        ],
    }
}

fn all_fixtures() -> Vec<Fixture> {
    vec![fixture_p707(), fixture_auto_populate(), fixture_notif_dedup()]
}

// ---------------------------------------------------------------------------
// Test harness: in-memory DB, seeding, and injected network steps
// ---------------------------------------------------------------------------

fn open() -> WorkDb {
    WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap()
}

/// Create product + project + the auto-created `kind=design` task, with the
/// project's design-doc pointer set (as `on_design_pr_merged` would leave it).
/// Returns `(product_id, project_id, design_task_id)`.
fn seed(db: &WorkDb) -> (String, String, String) {
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@github.com:owner/repo.git")
                .build(),
        )
        .unwrap();
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name("Alpha")
                .goal("build it")
                .build(),
        )
        .unwrap();
    let design_id = db
        .list_tasks(&product.id, Some(&project.id), None, false)
        .unwrap()
        .into_iter()
        .find(|task| task.kind == TaskKind::Design)
        .expect("project should have an auto-created design task")
        .id;
    db.set_project_design_doc(SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_path: Some("tools/boss/docs/designs/alpha.md".to_owned()),
        design_doc_branch: Some("main".to_owned()),
        design_doc_repo_remote_url: Some("git@github.com:owner/repo.git".to_owned()),
        unset: false,
    })
    .unwrap();
    (product.id, project.id, design_id)
}

fn ctx(product_id: &str, project_id: &str, design_id: &str) -> PopulateContext {
    PopulateContext {
        project_id: project_id.to_owned(),
        product_id: product_id.to_owned(),
        design_task_id: design_id.to_owned(),
        pr_url: "https://github.com/owner/repo/pull/1".to_owned(),
    }
}

/// Cloneable descriptors mapped to the real (non-Clone) outcome enums, so a
/// single fake can be reused across a double-fire.
enum FakeFetch {
    Content(String),
    Missing,
    Failed,
}
enum FakePlan {
    Success(PlannerOutput),
    NoApiKey,
}

struct FakeSteps {
    fetch: FakeFetch,
    plan: FakePlan,
}

#[async_trait]
impl PopulatorSteps for FakeSteps {
    async fn fetch_doc(&self, _repo: &str, _path: &str, _git_ref: &str) -> DocFetchOutcome {
        match &self.fetch {
            FakeFetch::Content(s) => DocFetchOutcome::Content(s.clone()),
            FakeFetch::Missing => DocFetchOutcome::DocMissing,
            FakeFetch::Failed => DocFetchOutcome::FetchFailed {
                reason: "simulated transient failure".to_owned(),
            },
        }
    }

    async fn plan(&self, _input: &PlannerInput) -> PlannerOutcome {
        match &self.plan {
            FakePlan::Success(out) => PlannerOutcome::Success(out.clone()),
            FakePlan::NoApiKey => PlannerOutcome::NoApiKey,
        }
    }
}

/// Steps that serve the given doc content and plan the given fixture graph.
fn steps_for(doc: &str, output: PlannerOutput) -> FakeSteps {
    FakeSteps {
        fetch: FakeFetch::Content(doc.to_owned()),
        plan: FakePlan::Success(output),
    }
}

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

/// Non-design tasks in the project, keyed by name.
fn materialized_tasks(db: &WorkDb, product_id: &str, project_id: &str) -> HashMap<String, boss_protocol::Task> {
    db.list_tasks(product_id, Some(project_id), None, false)
        .unwrap()
        .into_iter()
        .filter(|task| task.kind != TaskKind::Design)
        .map(|task| (task.name.clone(), task))
        .collect()
}

/// Count all `blocks` prerequisite edges across the project's tasks.
fn total_blocks_edges(db: &WorkDb, tasks: &HashMap<String, boss_protocol::Task>) -> usize {
    tasks
        .values()
        .map(|task| {
            db.list_dependencies_detailed(ListDependenciesInput {
                work_item: task.id.clone(),
                direction: Some(DependencyDirection::Prereqs),
            })
            .unwrap()
            .prerequisites
            .into_iter()
            .filter(|edge| edge.relation == "blocks")
            .count()
        })
        .sum()
}

/// Assert the materialized project graph matches the fixture exactly: the same
/// set of tasks (name, kind, effort, staged, effort-audit in description), the
/// same provenance tag, and the same dependency edges — no more, no fewer.
fn assert_graph_matches(db: &WorkDb, product_id: &str, project_id: &str, run_id: &str, fixture: &Fixture) {
    let tasks = materialized_tasks(db, product_id, project_id);
    assert_eq!(
        tasks.len(),
        fixture.tasks.len(),
        "[{}] task count mismatch: {:?}",
        fixture.label,
        tasks.keys().collect::<Vec<_>>()
    );

    for spec in &fixture.tasks {
        let task = tasks
            .get(spec.name)
            .unwrap_or_else(|| panic!("[{}] missing task {:?}", fixture.label, spec.name));
        assert_eq!(task.kind, spec.kind, "[{}] {:?} kind", fixture.label, spec.name);
        assert_eq!(
            task.effort_level,
            Some(spec.effort),
            "[{}] {:?} effort",
            fixture.label,
            spec.name
        );
        assert!(!task.autostart, "[{}] {:?} must be staged", fixture.label, spec.name);
        assert!(
            task.description.contains("[effort-classification]"),
            "[{}] {:?} keeps its effort-classification line",
            fixture.label,
            spec.name
        );
    }

    // Provenance: every created task (and only those) is tagged with the run.
    let tagged: HashSet<String> = db.list_task_ids_for_planner_run(run_id).unwrap().into_iter().collect();
    let expected_ids: HashSet<String> = tasks.values().map(|task| task.id.clone()).collect();
    assert_eq!(
        tagged, expected_ids,
        "[{}] every staged task is tagged with the planner run",
        fixture.label
    );

    // Edges: exact count, then each expected edge present as a `blocks` edge.
    assert_eq!(
        total_blocks_edges(db, &tasks),
        fixture.edges.len(),
        "[{}] total edge count",
        fixture.label
    );
    let handle_names = fixture.handle_names();
    for edge in &fixture.edges {
        let dep_name = handle_names[edge.dependent];
        let pre_name = handle_names[edge.prerequisite];
        let dep_id = &tasks[dep_name].id;
        let detail = db
            .list_dependencies_detailed(ListDependenciesInput {
                work_item: dep_id.clone(),
                direction: Some(DependencyDirection::Prereqs),
            })
            .unwrap();
        assert!(
            detail
                .prerequisites
                .iter()
                .any(|peer| peer.name == pre_name && peer.relation == "blocks"),
            "[{}] edge {dep_name:?} depends on {pre_name:?} missing",
            fixture.label,
        );
    }
}

fn open_attention_count(db: &WorkDb, design_id: &str) -> usize {
    db.list_attention_items_for_work_item(design_id)
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == "auto_populate" && item.status == "open")
        .count()
}

// ---------------------------------------------------------------------------
// 1–3. Each real-doc fixture materializes its exact ground-truth graph
// ---------------------------------------------------------------------------

async fn run_fixture_and_assert(fixture: &Fixture) {
    let db = open();
    let (product_id, project_id, design_id) = seed(&db);

    let outcome = Populator::run(
        &db,
        &steps_for(fixture.doc, fixture.output()),
        &ctx(&product_id, &project_id, &design_id),
        DEFAULT_MAX_TASKS,
    )
    .await;

    assert_eq!(
        outcome,
        PopulateOutcome::Staged {
            created: fixture.tasks.len(),
            edges: fixture.edges.len(),
            low_confidence: false,
        },
        "[{}] outcome",
        fixture.label,
    );

    // The audit row records the run for the operator who could not watch it.
    let run = db.live_planner_run_for_project(&project_id).unwrap().unwrap();
    assert_eq!(run.outcome, PLANNER_OUTCOME_STAGED, "[{}] audit outcome", fixture.label);
    assert_eq!(
        run.model.as_deref(),
        Some(PLANNER_MODEL),
        "[{}] audit model",
        fixture.label
    );
    assert!(run.raw_output.is_some(), "[{}] raw output persisted", fixture.label);
    assert!(run.effort_audit.is_some(), "[{}] effort audit persisted", fixture.label);
    assert!(run.notes.is_some(), "[{}] notes persisted", fixture.label);

    // Exactly one operator-facing attention item.
    assert_eq!(
        open_attention_count(&db, &design_id),
        1,
        "[{}] attention item",
        fixture.label
    );

    assert_graph_matches(&db, &product_id, &project_id, &run.id, fixture);
}

#[tokio::test]
async fn p707_fixture_materializes_expected_graph() {
    run_fixture_and_assert(&fixture_p707()).await;
}

#[tokio::test]
async fn auto_populate_fixture_materializes_expected_graph() {
    run_fixture_and_assert(&fixture_auto_populate()).await;
}

#[tokio::test]
async fn notification_dedup_fixture_materializes_expected_graph() {
    run_fixture_and_assert(&fixture_notif_dedup()).await;
}

// ---------------------------------------------------------------------------
// 4. Fan-out preserves parallelism: independent roots stay unedged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fan_out_roots_have_no_prerequisites() {
    let db = open();
    let (product_id, project_id, design_id) = seed(&db);
    let fixture = fixture_auto_populate();
    Populator::run(
        &db,
        &steps_for(fixture.doc, fixture.output()),
        &ctx(&product_id, &project_id, &design_id),
        DEFAULT_MAX_TASKS,
    )
    .await;

    let tasks = materialized_tasks(&db, &product_id, &project_id);
    // contract / runs_table / doc_fetch are depth-0 roots — no prerequisites,
    // so the dispatcher can start them in parallel.
    for root in [
        "Protocol: Planner contract types",
        "Engine: planner_runs table + migration",
        "Engine: live doc fetch",
    ] {
        let detail = db
            .list_dependencies_detailed(ListDependenciesInput {
                work_item: tasks[root].id.clone(),
                direction: Some(DependencyDirection::Prereqs),
            })
            .unwrap();
        assert!(detail.prerequisites.is_empty(), "root {root:?} must be unedged");
    }
}

// ---------------------------------------------------------------------------
// 5. Direct Materializer::apply reproduces the fixture graph (no Populator)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn materializer_direct_apply_matches_fixture() {
    let db = open();
    let (product_id, project_id, _design_id) = seed(&db);
    let fixture = fixture_notif_dedup();

    // Claim a run row the way the Populator would, then apply directly.
    let run = db
        .claim_planner_run(boss_engine::work::ClaimPlannerRunInput {
            project_id: &project_id,
            product_id: &product_id,
            design_task_id: None,
            caller: "operator",
        })
        .unwrap()
        .unwrap();

    let result = Materializer::apply(&db, &project_id, &run.id, &fixture.output()).unwrap();
    assert_eq!(result.created.len(), fixture.tasks.len());
    assert!(result.skipped.is_empty());
    assert_eq!(result.edges_created, fixture.edges.len());

    assert_graph_matches(&db, &product_id, &project_id, &run.id, &fixture);
}

// ---------------------------------------------------------------------------
// 6. Idempotency under double-fire: the second trigger is a clean skip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn double_fire_is_idempotent_for_every_fixture() {
    for fixture in all_fixtures() {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        let ctx = ctx(&product_id, &project_id, &design_id);

        let first = Populator::run(&db, &steps_for(fixture.doc, fixture.output()), &ctx, DEFAULT_MAX_TASKS).await;
        assert!(
            matches!(first, PopulateOutcome::Staged { .. }),
            "[{}] first fire stages",
            fixture.label
        );

        let tasks_after_first = materialized_tasks(&db, &product_id, &project_id).len();
        let edges_after_first = total_blocks_edges(&db, &materialized_tasks(&db, &product_id, &project_id));

        // Second trigger for the same project (poller restart / concurrent
        // merge / manual retry). The live `planner_runs` row is the gate.
        let second = Populator::run(&db, &steps_for(fixture.doc, fixture.output()), &ctx, DEFAULT_MAX_TASKS).await;
        assert_eq!(
            second,
            PopulateOutcome::SkippedAlreadyPopulated,
            "[{}] second fire skips",
            fixture.label
        );

        // No duplicate tasks or edges accumulated, and still exactly one live run.
        assert_eq!(
            materialized_tasks(&db, &product_id, &project_id).len(),
            tasks_after_first,
            "[{}] no duplicate tasks",
            fixture.label
        );
        assert_eq!(
            total_blocks_edges(&db, &materialized_tasks(&db, &product_id, &project_id)),
            edges_after_first,
            "[{}] no duplicate edges",
            fixture.label
        );
        assert_eq!(
            db.list_planner_runs_for_project(&project_id).unwrap().len(),
            1,
            "[{}] one planner run",
            fixture.label
        );
        // Only one attention item — the skip does not re-raise one.
        assert_eq!(
            open_attention_count(&db, &design_id),
            1,
            "[{}] one attention item",
            fixture.label
        );
    }
}

// ---------------------------------------------------------------------------
// 7. No-breakdown path: a pure-rationale doc is a clean no-op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_breakdown_is_clean_no_op() {
    let db = open();
    let (product_id, project_id, design_id) = seed(&db);
    // A real coordinator (and the Planner) returns breakdown_found = false for
    // a doc with no enumerable task list.
    let output = output_from(&[], &[], false, Confidence::High);
    let outcome = Populator::run(
        &db,
        &steps_for(DOC_NO_BREAKDOWN, output),
        &ctx(&product_id, &project_id, &design_id),
        DEFAULT_MAX_TASKS,
    )
    .await;

    assert_eq!(outcome, PopulateOutcome::NoBreakdown);
    assert!(materialized_tasks(&db, &product_id, &project_id).is_empty());
    // no_breakdown is terminal, not a live outcome — the gate is released.
    assert!(db.live_planner_run_for_project(&project_id).unwrap().is_none());
    assert_eq!(open_attention_count(&db, &design_id), 1);
}

// ---------------------------------------------------------------------------
// 8. Cyclic path: a back-edge on a real graph is rejected whole
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cyclic_proposal_on_real_graph_is_rejected() {
    let db = open();
    let (product_id, project_id, design_id) = seed(&db);
    let fixture = fixture_auto_populate();

    // Take the real 11-task graph and inject a back-edge that closes a cycle:
    // `contract` (a root) now depends on `populator`, which transitively
    // depends on `contract` — an impossible ordering.
    let mut output = fixture.output();
    output.edges.push(ProposedEdge {
        dependent: "contract".to_owned(),
        prerequisite: "populator".to_owned(),
    });

    let outcome = Populator::run(
        &db,
        &steps_for(fixture.doc, output),
        &ctx(&product_id, &project_id, &design_id),
        DEFAULT_MAX_TASKS,
    )
    .await;

    assert_eq!(outcome, PopulateOutcome::RejectedBadGraph);
    // No partial graph: the rejection happens before any write.
    assert!(materialized_tasks(&db, &product_id, &project_id).is_empty());
    assert_eq!(open_attention_count(&db, &design_id), 1);
}

// ---------------------------------------------------------------------------
// 9. Over-cap path: a real graph above the cap is rejected, never truncated
// ---------------------------------------------------------------------------

#[tokio::test]
async fn over_cap_rejects_whole_real_graph() {
    let db = open();
    let (product_id, project_id, design_id) = seed(&db);
    let fixture = fixture_auto_populate(); // 11 tasks

    let outcome = Populator::run(
        &db,
        &steps_for(fixture.doc, fixture.output()),
        &ctx(&product_id, &project_id, &design_id),
        5, // cap below the fixture's task count
    )
    .await;

    assert_eq!(
        outcome,
        PopulateOutcome::RejectedTooMany {
            count: fixture.tasks.len(),
            max: 5,
        }
    );
    // Nothing is silently truncated — zero tasks created.
    assert!(materialized_tasks(&db, &product_id, &project_id).is_empty());
    assert_eq!(open_attention_count(&db, &design_id), 1);
}

// ---------------------------------------------------------------------------
// 10. Pre-seeded path: a project with an existing impl task is refused
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pre_seeded_project_is_refused() {
    let db = open();
    let (product_id, project_id, design_id) = seed(&db);
    // Operator hand-filed an implementation task before the design merged.
    db.create_task(
        CreateTaskInput::builder()
            .product_id(product_id.clone())
            .project_id(project_id.clone())
            .name("Hand-written task")
            .build(),
    )
    .unwrap();

    let fixture = fixture_auto_populate();
    let outcome = Populator::run(
        &db,
        &steps_for(fixture.doc, fixture.output()),
        &ctx(&product_id, &project_id, &design_id),
        DEFAULT_MAX_TASKS,
    )
    .await;

    assert_eq!(outcome, PopulateOutcome::SkippedPreSeeded { existing: 1 });
    // The planner's tasks were NOT merged in; only the pre-seeded one remains.
    let tasks = materialized_tasks(&db, &product_id, &project_id);
    assert_eq!(tasks.len(), 1);
    assert!(tasks.contains_key("Hand-written task"));
    assert_eq!(open_attention_count(&db, &design_id), 1);
    // The claimed row went terminal, releasing the gate for a `--force` replan.
    assert!(db.live_planner_run_for_project(&project_id).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// 11. Fetch-failure path: transient failure and a hard 404, both no-ops
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_failure_is_recorded_no_op() {
    let db = open();
    let (product_id, project_id, design_id) = seed(&db);
    let steps = FakeSteps {
        fetch: FakeFetch::Failed,
        plan: FakePlan::NoApiKey,
    };
    let outcome = Populator::run(
        &db,
        &steps,
        &ctx(&product_id, &project_id, &design_id),
        DEFAULT_MAX_TASKS,
    )
    .await;

    assert_eq!(outcome, PopulateOutcome::FetchFailed);
    assert!(materialized_tasks(&db, &product_id, &project_id).is_empty());
    assert_eq!(open_attention_count(&db, &design_id), 1);
    // Terminal outcome releases the gate so a later `boss project plan` retries.
    assert!(db.live_planner_run_for_project(&project_id).unwrap().is_none());
}

#[tokio::test]
async fn doc_missing_is_recorded_no_op() {
    let db = open();
    let (product_id, project_id, design_id) = seed(&db);
    let steps = FakeSteps {
        fetch: FakeFetch::Missing,
        plan: FakePlan::NoApiKey,
    };
    let outcome = Populator::run(
        &db,
        &steps,
        &ctx(&product_id, &project_id, &design_id),
        DEFAULT_MAX_TASKS,
    )
    .await;

    assert_eq!(outcome, PopulateOutcome::DocMissing);
    assert!(materialized_tasks(&db, &product_id, &project_id).is_empty());
    assert_eq!(open_attention_count(&db, &design_id), 1);
}

// ---------------------------------------------------------------------------
// 12. Planner-failure path: no API key degrades gracefully
// ---------------------------------------------------------------------------

#[tokio::test]
async fn planner_no_api_key_is_recorded_no_op() {
    let db = open();
    let (product_id, project_id, design_id) = seed(&db);
    let steps = FakeSteps {
        fetch: FakeFetch::Content(DOC_AUTO_POPULATE.to_owned()),
        plan: FakePlan::NoApiKey,
    };
    let outcome = Populator::run(
        &db,
        &steps,
        &ctx(&product_id, &project_id, &design_id),
        DEFAULT_MAX_TASKS,
    )
    .await;

    assert_eq!(outcome, PopulateOutcome::PlannerFailed);
    assert!(materialized_tasks(&db, &product_id, &project_id).is_empty());
    assert_eq!(open_attention_count(&db, &design_id), 1);
}

// ---------------------------------------------------------------------------
// 13. The hermetically-testable slice of the Planner: request/prompt building
// ---------------------------------------------------------------------------

#[tokio::test]
async fn planner_prompt_embeds_real_doc_and_context() {
    let db = open();
    let (product_id, project_id, _design_id) = seed(&db);
    let project = db.get_project(&project_id).unwrap();
    let product = db.get_product(&product_id).unwrap().unwrap();

    let input = PlannerInput::builder()
        .design_doc(DOC_AUTO_POPULATE)
        .design_doc_ref(boss_protocol::DocRef {
            repo_remote_url: "git@github.com:owner/repo.git".to_owned(),
            git_ref: "main".to_owned(),
            path: "tools/boss/docs/designs/auto-populate.md".to_owned(),
        })
        .project(ProjectContext {
            id: project.id.clone(),
            name: project.name.clone(),
            slug: project.slug.clone(),
            description: project.description.clone(),
            goal: project.goal.clone(),
        })
        .product(ProductContext {
            id: product.id.clone(),
            slug: product.slug.clone(),
            name: product.name.clone(),
            repo_remote_url: "git@github.com:owner/repo.git".to_owned(),
        })
        .existing_tasks(vec![TaskBrief {
            id: "task_existing".to_owned(),
            name: "Already here".to_owned(),
        }])
        .max_tasks(17)
        .build();

    // The user prompt embeds the real doc, the cap, and the existing-task hint.
    let prompt = build_user_prompt(&input);
    assert!(
        prompt.contains("Protocol: Planner contract types"),
        "prompt embeds the real design doc content"
    );
    assert!(prompt.contains("--- BEGIN DESIGN DOC"), "prompt frames the doc");
    assert!(prompt.contains("17"), "prompt states the task cap");
    assert!(
        prompt.contains("Already here"),
        "prompt lists existing task names to dedup"
    );
    assert!(prompt.contains("Alpha"), "prompt carries the project name");

    // The request body forces the structured-output tool call and pins model.
    let body = build_request_body(&input);
    assert_eq!(body["model"], PLANNER_MODEL);
    assert_eq!(body["tool_choice"]["type"], "tool");
    assert_eq!(body["tool_choice"]["name"], "emit_task_graph");
    let tool = &body["tools"][0];
    assert_eq!(tool["name"], "emit_task_graph");
    // The forced tool's input schema is the PlannerOutput contract.
    let schema = &tool["input_schema"];
    assert_eq!(schema["type"], "object");
    let required = schema["required"].as_array().unwrap();
    for field in [
        "tasks",
        "edges",
        "confidence",
        "breakdown_found",
        "notes",
        "effort_audit",
    ] {
        assert!(required.iter().any(|value| value == field), "schema requires {field:?}");
    }
}

// ---------------------------------------------------------------------------
// 14. Every real fixture's ground-truth graph is itself a valid proposal
// ---------------------------------------------------------------------------

#[test]
fn every_fixture_graph_validates() {
    for fixture in all_fixtures() {
        assert_eq!(
            validate(&fixture.output(), DEFAULT_MAX_TASKS),
            ValidationResult::Valid { low_confidence: false },
            "[{}] fixture graph must be a valid DAG within the cap",
            fixture.label,
        );
    }
}
