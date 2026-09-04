//! Deterministic resolution of engine-owned binaries a worker will actually run.
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
//! repobin shim wins — and materializes a tiny per-workspace launcher
//! directory containing exactly one executable, `boss`, that `exec`s it.
//!
//! Two properties are load-bearing:
//!
//! * **The launcher dir holds only worker-safe launchers.** Unlike
//!   prepending `$BOSS_BIN_DIR` (a bundle directory that also contains
//!   `bossctl`, `cube` and the engine), a dedicated dir cannot leak
//!   Boss-tier tooling into a worker session. `bossctl` is Boss-tier and
//!   stays off the worker's `PATH`. A derived-PR worker also gets a `cube`
//!   wrapper that passes an engine-owned opaque prefix to `cube pr create`.
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
use std::time::{SystemTime, UNIX_EPOCH};

/// Basename of the launcher the engine writes for workers.
const BOSS_LAUNCHER_NAME: &str = "boss";

/// Basename of the optional wrapper that composes an engine-owned PR body
/// (origin header + worker body) for PR-creating cube calls in one worker
/// session. Cube itself gains no provenance awareness — the wrapper hands it
/// an ordinary `--body-file`.
const CUBE_LAUNCHER_NAME: &str = "cube";

/// Basename of the repobin multiplexer. A candidate that resolves to
/// this is a build-from-source shim, not a built binary — see
/// [`is_build_from_source_shim`].
const REPOBIN_NAME: &str = "repobin";

/// Basename of the optional `checkleft` launcher. Written only when the
/// workspace's `REPOBIN.toml` declares `checkleft` as a repobin tool
/// ([`repobin_declares_tool`]); it pins the bare word to the
/// repobin-managed copy in `<workspace>/bin/` and fails loudly when that
/// copy is missing — never a PATH copy. See [`write_checkleft_launcher`].
const CHECKLEFT_LAUNCHER_NAME: &str = "checkleft";

/// Repo-root file repobin reads its tool declarations from.
pub const REPOBIN_CONFIG_FILE: &str = "REPOBIN.toml";

/// Directory, relative to a workspace root, that `repobin install` (or a
/// repo's own shim installer, e.g. mono's `.cube/setup.yaml` step)
/// populates with one entry per declared tool.
pub const WORKSPACE_REPOBIN_BIN_SUBDIR: &str = "bin";

/// Workspace-relative path of the `boss` CLI under runfiles / bazel-bin.
const BOSS_CLI_RUNFILES_REL: &str = "tools/boss/cli/boss";

/// Workspace-relative path of the `boss-event` shim under runfiles / bazel-bin.
const BOSS_EVENT_RUNFILES_REL: &str = "tools/boss/event-shim/boss-event";

/// Directory name, relative to the worker settings dir, that holds the
/// generated launcher(s). Each workspace gets its own subdirectory under
/// this (keyed by workspace name, same scheme as worker settings files).
pub const WORKER_BIN_SUBDIR: &str = "bin";

/// Name of the env var carrying the launcher dir to the worker. The
/// pane's first shell line prepends it to `PATH` *after* the
/// `$BOSS_BIN_DIR` prepend, so the launcher wins over the bundle.
pub const WORKER_BIN_DIR_ENV: &str = "BOSS_WORKER_BIN_DIR";

/// Env var carrying the absolute path of this workspace's `boss` launcher.
/// Workers are taught to invoke [`WORKER_BOSS_INVOCATION`] rather than the
/// bare word `boss`: a driver shell snapshot can demote the launcher
/// directory on `PATH`, and a PATH entry is not a binary.
pub const BOSS_BIN_ENV: &str = "BOSS_BIN";

/// Env var carrying the absolute path of this workspace's `cube` launcher.
/// Same contract as [`BOSS_BIN_ENV`]: name the binary, not a PATH entry.
pub const CUBE_BIN_ENV: &str = "CUBE_BIN";

/// Shell token workers are taught to run instead of a PATH-resolved `boss`.
pub const WORKER_BOSS_INVOCATION: &str = r#""$BOSS_BIN""#;

/// Shell token workers are taught to run instead of a PATH-resolved `cube`.
pub const WORKER_CUBE_INVOCATION: &str = r#""$CUBE_BIN""#;

/// Env var that overrides `boss` CLI resolution outright. Tests and
/// operators use it; it is honoured before every filesystem candidate.
pub const BOSS_CLI_BIN_ENV: &str = "BOSS_CLI_BIN";

/// Env var that overrides `cube` CLI resolution outright. Same contract
/// as [`BOSS_CLI_BIN_ENV`].
pub const CUBE_CLI_BIN_ENV: &str = "CUBE_CLI_BIN";

/// Workspace-relative path of the `cube` CLI under runfiles / bazel-bin.
const CUBE_CLI_RUNFILES_REL: &str = "tools/cube/cube";

/// Every executable name the engine may write into the launcher directory.
/// `boss` is always present; `cube` is always present too (a thin exec of
/// the bundled CLI, overwritten by the derived-PR compose wrapper when
/// that worker needs it); `checkleft` is present only when the workspace's
/// `REPOBIN.toml` declares it ([`write_checkleft_launcher`]) and then
/// execs the repobin-managed copy in the workspace's `bin/` — a repo that
/// does not route checkleft through repobin keeps PATH behaviour for the
/// bare word. No entry exposes the Boss-tier `bossctl`
/// control surface.
///
/// The engine deliberately keeps `bossctl` off the worker `PATH`: it is
/// the Boss-tier control surface (host registry, agent fleet, engine
/// control) and a worker has no business holding it. Prepending
/// `$BOSS_BIN_DIR` — a whole bundle directory — cannot express that
/// distinction; a launcher dir can, and this constant is what the test
/// suite pins it to.
pub fn launcher_names() -> &'static [&'static str] {
    &[BOSS_LAUNCHER_NAME, CUBE_LAUNCHER_NAME, CHECKLEFT_LAUNCHER_NAME]
}

