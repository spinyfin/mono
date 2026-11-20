//! Real shared-store commands and lease dispatch during a slow collection.
use super::*;
use crate::app::dispatch::run_with_dependencies;
use crate::app::store_gc::{Collection, StoreGcOutcome, gc_repo_store, record_store_gc_outcome, store_gc_attention};
use crate::cli::Cli;
use crate::command_runner::JjGate;
use crate::metadata::RepoRecord;
use crate::store::Store;
use clap::Parser;
use std::cell::Cell;
use std::sync::mpsc;
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

#[path = "store_gc_fixture.rs"]
mod fixture;

#[path = "store_gc_upgrade_tests.rs"]
mod upgrade;

#[derive(bon::Builder)]
#[builder(on(String, into))]
struct Runner {
    binary: PathBuf,
    config: PathBuf,
    data_dir: PathBuf,
    collecting: Option<mpsc::Sender<()>>,
    resume: Option<mpsc::Receiver<()>>,
    cut: Option<String>,
    collector_active: Option<Arc<AtomicBool>>,
    collection_start: Option<Arc<Barrier>>,
}

impl CommandRunner for Runner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        self.run_with_timeout(invocation, Duration::from_secs(30))
    }

    fn run_with_timeout(&self, invocation: &CommandInvocation, timeout: Duration) -> Result<String> {
        // The test controls its own store. Host process activity must not
        // skip the rewrite just because other tests also run jj.
        if invocation.program == "ps" {
            return Ok(String::new());
        }
        let reported_program = invocation.program.clone();
        let mut invocation = invocation.clone();
        if invocation.program == "jj" {
            if invocation.args.iter().any(|arg| arg == "gc")
                && let Some(tx) = &self.collecting
            {
                tx.send(()).unwrap();
                self.resume
                    .as_ref()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(20))
                    .unwrap();
            }
            invocation.program = self.binary.display().to_string();
            invocation
                .env
                .push(("JJ_CONFIG".into(), self.config.display().to_string()));
            invocation
                .env
                .push(("CUBE_DATA_DIR".into(), self.data_dir.display().to_string()));
        }
        let start = Instant::now();
        let collecting = invocation.args.iter().any(|arg| arg == "gc");
        if collecting && let Some(active) = &self.collector_active {
            active.store(true, Ordering::SeqCst);
        }
        if collecting && let Some(start) = &self.collection_start {
            // Let the first live lease observe the active collector before
            // both launch their commands, even with a tiny view store.
            start.wait();
            start.wait();
        }
        let output = RealCommandRunner.run_with_timeout(&invocation, timeout);
        if collecting && let Some(active) = &self.collector_active {
            active.store(false, Ordering::SeqCst);
        }
        // Wall-clock end times place a command against the collector's own.
        eprintln!(
            "[{:.3}] scratch command {:?} in {}: {:?}, success={}",
            std::time::UNIX_EPOCH.elapsed().unwrap_or_default().as_secs_f64(),
            invocation.args,
            invocation.cwd.file_name().unwrap_or_default().to_string_lossy(),
            start.elapsed(),
            output.is_ok()
        );
        // A failure's own words say whether jj timed out, found the working
        // copy stale, or lost a race; the duration alone cannot.
        if let Err(error) = &output {
            eprintln!("scratch command failed: {error}");
        }
        // Report failures under the production program name. Cube recognises
        // jj's stale-working-copy and diverged-operation errors by it, so the
        // concurrent leases below recover from a racing sibling workspace the
        // way they do against `jj` on PATH instead of failing outright.
        let output = output.map_err(|error| match error {
            CubeError::CommandFailed {
                program: _,
                args,
                status,
                stderr,
            } => CubeError::CommandFailed {
                program: reported_program.clone(),
                args,
                status,
                stderr,
            },
            other => other,
        });
        let output = output?;
        if invocation
            .args
            .iter()
            .any(|arg| arg == crate::app::store_gc::OP_LOG_TEMPLATE)
            && let Some(cut) = &self.cut
        {
            // Keep the real DAG and age only the selected cut. Cached fixture
            // timestamps may be months old; retained operations must still be
            // inside the keep window when this test runs again.
            let now = crate::app::util::current_epoch_s().unwrap();
            return Ok(output
                .lines()
                .map(|line| {
                    let (id, _) = line.split_once(' ').expect("operation id and timestamp");
                    let ended = if id == cut { 1 } else { now };
                    format!("{id} {ended}")
                })
                .collect::<Vec<_>>()
                .join("\n"));
        }
        Ok(output)
    }
}

