//! Build-gate prompt text for repositories that are not Bazel workspaces.
//!
//! Bazel workspaces keep the Bazel-specific gates in the parent module.
//! Every other repository still gets a gate — the repo's own documented
//! compile, type-check and test commands — rather than no gate at all, and
//! never a Bazel command it cannot run.

use std::path::Path;

/// Manifest markers and the ecosystem each one implies, in the order they
/// are reported. Used only to point the worker at the right place to look
/// for commands; the commands themselves come from the repo's own docs.
const MANIFEST_HINTS: &[(&str, &str)] = &[
    (
        "package.json",
        "npm/Node (`package.json` scripts, plus the lockfile `package-lock.json`, `yarn.lock` or `pnpm-lock.yaml`)",
    ),
    ("Cargo.toml", "Cargo (`Cargo.toml`, lockfile `Cargo.lock`)"),
    ("go.mod", "Go modules (`go.mod`, `go.sum`)"),
    ("pyproject.toml", "Python (`pyproject.toml`, any lockfile it names)"),
    ("pom.xml", "Maven (`pom.xml`)"),
    ("build.gradle", "Gradle (`build.gradle`)"),
    ("build.gradle.kts", "Gradle (`build.gradle.kts`)"),
    ("Makefile", "Make (`Makefile` targets)"),
];

fn detected_manifests(workspace_path: &Path) -> Vec<&'static str> {
    MANIFEST_HINTS
        .iter()
        .filter(|(marker, _)| workspace_path.join(marker).exists())
        .map(|(_, hint)| *hint)
        .collect()
}

/// The pre-push gate for a confirmed non-Bazel repository. `conflict_resolution`
/// selects the merge-correctness variant, which still requires the repo's
/// documented compile, type-check AND test commands with exit codes
/// reported. Bazel conflict policy (compile only, tests in CI) lives in
/// the parent module.
pub(super) fn non_bazel_gate_text(
    workspace_path: &Path,
    conflict_resolution: bool,
    worker_signal_seam_enabled: bool,
) -> String {
    let manifests = detected_manifests(workspace_path);
    let detected = if manifests.is_empty() {
        "No build-system manifest was detected at the workspace root; look for the documented commands in the repo docs instead.".to_owned()
    } else {
        format!(
            "Build-system markers found at the workspace root: {}.",
            manifests.join("; ")
        )
    };
    let blocked_action = if worker_signal_seam_enabled {
        "call `\"$BOSS_BIN\" propose blocked --reason \"...\"` naming the missing command and stop"
    } else {
        "emit a `[blocked] reason=\"...\"` marker naming the missing command and stop"
    };
    let (heading, scope, tests) = if conflict_resolution {
        (
            "Pre-push gate for conflict resolution (non-Bazel repository) — merge correctness first, then push",
            "For a conflict-resolution revision the gate you MUST clear before pushing is **merge correctness**: the merged code must compile, type-check, and pass the repo's documented test commands. Run every command with its real exit code reported.",
            "Run the repo's documented compile, type-check and test commands (for example `npm run typecheck` / `npm run build` / `npm test`, `cargo test`, `go test ./...`) over the code your resolution touched and the code the rebased-in upstream change touched. A repo whose only documented check is a test command still requires that test. Regenerate any lockfile or generated artifact the rebase invalidated using the repo's own package manager (for example `npm install` to refresh `package-lock.json`, `cargo update -w` / `cargo check` for `Cargo.lock`) and include it in your commit.",
        )
    } else {
        (
            "Pre-push build gate (non-Bazel repository)",
            "Before you push a branch or update a PR with code changes, you MUST run the repo's own documented compile, type-check and test commands against what you touched and confirm they pass.",
            "Run the repo's documented compile, type-check and test commands (for example `npm run typecheck` and `npm test`, `cargo build` and `cargo test`, `go build ./... && go test ./...`). Regenerate any lockfile your change invalidated with the repo's own package manager.",
        )
    };
    format!(
        "\n## {heading}\n\
         \n\
         This repository is not a Bazel workspace (no `MODULE.bazel`/`WORKSPACE` marker): do NOT run `bazel` commands. {scope} \"I think it should work\" is NOT a substitute for running the commands.\n\
         \n\
         {detected}\n\
         \n\
         Required BEFORE you push:\n\
         - Discover the commands from the repo itself, in this order: `AGENTS.md` / `CLAUDE.md`, the README and contributor docs, `package.json` `scripts` (or the Cargo / Go / Gradle / Maven / Make equivalent), and any CI workflow under `.github/workflows/`. Prefer the commands CI runs.\n\
         - {tests}\n\
         - Run them in the FOREGROUND, read each exit code directly, and report the commands and their exit codes in your Validation section. Do not wrap them in a shell `timeout`.\n\
         - Every command must exit 0 before you push. If a command fails, fix the cause; do NOT push code that does not compile or type-check.\n\
         - If you cannot find ANY documented build, type-check or test command, do not invent one and do not skip validation silently: say explicitly that no build command was found, then {blocked_action}.\n"
    )
}

