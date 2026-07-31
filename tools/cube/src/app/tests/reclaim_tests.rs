//! Tests for the disk-reclamation passes: build-artifact compaction and the
//! free-workspace high-water-mark trim.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::support::{
    ENV_MUTEX, ExpectedCommand, FakeRunner, audit_events, head_status_command, head_status_output,
    unpushed_probe_command, with_database_path,
};

use crate::app::disk::testing::{AMPLE_TEST_VOLUME, volume_with_free, with_reading, with_readings};
use crate::app::errors::CubeError;
use crate::app::gc::{POOL_GC_LAST_AT_KEY, POOL_GC_STARTED_AT_KEY, POOL_GC_TRIM_PROGRESS_KEY, maybe_trigger_pool_gc};
use crate::app::provision::discover_workspaces;
use crate::app::reclaim::{
    assert_mint_headroom, compact_free_workspaces, has_free_workspace_surplus, is_safe_artifact_dir_name,
    relieve_disk_pressure, stage_workspace_for_removal, trim_free_workspaces_to_mark,
};
use crate::metadata::{RepoRecord, WorkspaceCandidate};
use crate::store::Store;

const NOW: i64 = 1_800_000_000;
/// Comfortably past the 6h default compaction idle window.
const LONG_IDLE: i64 = NOW - 48 * 3_600;

/// Point `CUBE_CONFIG_DIR` at a tempdir holding `body` as `cube.toml`, for the
/// duration of the returned guard. Held together with [`ENV_MUTEX`] because
/// `set_var` is not thread-safe and the config path is process-global.
struct ConfigGuard {
    _dir: TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl ConfigGuard {
    fn new(body: &str) -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("config tempdir");
        std::fs::write(dir.path().join("cube.toml"), body).expect("write cube.toml");
        // SAFETY: test-only, and ENV_MUTEX serialises every test that reads or
        // writes the process environment.
        unsafe { std::env::set_var("CUBE_CONFIG_DIR", dir.path()) };
        Self { _dir: dir, _lock: lock }
    }
}

impl Drop for ConfigGuard {
    fn drop(&mut self) {
        // SAFETY: as above; still holding ENV_MUTEX.
        unsafe { std::env::remove_var("CUBE_CONFIG_DIR") };
    }
}

/// Seed a repo with `count` free workspaces, each with a `.jj` dir so it looks
/// like a real attachment. Returns the store and the workspace root.
fn seed_pool(tempdir: &TempDir, database_path: &Path, count: usize, source: Option<PathBuf>) -> (Store, PathBuf) {
    let workspace_root = tempdir.path().join("workspaces");
    let mut candidates = Vec::new();
    for n in 1..=count {
        let id = format!("mono-agent-{n:03}");
        let path = workspace_root.join(&id);
        std::fs::create_dir_all(path.join(".jj")).expect("workspace dir");
        candidates.push(WorkspaceCandidate {
            workspace_id: id,
            workspace_path: path,
        });
    }
    let mut store = Store::open_at(database_path).expect("store");
    store
        .upsert_repo(&RepoRecord {
            repo: "mono".to_string(),
            origin: "git@github.com:spinyfin/mono.git".to_string(),
            main_branch: "main".to_string(),
            workspace_root: workspace_root.clone(),
            workspace_prefix: "mono-agent-".to_string(),
            source,
            clone_command: None,
        })
        .expect("repo");
    store.sync_workspaces("mono", &candidates).expect("sync");
    (store, workspace_root)
}

/// Set a workspace's idle clock directly, the way `force_lease_expiry` sets a
/// lease's, so a test can age one without waiting.
fn set_last_activity(database_path: &Path, workspace_id: &str, epoch_s: i64) {
    let conn = rusqlite::Connection::open(database_path).expect("sqlite open");
    let updated = conn
        .execute(
            "UPDATE workspaces SET last_activity_at_epoch_s = ?1 WHERE workspace_id = ?2",
            rusqlite::params![epoch_s, workspace_id],
        )
        .expect("set last activity");
    assert_eq!(updated, 1, "expected exactly one row updated");
}

fn write_artifact_tree(workspace_path: &Path, dir_name: &str) -> PathBuf {
    let dir = workspace_path.join(dir_name);
    std::fs::create_dir_all(dir.join("debug/deps")).expect("artifact dir");
    std::fs::write(dir.join("debug/deps/libthing.rlib"), vec![0u8; 4096]).expect("artifact file");
    dir
}

/// The `jj --ignore-working-copy file list` probe compaction runs before it
/// deletes anything. Empty output means "nothing tracked in there".
fn tracked_probe(workspace_path: &Path, dir_name: &str, output: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        workspace_path.to_path_buf(),
        "jj",
        &["--ignore-working-copy", "file", "list", "-r", "@", dir_name],
        output,
    )
}

// ── Compaction ──────────────────────────────────────────────────────────────

#[test]
fn compaction_removes_a_build_artifact_tree_from_a_free_workspace() {
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"target\"]\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    let target = write_artifact_tree(&ws, "target");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    let runner = FakeRunner::new(vec![tracked_probe(&ws, "target", "")]);
    let report = compact_free_workspaces(&runner, &store, Some(&database_path), None, NOW, false, None);
    runner.assert_exhausted();

    assert_eq!(report.workspaces_compacted, 1);
    assert_eq!(report.dirs_removed, 1);
    assert!(!target.exists(), "target/ should have been reclaimed");
    // The checkout itself survives — that is the whole point of preferring
    // compaction over removal.
    assert!(
        ws.join(".jj").is_dir(),
        "the workspace checkout must survive compaction"
    );

    let compacted = audit_events(&tempdir)
        .into_iter()
        .find(|e| e["event"] == "workspace.compacted")
        .expect("workspace.compacted audit event");
    assert_eq!(compacted["workspace_id"], "mono-agent-001");
    assert_eq!(compacted["dirs"][0], "target");
    assert_eq!(compacted["urgent"], false);
}