/// Path / env inputs shared by every engine-binary resolution.
///
/// Grouped so [`resolve_engine_binary`] stays under the clippy argument
/// limit while still exposing the full candidate set (bundle, stable,
/// runfiles, bazel-bin, sibling) to both `boss` and `boss-event`.
#[derive(Debug, Clone, Copy)]
pub struct ResolvePaths<'a> {
    pub engine_path: &'a Path,
    pub workspace_dir: Option<&'a Path>,
    pub env_override: Option<&'a Path>,
    pub boss_bin_dir: Option<&'a Path>,
    /// Engine-installed stable bin dir. Pass `None` for binaries that
    /// are never installed there (notably `boss`); pass `Some` for
    /// `boss-event`, which the engine copies on startup.
    pub stable_bin_dir: Option<&'a Path>,
}

/// Shared resolution of an engine-owned binary by absolute path.
///
/// Order (no `PATH` search):
///
/// 1. `paths.env_override` (caller-controlled; used as-is, including shims).
/// 2. `$boss_bin_dir/<basename>` — installed-mode bundle directory.
/// 3. `$stable_bin_dir/<basename>` when provided (engine-installed copy
///    under the Boss state root; used for `boss-event`, not for `boss`).
/// 4. Bazel runfiles beside the engine binary
///    (`<engine>.runfiles/_main/<runfiles_relpath>`).
/// 5. `<workspace>/bazel-bin/<runfiles_relpath>` when
///    `workspace_dir` is set (engine launched via `bazel run`).
/// 6. Engine-sibling `<engine_dir>/<basename>`.
///
/// When `reject_build_from_source_shim` is true, candidates whose
/// basename (or canonical target basename) is `repobin` are skipped —
/// see [`is_build_from_source_shim`]. Env overrides are never rejected.
pub fn resolve_engine_binary(
    basename: &str,
    runfiles_relpath: &str,
    paths: ResolvePaths<'_>,
    reject_build_from_source_shim: bool,
) -> Option<PathBuf> {
    if let Some(override_path) = paths.env_override {
        // Used as-is: an explicit operator/test override outranks every
        // heuristic, including the shim check.
        return Some(override_path.to_path_buf());
    }

    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(bin_dir) = paths.boss_bin_dir {
        candidates.push(bin_dir.join(basename));
    }
    if let Some(bin_dir) = paths.stable_bin_dir {
        candidates.push(bin_dir.join(basename));
    }

    let mut runfiles_root = paths.engine_path.as_os_str().to_owned();
    runfiles_root.push(".runfiles");
    candidates.push(PathBuf::from(runfiles_root).join("_main").join(runfiles_relpath));

    if let Some(workspace) = paths.workspace_dir {
        candidates.push(workspace.join("bazel-bin").join(runfiles_relpath));
    }
    if let Some(engine_dir) = paths.engine_path.parent() {
        candidates.push(engine_dir.join(basename));
    }

    candidates.into_iter().find(|candidate| {
        candidate.exists() && !(reject_build_from_source_shim && is_build_from_source_shim(candidate))
    })
}

/// Resolve the absolute path of the `boss` CLI that belongs to the
/// running engine.
///
/// Thin wrapper over [`resolve_engine_binary`] with the `boss` basename
/// and runfiles path. Deliberately does **not** consult a stable-bin-dir
/// candidate: nothing installs `boss` there (only `boss-event` is copied
/// at engine startup), so including it would prefer a stale leftover
/// over a current runfiles / bazel-bin build.
///
/// Resolution order:
///
/// 1. `BOSS_CLI_BIN` env override (caller-controlled; used as-is).
/// 2. `$BOSS_BIN_DIR/boss` — installed mode. The macOS app sets
///    `BOSS_BIN_DIR` to `Boss.app/Contents/Resources/bin`, where every
///    bundled CLI lives. Checked ahead of the dev-mode candidates so an
///    installed bundle never falls through to a workspace clone.
/// 3. Bazel runfiles beside the engine binary
///    (`<engine>.runfiles/_main/tools/boss/cli/boss`). Requires the
///    engine `rust_binary` to carry a `data` dep on
///    `//tools/boss/cli:boss`.
/// 4. `<workspace>/bazel-bin/tools/boss/cli/boss` when
///    `BUILD_WORKSPACE_DIRECTORY` is set (engine launched via
///    `bazel run` from a checkout).
/// 5. Engine-sibling `<engine_dir>/boss` (hand-built / cargo layout).
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
) -> Option<PathBuf> {
    resolve_engine_binary(
        BOSS_LAUNCHER_NAME,
        BOSS_CLI_RUNFILES_REL,
        ResolvePaths {
            engine_path,
            workspace_dir,
            env_override,
            boss_bin_dir,
            stable_bin_dir: None,
        },
        true,
    )
}

/// Resolve the absolute path of the `cube` CLI that belongs to the
/// running engine. Same candidate order and shim rejection as
/// [`resolve_boss_cli`], with the `cube` basename and runfiles path.
///
/// Ordinary workers previously had no `cube` launcher at all, so a
/// Codex shell snapshot that demoted the bundle dir on `PATH` sent
/// every `cube` invocation through repobin. The launcher written next
/// to `boss` closes that, and [`CUBE_BIN_ENV`] names it explicitly.
pub fn resolve_cube_cli(
    engine_path: &Path,
    workspace_dir: Option<&Path>,
    env_override: Option<&Path>,
    boss_bin_dir: Option<&Path>,
) -> Option<PathBuf> {
    resolve_engine_binary(
        CUBE_LAUNCHER_NAME,
        CUBE_CLI_RUNFILES_REL,
        ResolvePaths {
            engine_path,
            workspace_dir,
            env_override,
            boss_bin_dir,
            stable_bin_dir: None,
        },
        true,
    )
}