fn runner(f: &Fixture) -> Runner {
    Runner::builder()
        .binary(f.runner.binary.clone())
        .config(f.runner.config.clone())
        .data_dir(f._dir.path().to_path_buf())
        .build()
}

#[test]
fn real_leases_continue_during_collection_and_exclusivity_is_retryable() {
    let _env = crate::app::tests::support::ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let f = Fixture::with_ws_path("pool/mono-agent-001");
    // Exercise a neglected operation store's file count and deletion I/O.
    // Contents of unreachable views are never decoded by the collector, and
    // sparse files keep their count and logical size without writing a
    // gigabyte of zeros. Creating that many files is bound by filesystem
    // metadata latency, so they are staged beside the store while the
    // small-store phases below run, then swapped in once the history is installed.
    let staged_views = f._dir.path().join("unreachable-views");
    let unreachable_views = std::thread::spawn({
        let staged_views = staged_views.clone();
        move || {
            std::fs::create_dir(&staged_views)?;
            crate::app::test_fs::create_sparse_files(&staged_views, fixture::unreachable_views(), 8192)
        }
    });
    let database = f._dir.path().join("state.db");
    let record = RepoRecord {
        repo: "mono".into(),
        origin: f.root.display().to_string(),
        main_branch: "main".into(),
        workspace_root: f._dir.path().join("pool"),
        workspace_prefix: "mono-agent-".into(),
        source: Some(f.root.clone()),
        clone_command: None,
    };
    // Fetch is local and deterministic; no network or user configuration.
    let upstream = f._dir.path().join("upstream");
    f.jj(
        f._dir.path(),
        &[
            "git",
            "clone",
            "--colocate",
            f.root.to_str().unwrap(),
            upstream.to_str().unwrap(),
        ],
    );
    f.jj(&f.root, &["git", "remote", "add", "origin", upstream.to_str().unwrap()]);
    f.jj(&f.ws, &["new", "main"]);
    let store = Store::open_at(&database).unwrap();
    store.upsert_repo(&record).unwrap();
    store
        .set_pool_metadata_i(
            crate::app::gc::POOL_GC_LAST_AT_KEY,
            crate::app::util::current_epoch_s().unwrap(),
        )
        .unwrap();
    let collector_active = Arc::new(AtomicBool::new(false));
    let overlapping_leases = AtomicUsize::new(0);
    let lease = |collection_start: Option<&Barrier>| {
        if let Some(start) = collection_start {
            start.wait();
        }
        let collecting_at_start = collector_active.load(Ordering::SeqCst);
        if let Some(start) = collection_start {
            start.wait();
        }
        let started = Instant::now();
        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "gc contention"]),
            Some(&database),
            &runner(&f),
        )
        .unwrap();
        let collecting_at_end = collector_active.load(Ordering::SeqCst);
        if collecting_at_start || collecting_at_end {
            overlapping_leases.fetch_add(1, Ordering::SeqCst);
        }
        eprintln!(
            "lease {:?}: collector at start={collecting_at_start}, at end={collecting_at_end}",
            started.elapsed()
        );
        (started.elapsed(), result)
    };
    let (baseline, first) = lease(None);
    // Release through the real command path before timing reuse again.
    let release = |result: &crate::app::errors::RunResult| {
        let id = result.payload["workspace"]["lease_id"].as_str().expect("lease id");
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "release", "--lease", id]),
            Some(&database),
            &runner(&f),
        )
        .unwrap();
    };
    release(&first);

    // The release above closed its last shared hold an instant ago; see
    // `EXCLUSIVE_GATE_WAIT` for why a zero wait here is a race against every
    // other test's process spawns.
    let gate = JjGate::exclusive(
        &JjGate::path(f._dir.path()),
        crate::app::tests::support::EXCLUSIVE_GATE_WAIT,
    )
    .unwrap();
    let started = Instant::now();
    let error = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "gc contention"]),
        Some(&database),
        &runner(&f),
    )
    .unwrap_err();
    assert!(matches!(error, CubeError::MaintenanceBusy), "{error}");
    assert_eq!(error.exit_code(), std::process::ExitCode::from(75));
    assert!(error.to_string().contains("retry"));
    let busy = started.elapsed();
    assert!(busy < Duration::from_secs(4), "{busy:?}");
    drop(gate);

    // Installing the cached history and placing the unreachable views touch
    // only private paths and run jj by its declared absolute path, so
    // environment-dependent tests can proceed meanwhile. Reacquire before the
    // leases below spawn anything by bare name again.
    drop(_env);
    let cut = fixture::install(&f);
    unreachable_views.join().unwrap().unwrap();
    // The staged directory becomes the store's view directory: the history's
    // real views move in, then the directories trade places.
    let views = f.root.join(".jj/repo/op_store/views");
    for entry in std::fs::read_dir(&views).unwrap() {
        let entry = entry.unwrap();
        std::fs::rename(entry.path(), staged_views.join(entry.file_name())).unwrap();
    }
    std::fs::remove_dir(&views).unwrap();
    std::fs::rename(&staged_views, &views).unwrap();
    let _env = crate::app::tests::support::ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (scale_baseline, before_gc) = lease(None);
    release(&before_gc);
    eprintln!("lease baseline: small store={baseline:?}, populated store={scale_baseline:?}");

    let under_test = git_utils::jj_compat::detected_version(f.runner.binary.as_os_str())
        .unwrap()
        .0;
    store
        .set_pool_metadata_i(
            &crate::app::store_gc_integrity::oldest_client_key("mono"),
            i64::from(under_test),
        )
        .unwrap();

    let (collect_tx, collect_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let collection_start = Arc::new(Barrier::new(2));
    let mut gc_runner = runner(&f);
    gc_runner.collector_active = Some(collector_active.clone());
    gc_runner.collection_start = Some(collection_start.clone());
    gc_runner.cut = Some(cut.clone());
    gc_runner.collecting = Some(collect_tx);
    gc_runner.resume = Some(resume_rx);
    let gc_database = database.clone();
    let gc = std::thread::spawn(move || {
        gc_repo_store(
            &gc_runner,
            Some(&gc_database),
            &record,
            crate::app::store_gc::DEFAULT_KEEP,
            Duration::from_millis(100),
            false,
        )
    });
    if let Err(error) = collect_rx.recv_timeout(Duration::from_secs(30 * 60)) {
        let result = gc.join().unwrap();
        panic!("collector did not start: {error}; GC result: {result:?}");
    }
    for head in std::fs::read_dir(f.root.join(".jj/repo/op_heads/heads")).unwrap() {
        assert!(
            f.root
                .join(".jj/repo/index")
                .join(if under_test >= 33 { "op_links" } else { "operations" })
                .join(head.unwrap().file_name())
                .is_file(),
            "maintenance must populate the rewritten head's commit index before leases resume"
        );
    }
    // The rewrite gave every operation a new id. jj newer than 0.26 then
    // refuses commands in a working copy still recorded at an old one, after a
    // walk of both logs that outlasts a lease's probe; the collector has
    // refreshed the free workspaces, so the probe succeeds and the lease below
    // reuses one instead of provisioning.
    f.jj(&f.ws, &["status", "--no-pager"]);
    let pooled = || std::fs::read_dir(f._dir.path().join("pool")).unwrap().count();
    let pooled_before = pooled();
    // Deterministically hold the collection phase open: accidentally
    // extending exclusivity here would fail regardless of machine speed.
    let (during, second) = lease(None);
    assert_eq!(
        pooled(),
        pooled_before,
        "a lease after the rewrite must reuse a pooled workspace"
    );
    release(&second);
    resume_tx.send(()).unwrap();
    let (concurrent, third) = lease(Some(&collection_start));
    release(&third);
    let mut live_samples = vec![concurrent];
    std::thread::scope(|scope| {
        let attempts: Vec<_> = (0..13).map(|_| scope.spawn(|| lease(None))).collect();
        for attempt in attempts {
            let (latency, result) = attempt.join().unwrap();
            live_samples.push(latency);
            release(&result);
        }
    });
    while !gc.is_finished() {
        let (latency, result) = lease(None);
        live_samples.push(latency);
        release(&result);
    }
    let report = gc.join().unwrap().unwrap();
    assert!(
        overlapping_leases.load(Ordering::SeqCst) > 0,
        "must measure leases during the actual collector"
    );
    assert!(
        live_samples.iter().all(|latency| *latency < Duration::from_secs(20)),
        "collection must not exhaust the dispatch deadline: {live_samples:?}"
    );
    let retained = fixture::retained_operations();
    assert!(report.before.operations >= retained);
    assert!(report.after.operations >= retained);
    assert_eq!(report.abandoned_through.as_deref(), Some(cut.as_str()));
    assert!(
        !f.jj(
            &f.root,
            &[
                "--ignore-working-copy",
                "op",
                "log",
                "--no-graph",
                "-T",
                "id ++ \"\\n\""
            ]
        )
        .lines()
        .any(|id| id == cut),
        "the selected cut must no longer be reachable after the rewrite"
    );
    eprintln!(
        "live collector lease samples: {live_samples:?}; operations {} -> {}",
        report.before.operations, report.after.operations
    );
    assert_eq!(report.collection, Collection::Immediate);
    // Every lease above fetched during the collection. Git's maintenance after
    // a fetch packs loose refs, which would have cost the commits those leases
    // wrote their protection from `--expire now`; the pass turned it off.
    assert!(
        std::fs::read_to_string(f.root.join(".git/config"))
            .unwrap()
            .lines()
            .any(|line| line.trim() == "auto = false"),
        "collection must turn off git's automatic maintenance in the store"
    );
    assert!(!views.join(format!("{:0128x}", 1)).exists());
    assert!(
        during < scale_baseline * 5 + Duration::from_secs(2),
        "baseline={scale_baseline:?}, during={during:?}"
    );
    assert!(
        concurrent < scale_baseline * 5 + Duration::from_secs(2),
        "baseline={scale_baseline:?}, concurrent={concurrent:?}"
    );
    eprintln!(
        "GC lease latency: baseline={baseline:?}, paused_collection={during:?}, live_collection={concurrent:?}, retryable_busy={busy:?}; exclusive={}ms, collection={}ms",
        report.exclusive_ms, report.collection_ms
    );
    f.jj(&f.ws, &["status"]);
    // Tens of thousands of rewritten operation and index files remain. Their
    // removal is not part of what this test measures, and as the binary's
    // longest test it would otherwise end the whole run seconds later, so it
    // proceeds alongside the remaining tests; the test runner's private temp
    // root is removed in full after the binary exits regardless.
    std::thread::spawn(move || drop(f));
}

