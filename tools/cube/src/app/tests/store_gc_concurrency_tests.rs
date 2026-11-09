//! Real shared-store commands and lease dispatch during a slow collection.
use super::*;
use crate::app::dispatch::run_with_dependencies;
use crate::app::store_gc::{Collection, gc_repo_store};
use crate::cli::Cli;
use crate::command_runner::JjGate;
use crate::metadata::RepoRecord;
use crate::store::Store;
use clap::Parser;
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

#[path = "store_gc_fixture.rs"]
mod fixture;

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
        let output = RealCommandRunner.run_with_timeout(&invocation, timeout);
        if collecting && let Some(active) = &self.collector_active {
            active.store(false, Ordering::SeqCst);
        }
        eprintln!(
            "scratch command {:?}: {:?}, success={}",
            invocation.args,
            start.elapsed(),
            output.is_ok()
        );
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
            crate::app::test_fs::create_sparse_files(&staged_views, 137_000, 8192)
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
    let lease = || {
        let collecting_at_start = collector_active.load(Ordering::SeqCst);
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
    let (baseline, first) = lease();
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
    let (scale_baseline, before_gc) = lease();
    release(&before_gc);
    eprintln!("lease baseline: small store={baseline:?}, populated store={scale_baseline:?}");

    let (collect_tx, collect_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let mut gc_runner = runner(&f);
    gc_runner.collector_active = Some(collector_active.clone());
    gc_runner.cut = Some(cut);
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
    if let Err(error) = collect_rx.recv_timeout(Duration::from_secs(180)) {
        let result = gc.join().unwrap();
        panic!("collector did not start: {error}; GC result: {result:?}");
    }
    for head in std::fs::read_dir(f.root.join(".jj/repo/op_heads/heads")).unwrap() {
        assert!(
            f.root
                .join(".jj/repo/index/operations")
                .join(head.unwrap().file_name())
                .is_file(),
            "maintenance must populate the rewritten head's commit index before leases resume"
        );
    }
    // Deterministically hold the collection phase open: accidentally
    // extending exclusivity here would fail regardless of machine speed.
    let (during, second) = lease();
    release(&second);
    resume_tx.send(()).unwrap();
    let (concurrent, third) = lease();
    release(&third);
    let mut live_samples = vec![concurrent];
    std::thread::scope(|scope| {
        let attempts: Vec<_> = (0..13).map(|_| scope.spawn(lease)).collect();
        for attempt in attempts {
            let (latency, result) = attempt.join().unwrap();
            live_samples.push(latency);
            release(&result);
        }
    });
    while !gc.is_finished() {
        let (latency, result) = lease();
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
    assert!(report.before.operations >= 16_800);
    assert!(report.after.operations >= 16_800);
    eprintln!(
        "live collector lease samples: {live_samples:?}; operations {} -> {}",
        report.before.operations, report.after.operations
    );
    assert_eq!(report.collection, Collection::Immediate);
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