/// Resolve the absolute path of the `boss-event` shim that belongs to
/// the running engine.
///
/// Thin wrapper over [`resolve_engine_binary`] with the `boss-event`
/// basename and runfiles path. Keeps the same order as the historical
/// in-engine resolver, including the engine-installed stable bin dir
/// (copied at startup so hook paths baked into worker settings.json
/// survive `bazel clean`).
///
/// Resolution order:
///
/// 1. `BOSS_EVENT_BIN` env override (caller-controlled; used as-is).
/// 2. `$BOSS_BIN_DIR/boss-event` — installed-mode path.
/// 3. `stable_bin_dir/boss-event` — the copy installed by the engine at
///    startup into the Boss state root.
/// 4. Bazel runfiles
///    (`<engine>.runfiles/_main/tools/boss/event-shim/boss-event`).
/// 5. `<workspace>/bazel-bin/tools/boss/event-shim/boss-event`.
/// 6. Engine-sibling `<engine_dir>/boss-event`.
///
/// Returns `None` when no candidate resolves. Callers that bake the path
/// into hook commands treat `None` as a hard error (typically panic) —
/// a bare `boss-event` name fails silently under a sanitized PATH.
pub fn resolve_boss_event_binary(
    engine_path: &Path,
    workspace_dir: Option<&Path>,
    env_override: Option<&Path>,
    boss_bin_dir: Option<&Path>,
    stable_bin_dir: Option<&Path>,
) -> Option<PathBuf> {
    resolve_engine_binary(
        "boss-event",
        BOSS_EVENT_RUNFILES_REL,
        ResolvePaths {
            engine_path,
            workspace_dir,
            env_override,
            boss_bin_dir,
            stable_bin_dir,
        },
        // Same shim rejection as `boss`: a repobin-backed boss-event is
        // not a usable hook binary either.
        true,
    )
}

/// Is `path` a repobin multiplexer rather than a built binary?
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