// A staged operation-log rewrite against a real store: live operations during
// its preparation, an incomplete working-copy refresh, a dirty primary working
// copy and a racing head. These run against whichever jj is under test; jj
// 0.26 rewrites in place, so the staged-only assertions apply from 0.27.

type Hook<'a> = Box<dyn Fn(&Fixture, usize) -> Option<CubeError> + 'a>;

/// Real jj, with a hook on the scratch abandon of a staged rewrite and on the
/// working-copy refresh, and the operation ages replaced so that everything
/// but the head is old enough to abandon.
struct Hooked<'a> {
    f: &'a Fixture,
    on_scratch_abandon: Hook<'a>,
    on_refresh: Hook<'a>,
    scratch_abandons: Cell<usize>,
    refreshes: Cell<usize>,
}

impl<'a> Hooked<'a> {
    fn new(f: &'a Fixture) -> Self {
        Self {
            f,
            on_scratch_abandon: Box::new(|_, _| None),
            on_refresh: Box::new(|_, _| None),
            scratch_abandons: Cell::new(0),
            refreshes: Cell::new(0),
        }
    }
}

impl CommandRunner for Hooked<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        if invocation.program == "ps" {
            return Ok(String::new());
        }
        // The gate under test is the one the pass takes, which lives in the
        // collector's data dir; point every jj this runner starts at it.
        let mut invocation = invocation.clone();
        if invocation.program == "jj" {
            invocation
                .env
                .push(("CUBE_DATA_DIR".into(), self.f._dir.path().display().to_string()));
        }
        let invocation = &invocation;
        let has = |word: &str| invocation.args.iter().any(|arg| arg == word);
        if has("abandon") && invocation.cwd.to_string_lossy().contains("cube-rewrite-") {
            let n = self.scratch_abandons.replace(self.scratch_abandons.get() + 1);
            if let Some(error) = (self.on_scratch_abandon)(self.f, n) {
                return Err(error);
            }
        }
        if has("update-stale") {
            let n = self.refreshes.replace(self.refreshes.get() + 1);
            if let Some(error) = (self.on_refresh)(self.f, n) {
                return Err(error);
            }
        }
        let output = self.f.runner.run(invocation)?;
        if has(crate::app::store_gc::OP_LOG_TEMPLATE) {
            return Ok(output
                .lines()
                .map(|line| format!("{} 1\n", line.split_once(' ').unwrap().0))
                .collect());
        }
        Ok(output)
    }
}

