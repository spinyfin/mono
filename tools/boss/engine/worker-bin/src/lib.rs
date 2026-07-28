//! Deterministic resolution of the `boss` CLI a worker will actually run.
//!
//! # The problem this exists to solve
//!
//! Workers are told (by the engine-authored `CLAUDE.md` / `AGENTS.md`) to
//! run `boss pr status` before deciding between `cube pr create` and
//! `cube pr update`. That instruction is only worth anything if the bare
//! word `boss` resolves to the CLI that ships with the running engine.
//!
//! It did not. The engine's guarantee was *positional*: `spawn_flow`
//! hands the pane a sanitized `PATH`, and the pane's first shell line
//! re-prepends `$BOSS_BIN_DIR` after the login shell's rc files have
//! rebuilt `PATH`. Both of those are one-shot prepends of a directory,
//! and both are no-ops when `BOSS_BIN_DIR` is unset — which is exactly
//! the case for an engine started from a checkout (`bazel run
//! //tools/boss/engine/core:engine`). In that mode no `boss` exists on
//! the worker's `PATH` at all except whatever the user's dotfiles put
//! there, and on a developer machine that is typically a
//! [repobin](../../../repobin) multiplexer symlink in `~/bin` which
//! *builds the CLI from source with Bazel on first use*.
//!
//! When that build fails the worker pays ~30 seconds, gets a wall of
//! Bazel output, and — as observed in the field — quietly drops the
//! `boss pr status` step and carries on making its create-vs-update
//! decision blind.
//!
//! # The fix
//!
//! The engine already *has* an already-built `boss`: bundled next to
//! itself in installed mode, and in its own runfiles in dev mode. This
//! crate resolves that binary by absolute path — deliberately **without
//! ever searching `PATH`**, since a `PATH` search is precisely how the
//! repobin shim wins — and materializes a tiny worker-owned launcher
//! directory containing exactly one executable, `boss`, that `exec`s it.
//!
//! Two properties are load-bearing:
//!
//! * **The launcher dir holds `boss` and nothing else.** Unlike
//!   prepending `$BOSS_BIN_DIR` (a bundle directory that also contains
//!   `bossctl`, `cube` and the engine), a dedicated dir cannot leak
//!   Boss-tier tooling into a worker session. `bossctl` is Boss-tier and
//!   stays off the worker's `PATH` — see [`launcher_names`].
//! * **An unresolvable `boss` fails loudly and instantly.** When
//!   resolution comes up empty the launcher is still written, but it
//!   prints a specific diagnostic and exits 127 immediately. It never
//!   falls through to a build-from-source shim and never exits 0. A
//!   silent no-op is the worst outcome; a fast, named failure is the
//!   whole point.
//!
//! Nothing here swallows a failure or substitutes a stale prebuilt: the
//! only binary the launcher will ever `exec` is the one shipped with the
//! engine that spawned the worker, which is the coherent build by
//! construction.

use std::io;
use std::path::{Path, PathBuf};

/// Basename of the launcher the engine writes for workers.
const BOSS_LAUNCHER_NAME: &str = "boss";

/// Basename of the repobin multiplexer. A candidate that resolves to
/// this is a build-from-source shim, not a built binary — see
/// [`is_build_from_source_shim`].
const REPOBIN_NAME: &str = "repobin";

/// Directory name, relative to the worker settings dir, that holds the
/// generated launcher(s).
pub const WORKER_BIN_SUBDIR: &str = "bin";

/// Name of the env var carrying the launcher dir to the worker. The
/// pane's first shell line prepends it to `PATH` *after* the
/// `$BOSS_BIN_DIR` prepend, so the launcher wins over the bundle.
pub const WORKER_BIN_DIR_ENV: &str = "BOSS_WORKER_BIN_DIR";

/// Env var that overrides `boss` CLI resolution outright. Tests and
/// operators use it; it is honoured before every filesystem candidate.
pub const BOSS_CLI_BIN_ENV: &str = "BOSS_CLI_BIN";

/// Every executable name the engine will ever write into the launcher
/// dir. Exactly one entry, and it is not `bossctl`.
///
/// The engine deliberately keeps `bossctl` off the worker `PATH`: it is
/// the Boss-tier control surface (host registry, agent fleet, engine
/// control) and a worker has no business holding it. Prepending
/// `$BOSS_BIN_DIR` — a whole bundle directory — cannot express that
/// distinction; a launcher dir can, and this constant is what the test
/// suite pins it to.
pub fn launcher_names() -> &'static [&'static str] {
    &[BOSS_LAUNCHER_NAME]
}