#[test]
fn compaction_never_touches_a_leased_workspace() {
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"target\"]\n");
    let (tempdir, database_path) = with_database_path();
    let (mut store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    let target = write_artifact_tree(&ws, "target");

    store
        .claim_specific_workspace("mono", "mono-agent-001", "boss", "task", "lease-1", LONG_IDLE, None)
        .expect("claim")
        .expect("claimed");
    // Age it well past the idle window: only the lease should be protecting it.
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    // No expectations at all — a leased workspace must not even be probed.
    let runner = FakeRunner::new(vec![]);
    let report = compact_free_workspaces(&runner, &store, Some(&database_path), None, NOW, false, None);
    runner.assert_exhausted();

    assert_eq!(report.workspaces_compacted, 0);
    assert!(target.is_dir(), "a live worker's build tree must be left alone");
}

#[test]
fn compaction_does_not_follow_a_symlinked_artifact_dir() {
    // Bazel puts `bazel-out` and friends in every workspace as symlinks into
    // one shared output base. Following one would delete through it into a
    // cache every workspace on the machine shares.
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"bazel-out\"]\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    let shared_output_base = tempdir.path().join("shared-bazel-cache");
    std::fs::create_dir_all(&shared_output_base).expect("shared cache");
    std::fs::write(shared_output_base.join("precious.o"), b"every workspace shares this").expect("cache file");
    std::os::unix::fs::symlink(&shared_output_base, ws.join("bazel-out")).expect("symlink");

    // Not even the tracked-files probe should run: the symlink check comes
    // first and short-circuits.
    let runner = FakeRunner::new(vec![]);
    let report = compact_free_workspaces(&runner, &store, Some(&database_path), None, NOW, false, None);
    runner.assert_exhausted();

    assert_eq!(report.dirs_removed, 0);
    assert!(ws.join("bazel-out").exists(), "the symlink itself should be untouched");
    assert!(
        shared_output_base.join("precious.o").exists(),
        "compaction must never delete through a symlink into the shared output base",
    );
}

#[test]
fn compaction_skips_a_dir_jj_is_tracking() {
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"target\"]\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    let target = write_artifact_tree(&ws, "target");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    // A repo that actually tracks files under `target/` — deleting them would
    // destroy source, not build output.
    let runner = FakeRunner::new(vec![tracked_probe(&ws, "target", "target/keep-me.rs\n")]);
    let report = compact_free_workspaces(&runner, &store, Some(&database_path), None, NOW, false, None);
    runner.assert_exhausted();

    assert_eq!(report.dirs_removed, 0);
    assert!(target.is_dir(), "a tracked directory must never be reclaimed");
}

#[test]
fn compaction_skips_a_workspace_whose_tracked_probe_fails() {
    // An unanswered safety question is not a yes.
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"target\"]\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    let target = write_artifact_tree(&ws, "target");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    let runner = FakeRunner::new(vec![ExpectedCommand::failing(
        ws.clone(),
        "jj",
        &["--ignore-working-copy", "file", "list", "-r", "@", "target"],
        "Error: something went wrong",
    )]);
    let report = compact_free_workspaces(&runner, &store, Some(&database_path), None, NOW, false, None);
    runner.assert_exhausted();

    assert_eq!(report.dirs_removed, 0);
    assert!(target.is_dir());
}

#[test]
fn a_stale_workspace_is_skipped_rather_than_recovered() {
    // The tracked-files probe is a read taken to decide whether deleting is
    // safe, so it deliberately bypasses `run_jj`'s stale auto-recovery: a pass
    // that only wants to know something must not mutate the workspace it is
    // asking about. The FakeRunner would panic on the unscripted
    // `jj workspace update-stale` if that wrapper were reintroduced here.
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"target\"]\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    let target = write_artifact_tree(&ws, "target");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    let runner = FakeRunner::new(vec![ExpectedCommand::stale(
        ws.clone(),
        "jj",
        &["--ignore-working-copy", "file", "list", "-r", "@", "target"],
    )]);
    let report = compact_free_workspaces(&runner, &store, Some(&database_path), None, NOW, false, None);
    runner.assert_exhausted();

    assert_eq!(report.dirs_removed, 0);
    assert!(target.is_dir());
}

#[test]
fn the_tracked_files_probe_is_bounded_even_without_a_caller_deadline() {
    // The probe deliberately bypasses `run_jj` (it must not trigger the stale
    // working-copy recovery that wrapper performs) but must not thereby bypass
    // its timeout discipline: it runs on the lease path with the repo lock
    // held, where an unbounded subprocess wedges every lease for that repo.
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"target\"]\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    write_artifact_tree(&ws, "target");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    let runner = FakeRunner::new(vec![tracked_probe(&ws, "target", "")]);
    compact_free_workspaces(&runner, &store, Some(&database_path), None, NOW, false, None);
    runner.assert_exhausted();

    let timeouts = runner.observed_timeouts();
    assert_eq!(timeouts.len(), 1, "the probe must run under a wall-clock bound");
    assert!(
        timeouts[0] <= Duration::from_secs(30),
        "an unbounded probe would hang the lease path: {:?}",
        timeouts[0],
    );
}