fn staged_jj() -> bool {
    let binary = std::fs::canonicalize(std::env::var("BOSS_JJ_BIN").unwrap()).unwrap();
    git_utils::jj_compat::detected_version(binary.as_os_str()).unwrap().0 > 26
}

/// A fixture with some history, and a database that declares the jj under
/// test as the oldest client so it may collect.
fn staged_fixture() -> (Fixture, std::path::PathBuf) {
    let f = Fixture::new();
    for description in ["one", "two", "three"] {
        f.jj(&f.ws, &["describe", "-m", description]);
    }
    let database = f._dir.path().join("state.db");
    let binary = std::fs::canonicalize(std::env::var("BOSS_JJ_BIN").unwrap()).unwrap();
    let under_test = git_utils::jj_compat::detected_version(binary.as_os_str()).unwrap().0;
    Store::open_at(&database)
        .unwrap()
        .set_pool_metadata_i(
            &crate::app::store_gc_integrity::oldest_client_key("scratch"),
            i64::from(under_test),
        )
        .unwrap();
    (f, database)
}

fn staged_record(f: &Fixture) -> RepoRecord {
    RepoRecord {
        repo: "scratch".into(),
        origin: f.root.display().to_string(),
        main_branch: "main".into(),
        workspace_root: f._dir.path().to_owned(),
        workspace_prefix: "ws".into(),
        source: Some(f.root.clone()),
        clone_command: None,
    }
}

