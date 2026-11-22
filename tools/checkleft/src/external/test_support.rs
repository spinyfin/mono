//! Test-only helpers shared by checkleft's component-runtime tests.
//!
//! The headline helper, [`executor_with_precompiled_cache`], points a
//! [`DefaultExternalCheckExecutor`] at the build-time precompiled `.cwasm`
//! fixture directory (`//tools/checkleft:precompiled_cwasm`) so the heavy
//! `rust/giant-structs` tests *deserialize* the AOT artifact (~20 ms) instead of
//! JIT-compiling the component at runtime (~10 s in a `fastbuild`/debug test
//! binary, stacking up under test parallelism). That runtime compile is what
//! pushed `checkleft_lib_test` toward its 60 s `size = "small"` timeout — see
//! `docs/investigations/checkleft-lib-test-wasm-compile-timeout.md`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::DefaultExternalCheckExecutor;
use crate::input::{ChangeKind, ChangeSet, ChangedFile, DiffHunk, FileDiff};
use crate::source_tree::BaseCommandRunner;

/// Whether a staged runfiles path is expected to be a regular file (a binary)
/// or a directory (a fixture tree). Distinguishes the `exists()` vs `is_dir()`
/// existence check in [`staged_path_from_runfiles`].
pub(crate) enum StagedPathKind {
    File,
    Dir,
}

/// Resolve a Bazel-staged `data` dependency (binary or directory) from the
/// test's runfiles via `env_var`, or `None` when it was not staged (e.g.
/// running under plain `cargo test`, where there are no runfiles).
///
/// Shared by every hermetic e2e test that stages a real external tool
/// (buildifier, rustfmt) or fixture directory (the precompiled `.cwasm`
/// cache) as Bazel `data` + `env`: under `bazel test` the env var is always
/// set to the dependency's runfiles path, so the staged path is always taken
/// in CI. When the env var is absent we *assert* we are genuinely outside
/// Bazel (no `TEST_SRCDIR`), so a broken `data`/`env` wiring can never
/// silently skip the test it backs instead of failing loudly. `what` names
/// the dependency in the resulting panic messages (e.g. `"buildifier"`,
/// `"rustfmt wrapper"`, `"precompiled .cwasm dir"`).
pub(crate) fn staged_path_from_runfiles(env_var: &str, what: &str, kind: StagedPathKind) -> Option<PathBuf> {
    match std::env::var(env_var) {
        Ok(rlocationpath) => {
            let runfiles = runfiles::Runfiles::create().expect("runfiles must initialize under `bazel test`");
            let path = runfiles
                .rlocation(&rlocationpath)
                .unwrap_or_else(|| panic!("{what} rlocation must resolve"));
            let staged = match kind {
                StagedPathKind::File => path.exists(),
                StagedPathKind::Dir => path.is_dir(),
            };
            assert!(staged, "staged {what} must exist at {}", path.display());
            Some(path)
        }
        Err(_) => {
            assert!(
                std::env::var_os("TEST_SRCDIR").is_none(),
                "running under `bazel test` but {env_var} is unset — the {what} `data`/`env` \
                 wiring on checkleft_lib_test is broken; refusing to silently skip the test it backs"
            );
            None
        }
    }
}

/// Resolve the build-time precompiled `.cwasm` fixture directory from the test's
/// runfiles, or `None` when it was not staged (e.g. running under plain
/// `cargo test`, where there are no runfiles).
///
/// Under `bazel test`, `checkleft_lib_test` sets `CHECKLEFT_PRECOMPILED_CWASM_DIR`
/// to the runfiles path of the `:precompiled_cwasm` directory via
/// `$(rlocationpath ...)`, so the precompiled path is always taken in CI. A
/// broken `data`/`env` wiring can never silently fall back to JIT-compiling the
/// component and re-open the timeout (see [`staged_path_from_runfiles`]).
pub(crate) fn precompiled_cwasm_dir() -> Option<PathBuf> {
    staged_path_from_runfiles(
        "CHECKLEFT_PRECOMPILED_CWASM_DIR",
        "precompiled .cwasm dir",
        StagedPathKind::Dir,
    )
}

/// Build a [`DefaultExternalCheckExecutor`] rooted at `root`, pointed at the
/// build-time precompiled `.cwasm` cache when available (under `bazel test`) so
/// component checks deserialize the AOT fixture instead of compiling at runtime.
///
/// Outside Bazel (`cargo test`) there is no fixture, so it falls back to the
/// platform-cache-backed `::new` constructor — correct, just slower on a cold
/// cache.
pub(crate) fn executor_with_precompiled_cache(root: &Path) -> DefaultExternalCheckExecutor {
    match precompiled_cwasm_dir() {
        Some(dir) => {
            DefaultExternalCheckExecutor::new_with_cache(root, dir).expect("executor with precompiled .cwasm cache")
        }
        None => DefaultExternalCheckExecutor::new(root).expect("executor"),
    }
}