#[test]
fn a_probe_that_outlives_the_pass_budget_leaves_the_artifact_tree_alone() {
    // A timeout is just another way of not getting an answer, and an
    // unanswered safety question is not a yes.
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"target\"]\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    let target = write_artifact_tree(&ws, "target");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    let runner = FakeRunner::new(vec![tracked_probe(&ws, "target", "").taking(Duration::from_secs(30))]);
    // Comfortably above `MIN_SUBPROCESS_BUDGET` so the probe is still spawned
    // (and clipped) rather than deferred outright — that floor has its own
    // coverage below.
    let deadline = Instant::now() + Duration::from_secs(2);
    let report = compact_free_workspaces(&runner, &store, Some(&database_path), None, NOW, false, Some(deadline));
    runner.assert_exhausted();

    assert_eq!(report.dirs_removed, 0);
    assert!(
        target.is_dir(),
        "a probe cube could not complete means keep, not delete"
    );
    let timeouts = runner.observed_timeouts();
    assert!(
        timeouts[0] <= Duration::from_secs(2),
        "the probe must be clipped to what the pass deadline has left: {:?}",
        timeouts[0],
    );
}

#[test]
fn a_pass_deadline_too_small_to_finish_a_probe_defers_without_spawning_one() {
    // Below `MIN_SUBPROCESS_BUDGET` the remaining time is not just tight, it
    // is doomed: `jj`'s own startup overhead alone would outlast it. Spawning
    // anyway produces a subprocess that is killed almost immediately, and
    // that kill is indistinguishable from a genuine probe failure to the
    // caller — this is the off-by-nothing bug. The fix must refuse to spawn
    // at all, the same as when the deadline has already passed outright.
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"target\"]\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    let target = write_artifact_tree(&ws, "target");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    // No expected commands: a doomed spawn must never reach the runner.
    let runner = FakeRunner::new(vec![]);
    let deadline = Instant::now() + Duration::from_millis(50);
    let report = compact_free_workspaces(&runner, &store, Some(&database_path), None, NOW, false, Some(deadline));
    runner.assert_exhausted();

    assert_eq!(report.dirs_removed, 0);
    assert!(
        target.is_dir(),
        "a candidate that could not be evaluated in time must be left alone, not deleted"
    );
    assert!(
        runner.observed_timeouts().is_empty(),
        "no subprocess should have been spawned at all"
    );
}

#[test]
fn routine_compaction_honours_the_idle_window_but_urgent_ignores_it() {
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"target\"]\ncompact-idle-hours = 6\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    let target = write_artifact_tree(&ws, "target");
    // Released an hour ago: still in active rotation, cache is worth keeping.
    set_last_activity(&database_path, "mono-agent-001", NOW - 3_600);

    let routine = FakeRunner::new(vec![]);
    let report = compact_free_workspaces(&routine, &store, Some(&database_path), None, NOW, false, None);
    routine.assert_exhausted();
    assert_eq!(
        report.dirs_removed, 0,
        "a recently-used workspace keeps its build cache"
    );
    assert!(target.is_dir());

    // Same workspace, same instant — but the volume is now near exhaustion, so
    // a cold build is unambiguously cheaper than a full disk.
    let urgent = FakeRunner::new(vec![tracked_probe(&ws, "target", "")]);
    let report = compact_free_workspaces(&urgent, &store, Some(&database_path), None, NOW, true, None);
    urgent.assert_exhausted();
    assert_eq!(report.dirs_removed, 1);
    assert!(!target.exists());
}

#[test]
fn compaction_can_be_scoped_to_one_repo() {
    // The lease path holds only its own repo's lock, so its urgent pass must
    // stay inside that repo.
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"target\"]\n");
    let (tempdir, database_path) = with_database_path();
    let (mut store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);

    let other_root = tempdir.path().join("flunge-workspaces");
    let other_ws = other_root.join("flunge-agent-001");
    std::fs::create_dir_all(other_ws.join(".jj")).expect("flunge ws");
    store
        .upsert_repo(&RepoRecord {
            repo: "flunge".to_string(),
            origin: "git@github.com:spinyfin/flunge.git".to_string(),
            main_branch: "main".to_string(),
            workspace_root: other_root,
            workspace_prefix: "flunge-agent-".to_string(),
            source: None,
            clone_command: None,
        })
        .expect("flunge repo");
    store
        .sync_workspaces(
            "flunge",
            &[WorkspaceCandidate {
                workspace_id: "flunge-agent-001".to_string(),
                workspace_path: other_ws.clone(),
            }],
        )
        .expect("sync flunge");

    let mono_target = write_artifact_tree(&workspace_root.join("mono-agent-001"), "target");
    let flunge_target = write_artifact_tree(&other_ws, "target");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);
    set_last_activity(&database_path, "flunge-agent-001", LONG_IDLE);

    let runner = FakeRunner::new(vec![tracked_probe(
        &workspace_root.join("mono-agent-001"),
        "target",
        "",
    )]);
    compact_free_workspaces(&runner, &store, Some(&database_path), Some("mono"), NOW, false, None);
    runner.assert_exhausted();

    assert!(!mono_target.exists());
    assert!(flunge_target.is_dir(), "another repo's workspaces are out of scope");
}