fn staged_collect(
    f: &Fixture,
    database: &Path,
    runner: &dyn CommandRunner,
) -> Result<crate::app::store_gc::StoreGcReport> {
    gc_repo_store(
        runner,
        Some(database),
        &staged_record(f),
        Duration::ZERO,
        Duration::from_millis(100),
        false,
    )
}

fn staged_op_descriptions(f: &Fixture) -> String {
    f.jj(
        &f.root,
        &[
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "-T",
            "description ++ \"\\n\"",
        ],
    )
}

fn staged_head_id(f: &Fixture) -> String {
    f.jj(
        &f.root,
        &[
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "--limit",
            "1",
            "-T",
            "id",
        ],
    )
}

fn staged_markers(database: &Path) -> (bool, bool) {
    let quarantine = JjGate::quarantine_path(&JjGate::path(database.parent().unwrap()));
    (quarantine.exists(), quarantine.with_extension("refresh").exists())
}

#[test]
fn an_operation_during_preparation_does_not_cost_the_pass() {
    let (f, database) = staged_fixture();
    let mut runner = Hooked::new(&f);
    // A worker describes its commit while the rewrite is being prepared.
    runner.on_scratch_abandon = Box::new(|f, n| {
        if n == 0 {
            f.jj(&f.ws, &["describe", "-m", "worker progress"]);
        }
        None
    });
    let report = staged_collect(&f, &database, &runner).unwrap();
    if staged_jj() {
        assert_eq!(runner.scratch_abandons.get(), 2, "prepared again from the new head");
    }
    assert!(
        matches!(report.collection, Collection::Immediate | Collection::Deferred),
        "{:?}",
        report.collection
    );
    assert_eq!(crate::app::store_gc::count_op_heads(&f.root).unwrap(), 1);
    assert_eq!(staged_markers(&database), (false, false));
    assert!(!staged_op_descriptions(&f).contains("reconcile"));
    // The store still hands out workspaces, and the worker's work survived.
    f.jj(
        &f.root,
        &["workspace", "add", f._dir.path().join("second").to_str().unwrap()],
    );
    if staged_jj() {
        // Other workspaces are left stale by the rewrite, as with any abandon.
        f.jj(&f.ws, &["workspace", "update-stale"]);
        assert!(
            f.jj(&f.ws, &["log", "-r", "all()", "--no-graph", "-T", "description"])
                .contains("worker progress")
        );
    }
}

