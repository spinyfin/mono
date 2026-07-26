use crate::app::excludes::{
    BOSS_INFRA_EXCLUDE_BEGIN, BOSS_INFRA_EXCLUDE_END, ensure_boss_infra_excluded, render_boss_infra_exclude_block,
    upsert_managed_exclude,
};

#[test]
fn boss_infra_exclude_block_names_the_per_workspace_log() {
    let block = render_boss_infra_exclude_block("rdev-base-image-agent-001");
    assert!(block.contains("/logs/rdev-base-image-agent-001.log"));
    assert!(block.contains(".boss/"));
    assert!(block.starts_with(BOSS_INFRA_EXCLUDE_BEGIN));
    assert!(block.trim_end().ends_with(BOSS_INFRA_EXCLUDE_END));
}

#[test]
fn upsert_managed_exclude_appends_to_empty_body() {
    let block = render_boss_infra_exclude_block("mono-agent-004");
    assert_eq!(upsert_managed_exclude("", &block), block);
}

#[test]
fn upsert_managed_exclude_preserves_operator_excludes() {
    let block = render_boss_infra_exclude_block("mono-agent-004");
    let existing = "# operator-added\n*.tmp\n";
    let result = upsert_managed_exclude(existing, &block);
    assert!(result.starts_with("# operator-added\n*.tmp\n"));
    assert!(result.contains("/logs/mono-agent-004.log"));
}

#[test]
fn upsert_managed_exclude_is_idempotent() {
    let block = render_boss_infra_exclude_block("mono-agent-004");
    let once = upsert_managed_exclude("*.tmp\n", &block);
    let twice = upsert_managed_exclude(&once, &block);
    assert_eq!(once, twice);
    // The managed marker appears exactly once after repeated rewrites.
    assert_eq!(twice.matches(BOSS_INFRA_EXCLUDE_BEGIN).count(), 1);
}

#[test]
fn upsert_managed_exclude_rewrites_stale_block_in_place() {
    let stale = render_boss_infra_exclude_block("old-workspace-id");
    let existing = format!("*.tmp\n{stale}# trailing operator line\n");
    let fresh = render_boss_infra_exclude_block("new-workspace-id");
    let result = upsert_managed_exclude(&existing, &fresh);
    assert!(result.contains("/logs/new-workspace-id.log"));
    assert!(!result.contains("old-workspace-id"));
    // Operator content on both sides of the block survives.
    assert!(result.starts_with("*.tmp\n"));
    assert!(result.contains("# trailing operator line\n"));
    assert_eq!(result.matches(BOSS_INFRA_EXCLUDE_BEGIN).count(), 1);
}

#[test]
fn ensure_boss_infra_excluded_writes_git_info_exclude() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let workspace = tempdir.path().join("mono-agent-004");
    std::fs::create_dir_all(workspace.join(".git")).expect("colocated .git dir");

    ensure_boss_infra_excluded(&workspace, "mono-agent-004");

    let exclude = std::fs::read_to_string(workspace.join(".git/info/exclude")).expect("exclude written");
    assert!(exclude.contains("/logs/mono-agent-004.log"));
    assert!(exclude.contains(".boss/"));

    // Second call is a no-op: same bytes, single managed block.
    ensure_boss_infra_excluded(&workspace, "mono-agent-004");
    let again = std::fs::read_to_string(workspace.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude, again);
    assert_eq!(again.matches(BOSS_INFRA_EXCLUDE_BEGIN).count(), 1);
}

#[test]
fn ensure_boss_infra_excluded_writes_self_ignoring_boss_gitignore_when_not_colocated() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let workspace = tempdir.path().join("mono-agent-004");
    // Secondary jj workspace: `.jj` but no colocated `.git` directory.
    std::fs::create_dir_all(workspace.join(".jj")).expect("jj dir");

    ensure_boss_infra_excluded(&workspace, "mono-agent-004");

    // No `.git/info/exclude` is created (there is no `.git` to hold it).
    assert!(!workspace.join(".git").exists());
    // Instead, a self-ignoring `.boss/.gitignore` keeps the engine's scratch
    // dir — and the guard file itself — out of the worker's jj snapshot.
    let gitignore = std::fs::read_to_string(workspace.join(".boss/.gitignore")).expect("boss gitignore written");
    assert_eq!(gitignore, "*\n");

    // Idempotent: a second call leaves the same bytes.
    ensure_boss_infra_excluded(&workspace, "mono-agent-004");
    let again = std::fs::read_to_string(workspace.join(".boss/.gitignore")).unwrap();
    assert_eq!(again, "*\n");
}

// ── unhealthy GC tests ────────────────────────────────────────────────────