#[test]
fn artifact_dir_names_that_escape_the_workspace_are_rejected() {
    // `remove_dir_all` is aimed at `workspace_path.join(name)`, so a name
    // carrying a separator or `..` would let a cube.toml edit point it
    // anywhere on the filesystem.
    assert!(is_safe_artifact_dir_name("target"));
    assert!(is_safe_artifact_dir_name(".build"));
    assert!(is_safe_artifact_dir_name("node_modules"));

    assert!(!is_safe_artifact_dir_name(""));
    assert!(!is_safe_artifact_dir_name("."));
    assert!(!is_safe_artifact_dir_name(".."));
    assert!(!is_safe_artifact_dir_name("../../etc"));
    assert!(!is_safe_artifact_dir_name("a/b"));
    assert!(!is_safe_artifact_dir_name("/absolute"));
    // The workspace is not its own build output.
    assert!(!is_safe_artifact_dir_name(".jj"));
    assert!(!is_safe_artifact_dir_name(".git"));
}

#[test]
fn an_unsafe_configured_dir_name_is_refused_at_the_point_of_use() {
    // Belt to the braces above: even configured explicitly, `.jj` must survive.
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\".jj\", \"../escape\"]\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    let escape_target = tempdir.path().join("escape");
    std::fs::create_dir_all(&escape_target).expect("escape dir");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    let runner = FakeRunner::new(vec![]);
    let report = compact_free_workspaces(&runner, &store, Some(&database_path), None, NOW, false, None);
    runner.assert_exhausted();

    assert_eq!(report.dirs_removed, 0);
    assert!(ws.join(".jj").is_dir(), "the jj store must never be reclaimed");
    assert!(escape_target.is_dir(), "compaction must not escape the workspace root");
}

// ── Free-pool trim ──────────────────────────────────────────────────────────

/// The reuse probe the trim runs per candidate: fetch, then a head status
/// showing an empty `@` sitting on main — nothing would be lost.
fn trim_probe_reusable(ws_path: &Path) -> Vec<ExpectedCommand> {
    vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(ws_path, &head_status_output("empty001", true, "main", "main", "main")),
    ]
}

/// A candidate whose `@` holds a non-empty commit no remote has.
fn trim_probe_holds_work(ws_path: &Path) -> Vec<ExpectedCommand> {
    vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(ws_path, &head_status_output("wip00001", false, "", "", "")),
        unpushed_probe_command(ws_path, "wip00001\tunpushed work\n"),
    ]
}

fn forget_registration(workspace_root: &Path, source: &Path, workspace_id: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        workspace_root.to_path_buf(),
        "jj",
        &["-R", &source.display().to_string(), "workspace", "forget", workspace_id],
        "",
    )
}

/// The `bazel info output_base` probe `remove_workspace_from_disk` issues,
/// lazily and once per repo pass, to discover the shared `_bazel_<user>`
/// root — run in the repo's canonical `source` checkout. The output base
/// this fakes up does not need to exist on disk: the workspace being
/// removed in these tests never actually populated one, so the resulting
/// cleanup is a harmless `NothingToRemove`.
fn bazel_root_probe(source: &Path) -> ExpectedCommand {
    ExpectedCommand::ok(
        source.to_path_buf(),
        "bazel",
        &["info", "output_base"],
        "/fake-bazel-cache/_bazel_testuser/deadbeefdeadbeefdeadbeefdeadbeef",
    )
}

#[test]
fn a_staged_workspace_cannot_be_rediscovered_and_re_registered() {
    // The window the staging rename exists to close. Dropping the registry row
    // is NOT enough on its own: the lease path rebuilds the registry from disk
    // (`discover_workspaces` + `sync_workspaces`) under the same repo lock, so
    // a directory still matching the prefix comes straight back as a free,
    // most-idle-looking candidate — and gets handed to a worker while the trim
    // is unlinking it. Once staged, the directory no longer matches the prefix,
    // so the rediscovery finds nothing to resurrect.
    let _config = ConfigGuard::new("[pool]\n");
    let (tempdir, database_path) = with_database_path();
    let source = tempdir.path().join("source/mono");
    std::fs::create_dir_all(source.join(".jj")).expect("canonical source");
    let (mut store, workspace_root) = seed_pool(&tempdir, &database_path, 1, Some(source.clone()));
    let repo_record = store.get_repo("mono").expect("get repo").expect("repo row");
    let record = store
        .list_workspaces("mono")
        .expect("list")
        .pop()
        .expect("one workspace");

    let runner = FakeRunner::new(vec![forget_registration(&workspace_root, &source, "mono-agent-001")]);
    store.forget_workspace("mono", "mono-agent-001").expect("forget row");
    let staged = stage_workspace_for_removal(&runner, &repo_record, &record, None).expect("stage");
    runner.assert_exhausted();

    assert!(!record.workspace_path.exists(), "the prefixed name is gone");
    assert!(staged.is_dir(), "the tree is still on disk, just out of circulation");
    assert_eq!(staged.file_name().unwrap(), ".removing-mono-agent-001");

    // Now do exactly what a concurrent lease would do next.
    let rediscovered = discover_workspaces(&repo_record).expect("discover");
    assert!(
        rediscovered.is_empty(),
        "a staged workspace must be invisible to rediscovery, got {rediscovered:?}",
    );
    store.sync_workspaces("mono", &rediscovered).expect("sync");
    assert!(
        store.list_workspaces("mono").expect("list").is_empty(),
        "the lease path must not be able to re-register a workspace being removed",
    );
}