/// Rebase-only exception wording for a confirmed non-Bazel repository.
pub(super) fn non_bazel_rebase_clause(conflict_resolution: bool) -> &'static str {
    if conflict_resolution {
        "Rebase-only exception (VCS only — not a build-gate skip): if the ONLY thing needed to satisfy this revision is a rebase (e.g. rebasing the branch onto updated main) and the rebase produces NO diff whatsoever (zero changed files), it is valid to have NO new commit. Do not manufacture an empty or cosmetic commit. In that case, push the rebased branch and explain in your response that the revision was satisfied by a rebase with no code change. IMPORTANT: this exception covers VCS mechanics only — whether to add a new commit. It does NOT exempt you from the merge-correctness gate. Any rebase, merge, or conflict resolution MUST run the repo's own documented compile, type-check and test commands (and regenerate invalidated lockfiles with the repo's package manager) before pushing, even when the rebase appeared clean — a rebase merges upstream changes in and the resulting code is new and must compile, type-check, and pass those tests. Report each command and its real exit code.\n"
    } else {
        "Rebase-only exception (VCS only — not a build-gate skip): if the ONLY thing needed to satisfy this revision is a rebase (e.g. rebasing the branch onto updated main) and the rebase produces NO diff whatsoever (zero changed files), it is valid to have NO new commit. Do not manufacture an empty or cosmetic commit. In that case, push the rebased branch and explain in your response that the revision was satisfied by a rebase with no code change. IMPORTANT: this exception covers VCS mechanics only — whether to add a new commit. It does NOT exempt you from the pre-push build gate. Any revision that involves a rebase, merge, or conflict resolution MUST run the repo's own documented compile, type-check and test commands before pushing, even when the rebase appeared clean. A rebase merges upstream changes into your branch — the resulting code is new and must be compiled and tested. This is exactly where compile errors get reintroduced.\n"
    }
}

/// Pre-push gate when the build system could not be confirmed (`None`).
/// The worker discovers markers on the execution host and then applies
/// the matching policy; an unavailable probe is not treated as non-Bazel.
pub(super) fn unresolved_gate_text(conflict_resolution: bool, worker_signal_seam_enabled: bool) -> String {
    let blocked_action = if worker_signal_seam_enabled {
        "call `\"$BOSS_BIN\" propose blocked --reason \"...\"` naming that no build command was found and stop"
    } else {
        "emit a `[blocked] reason=\"...\"` marker naming that no build command was found and stop"
    };
    let (heading, scope) = if conflict_resolution {
        (
            "Pre-push gate for conflict resolution (build system unresolved) — discover, then merge correctness",
            "The gate you MUST clear before pushing is **merge correctness** after you have identified the build system. If Bazel markers are present, compile the touched/upstream targets with `bazel build` and regenerate invalidated lockfiles. If they are not, run the repo's documented compile, type-check and test commands and regenerate lockfiles with the owning package manager. Report each command and its real exit code.",
        )
    } else {
        (
            "Pre-push build gate (build system unresolved)",
            "Before you push a branch or update a PR with code changes, you MUST identify the build system on this execution host and run the matching gate. If Bazel markers are present, run `bazel build` and `bazel test` of the touched targets. If they are not, run the repo's documented compile, type-check and test commands. \"I think it should work\" is NOT a substitute for running the commands.",
        )
    };
    format!(
        "\n## {heading}\n\
         \n\
         The engine could not confirm this workspace's build system (the marker probe did not complete). Do not assume Bazel and do not assume a non-Bazel repository.\n\
         \n\
         {scope}\n\
         \n\
         Required BEFORE you push:\n\
         - On this session's cwd, look for `MODULE.bazel`, `WORKSPACE`, `WORKSPACE.bazel`, or `BUILD.bazel` at the workspace root.\n\
         - If any of those Bazel markers is present, apply the full Bazel policy for this kind of run (build, and tests except where a conflict-resolution compile-only gate applies).\n\
         - If none is present, discover the repo's documented compile, type-check and test commands from `AGENTS.md` / `CLAUDE.md`, the README, package-manager manifests, and CI workflows, then run those. Prefer the commands CI runs.\n\
         - Run them in the FOREGROUND, read each exit code directly, and report the commands and their exit codes in your Validation section. Do not wrap them in a shell `timeout`.\n\
         - Every required command must exit 0 before you push.\n\
         - If nothing is discoverable, do not invent a command and do not skip validation silently: say explicitly that no build command was found, then {blocked_action}.\n"
    )
}

/// Rebase-only exception wording when the build system is unresolved.
pub(super) fn unresolved_rebase_clause(conflict_resolution: bool) -> &'static str {
    if conflict_resolution {
        "Rebase-only exception (VCS only — not a build-gate skip): if the ONLY thing needed to satisfy this revision is a rebase (e.g. rebasing the branch onto updated main) and the rebase produces NO diff whatsoever (zero changed files), it is valid to have NO new commit. Do not manufacture an empty or cosmetic commit. In that case, push the rebased branch and explain in your response that the revision was satisfied by a rebase with no code change. IMPORTANT: this exception covers VCS mechanics only — whether to add a new commit. It does NOT exempt you from the merge-correctness gate above. Any rebase, merge, or conflict resolution MUST identify the build system on this execution host and run the matching merge-correctness commands (Bazel compile if markers are present; otherwise the repo's documented compile, type-check and test commands) before pushing, even when the rebase appeared clean.\n"
    } else {
        "Rebase-only exception (VCS only — not a build-gate skip): if the ONLY thing needed to satisfy this revision is a rebase (e.g. rebasing the branch onto updated main) and the rebase produces NO diff whatsoever (zero changed files), it is valid to have NO new commit. Do not manufacture an empty or cosmetic commit. In that case, push the rebased branch and explain in your response that the revision was satisfied by a rebase with no code change. IMPORTANT: this exception covers VCS mechanics only — whether to add a new commit. It does NOT exempt you from the pre-push build gate above. Any revision that involves a rebase, merge, or conflict resolution MUST identify the build system on this execution host and run the matching gate (Bazel build+test if markers are present; otherwise the repo's documented compile, type-check and test commands) before pushing, even when the rebase appeared clean.\n"
    }
}