/// Resolve the absolute path of the `boss` CLI that belongs to the
/// running engine.
///
/// Mirrors the resolution order of the engine's `boss-event` resolver so
/// the two shims cannot disagree about which build is current:
///
/// 1. `BOSS_CLI_BIN` env override (caller-controlled; used as-is).
/// 2. `$BOSS_BIN_DIR/boss` — installed mode. The macOS app sets
///    `BOSS_BIN_DIR` to `Boss.app/Contents/Resources/bin`, where every
///    bundled CLI lives. Checked ahead of the dev-mode candidates so an
///    installed bundle never falls through to a workspace clone.
/// 3. `stable_bin_dir/boss` — the engine-installed copy under the Boss
///    state root, stable across `bazel clean` and workspace re-leases.
/// 4. Bazel runfiles beside the engine binary
///    (`<engine>.runfiles/_main/tools/boss/cli/boss`). Requires the
///    engine `rust_binary` to carry a `data` dep on
///    `//tools/boss/cli:boss`.
/// 5. `<workspace>/bazel-bin/tools/boss/cli/boss` when
///    `BUILD_WORKSPACE_DIRECTORY` is set (engine launched via
///    `bazel run` from a checkout).
/// 6. Engine-sibling `<engine_dir>/boss` (hand-built / cargo layout).
///
/// There is deliberately **no `PATH` fallback**. A `PATH` search is what
/// produced the bug: it finds the user's `~/bin/boss` repobin shim,
/// which builds the CLI from source on every cold cache. Returning
/// `None` and letting the caller write a loudly-failing launcher is
/// strictly better than resolving to something that may take 30 seconds
/// to not work.
///
/// Any candidate that is itself a build-from-source shim is skipped —
/// see [`is_build_from_source_shim`].
pub fn resolve_boss_cli(
    engine_path: &Path,
    workspace_dir: Option<&Path>,
    env_override: Option<&Path>,
    boss_bin_dir: Option<&Path>,
    stable_bin_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(override_path) = env_override {
        // Used as-is: an explicit operator/test override outranks every
        // heuristic, including the shim check.
        return Some(override_path.to_path_buf());
    }

    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(bin_dir) = boss_bin_dir {
        candidates.push(bin_dir.join(BOSS_LAUNCHER_NAME));
    }
    if let Some(bin_dir) = stable_bin_dir {
        candidates.push(bin_dir.join(BOSS_LAUNCHER_NAME));
    }

    let mut runfiles_root = engine_path.as_os_str().to_owned();
    runfiles_root.push(".runfiles");
    candidates.push(PathBuf::from(runfiles_root).join("_main").join("tools/boss/cli/boss"));

    if let Some(workspace) = workspace_dir {
        candidates.push(workspace.join("bazel-bin/tools/boss/cli/boss"));
    }
    if let Some(engine_dir) = engine_path.parent() {
        candidates.push(engine_dir.join(BOSS_LAUNCHER_NAME));
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.exists() && !is_build_from_source_shim(candidate))
}

/// Is `path` a repobin multiplexer rather than a built `boss` binary?
///
/// repobin installs its tools as symlinks whose target basename is
/// `repobin`; invoked under any of those names it looks the name up in
/// `REPOBIN.toml` and runs `bazel build` for the mapped target. Such a
/// path must never be handed to a worker as "the boss CLI" — it is a
/// build trigger, not a build.
///
/// Symlinks are followed, so `~/bin/boss -> repobin` is caught. A path
/// that cannot be canonicalised (broken symlink, permission error) is
/// reported as *not* a shim; the caller's `exists()` check already
/// excludes it, and guessing would only add a way to reject a
/// legitimate binary.
pub fn is_build_from_source_shim(path: &Path) -> bool {
    if path.file_name().is_some_and(|name| name == REPOBIN_NAME) {
        return true;
    }
    std::fs::canonicalize(path)
        .ok()
        .and_then(|target| target.file_name().map(|name| name == REPOBIN_NAME))
        .unwrap_or(false)
}