#[test]
fn a_store_that_is_busy_every_attempt_is_reported_not_silently_skipped() {
    if !staged_jj() {
        return;
    }
    let (f, database) = staged_fixture();
    let mut runner = Hooked::new(&f);
    runner.on_scratch_abandon = Box::new(|f, n| {
        f.jj(&f.ws, &["describe", "-m", &format!("progress {n}")]);
        None
    });
    let report = staged_collect(&f, &database, &runner).unwrap();
    assert_eq!(report.collection, Collection::Concurrent);
    assert_eq!(runner.scratch_abandons.get(), 4, "one preparation plus three more");
    assert_eq!(staged_markers(&database), (false, false));

    let store = Store::open_at(&database).unwrap();
    let repos = vec!["scratch".to_owned()];
    for pass in 1..=2 {
        record_store_gc_outcome(&store, "scratch", StoreGcOutcome::Concurrent).unwrap();
        let attention = store_gc_attention(&store, &repos);
        assert_eq!(attention.len(), 1, "pass {pass}");
        assert_eq!(attention[0].last_outcome, StoreGcOutcome::Concurrent);
        assert_eq!(attention[0].consecutive_failures, pass);
    }
}

#[test]
fn an_incomplete_refresh_keeps_the_quarantine_until_recovery_finishes_it() {
    if !staged_jj() {
        return;
    }
    let (f, database) = staged_fixture();
    let mut runner = Hooked::new(&f);
    runner.on_refresh = Box::new(|_, n| (n == 0).then(|| CubeError::InvalidArgument("refresh interrupted".into())));
    let error = staged_collect(&f, &database, &runner).unwrap_err();
    assert!(error.to_string().contains("refresh interrupted"), "{error}");
    // Published but not refreshed: readers stay out, and the owed refresh is on record.
    assert_eq!(staged_markers(&database), (true, true));
    assert_eq!(crate::app::store_gc::count_op_heads(&f.root).unwrap(), 1);

    // The next reader (a lease adding a workspace) finishes the refresh first.
    let gate = JjGate::shared_for_database_with_runner(Some(&database), &runner).unwrap();
    assert_eq!(staged_markers(&database), (false, false));
    f.jj(
        &f.root,
        &["workspace", "add", f._dir.path().join("second").to_str().unwrap()],
    );
    drop(gate);
    assert_eq!(runner.refreshes.get(), 2);
    assert!(!staged_op_descriptions(&f).contains("reconcile"));
}

#[test]
fn a_recovery_whose_refresh_keeps_failing_leaves_the_store_closed() {
    if !staged_jj() {
        return;
    }
    let (f, database) = staged_fixture();
    let mut runner = Hooked::new(&f);
    runner.on_refresh = Box::new(|_, _| Some(CubeError::InvalidArgument("still failing".into())));
    staged_collect(&f, &database, &runner).unwrap_err();
    let error = JjGate::shared_for_database_with_runner(Some(&database), &runner)
        .err()
        .expect("recovery cannot admit readers");
    assert!(matches!(error, CubeError::MaintenanceQuarantined(_)), "{error}");
    assert_eq!(staged_markers(&database), (true, true));
}

#[test]
fn edits_in_the_primary_working_copy_are_snapshotted_not_merged_in_from_old_history() {
    if !staged_jj() {
        return;
    }
    let (f, database) = staged_fixture();
    // An unsnapshotted edit: refreshing without snapshotting first would
    // snapshot it at the pre-rewrite operation and merge that history back.
    std::fs::write(f.root.join("dirty.txt"), "uncommitted\n").unwrap();
    let report = staged_collect(&f, &database, &Hooked::new(&f)).unwrap();
    assert!(
        matches!(report.collection, Collection::Immediate | Collection::Deferred),
        "{:?}",
        report.collection
    );
    assert!(
        !staged_op_descriptions(&f).contains("reconcile"),
        "{}",
        staged_op_descriptions(&f)
    );
    assert_eq!(crate::app::store_gc::count_op_heads(&f.root).unwrap(), 1);
    assert_eq!(
        std::fs::read_to_string(f.root.join("dirty.txt")).unwrap(),
        "uncommitted\n"
    );
}

#[test]
fn a_head_from_a_writer_on_the_old_history_is_refused_not_trusted() {
    if !staged_jj() {
        return;
    }
    let (f, database) = staged_fixture();
    let original = staged_head_id(&f);
    let mut runner = Hooked::new(&f);
    // A command that loaded the old head before publication commits just
    // before the refresh, which jj cannot be pinned against: it merges the
    // two heads. The merged head is not trusted as the rewritten one.
    runner.on_refresh = Box::new(move |f, _| {
        std::fs::write(f.root.join(".jj/repo/op_heads/heads").join(original.trim()), b"").unwrap();
        None
    });
    let error = staged_collect(&f, &database, &runner).unwrap_err();
    assert!(error.to_string().contains("does not descend safely"), "{error}");
    // Quarantine stays, so no reader loads the merged history.
    assert_eq!(staged_markers(&database), (true, true));
}

