//! The managed `.git/info/exclude` / `.gitignore` blocks that keep
//! boss-internal infrastructure out of a worker's diffs.

use std::path::Path;
use std::{fs, io};

/// Markers delimiting the cube-managed block inside a workspace's local
/// `.git/info/exclude`. We rewrite only the region between them, leaving
/// any operator-added excludes untouched, and they make the provenance
/// of the patterns obvious to anyone reading the file.
pub(super) const BOSS_INFRA_EXCLUDE_BEGIN: &str = "# >>> boss-internal infra (managed by cube) >>>";
pub(super) const BOSS_INFRA_EXCLUDE_END: &str = "# <<< boss-internal infra (managed by cube) <<<";

/// Render the cube-managed exclude block for a workspace.
///
/// `/logs/<workspace-id>.log` is anchored to the single empty infra log
/// some host tooling drops at workspace-setup time (issue #1174) — named
/// exactly after the cube workspace — rather than blanket-ignoring
/// `logs/`, which a repo may legitimately track. `.boss/` is the engine's
/// own per-run scratch/log dir (e.g. the remote runner's `worker.log`),
/// which is never part of a deliverable.
pub(super) fn render_boss_infra_exclude_block(workspace_id: &str) -> String {
    format!(
        "{BOSS_INFRA_EXCLUDE_BEGIN}\n\
         # Keeps Boss/host infra files out of the worker's jj snapshot so they\n\
         # never land in a PR (issue #1174). cube rewrites this block on every\n\
         # lease; edit patterns above/below it, not inside.\n\
         .boss/\n\
         /logs/{workspace_id}.log\n\
         {BOSS_INFRA_EXCLUDE_END}\n"
    )
}

/// Insert or replace the cube-managed block in an exclude-file body,
/// preserving everything outside the markers. Idempotent: a body already
/// carrying an identical block is returned byte-for-byte unchanged.
pub(super) fn upsert_managed_exclude(existing: &str, block: &str) -> String {
    if let (Some(start), Some(end_marker)) = (
        existing.find(BOSS_INFRA_EXCLUDE_BEGIN),
        existing.find(BOSS_INFRA_EXCLUDE_END),
    ) {
        let end = end_marker + BOSS_INFRA_EXCLUDE_END.len();
        // Swallow the newline after the END marker so repeated rewrites
        // don't accumulate blank lines between the block and any tail.
        let tail_start = if existing[end..].starts_with('\n') {
            end + 1
        } else {
            end
        };
        format!("{}{block}{}", &existing[..start], &existing[tail_start..])
    } else {
        let mut out = String::from(existing);
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(block);
        out
    }
}

/// Keep Boss/host infra files out of the worker's jj snapshot (and thus its
/// PR) — defense-in-depth for issue #1174. The mechanism depends on the
/// workspace layout, because jj sources its ignore patterns differently:
///
/// * **Colocated** (the canonical source repo, or any legacy colocated
///   workspace): write the cube-managed block to `.git/info/exclude`. That file
///   lives under `.git/` (never committed, never shipped in a PR) and jj honors
///   it for git-backed repos exactly like a tracked `.gitignore`. This carries
///   both `.boss/` and the `/logs/<id>.log` host-tooling drop.
///
/// * **Non-colocated** (the shared-store pool workspaces created by
///   `jj workspace add`): there is NO per-workspace `.git/`, and jj does NOT
///   read the shared backing store's `info/exclude` for a workspace working
///   copy — the only ignore source jj honors there is a `.gitignore` in the
///   working tree. Writing the engine's scratch dir off via a self-ignoring
///   `.boss/.gitignore` keeps it (and the guard file itself) out of the
///   snapshot without polluting the PR; see [`ensure_boss_dir_self_ignored`].
///
/// Best-effort throughout — an unwritable guard is logged and skipped rather
/// than failing the lease.
pub(super) fn ensure_boss_infra_excluded(workspace_path: &Path, workspace_id: &str) {
    let git_dir = workspace_path.join(".git");
    if git_dir.is_dir() {
        ensure_boss_infra_excluded_colocated(&git_dir, workspace_id);
    } else {
        ensure_boss_dir_self_ignored(workspace_path);
    }
}

/// Colocated path: rewrite the cube-managed block inside `<git_dir>/info/exclude`.
fn ensure_boss_infra_excluded_colocated(git_dir: &Path, workspace_id: &str) {
    let info_dir = git_dir.join("info");
    if let Err(e) = fs::create_dir_all(&info_dir) {
        eprintln!(
            "warning: cube could not create {} for the Boss-infra exclude guard: {e}",
            info_dir.display()
        );
        return;
    }
    let exclude_path = info_dir.join("exclude");
    let existing = match fs::read_to_string(&exclude_path) {
        Ok(body) => body,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            eprintln!(
                "warning: cube could not read {} for the Boss-infra exclude guard: {e}",
                exclude_path.display()
            );
            return;
        }
    };
    let next = upsert_managed_exclude(&existing, &render_boss_infra_exclude_block(workspace_id));
    if next == existing {
        return;
    }
    if let Err(e) = fs::write(&exclude_path, &next) {
        eprintln!(
            "warning: cube could not write {} for the Boss-infra exclude guard: {e}",
            exclude_path.display()
        );
    }
}

/// Single `*` pattern that ignores every path in its own directory — including
/// the `.gitignore` carrying it — so the guard file never appears as a change.
const BOSS_DIR_SELF_IGNORE: &str = "*\n";

/// Non-colocated path: make jj ignore the engine's `.boss/` scratch dir (where
/// the remote runner drops `worker.log`, `settings.json`, `initial-input.txt`,
/// …) via a self-ignoring `.boss/.gitignore`. jj honors working-tree
/// `.gitignore` files in every workspace layout, and the `*` pattern ignores
/// the whole dir plus the guard file itself, so nothing leaks into the worker's
/// snapshot or PR. `.boss/` is purely Boss/host infra and never a repo
/// deliverable, so writing into it can't collide with versioned content.
///
/// (The colocated path's `/logs/<id>.log` anchor is intentionally not mirrored
/// here: re-homing it for a non-colocated workspace would mean writing a
/// `.gitignore` into a `logs/` directory a repo may legitimately track, risking
/// PR pollution. The `.boss/` dir is the only Boss-owned infra location and is
/// covered cleanly above.)
fn ensure_boss_dir_self_ignored(workspace_path: &Path) {
    let boss_dir = workspace_path.join(".boss");
    if let Err(e) = fs::create_dir_all(&boss_dir) {
        eprintln!(
            "warning: cube could not create {} for the Boss-infra ignore guard: {e}",
            boss_dir.display()
        );
        return;
    }
    let gitignore = boss_dir.join(".gitignore");
    // Idempotent: skip the write when the guard is already in place.
    if matches!(fs::read_to_string(&gitignore), Ok(body) if body == BOSS_DIR_SELF_IGNORE) {
        return;
    }
    if let Err(e) = fs::write(&gitignore, BOSS_DIR_SELF_IGNORE) {
        eprintln!(
            "warning: cube could not write {} for the Boss-infra ignore guard: {e}",
            gitignore.display()
        );
    }
}