/// Render the `/bin/sh` launcher written to `<dir>/boss`.
///
/// `resolved` is the output of [`resolve_boss_cli`]. `Some` produces a
/// one-line `exec`; `None` produces a script that explains the situation
/// on stderr and exits 127 (`command not found`) without running
/// anything.
pub fn launcher_script(resolved: Option<&Path>) -> String {
    match resolved {
        Some(target) => format!(
            "#!/bin/sh\n\
             # Generated by the Boss engine for one worker session. Do not edit.\n\
             # Pins `boss` to the CLI shipped with the engine that spawned this\n\
             # worker, so a build-from-source shim on PATH can never shadow it.\n\
             exec {} \"$@\"\n",
            sh_quote(&target.to_string_lossy())
        ),
        None => {
            // `printf`, not a heredoc: a heredoc makes `sh` create a temp
            // file, which fails outright in a locked-down sandbox — and a
            // launcher whose whole job is to report a failure must not
            // have its own failure mode.
            let lines: Vec<String> = UNRESOLVED_MESSAGE.lines().map(sh_quote).collect();
            format!(
                "#!/bin/sh\n\
                 # Generated by the Boss engine for one worker session. Do not edit.\n\
                 printf '%s\\n' {} >&2\n\
                 exit 127\n",
                lines.join(" ")
            )
        }
    }
}

/// Body of the diagnostic printed by the unresolved launcher.
///
/// Deliberately names the failing invocation, the reason, and what a
/// worker should do about it, because the observed failure mode was a
/// worker treating an unreadable 30-second Bazel error as noise and
/// silently skipping the step.
const UNRESOLVED_MESSAGE: &str = "\
boss: unavailable in this worker session.

The Boss engine could not resolve an already-built `boss` CLI when it
spawned this worker, so there is no `boss` to run. This launcher exists
so that fact is immediate and visible: without it, `boss` would fall
through to a build-from-source shim on PATH (repobin), which spends ~30
seconds running `bazel build //tools/boss/cli:boss` before it can even
report a failure.

This is NOT something to work around, and it is NOT a reason to skip the
step you were about to run. Report it: say plainly, in your final
response, that `boss` was unavailable and name the command you could not
run. If you were about to run `boss pr status` to decide between
`cube pr create` and `cube pr update`, say so -- that decision is now
being made without it.

For whoever is reading this in an engine log: the engine looked for
$BOSS_CLI_BIN, $BOSS_BIN_DIR/boss, the stable bin dir, its own runfiles,
$BUILD_WORKSPACE_DIRECTORY/bazel-bin/tools/boss/cli/boss, and its own
sibling directory, and found none of them. Build it
(`bazel build //tools/boss/cli:boss`) or set BOSS_CLI_BIN.";