/// One more operation on top of the store's head, made the way a command
/// started outside cube would: without taking the maintenance gate.
fn operation_outside_the_gate(f: &Fixture, name: &str) {
    let mut invocation = RealCommandRunner::invocation(
        &f.root,
        "jj",
        &["--ignore-working-copy", "bookmark", "create", name, "-r", "root()"],
    );
    invocation
        .env
        .push((crate::command_runner::JJ_GATE_HELD_ENV.to_owned(), "1".to_owned()));
    f.runner.run(&invocation).unwrap();
}

fn add_workspace(f: &Fixture, name: &str) {
    f.jj(
        &f.root,
        &["workspace", "add", f._dir.path().join(name).to_str().unwrap()],
    );
}

#[test]
fn safe_progress_after_publication_is_verified_without_waiting_on_the_held_gate() {
    if !staged_jj() {
        return;
    }
    let (f, database) = staged_fixture();
    let mut runner = Hooked::new(&f);
    // An engine fetch or a worker snapshot lands on the published rewrite
    // while the refresh runs. The pass holds the gate exclusively, so the
    // ancestry walk that accepts it must not take the gate again.
    runner.on_refresh = Box::new(|f, n| {
        if n == 0 {
            operation_outside_the_gate(f, "safe-progress");
        }
        None
    });
    let started = Instant::now();
    let report = staged_collect(&f, &database, &runner).unwrap();
    assert!(started.elapsed() < Duration::from_secs(60), "{:?}", started.elapsed());
    assert!(
        matches!(report.collection, Collection::Immediate | Collection::Deferred),
        "{:?}",
        report.collection
    );
    assert_eq!(staged_markers(&database), (false, false));
    add_workspace(&f, "second");
}

#[test]
fn safe_progress_found_by_recovery_is_verified_without_waiting_on_the_held_gate() {
    if !staged_jj() {
        return;
    }
    let (f, database) = staged_fixture();
    let mut runner = Hooked::new(&f);
    runner.on_refresh = Box::new(|f, n| {
        (n == 0).then(|| {
            operation_outside_the_gate(f, "safe-progress");
            CubeError::InvalidArgument("refresh interrupted".into())
        })
    });
    staged_collect(&f, &database, &runner).unwrap_err();
    assert_eq!(staged_markers(&database), (true, true));

    // The next lease recovers under the exclusive gate it just took.
    let gate = JjGate::shared_for_database_with_runner(Some(&database), &runner).unwrap();
    assert_eq!(staged_markers(&database), (false, false));
    add_workspace(&f, "second");
    drop(gate);
}

/// Leave the markers a pass writes before publishing, for an operation that
/// never became a head.
fn seed_unpublished_rewrite(f: &Fixture, database: &Path) -> String {
    let original = staged_head_id(f).trim().to_owned();
    let quarantine = JjGate::quarantine_path(&JjGate::path(database.parent().unwrap()));
    std::fs::create_dir_all(quarantine.parent().unwrap()).unwrap();
    std::fs::write(&quarantine, f.root.display().to_string()).unwrap();
    std::fs::write(
        quarantine.with_extension("refresh"),
        format!("{original}\n{}", "ab".repeat(64)),
    )
    .unwrap();
    original
}

#[test]
fn a_refresh_record_for_an_unpublished_rewrite_does_not_close_the_store() {
    if !staged_jj() {
        return;
    }
    for progressed in [false, true] {
        let (f, database) = staged_fixture();
        let original = seed_unpublished_rewrite(&f, &database);
        if progressed {
            // A non-cube command advanced the head after the pass staged its rewrite.
            operation_outside_the_gate(&f, "progress");
            assert_ne!(staged_head_id(&f).trim(), original);
        }
        assert_eq!(staged_markers(&database), (true, true));
        let runner = Hooked::new(&f);
        let gate = JjGate::shared_for_database_with_runner(Some(&database), &runner).unwrap();
        assert_eq!(staged_markers(&database), (false, false), "progressed={progressed}");
        assert_eq!(
            runner.refreshes.get(),
            0,
            "nothing was rewritten, so nothing is refreshed"
        );
        add_workspace(&f, "second");
        drop(gate);
    }
}