/// First executable named `name` on a `PATH`-shaped string, or `None`.
///
/// Does not consult the process `PATH`; the caller passes the environment
/// the command will actually see. Absolute `name` values are returned as-is
/// when they exist.
pub fn resolve_on_path(name: &str, path: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.is_absolute() {
        return candidate.exists().then(|| candidate.to_path_buf());
    }
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// If `program` (a first-token argv, possibly an absolute path) would
/// resolve on `path` to a build-from-source shim, return that resolved
/// path. Used by tests and by the PreToolUse guard's documented contract;
/// the runtime guard is the Python in `BOSS_LAUNCH_GUARD_COMMAND`.
pub fn resolves_to_build_from_source_shim(program: &str, path: &str) -> Option<PathBuf> {
    let resolved = resolve_on_path(program, path)?;
    is_build_from_source_shim(&resolved).then_some(resolved)
}

/// Whether `token` names the engine-owned `boss` CLI, including the
/// `"$BOSS_BIN"` / `$BOSS_BIN` form workers are taught and an absolute
/// path whose basename is `boss`.
pub fn is_boss_cli_token(token: &str) -> bool {
    is_named_cli_token(token, BOSS_LAUNCHER_NAME, BOSS_BIN_ENV)
}

/// Whether `token` names the engine-owned `cube` CLI. Same shapes as
/// [`is_boss_cli_token`].
pub fn is_cube_cli_token(token: &str) -> bool {
    is_named_cli_token(token, CUBE_LAUNCHER_NAME, CUBE_BIN_ENV)
}

fn is_named_cli_token(token: &str, basename: &str, env_var: &str) -> bool {
    let token = token.trim_matches(|c| c == '"' || c == '\'');
    if token == basename {
        return true;
    }
    if token == format!("${env_var}") || token == format!("${{{env_var}}}") {
        return true;
    }
    Path::new(token).file_name().is_some_and(|name| name == basename)
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
             # Generated by the Boss engine for this workspace's worker session.\n\
             # Do not edit. Pins `boss` to the CLI shipped with the engine that\n\
             # spawned this worker, so a build-from-source shim on PATH can never\n\
             # shadow it.\n\
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
                 # Generated by the Boss engine for this workspace's worker session.\n\
                 # Do not edit.\n\
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
$BOSS_CLI_BIN, $BOSS_BIN_DIR/boss, its own runfiles,
$BUILD_WORKSPACE_DIRECTORY/bazel-bin/tools/boss/cli/boss, and its own
sibling directory, and found none of them. Build it
(`bazel build //tools/boss/cli:boss`) or set BOSS_CLI_BIN.";

/// Write the per-workspace `boss` launcher, atomically replacing an older
/// launcher with mode 0755. Returns the launcher's absolute path.
///
/// Rewritten unconditionally on every spawn so a worker never inherits a
/// launcher pointing at a previous engine's binary. The write is atomic
/// (temp sibling + `rename`) so a concurrent reader/`exec` of `boss`
/// never observes a truncated file.
///
/// Does not touch a co-located `cube` launcher: [`write_cube_launcher`]
/// (or the derived-PR compose wrapper) owns that file. A non-derived
/// spawn must still call [`write_cube_launcher`] so a stale compose
/// wrapper from a previous worker in this workspace cannot linger.
pub fn write_boss_launcher(dir: &Path, resolved: Option<&Path>) -> io::Result<PathBuf> {
    write_launcher(dir, BOSS_LAUNCHER_NAME, &launcher_script(resolved))
}

/// Write the per-workspace `cube` launcher, atomically replacing an older
/// launcher (including a stale derived-PR compose wrapper) with mode 0755.
///
/// `resolved` is the output of [`resolve_cube_cli`]. `Some` produces a
/// one-line `exec` of that absolute path; `None` produces a loudly-failing
/// launcher, same contract as [`write_boss_launcher`]. A derived-PR spawn
/// overwrites this afterwards with [`write_cube_pr_body_compose_launcher`].
pub fn write_cube_launcher(dir: &Path, resolved: Option<&Path>) -> io::Result<PathBuf> {
    write_launcher(dir, CUBE_LAUNCHER_NAME, &launcher_script(resolved))
}

/// Absolute path of the `boss` launcher inside `dir`.
pub fn boss_bin_in(dir: &Path) -> PathBuf {
    dir.join(BOSS_LAUNCHER_NAME)
}

/// Absolute path of the `cube` launcher inside `dir`.
pub fn cube_bin_in(dir: &Path) -> PathBuf {
    dir.join(CUBE_LAUNCHER_NAME)
}

/// Absolute path of the `checkleft` launcher inside `dir` (present only
/// when [`write_checkleft_launcher`] returned `Some`).
pub fn checkleft_bin_in(dir: &Path) -> PathBuf {
    dir.join(CHECKLEFT_LAUNCHER_NAME)
}

/// Whether `<workspace>/REPOBIN.toml` declares `tool` — as a
/// `[tools.<tool>]` target built from the checkout or a `[pins.<tool>]`
/// upstream pin.
///
/// A line scan, not a TOML parse: this crate has no dependencies by
/// design, and the two table-header shapes are the whole contract
/// (repobin's own `install` writes one symlink per such header). A missing
/// or unreadable file means "not declared".
pub fn repobin_declares_tool(workspace: &Path, tool: &str) -> bool {
    let Ok(contents) = std::fs::read_to_string(workspace.join(REPOBIN_CONFIG_FILE)) else {
        return false;
    };
    let tools_header = format!("[tools.{tool}]");
    let pins_header = format!("[pins.{tool}]");
    contents
        .lines()
        .map(str::trim)
        .any(|line| line == tools_header || line == pins_header)
}

/// Absolute path of the repobin-managed `checkleft` entry in `workspace`.
pub fn workspace_checkleft_path(workspace: &Path) -> PathBuf {
    workspace
        .join(WORKSPACE_REPOBIN_BIN_SUBDIR)
        .join(CHECKLEFT_LAUNCHER_NAME)
}

/// Render the `/bin/sh` launcher written to `<dir>/checkleft`.
///
/// Unlike [`launcher_script`], the target is not resolved at spawn time:
/// the repobin entry may legitimately appear after spawn (a repo's shim
/// installer runs at lease; a human may run `repobin install`), so the
/// launcher checks for it on every invocation and `exec`s it when present.
/// When absent it prints [`CHECKLEFT_UNAVAILABLE_MESSAGE`] and exits 127 —
/// it never searches PATH, because a PATH `checkleft` on a developer host
/// is typically a stale `cargo install checkleft` whose verdict says
/// nothing about the gate CI enforces.
pub fn checkleft_launcher_script(workspace: &Path) -> String {
    let target = workspace_checkleft_path(workspace).to_string_lossy().into_owned();
    // `printf`, not a heredoc, for the same sandbox reason as
    // `launcher_script`.
    let lines: Vec<String> = CHECKLEFT_UNAVAILABLE_MESSAGE
        .replace("{path}", &target)
        .lines()
        .map(sh_quote)
        .collect();
    format!(
        "#!/bin/sh\n\
         # Generated by the Boss engine for this workspace's worker session.\n\
         # Do not edit. Pins `checkleft` to the repobin-managed copy in this\n\
         # workspace's bin/ (its REPOBIN.toml declares checkleft), so a stale\n\
         # PATH copy -- e.g. an old `cargo install checkleft` -- can never run\n\
         # in its place. Missing copy => loud exit 127, never a PATH search.\n\
         target={}\n\
         if [ -x \"$target\" ]; then\n\
         \x20 exec \"$target\" \"$@\"\n\
         fi\n\
         printf '%s\\n' {} >&2\n\
         exit 127\n",
        sh_quote(&target),
        lines.join(" ")
    )
}

/// Body of the diagnostic printed by the `checkleft` launcher when the
/// repobin-managed entry is absent. `{path}` is the entry's absolute path.
///
/// Same rationale as [`UNRESOLVED_MESSAGE`]: name the failure, the reason,
/// and what to do — the observed failure mode was a worker whose
/// `./bin/checkleft run` failed retrying with a PATH lookup that silently
/// ran an ancient crates.io build and reporting "checkleft passed cleanly".
const CHECKLEFT_UNAVAILABLE_MESSAGE: &str = "\
checkleft: refusing to run -- the repobin-managed checkleft is missing.

This workspace's REPOBIN.toml declares `checkleft` as a repobin tool, so
the only checkleft a worker may run is the one repobin installs at
  {path}
and that path is missing or not executable. This launcher exists so the
fact is immediate and visible: without it a bare `checkleft` would fall
through to whatever copy is on PATH (on a developer host, typically an
ancient `cargo install checkleft`) -- not the program CI runs, and a
verdict from it means nothing.

Do NOT work around this by running a PATH copy, by `cargo install`ing
one, or by skipping the check. Populate bin/ the way this repository
documents (see its AGENTS.md; for a repobin repo that is its cube setup
step, or `repobin install --bin-dir bin/ --no-defaults` from the
workspace root), then retry. If that is not possible, say plainly in
your final response that checkleft was unavailable and name the command
you could not run.";

/// Write (or remove) the per-workspace `checkleft` launcher.
///
/// Written only when [`repobin_declares_tool`] finds `checkleft` in the
/// workspace's `REPOBIN.toml`; a repo that does not route checkleft
/// through repobin keeps today's PATH behaviour. When it does not, any
/// `checkleft` launcher left in `dir` by an earlier spawn is removed so a
/// stale one cannot linger. Returns the launcher path when one was
/// written. Same atomic write contract as [`write_boss_launcher`].
pub fn write_checkleft_launcher(dir: &Path, workspace: &Path) -> io::Result<Option<PathBuf>> {
    if !repobin_declares_tool(workspace, CHECKLEFT_LAUNCHER_NAME) {
        return match std::fs::remove_file(checkleft_bin_in(dir)) {
            Ok(()) => Ok(None),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        };
    }
    write_launcher(dir, CHECKLEFT_LAUNCHER_NAME, &checkleft_launcher_script(workspace)).map(Some)
}

/// Write the optional per-worker `cube` wrapper that composes `body_header`
/// with the worker's ordinary `--body` / `--body-file` into a single temp
/// body file, then invokes real `cube pr create` / `ensure` with only
/// `--body-file` (no cube feature flags). The wrapper removes its own
/// directory from `PATH` before delegating so it cannot recurse into itself.
pub fn write_cube_pr_body_compose_launcher(dir: &Path, body_header: &str) -> io::Result<PathBuf> {
    write_launcher(
        dir,
        CUBE_LAUNCHER_NAME,
        &cube_pr_body_compose_launcher_script(body_header),
    )
}

fn write_launcher(dir: &Path, launcher_name: &str, script: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(launcher_name);

    // Unique sibling so concurrent writers (different pids / threads)
    // do not clobber each other's temp files before rename.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".{launcher_name}.{}.{}.tmp", std::process::id(), nanos));

    let result = (|| {
        std::fs::write(&tmp, script)?;
        set_executable(&tmp)?;
        // rename is atomic on the same filesystem; replaces an existing
        // `boss` without an O_TRUNC window that concurrent execs could see.
        std::fs::rename(&tmp, &path)?;
        Ok(path.clone())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
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

/// Shell script that rewrites `cube pr create|ensure` so the engine-owned
/// header is part of the ordinary body cube already accepts.
///
/// Composition happens entirely outside cube: the wrapper strips any worker
/// `--body` / `--body-file`, writes `header` (+ optional worker body) to a
/// temp file, and re-invokes real cube with only `--body-file`. Cube never
/// learns about provenance, prefixes, or Boss.
fn cube_pr_body_compose_launcher_script(body_header: &str) -> String {
    // Embedded header is single-quoted via sh_quote so `$`, backticks, and
    // quotes in the header cannot be shell-evaluated when the script runs.
    format!(
        r##"#!/bin/sh
if [ -n "${{{env}:-}}" ]; then
    PATH="${{PATH#"${{{env}}}:"}}"
    export PATH
fi
if [ "$1" = "pr" ]; then
    case "$2" in
        create|ensure)
            subcommand="$2"
            shift 2
            header={header}
            worker_body=""
            worker_body_file=""
            has_body=0
            has_body_file=0
            # Rebuild non-body args with shell-safe quoting so `eval set --`
            # restores them exactly for the real cube invocation.
            kept=""
            q() {{
                # POSIX single-quote escape: foo'bar → 'foo'\''bar'
                printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\''/g")"
            }}
            add_kept() {{
                kept="$kept $(q "$1")"
            }}
            while [ "$#" -gt 0 ]; do
                case "$1" in
                    --body)
                        if [ "$has_body" -eq 1 ] || [ "$has_body_file" -eq 1 ]; then
                            printf '%s\n' "cube wrapper: --body specified more than once or with --body-file" >&2
                            exit 2
                        fi
                        if [ "$#" -lt 2 ]; then
                            printf '%s\n' "cube wrapper: --body requires a value" >&2
                            exit 2
                        fi
                        has_body=1
                        worker_body="$2"
                        shift 2
                        ;;
                    --body=*)
                        if [ "$has_body" -eq 1 ] || [ "$has_body_file" -eq 1 ]; then
                            printf '%s\n' "cube wrapper: --body specified more than once or with --body-file" >&2
                            exit 2
                        fi
                        has_body=1
                        worker_body="${{1#--body=}}"
                        shift
                        ;;
                    --body-file)
                        if [ "$has_body" -eq 1 ] || [ "$has_body_file" -eq 1 ]; then
                            printf '%s\n' "cube wrapper: --body-file specified more than once or with --body" >&2
                            exit 2
                        fi
                        if [ "$#" -lt 2 ]; then
                            printf '%s\n' "cube wrapper: --body-file requires a value" >&2
                            exit 2
                        fi
                        has_body_file=1
                        worker_body_file="$2"
                        shift 2
                        ;;
                    --body-file=*)
                        if [ "$has_body" -eq 1 ] || [ "$has_body_file" -eq 1 ]; then
                            printf '%s\n' "cube wrapper: --body-file specified more than once or with --body" >&2
                            exit 2
                        fi
                        has_body_file=1
                        worker_body_file="${{1#--body-file=}}"
                        shift
                        ;;
                    *)
                        add_kept "$1"
                        shift
                        ;;
                esac
            done
            body_tmp=$(mktemp "${{TMPDIR:-/tmp}}/boss-pr-body.XXXXXX") || exit 1
            if [ "$has_body_file" -eq 1 ]; then
                {{
                    printf '%s\n\n' "$header"
                    cat -- "$worker_body_file"
                }} > "$body_tmp" || exit 1
            elif [ "$has_body" -eq 1 ]; then
                printf '%s\n\n%s' "$header" "$worker_body" > "$body_tmp" || exit 1
            else
                # Header-only body: a derived PR must still carry its origin
                # link even when the worker supplied no description.
                printf '%s' "$header" > "$body_tmp" || exit 1
            fi
            eval "set -- $kept"
            exec cube pr "$subcommand" --body-file "$body_tmp" "$@"
            ;;
    esac
fi
exec cube "$@"
"##,
        env = WORKER_BIN_DIR_ENV,
        header = sh_quote(body_header),
    )
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

        assert_eq!(resolve_boss_cli(&engine, None, None, None), Some(runfiles));
    }

    #[test]
    fn resolves_from_workspace_bazel_bin_when_runfiles_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let built = workspace.join("bazel-bin/tools/boss/cli/boss");
        touch_exe(&built);

        assert_eq!(
            resolve_boss_cli(&tmp.path().join("engine"), Some(workspace.as_path()), None, None),
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

        assert_eq!(resolve_boss_cli(&engine, None, None, None), Some(sibling));
    }

    #[test]
    fn returns_none_rather_than_searching_path() {
        // The whole defect was a PATH search finding ~/bin/boss. With no
        // candidate on disk the answer must be None so the caller writes
        // a loudly-failing launcher.
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(resolve_boss_cli(&tmp.path().join("engine"), None, None, None), None);
    }

    #[test]
    fn does_not_consult_stable_bin_dir() {
        // Nothing installs boss into the stable bin dir; a leftover
        // there must not win over a current runfiles build (and must
        // not be the only candidate that makes resolution "succeed").
        let tmp = tempfile::tempdir().unwrap();
        let stable = tmp.path().join("stable");
        touch_exe(&stable.join("boss"));
        let engine = tmp.path().join("engine");
        touch_exe(&engine);
        let runfiles = tmp.path().join("engine.runfiles/_main/tools/boss/cli/boss");
        touch_exe(&runfiles);

        // resolve_boss_cli has no stable_bin_dir parameter — even with a
        // real file sitting under a stable-like path it is invisible.
        assert_eq!(
            resolve_boss_cli(&engine, None, None, None),
            Some(runfiles),
            "runfiles must win; stable_bin is not a candidate for boss"
        );
        // And with only a stable-like leftover and no real candidates:
        let empty_engine = tmp.path().join("other-engine");
        touch_exe(&empty_engine);
        assert_eq!(
            resolve_boss_cli(&empty_engine, None, None, None),
            None,
            "a leftover under some other bin dir is not consulted"
        );
        // Shared helper with stable_bin still finds it (boss-event path).
        assert_eq!(
            resolve_engine_binary(
                "boss",
                BOSS_CLI_RUNFILES_REL,
                ResolvePaths {
                    engine_path: &empty_engine,
                    workspace_dir: None,
                    env_override: None,
                    boss_bin_dir: None,
                    stable_bin_dir: Some(stable.as_path()),
                },
                true,
            ),
            Some(stable.join("boss")),
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

        let engine = tmp.path().join("engine");
        touch_exe(&engine);
        let runfiles = tmp.path().join("engine.runfiles/_main/tools/boss/cli/boss");
        touch_exe(&runfiles);

        let resolved = resolve_boss_cli(&engine, None, None, Some(shimmed.as_path()));
        assert_eq!(
            resolved,
            Some(runfiles),
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
            resolve_boss_cli(&tmp.path().join("engine"), None, None, Some(shimmed.as_path())),
            None,
            "resolution must fail rather than return a build-from-source shim"
        );
    }

    // ── resolve_boss_event_binary ───────────────────────────────────────────

    #[test]
    fn boss_event_prefers_stable_bin_over_runfiles() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = tmp.path().join("engine");
        touch_exe(&engine);
        let stable = tmp.path().join("stable");
        touch_exe(&stable.join("boss-event"));
        let runfiles = tmp
            .path()
            .join("engine.runfiles/_main/tools/boss/event-shim/boss-event");
        touch_exe(&runfiles);

        assert_eq!(
            resolve_boss_event_binary(&engine, None, None, None, Some(stable.as_path())),
            Some(stable.join("boss-event")),
        );
    }

    #[test]
    fn boss_event_and_boss_share_resolution_order() {
        // Same candidate classes, different basename/runfiles path.
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        touch_exe(&bundle.join("boss"));
        touch_exe(&bundle.join("boss-event"));

        assert_eq!(
            resolve_boss_cli(&tmp.path().join("engine"), None, None, Some(bundle.as_path())),
            Some(bundle.join("boss")),
        );
        assert_eq!(
            resolve_boss_event_binary(&tmp.path().join("engine"), None, None, Some(bundle.as_path()), None,),
            Some(bundle.join("boss-event")),
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
    fn unresolved_message_does_not_claim_stable_bin_is_checked() {
        let script = launcher_script(None);
        assert!(
            !script.contains("stable bin"),
            "boss resolution no longer consults the stable bin dir: {script}"
        );
    }

    #[test]
    fn scripts_are_valid_shell() {
        for script in [
            launcher_script(Some(Path::new("/tmp/boss"))),
            launcher_script(None),
            checkleft_launcher_script(Path::new("/tmp/it's a workspace")),
        ] {
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

    // ── worker launchers ────────────────────────────────────────────────────

    #[test]
    fn writes_boss_without_bossctl() {
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
        assert_eq!(launcher_names(), ["boss", "cube", "checkleft"]);
        assert!(
            !launcher_names().contains(&"bossctl"),
            "bossctl is Boss-tier and must stay off the worker PATH"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cube_compose_launcher_passes_full_body_via_body_file_when_worker_supplies_none() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let wrapper_dir = tmp.path().join("worker-bin");
        let real_bin = tmp.path().join("real-bin");
        let captured = tmp.path().join("captured-args");
        let captured_body = tmp.path().join("captured-body");
        let real_cube = real_bin.join("cube");
        std::fs::create_dir_all(&real_bin).expect("mkdir real bin");
        // Fake cube records argv and the contents of any --body-file so the
        // test can prove composition happened outside cube (ordinary flag).
        std::fs::write(
            &real_cube,
            "#!/bin/sh\n\
             : > \"$CUBE_CAPTURED_ARGS\"\n\
             prev=\n\
             for arg in \"$@\"; do\n\
               printf '%s\\n' \"$arg\" >> \"$CUBE_CAPTURED_ARGS\"\n\
               if [ \"$prev\" = \"--body-file\" ]; then\n\
                 cp -- \"$arg\" \"$CUBE_CAPTURED_BODY\"\n\
               fi\n\
               prev=$arg\n\
             done\n",
        )
        .expect("write fake cube");
        std::fs::set_permissions(&real_cube, std::fs::Permissions::from_mode(0o755)).expect("chmod fake cube");

        let header = "## Boss follow-up\n\nKeep `$(this text)` literal.";
        let launcher = write_cube_pr_body_compose_launcher(&wrapper_dir, header).expect("write wrapper");
        let path = format!("{}:{}:/usr/bin:/bin", wrapper_dir.display(), real_bin.display());
        let status = std::process::Command::new(&launcher)
            .args(["pr", "create", "--title", "Example"])
            .env(WORKER_BIN_DIR_ENV, &wrapper_dir)
            .env("PATH", path)
            .env("CUBE_CAPTURED_ARGS", &captured)
            .env("CUBE_CAPTURED_BODY", &captured_body)
            .status()
            .expect("run wrapper");
        assert!(status.success());

        let args = std::fs::read_to_string(&captured).expect("read captured args");
        assert!(
            args.starts_with("pr\ncreate\n--body-file\n"),
            "wrapper must pass ordinary --body-file, not a cube prefix flag; got:\n{args}"
        );
        assert!(
            args.contains("\n--title\nExample\n"),
            "other flags must be preserved; got:\n{args}"
        );
        assert!(
            !args.contains("body-prefix"),
            "cube must not receive a body-prefix feature flag; got:\n{args}"
        );
        assert_eq!(
            std::fs::read_to_string(&captured_body).expect("read captured body"),
            header,
            "header-only create must submit the engine header as the full body"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cube_compose_launcher_prepends_header_to_worker_body_file() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let wrapper_dir = tmp.path().join("worker-bin");
        let real_bin = tmp.path().join("real-bin");
        let captured_body = tmp.path().join("captured-body");
        let worker_body = tmp.path().join("worker-body.md");
        std::fs::write(&worker_body, "## Summary\n\nDo the thing.\n").expect("write worker body");
        let real_cube = real_bin.join("cube");
        std::fs::create_dir_all(&real_bin).expect("mkdir real bin");
        std::fs::write(
            &real_cube,
            "#!/bin/sh\n\
             prev=\n\
             for arg in \"$@\"; do\n\
               if [ \"$prev\" = \"--body-file\" ]; then\n\
                 cp -- \"$arg\" \"$CUBE_CAPTURED_BODY\"\n\
               fi\n\
               prev=$arg\n\
             done\n",
        )
        .expect("write fake cube");
        std::fs::set_permissions(&real_cube, std::fs::Permissions::from_mode(0o755)).expect("chmod fake cube");

        let header = "## Boss follow-up\n\nOrigin link.";
        let launcher = write_cube_pr_body_compose_launcher(&wrapper_dir, header).expect("write wrapper");
        let path = format!("{}:{}:/usr/bin:/bin", wrapper_dir.display(), real_bin.display());
        let status = std::process::Command::new(&launcher)
            .args([
                "pr",
                "create",
                "--title",
                "Example",
                "--body-file",
                worker_body.to_str().unwrap(),
            ])
            .env(WORKER_BIN_DIR_ENV, &wrapper_dir)
            .env("PATH", path)
            .env("CUBE_CAPTURED_BODY", &captured_body)
            .status()
            .expect("run wrapper");
        assert!(status.success());

        assert_eq!(
            std::fs::read_to_string(&captured_body).expect("read captured body"),
            format!("{header}\n\n## Summary\n\nDo the thing.\n"),
        );
    }

    #[test]
    fn writing_the_cube_launcher_replaces_a_stale_body_compose_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        write_cube_pr_body_compose_launcher(&dir, "## Stale").expect("write compose wrapper");
        write_boss_launcher(&dir, Some(Path::new("/opt/boss/bin/boss"))).expect("refresh boss launcher");
        // A non-derived spawn must still replace the compose wrapper; writing
        // only `boss` would leave the previous worker's header in place.
        write_cube_launcher(&dir, Some(Path::new("/opt/boss/bin/cube"))).expect("refresh cube launcher");

        assert!(dir.join(BOSS_LAUNCHER_NAME).exists());
        let cube = std::fs::read_to_string(dir.join(CUBE_LAUNCHER_NAME)).expect("read cube launcher");
        assert!(
            cube.contains("exec '/opt/boss/bin/cube'"),
            "stale compose wrapper must be replaced with a thin exec: {cube}"
        );
        assert!(
            !cube.contains("## Stale"),
            "a non-derived worker must not inherit a prior worker's body-compose wrapper"
        );
    }

    #[test]
    fn a_repobin_symlink_on_path_is_detected_as_a_shim_invocation() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let repobin = bin.join("repobin");
        std::fs::write(&repobin, b"#!/bin/sh\n").unwrap();
        let boss = bin.join("boss");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repobin, &boss).unwrap();
        #[cfg(not(unix))]
        std::fs::copy(&repobin, &boss).unwrap();

        let path = format!("{}:/usr/bin", bin.display());
        let hit = resolves_to_build_from_source_shim("boss", &path);
        assert_eq!(hit.as_deref(), Some(boss.as_path()));
        assert!(is_build_from_source_shim(&boss));
    }

    #[test]
    fn a_real_binary_on_path_is_not_a_shim_invocation() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let boss = bin.join("boss");
        std::fs::write(&boss, b"#!/bin/sh\n").unwrap();
        let path = format!("{}:/usr/bin", bin.display());
        assert_eq!(resolves_to_build_from_source_shim("boss", &path), None);
    }

    #[test]
    fn named_cli_tokens_cover_env_var_and_absolute_forms() {
        assert!(is_boss_cli_token("boss"));
        assert!(is_boss_cli_token("\"$BOSS_BIN\""));
        assert!(is_boss_cli_token("$BOSS_BIN"));
        assert!(is_boss_cli_token("${BOSS_BIN}"));
        assert!(is_boss_cli_token("/Applications/Boss.app/Contents/Resources/bin/boss"));
        assert!(!is_boss_cli_token("bossctl"));
        assert!(!is_boss_cli_token("notboss"));

        assert!(is_cube_cli_token("cube"));
        assert!(is_cube_cli_token("\"$CUBE_BIN\""));
        assert!(is_cube_cli_token("$CUBE_BIN"));
        assert!(is_cube_cli_token("/opt/bin/cube"));
        assert!(!is_cube_cli_token("notcube"));
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

    #[test]
    fn atomic_rewrite_leaves_no_temp_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        write_boss_launcher(&dir, Some(Path::new("/opt/boss"))).expect("write");
        write_boss_launcher(&dir, Some(Path::new("/opt/boss-v2"))).expect("rewrite");

        let mut entries: Vec<String> = std::fs::read_dir(&dir)
            .expect("readdir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            vec!["boss".to_owned()],
            "temp siblings from atomic write must be cleaned up / renamed away"
        );
    }

    // ── checkleft launcher ──────────────────────────────────────────────────

    fn write_repobin_toml(workspace: &Path, body: &str) {
        std::fs::create_dir_all(workspace).expect("mkdir workspace");
        std::fs::write(workspace.join(REPOBIN_CONFIG_FILE), body).expect("write REPOBIN.toml");
    }

    #[test]
    fn repobin_declares_tool_matches_tools_and_pins_headers_only() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        write_repobin_toml(
            &ws,
            "version = 1\n\n[tools.boss]\ntarget = \"//tools/boss/cli:boss\"\n\n  [pins.checkleft]  \nrepo = \"x\"\n",
        );
        assert!(repobin_declares_tool(&ws, "boss"));
        assert!(
            repobin_declares_tool(&ws, "checkleft"),
            "pins headers count, whitespace trimmed"
        );
        assert!(!repobin_declares_tool(&ws, "bos"), "must match the whole header");
        assert!(!repobin_declares_tool(&ws, "cube"));
        assert!(
            !repobin_declares_tool(&tmp.path().join("no-such-workspace"), "checkleft"),
            "no REPOBIN.toml means not declared"
        );
    }

    #[test]
    fn checkleft_launcher_is_written_only_when_repobin_declares_it_and_stale_ones_are_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        let ws = tmp.path().join("ws");

        // No REPOBIN.toml at all: nothing written.
        std::fs::create_dir_all(&ws).unwrap();
        assert_eq!(write_checkleft_launcher(&dir, &ws).expect("write"), None);
        assert!(!checkleft_bin_in(&dir).exists());

        // Declared: written, and it names the workspace's bin/checkleft.
        write_repobin_toml(
            &ws,
            "version = 1\n[tools.checkleft]\ntarget = \"//tools/checkleft:checkleft\"\n",
        );
        let launcher = write_checkleft_launcher(&dir, &ws)
            .expect("write")
            .expect("declared => launcher");
        assert_eq!(launcher, checkleft_bin_in(&dir));
        let body = std::fs::read_to_string(&launcher).unwrap();
        assert!(
            body.contains(&sh_quote(&workspace_checkleft_path(&ws).to_string_lossy())),
            "{body}"
        );

        // No longer declared: the stale launcher must not linger.
        write_repobin_toml(&ws, "version = 1\n[tools.boss]\ntarget = \"//tools/boss/cli:boss\"\n");
        assert_eq!(write_checkleft_launcher(&dir, &ws).expect("write"), None);
        assert!(
            !checkleft_bin_in(&dir).exists(),
            "stale checkleft launcher must be removed"
        );
        // And removing an absent one is not an error.
        assert_eq!(write_checkleft_launcher(&dir, &ws).expect("write"), None);
    }

    #[test]
    fn checkleft_launcher_never_searches_path_and_never_exits_zero_when_missing() {
        let script = checkleft_launcher_script(Path::new("/ws"));
        assert!(script.contains("exit 127"), "{script}");
        let code: Vec<&str> = script
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect();
        let code = code.join("\n");
        assert!(
            !code.contains("$PATH")
                && !code.contains("${PATH")
                && !code.contains("command -v")
                && !code.contains("which "),
            "must not consult PATH: {script}"
        );
        assert!(script.contains("'/ws/bin/checkleft'"), "{script}");
        assert!(
            script.contains("cargo install"),
            "must name the failure mode it exists to prevent: {script}"
        );
        assert!(
            !script.contains("repobin exec"),
            "must not nudge the worker into dispatching through repobin by hand: {script}"
        );
    }

    /// A fake `checkleft` in `dir` that records its argv and prints `tag`.
    #[cfg(unix)]
    fn fake_checkleft(dir: &Path, tag: &str, argv_log: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("checkleft");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\necho '{tag}'\n",
                argv_log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn checkleft_launcher_execs_the_workspace_repobin_entry_not_the_path_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        write_repobin_toml(
            &ws,
            "version = 1\n[tools.checkleft]\ntarget = \"//tools/checkleft:checkleft\"\n",
        );
        let ws_log = tmp.path().join("ws-argv");
        fake_checkleft(&ws.join("bin"), "workspace repobin checkleft", &ws_log);
        // The decoy: an ancient cargo-installed copy, first on PATH.
        let decoy_log = tmp.path().join("decoy-argv");
        let decoy_dir = tmp.path().join("cargo-bin");
        fake_checkleft(&decoy_dir, "DECOY cargo checkleft", &decoy_log);

        let dir = tmp.path().join("launchers");
        let launcher = write_checkleft_launcher(&dir, &ws).unwrap().expect("declared");
        let out = std::process::Command::new(&launcher)
            .args(["run", "--verbose"])
            .env("PATH", format!("{}:/usr/bin:/bin", decoy_dir.display()))
            .output()
            .expect("run launcher");
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("workspace repobin checkleft"), "{stdout}");
        assert_eq!(
            std::fs::read_to_string(&ws_log).unwrap(),
            "run\n--verbose\n",
            "arguments must pass through untouched"
        );
        assert!(!decoy_log.exists(), "the PATH copy must never run");
    }

    #[cfg(unix)]
    #[test]
    fn checkleft_launcher_fails_loudly_with_127_when_the_repobin_entry_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        write_repobin_toml(
            &ws,
            "version = 1\n[tools.checkleft]\ntarget = \"//tools/checkleft:checkleft\"\n",
        );
        // No ws/bin at all -- the fresh-workspace-without-setup case -- but
        // a decoy on PATH that a fallback would find.
        let decoy_log = tmp.path().join("decoy-argv");
        let decoy_dir = tmp.path().join("cargo-bin");
        fake_checkleft(&decoy_dir, "DECOY cargo checkleft", &decoy_log);

        let dir = tmp.path().join("launchers");
        let launcher = write_checkleft_launcher(&dir, &ws).unwrap().expect("declared");
        let out = std::process::Command::new(&launcher)
            .arg("run")
            .env("PATH", format!("{}:/usr/bin:/bin", decoy_dir.display()))
            .output()
            .expect("run launcher");
        assert_eq!(out.status.code(), Some(127), "{out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let expected_path = workspace_checkleft_path(&ws);
        assert!(stderr.contains("refusing to run"), "{stderr}");
        assert!(
            stderr.contains(&expected_path.to_string_lossy().into_owned()),
            "must name what it looked for: {stderr}"
        );
        assert!(stderr.contains("cargo install"), "{stderr}");
        assert!(
            stderr.contains("final response"),
            "must tell the worker to report it: {stderr}"
        );
        assert!(
            out.stdout.is_empty(),
            "nothing checkleft-shaped may appear on stdout: {out:?}"
        );
        assert!(!decoy_log.exists(), "the PATH copy must never run");
    }
}