#[test]
fn staging_clears_a_leftover_from_a_pass_that_died_mid_removal() {
    // A crash between the rename and the unlink leaves a dotted directory. It
    // is invisible and therefore harmless, but the next removal of the same id
    // has to be able to reuse the name — `fs::rename` onto a non-empty
    // directory fails.
    let _config = ConfigGuard::new("[pool]\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let repo_record = store.get_repo("mono").expect("get repo").expect("repo row");
    let record = store
        .list_workspaces("mono")
        .expect("list")
        .pop()
        .expect("one workspace");

    let leftover = workspace_root.join(".removing-mono-agent-001");
    std::fs::create_dir_all(leftover.join("target/debug")).expect("leftover");
    assert!(
        discover_workspaces(&repo_record).expect("discover").len() == 1,
        "only the live workspace is discoverable; the leftover is not",
    );

    let runner = FakeRunner::new(vec![]);
    store.forget_workspace("mono", "mono-agent-001").expect("forget row");
    let staged = stage_workspace_for_removal(&runner, &repo_record, &record, None).expect("stage");
    runner.assert_exhausted();

    assert_eq!(staged, leftover);
    assert!(
        staged.join(".jj").is_dir(),
        "the staged tree is the workspace, not the leftover"
    );
    assert!(!staged.join("target").exists(), "the leftover was cleared first");
}

#[test]
fn trim_removes_the_most_idle_surplus_down_to_the_mark() {
    let _config = ConfigGuard::new("[pool]\nmax-free-workspaces = 2\n");
    let (tempdir, database_path) = with_database_path();
    let source = tempdir.path().join("source/mono");
    std::fs::create_dir_all(source.join(".jj")).expect("canonical source");
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 4, Some(source.clone()));

    // 001 and 002 are the stalest; 003 and 004 were used recently and must
    // survive as warm capacity.
    set_last_activity(&database_path, "mono-agent-001", NOW - 90 * 3_600);
    set_last_activity(&database_path, "mono-agent-002", NOW - 80 * 3_600);
    set_last_activity(&database_path, "mono-agent-003", NOW - 2 * 3_600);
    set_last_activity(&database_path, "mono-agent-004", NOW - 3_600);

    let ws1 = workspace_root.join("mono-agent-001");
    let ws2 = workspace_root.join("mono-agent-002");
    let mut script = trim_probe_reusable(&ws1);
    script.push(forget_registration(&workspace_root, &source, "mono-agent-001"));
    // Discovered once, lazily, on the first removal's bazel-output-base
    // cleanup, then reused (not re-probed) for the second.
    script.push(bazel_root_probe(&source));
    script.extend(trim_probe_reusable(&ws2));
    script.push(forget_registration(&workspace_root, &source, "mono-agent-002"));
    let runner = FakeRunner::new(script);

    let report = trim_free_workspaces_to_mark(&runner, &store, Some(&database_path), NOW, None);
    runner.assert_exhausted();

    assert_eq!(report.workspaces_removed, 2);
    assert!(!ws1.exists(), "the stalest surplus workspace should be gone from disk");
    assert!(!ws2.exists());
    assert!(workspace_root.join("mono-agent-003").is_dir(), "warm capacity survives");
    assert!(workspace_root.join("mono-agent-004").is_dir());

    // And the registry agrees — a removal that left a dangling row would have
    // the next lease try to hand out a directory that is not there.
    let remaining = store.list_workspaces("mono").expect("list");
    let ids: Vec<&str> = remaining.iter().map(|r| r.workspace_id.as_str()).collect();
    assert_eq!(ids, vec!["mono-agent-003", "mono-agent-004"]);

    let removed: Vec<_> = audit_events(&tempdir)
        .into_iter()
        .filter(|e| e["event"] == "workspace.gc_removed")
        .collect();
    assert_eq!(removed.len(), 2);
    assert_eq!(removed[0]["mark"], 2);
    assert_eq!(removed[0]["free_before"], 4);
}

#[test]
fn trim_does_nothing_when_the_free_count_is_at_or_under_the_mark() {
    let _config = ConfigGuard::new("[pool]\nmax-free-workspaces = 4\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 4, None);
    for n in 1..=4 {
        set_last_activity(&database_path, &format!("mono-agent-{n:03}"), NOW - 500 * 3_600);
    }

    // No expectations: nothing should even be probed.
    let runner = FakeRunner::new(vec![]);
    let report = trim_free_workspaces_to_mark(&runner, &store, Some(&database_path), NOW, None);
    runner.assert_exhausted();

    assert_eq!(report.workspaces_removed, 0);
    for n in 1..=4 {
        assert!(workspace_root.join(format!("mono-agent-{n:03}")).is_dir());
    }
}

#[test]
fn trim_never_removes_a_workspace_holding_unpushed_work() {
    let _config = ConfigGuard::new("[pool]\nmax-free-workspaces = 1\n");
    let (tempdir, database_path) = with_database_path();
    let source = tempdir.path().join("source/mono");
    std::fs::create_dir_all(source.join(".jj")).expect("canonical source");
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 2, Some(source.clone()));
    set_last_activity(&database_path, "mono-agent-001", NOW - 90 * 3_600);
    set_last_activity(&database_path, "mono-agent-002", NOW - 10 * 3_600);

    let ws1 = workspace_root.join("mono-agent-001");
    let ws2 = workspace_root.join("mono-agent-002");
    // Both candidates hold work no remote has, so both must be skipped even
    // though that leaves the repo above its mark: the mark is a disk target,
    // not a promise that outranks somebody's afternoon. (Skipping one does not
    // end the pass — the trim keeps looking for an eligible victim, which is
    // why both get probed.)
    let mut script = trim_probe_holds_work(&ws1);
    script.extend(trim_probe_holds_work(&ws2));
    let runner = FakeRunner::new(script);
    let report = trim_free_workspaces_to_mark(&runner, &store, Some(&database_path), NOW, None);
    runner.assert_exhausted();

    assert_eq!(report.workspaces_removed, 0);
    assert!(ws1.is_dir(), "a workspace holding unpushed work must survive the trim");
    assert!(ws2.is_dir());

    let skipped: Vec<_> = audit_events(&tempdir)
        .into_iter()
        .filter(|e| e["event"] == "workspace.trim_skipped")
        .collect();
    assert_eq!(skipped.len(), 2);
    assert_eq!(skipped[0]["workspace_id"], "mono-agent-001");
    assert_eq!(skipped[0]["reason"], "holds_unpushed_work");
}