/// Write the worker launcher dir: create `dir`, then write `<dir>/boss`
/// with mode 0755. Returns the launcher's absolute path.
///
/// Rewritten unconditionally on every spawn so a worker never inherits a
/// launcher pointing at a previous engine's binary.
pub fn write_boss_launcher(dir: &Path, resolved: Option<&Path>) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(BOSS_LAUNCHER_NAME);
    std::fs::write(&path, launcher_script(resolved))?;
    set_executable(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Single-quote `value` for `/bin/sh`. A path is interpolated into the
/// generated script, so `$`, backticks and embedded quotes must not be
/// evaluable.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create an empty executable file at `path`, making parents as needed.
    fn touch_exe(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("candidate has a parent")).expect("mkdir");
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }

    // ── resolve_boss_cli ────────────────────────────────────────────────────

    #[test]
    fn env_override_wins_over_every_filesystem_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        touch_exe(&bundle.join("boss"));
        let override_path = tmp.path().join("elsewhere/boss");

        let resolved = resolve_boss_cli(
            &tmp.path().join("engine"),
            None,
            Some(override_path.as_path()),
            Some(bundle.as_path()),
            None,
        );
        assert_eq!(
            resolved.as_deref(),
            Some(override_path.as_path()),
            "BOSS_CLI_BIN must be used as-is, ahead of a present bundle copy"
        );
    }

    #[test]
    fn bundle_bin_dir_wins_over_dev_mode_candidates() {
        // Installed mode must never fall through to a workspace clone.
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let workspace = tmp.path().join("workspace");
        touch_exe(&bundle.join("boss"));
        touch_exe(&workspace.join("bazel-bin/tools/boss/cli/boss"));

        let resolved = resolve_boss_cli(
            &tmp.path().join("engine"),
            Some(workspace.as_path()),
            None,
            Some(bundle.as_path()),
            None,
        );
        assert_eq!(resolved, Some(bundle.join("boss")));
    }

    #[test]
    fn resolves_from_engine_runfiles_in_dev_mode() {
        // `bazel run //tools/boss/engine/core:engine` sets no
        // BOSS_BIN_DIR; the runfiles copy is what saves the worker from
        // PATH-resolving a repobin shim.
        let tmp = tempfile::tempdir().unwrap();
        let engine = tmp.path().join("engine");
        touch_exe(&engine);
        let runfiles = tmp.path().join("engine.runfiles/_main/tools/boss/cli/boss");
        touch_exe(&runfiles);

        assert_eq!(resolve_boss_cli(&engine, None, None, None, None), Some(runfiles));
    }

    #[test]
    fn resolves_from_workspace_bazel_bin_when_runfiles_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let built = workspace.join("bazel-bin/tools/boss/cli/boss");
        touch_exe(&built);

        assert_eq!(
            resolve_boss_cli(&tmp.path().join("engine"), Some(workspace.as_path()), None, None, None),
            Some(built)
        );
    }

    #[test]
    fn resolves_engine_sibling_last() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = tmp.path().join("bin/engine");
        touch_exe(&engine);
        let sibling = tmp.path().join("bin/boss");
        touch_exe(&sibling);

        assert_eq!(resolve_boss_cli(&engine, None, None, None, None), Some(sibling));
    }

    #[test]
    fn returns_none_rather_than_searching_path() {
        // The whole defect was a PATH search finding ~/bin/boss. With no
        // candidate on disk the answer must be None so the caller writes
        // a loudly-failing launcher.
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_boss_cli(&tmp.path().join("engine"), None, None, None, None),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn skips_a_repobin_symlink_candidate_and_falls_through() {
        // A bin dir whose `boss` is a repobin symlink must be rejected,
        // not handed to the worker, even though it exists.
        let tmp = tempfile::tempdir().unwrap();
        let shimmed = tmp.path().join("home-bin");
        touch_exe(&shimmed.join("repobin"));
        std::os::unix::fs::symlink(shimmed.join("repobin"), shimmed.join("boss")).expect("symlink");

        let real = tmp.path().join("stable");
        touch_exe(&real.join("boss"));

        let resolved = resolve_boss_cli(
            &tmp.path().join("engine"),
            None,
            None,
            Some(shimmed.as_path()),
            Some(real.as_path()),
        );
        assert_eq!(
            resolved,
            Some(real.join("boss")),
            "a repobin symlink must be skipped in favour of a real build"
        );
    }

    #[cfg(unix)]
    #[test]
    fn all_candidates_shimmed_resolves_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        let shimmed = tmp.path().join("home-bin");
        touch_exe(&shimmed.join("repobin"));
        std::os::unix::fs::symlink(shimmed.join("repobin"), shimmed.join("boss")).expect("symlink");

        assert_eq!(
            resolve_boss_cli(&tmp.path().join("engine"), None, None, Some(shimmed.as_path()), None),
            None,
            "resolution must fail rather than return a build-from-source shim"
        );
    }

    // ── is_build_from_source_shim ───────────────────────────────────────────

    #[test]
    fn a_path_literally_named_repobin_is_a_shim() {
        assert!(is_build_from_source_shim(Path::new("/anywhere/repobin")));
    }

    #[test]
    fn a_plain_missing_path_is_not_reported_as_a_shim() {
        assert!(!is_build_from_source_shim(Path::new("/nonexistent/boss")));
    }

    // ── launcher contents ───────────────────────────────────────────────────

    #[test]
    fn resolved_launcher_execs_the_absolute_path() {
        let script = launcher_script(Some(Path::new("/opt/boss/bin/boss")));
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(
            script.contains("exec '/opt/boss/bin/boss' \"$@\""),
            "launcher must exec the resolved absolute path: {script}"
        );
    }

    #[test]
    fn resolved_launcher_quotes_paths_that_would_otherwise_be_evaluated() {
        // A path containing `$(...)`/backticks must not be evaluated by
        // the shell when the launcher runs.
        let script = launcher_script(Some(Path::new("/tmp/a $(rm -rf /) `x`/boss")));
        assert!(
            script.contains("exec '/tmp/a $(rm -rf /) `x`/boss' \"$@\""),
            "path must be single-quoted verbatim: {script}"
        );
    }

    #[test]
    fn resolved_launcher_escapes_embedded_single_quotes() {
        let script = launcher_script(Some(Path::new("/tmp/it's/boss")));
        assert!(
            script.contains(r#"exec '/tmp/it'\''s/boss' "$@""#),
            "embedded quote must be escaped: {script}"
        );
    }

    #[test]
    fn unresolved_launcher_fails_loudly_and_does_not_exit_zero() {
        let script = launcher_script(None);
        assert!(script.contains("exit 127"), "must exit non-zero: {script}");
        assert!(!script.contains("exec "), "must not exec anything: {script}");
        assert!(
            script.contains("boss: unavailable in this worker session."),
            "must name the failure: {script}"
        );
        assert!(
            script.contains("Report it"),
            "must tell the worker to surface it rather than skip the step: {script}"
        );
    }

    #[test]
    fn unresolved_launcher_never_mentions_a_build_from_source_fallback_as_a_remedy() {
        // The worker must not be nudged into invoking repobin by hand.
        let script = launcher_script(None);
        assert!(
            !script.contains("repobin exec"),
            "must not suggest dispatching through repobin: {script}"
        );
    }

    #[test]
    fn scripts_are_valid_shell() {
        for script in [launcher_script(Some(Path::new("/tmp/boss"))), launcher_script(None)] {
            let mut child = std::process::Command::new("sh")
                .arg("-n")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("sh must be available");
            {
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .expect("stdin")
                    .write_all(script.as_bytes())
                    .expect("write script");
            }
            let status = child.wait().expect("wait");
            assert!(status.success(), "generated launcher must parse: {script}");
        }
    }

    // ── write_boss_launcher ─────────────────────────────────────────────────

    #[test]
    fn writes_only_boss_and_never_bossctl() {
        // The Boss-tier / worker-tier distinction: this directory is
        // prepended to the worker's PATH, so anything written here is
        // handed to the worker. `bossctl` must never be.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        write_boss_launcher(&dir, Some(Path::new("/opt/boss/bin/boss"))).expect("write");

        let mut entries: Vec<String> = std::fs::read_dir(&dir)
            .expect("readdir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, vec!["boss".to_owned()]);
        assert_eq!(launcher_names(), ["boss"]);
        assert!(
            !launcher_names().contains(&"bossctl"),
            "bossctl is Boss-tier and must stay off the worker PATH"
        );
    }

    #[cfg(unix)]
    #[test]
    fn written_launcher_is_executable_and_runs() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real-boss");
        std::fs::write(&target, "#!/bin/sh\necho ran \"$@\"\n").expect("write target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let dir = tmp.path().join("bin");
        let launcher = write_boss_launcher(&dir, Some(target.as_path())).expect("write launcher");
        let mode = std::fs::metadata(&launcher).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o755);

        let out = std::process::Command::new(&launcher)
            .args(["pr", "status"])
            .output()
            .expect("run launcher");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ran pr status");
    }

    #[cfg(unix)]
    #[test]
    fn written_unresolved_launcher_exits_127_with_a_message_on_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        let launcher = write_boss_launcher(&dir, None).expect("write launcher");

        let out = std::process::Command::new(&launcher)
            .args(["pr", "status"])
            .output()
            .expect("run launcher");
        assert_eq!(out.status.code(), Some(127), "must exit 127 (command not found)");
        assert!(out.stdout.is_empty(), "diagnostic belongs on stderr, not stdout");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("boss: unavailable in this worker session."),
            "stderr must name the failure: {stderr}"
        );
    }

    #[test]
    fn rewrites_a_stale_launcher_in_place() {
        // A re-spawn must not inherit a previous engine's target.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        write_boss_launcher(&dir, Some(Path::new("/old/boss"))).expect("first write");
        let launcher = write_boss_launcher(&dir, Some(Path::new("/new/boss"))).expect("second write");

        let body = std::fs::read_to_string(&launcher).expect("read");
        assert!(body.contains("'/new/boss'"), "{body}");
        assert!(!body.contains("'/old/boss'"), "{body}");
    }
}