/// Test double for [`BaseCommandRunner`] that records `(binary, args)` calls
/// and serves canned responses keyed by the path argument.
pub(crate) struct RecordingBaseCommandRunner {
    responses: HashMap<PathBuf, Vec<u8>>,
    calls: Mutex<Vec<(String, Vec<String>)>>,
}

impl RecordingBaseCommandRunner {
    pub(crate) fn new(responses: Vec<(&str, &[u8])>) -> Self {
        Self {
            responses: responses
                .into_iter()
                .map(|(path, bytes)| (PathBuf::from(path), bytes.to_vec()))
                .collect(),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }

    pub(crate) fn assert_jj_base_reads(&self, expected_rev: &str, expected_paths: &[&str]) {
        let calls = self.calls();
        assert_eq!(
            calls.len(),
            expected_paths.len(),
            "expected {} unique jj base reads, got: {calls:?}",
            expected_paths.len()
        );
        let read_paths: HashSet<&str> = calls
            .iter()
            .map(|(binary, args)| {
                assert_eq!(binary, "jj");
                assert_eq!(
                    &args[..4],
                    ["file", "show", "--ignore-working-copy", "-r"],
                    "every base jj read must pass --ignore-working-copy"
                );
                assert_eq!(args[4], expected_rev);
                args[5].as_str()
            })
            .collect();
        let expected: HashSet<&str> = expected_paths.iter().copied().collect();
        assert_eq!(read_paths, expected);
    }
}

impl BaseCommandRunner for RecordingBaseCommandRunner {
    fn run(&self, _root: &Path, binary: &str, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        self.calls
            .lock()
            .unwrap()
            .push((binary.to_owned(), args.iter().map(|s| (*s).to_owned()).collect()));
        let path = args.last().map(PathBuf::from).unwrap_or_default();
        self.responses
            .get(&path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing base file {}", path.display()))
    }
}

/// Shared fixture for tests verifying base-file read deduplication across
/// `proto/breaking` (modified, deleted, renamed, added) and `file/ifchange`
/// (`declared-files` scope).
pub(crate) fn base_reads_fixture(root: &Path) -> (Arc<RecordingBaseCommandRunner>, ChangeSet) {
    fs::create_dir_all(root.join("api")).expect("create api dir");
    // Modified proto: field `old_name` (tag 1) removed without reservation.
    fs::write(
        root.join("api/modified.proto"),
        "syntax = \"proto3\"; package p; message M {}\n",
    )
    .unwrap();
    // Renamed proto (from api/old_renamed.proto -> api/new_renamed.proto): field tag 1 removed.
    fs::write(
        root.join("api/new_renamed.proto"),
        "syntax = \"proto3\"; package p; message R {}\n",
    )
    .unwrap();
    // Added proto: new file, no base read needed.
    fs::write(
        root.join("api/added.proto"),
        "syntax = \"proto3\"; package p; message Added { string id = 1; }\n",
    )
    .unwrap();
    // Modified rs file with removed LINT.IfChange block and unupdated target.
    fs::write(root.join("changed.rs"), "pub fn updated() {}\n").unwrap();
    fs::write(root.join("companion.txt"), "unchanged\n").unwrap();

    let runner = Arc::new(RecordingBaseCommandRunner::new(vec![
        (
            "api/modified.proto",
            b"syntax = \"proto3\"; package p; message M { string x = 1; }\n",
        ),
        (
            "api/deleted.proto",
            b"syntax = \"proto3\"; package p; message Gone {}\n",
        ),
        (
            "api/old_renamed.proto",
            b"syntax = \"proto3\"; package p; message R { int32 old_id = 1; }\n",
        ),
        (
            "changed.rs",
            b"// LINT.IfChange\npub fn old() {}\n// LINT.ThenChange(companion.txt)\n",
        ),
    ]));

    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: PathBuf::from("api/modified.proto"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: PathBuf::from("api/deleted.proto"),
            kind: ChangeKind::Deleted,
            old_path: None,
        },
        ChangedFile {
            path: PathBuf::from("api/new_renamed.proto"),
            kind: ChangeKind::Renamed,
            old_path: Some(PathBuf::from("api/old_renamed.proto")),
        },
        ChangedFile {
            path: PathBuf::from("api/added.proto"),
            kind: ChangeKind::Added,
            old_path: None,
        },
        ChangedFile {
            path: PathBuf::from("changed.rs"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ])
    .with_file_diff(
        PathBuf::from("changed.rs"),
        FileDiff {
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 3,
                new_start: 1,
                new_lines: 1,
                added_lines: 1,
                removed_lines: 3,
            }],
            added_line_ranges: vec![(1, 1)],
        },
    );

    (runner, changeset)
}