#[test]
fn trim_keeps_a_workspace_whose_reuse_probe_errors() {
    let _config = ConfigGuard::new("[pool]\nmax-free-workspaces = 1\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 2, None);
    set_last_activity(&database_path, "mono-agent-001", NOW - 90 * 3_600);
    set_last_activity(&database_path, "mono-agent-002", NOW - 10 * 3_600);

    let ws1 = workspace_root.join("mono-agent-001");
    let ws2 = workspace_root.join("mono-agent-002");
    let mut script = vec![ExpectedCommand::failing(
        ws1.clone(),
        "jj",
        &["git", "fetch"],
        "Error: could not reach the remote",
    )];
    // Having failed on 001 it moves on to 002, which is also refused for
    // holding work — so nothing is removed at all.
    script.extend(trim_probe_holds_work(&ws2));
    let runner = FakeRunner::new(script);

    let report = trim_free_workspaces_to_mark(&runner, &store, Some(&database_path), NOW, None);
    runner.assert_exhausted();

    assert_eq!(report.workspaces_removed, 0);
    assert!(ws1.is_dir(), "an unanswerable probe means keep, not remove");
}

#[test]
fn trim_neither_counts_nor_removes_leased_workspaces() {
    let _config = ConfigGuard::new("[pool]\nmax-free-workspaces = 2\n");
    let (tempdir, database_path) = with_database_path();
    let (mut store, workspace_root) = seed_pool(&tempdir, &database_path, 4, None);
    for n in 1..=4 {
        set_last_activity(&database_path, &format!("mono-agent-{n:03}"), NOW - 500 * 3_600);
    }

    // Two of the four are leased — by orphaned holders, as far as cube can
    // tell. That leaves exactly 2 free, which is the mark, so there is no
    // surplus and nothing to do.
    for id in ["mono-agent-001", "mono-agent-002"] {
        store
            .claim_specific_workspace(
                "mono",
                id,
                "boss",
                "task",
                &format!("lease-{id}"),
                NOW - 500 * 3_600,
                None,
            )
            .expect("claim")
            .expect("claimed");
    }

    let runner = FakeRunner::new(vec![]);
    let report = trim_free_workspaces_to_mark(&runner, &store, Some(&database_path), NOW, None);
    runner.assert_exhausted();

    assert_eq!(report.workspaces_removed, 0);
    for n in 1..=4 {
        assert!(
            workspace_root.join(format!("mono-agent-{n:03}")).is_dir(),
            "leases are honoured even when they look stale",
        );
    }
}

#[test]
fn a_row_whose_directory_is_already_gone_does_not_inflate_the_surplus() {
    // A phantom row — directory deleted out from under cube, reconcile has not
    // caught up — occupies no disk. Counting it as surplus would have the trim
    // remove that many *real* workspaces and leave the live free pool below
    // the mark, which is the opposite of what the mark is for.
    let _config = ConfigGuard::new("[pool]\nmax-free-workspaces = 2\n");
    let (tempdir, database_path) = with_database_path();
    let source = tempdir.path().join("source/mono");
    std::fs::create_dir_all(source.join(".jj")).expect("canonical source");
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 4, Some(source.clone()));
    set_last_activity(&database_path, "mono-agent-001", NOW - 90 * 3_600);
    set_last_activity(&database_path, "mono-agent-002", NOW - 80 * 3_600);
    set_last_activity(&database_path, "mono-agent-003", NOW - 2 * 3_600);
    set_last_activity(&database_path, "mono-agent-004", NOW - 3_600);

    // 001's directory is gone but its row is not, so 3 free workspaces
    // actually exist against a mark of 2: exactly one may go.
    std::fs::remove_dir_all(workspace_root.join("mono-agent-001")).expect("orphan the row");

    let ws2 = workspace_root.join("mono-agent-002");
    let mut script = trim_probe_reusable(&ws2);
    script.push(forget_registration(&workspace_root, &source, "mono-agent-002"));
    script.push(bazel_root_probe(&source));
    let runner = FakeRunner::new(script);

    let report = trim_free_workspaces_to_mark(&runner, &store, Some(&database_path), NOW, None);
    runner.assert_exhausted();

    assert_eq!(report.workspaces_removed, 1, "one real surplus workspace, not two");
    assert!(!ws2.exists());
    assert!(workspace_root.join("mono-agent-003").is_dir(), "the mark is respected");
    assert!(workspace_root.join("mono-agent-004").is_dir());
}

