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
use std::time::{Duration, Instant};

struct Runner {
    binary: PathBuf,
    config: PathBuf,
    data_dir: PathBuf,
    collecting: Option<mpsc::Sender<()>>,
    resume: Option<mpsc::Receiver<()>>,
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
        RealCommandRunner.run_with_timeout(&invocation, timeout)
    }
}

fn runner(f: &Fixture) -> Runner {
    Runner {
        binary: f.runner.binary.clone(),
        config: f.runner.config.clone(),
        data_dir: f._dir.path().to_path_buf(),
        collecting: None,
        resume: None,
    }
}

#[test]
fn real_leases_continue_during_collection_and_exclusivity_is_retryable() {
    let _env = crate::app::tests::support::ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let f = Fixture::with_ws_path("pool/mono-agent-001");
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
    let lease = || {
        let started = Instant::now();
        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "gc contention"]),
            Some(&database),
            &runner(&f),
        )
        .unwrap();
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

    let gate = JjGate::exclusive(&JjGate::path(f._dir.path()), Duration::ZERO).unwrap();
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

    // Valid hex filenames representing unreachable views let the real
    // collector exercise deletion without a huge history or memory load.
    let views = f.root.join(".jj/repo/op_store/views");
    for n in 1..=10_000_u64 {
        std::fs::write(views.join(format!("{n:0128x}")), b"unreachable").unwrap();
    }

    let (collect_tx, collect_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let mut gc_runner = runner(&f);
    gc_runner.collecting = Some(collect_tx);
    gc_runner.resume = Some(resume_rx);
    let gc_database = database.clone();
    let gc = std::thread::spawn(move || {
        gc_repo_store(
            &gc_runner,
            Some(&gc_database),
            &record,
            Duration::ZERO,
            Duration::from_millis(100),
            false,
        )
    });
    collect_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    // Deterministically hold the collection phase open: accidentally
    // extending exclusivity here would fail regardless of machine speed.
    let (during, second) = lease();
    release(&second);
    resume_tx.send(()).unwrap();
    let (concurrent, third) = lease();
    let report = gc.join().unwrap().unwrap();
    assert_eq!(report.collection, Collection::Immediate);
    release(&third);
    assert!(!views.join(format!("{:0128x}", 1)).exists());
    assert!(
        during < baseline * 5 + Duration::from_secs(2),
        "baseline={baseline:?}, during={during:?}"
    );
    assert!(
        concurrent < baseline * 5 + Duration::from_secs(2),
        "baseline={baseline:?}, concurrent={concurrent:?}"
    );
    eprintln!(
        "GC lease latency: baseline={baseline:?}, paused_collection={during:?}, live_collection={concurrent:?}, retryable_busy={busy:?}; exclusive={}ms, collection={}ms",
        report.exclusive_ms, report.collection_ms
    );
    f.jj(&f.ws, &["status"]);
}