#[test]
fn a_removal_drops_the_registry_row_before_it_touches_the_directory() {
    // Ordering is the whole defence against handing a lease a directory that
    // is being unlinked: once the row is gone nothing can select the
    // workspace. So a removal interrupted between the two steps must leave a
    // stray directory (which the next lease rediscovers and re-registers) and
    // never a live row pointing at a half-deleted checkout.
    let _config = ConfigGuard::new("[pool]\nmax-free-workspaces = 1\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 2, None);
    set_last_activity(&database_path, "mono-agent-001", NOW - 90 * 3_600);
    set_last_activity(&database_path, "mono-agent-002", NOW - 10 * 3_600);

    let ws1 = workspace_root.join("mono-agent-001");
    let ws2 = workspace_root.join("mono-agent-002");
    // Make the unlink fail: a directory entry cannot be removed from a
    // read-only parent. Skip the assertions entirely if the running user
    // ignores that (a root-owned CI container would) rather than asserting
    // something vacuous.
    let mut perms = std::fs::metadata(&workspace_root).expect("root metadata").permissions();
    let original = perms.clone();
    perms.set_readonly(true);
    std::fs::set_permissions(&workspace_root, perms).expect("make the root read-only");
    let probe = workspace_root.join(".writable-probe");
    let unlink_is_blocked = std::fs::write(&probe, "").is_err();

    // 001's removal fails, so the pass moves on to 002 — which is refused for
    // holding work, leaving the repo above its mark and nothing removed.
    let mut script = trim_probe_reusable(&ws1);
    script.extend(trim_probe_holds_work(&ws2));
    let runner = FakeRunner::new(script);
    let report = if unlink_is_blocked {
        Some(trim_free_workspaces_to_mark(
            &runner,
            &store,
            Some(&database_path),
            NOW,
            None,
        ))
    } else {
        None
    };
    std::fs::set_permissions(&workspace_root, original).expect("restore the root");
    let _ = std::fs::remove_file(&probe);

    let Some(report) = report else {
        return;
    };
    runner.assert_exhausted();
    assert_eq!(report.workspaces_removed, 0, "the unlink failed, so nothing was freed");
    assert!(ws1.is_dir(), "the directory survives an unlink that could not run");
    let remaining = store.list_workspaces("mono").expect("list");
    let ids: Vec<&str> = remaining.iter().map(|r| r.workspace_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["mono-agent-002"],
        "the row is dropped first, so no lease can be handed the directory being removed",
    );
}

#[test]
fn surplus_backlog_is_visible_to_the_gc_throttle() {
    let _config = ConfigGuard::new("[pool]\nmax-free-workspaces = 2\n");
    let (tempdir, database_path) = with_database_path();
    let (mut store, _root) = seed_pool(&tempdir, &database_path, 3, None);
    assert!(has_free_workspace_surplus(&store), "3 free against a mark of 2");

    store
        .claim_specific_workspace("mono", "mono-agent-001", "boss", "t", "lease-1", NOW, None)
        .expect("claim")
        .expect("claimed");
    assert!(
        !has_free_workspace_surplus(&store),
        "leasing one drops the free count to the mark",
    );
}

#[test]
fn a_surplus_the_last_trim_could_not_act_on_does_not_pin_the_gc_at_five_minutes() {
    // A repo whose surplus is entirely unpushed work sits above its mark
    // permanently and correctly. Treating that steady state as backlog would
    // have every lease past the five-minute mark pay a synchronous pass whose
    // trim re-fetches those same candidates and removes nothing, forever.
    let _config = ConfigGuard::new("[pool]\nmax-free-workspaces = 2\n");
    let (tempdir, database_path) = with_database_path();
    let (mut store, _root) = seed_pool(&tempdir, &database_path, 3, None);
    let now = NOW;

    store.set_pool_metadata_i(POOL_GC_LAST_AT_KEY, now - 10 * 60).unwrap();
    // The previous pass walked every candidate it had and could remove none.
    store.set_pool_metadata_i(POOL_GC_TRIM_PROGRESS_KEY, 0).unwrap();
    assert!(
        has_free_workspace_surplus(&store),
        "the surplus is still there — it is just not actionable",
    );

    maybe_trigger_pool_gc(&mut store, Some(&database_path), now).expect("throttle");

    assert!(
        store.get_pool_metadata_i(POOL_GC_STARTED_AT_KEY).unwrap().is_none(),
        "an unactionable surplus must fall back to the 24h interval, not retry in 5 minutes",
    );
}

// ── Free space as an input ──────────────────────────────────────────────────

#[test]
fn no_disk_pressure_relief_when_the_volume_is_above_its_floor() {
    // A floor of one byte is below the injected test volume, so this
    // exercises the ordinary path: no reclamation, no audit noise, no cost.
    let _config = ConfigGuard::new("[pool]\nmin-free-bytes = 1\nmin-free-percent = 0\n");
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    write_artifact_tree(&workspace_root.join("mono-agent-001"), "target");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    let runner = FakeRunner::new(vec![]);
    let relief = relieve_disk_pressure(
        &runner,
        &store,
        Some(&database_path),
        "mono",
        &workspace_root,
        NOW,
        None,
    );
    runner.assert_exhausted();

    assert!(relief.is_none(), "an ample volume should not trigger reclamation");
    assert!(
        workspace_root.join("mono-agent-001/target").is_dir(),
        "nothing should have been reclaimed",
    );
}

#[test]
fn disk_pressure_compacts_before_the_lease_continues() {
    // A floor of u64::MAX is above the injected test volume, so the pressure
    // path always fires and no re-measure can ever clear it — the cheapest way
    // to exercise "below the floor, and still below it afterwards".
    let _config = ConfigGuard::new(
        "[pool]\n\
         min-free-bytes = 18446744073709551615\n\
         build-artifact-dirs = [\"target\"]\n",
    );
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    let target = write_artifact_tree(&ws, "target");
    // Recently used: the urgent pass ignores the idle window.
    set_last_activity(&database_path, "mono-agent-001", NOW - 60);

    let runner = FakeRunner::new(vec![tracked_probe(&ws, "target", "")]);
    let relief = relieve_disk_pressure(
        &runner,
        &store,
        Some(&database_path),
        "mono",
        &workspace_root,
        NOW,
        None,
    )
    .expect("relief should have run");
    runner.assert_exhausted();

    assert_eq!(relief.report.workspaces_compacted, 1);
    assert!(!target.exists(), "urgent compaction ignores the idle window");
    // The injected volume cannot clear a u64::MAX floor, so this stays false —
    // which is what makes a *mint* in the same lease call fail loudly, while
    // reusing an existing workspace carries on. The opposite arm is pinned by
    // `disk_pressure_that_recovers_lets_the_mint_through`.
    assert!(!relief.recovered);

    let pressure = audit_events(&tempdir)
        .into_iter()
        .find(|e| e["event"] == "pool.disk_pressure")
        .expect("pool.disk_pressure audit event");
    assert_eq!(pressure["repo"], "mono");
    assert_eq!(pressure["workspaces_compacted"], 1);
    assert_eq!(pressure["recovered"], false);
}

#[test]
fn disk_pressure_that_recovers_lets_the_mint_through() {
    // The whole point of reclaim-then-proceed: a lease that arrives on a
    // volume below its floor compacts, the re-measure clears the floor, and
    // the mint that would have been refused goes ahead instead. Scripting the
    // probe is the only way to express "space changed" — the first reading is
    // 1 GiB against the default `max(20 GiB, 2% of total)` floor, every
    // reading after it is the ample volume, which is what compaction is
    // standing in for.
    let _config = ConfigGuard::new("[pool]\nbuild-artifact-dirs = [\"target\"]\n");
    let _disk = with_readings(&[volume_with_free(1024 * 1024 * 1024), AMPLE_TEST_VOLUME]);
    let (tempdir, database_path) = with_database_path();
    let (store, workspace_root) = seed_pool(&tempdir, &database_path, 1, None);
    let ws = workspace_root.join("mono-agent-001");
    let target = write_artifact_tree(&ws, "target");
    set_last_activity(&database_path, "mono-agent-001", LONG_IDLE);

    let runner = FakeRunner::new(vec![tracked_probe(&ws, "target", "")]);
    let relief = relieve_disk_pressure(
        &runner,
        &store,
        Some(&database_path),
        "mono",
        &workspace_root,
        NOW,
        None,
    )
    .expect("the first reading is below the floor, so relief should have run");
    runner.assert_exhausted();

    assert_eq!(relief.report.workspaces_compacted, 1);
    assert!(!target.exists());
    assert!(relief.recovered, "the re-measure cleared the floor");
    assert_eq!(
        audit_events(&tempdir)
            .into_iter()
            .find(|e| e["event"] == "pool.disk_pressure")
            .expect("pool.disk_pressure audit event")["recovered"],
        true,
    );
    assert_mint_headroom("mono", &workspace_root).expect("a mint refused before reclamation must be allowed after it");
}

#[test]
fn a_mint_is_allowed_on_a_volume_above_its_floor() {
    // The ordinary case, and the one that must not depend on the host: the
    // default floor is `max(20 GiB, 2% of total)`, which the injected volume
    // clears with room to spare no matter how full the machine running the
    // suite happens to be.
    let _config = ConfigGuard::new("[pool]\n");
    let _disk = with_reading(AMPLE_TEST_VOLUME);
    let (tempdir, _database_path) = with_database_path();

    assert_mint_headroom("mono", tempdir.path()).expect("an ample volume must not refuse a mint");
}

#[test]
fn a_mint_is_refused_on_a_volume_below_its_floor() {
    // 7.4 GiB free against the default floor — the reading taken from a CI
    // agent that had filled up. Injected rather than provoked, because the
    // refusal is the behaviour under test and "run the suite on a full disk"
    // is not a test.
    let _config = ConfigGuard::new("[pool]\n");
    let _disk = with_reading(volume_with_free(7_945_670_000));
    let (tempdir, _database_path) = with_database_path();

    let err = assert_mint_headroom("mono", tempdir.path()).expect_err("a volume below its floor must refuse a mint");
    let CubeError::InsufficientDiskSpace {
        repo,
        available,
        floor,
        shortfall,
        ..
    } = err
    else {
        panic!("expected InsufficientDiskSpace, got {err:?}");
    };
    assert_eq!(repo, "mono");
    assert_eq!(available, "7.4 GiB");
    assert_eq!(
        floor, "20.5 GiB",
        "the floor is max(20 GiB, 2% of total), and 2% of 1 TiB just edges it out",
    );
    assert_eq!(shortfall, "13.1 GiB");
}
