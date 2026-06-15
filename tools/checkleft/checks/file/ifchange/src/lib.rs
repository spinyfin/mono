//! Checkleft check: require linked files or blocks to change together.
//!
//! This is the Component Model wasm port of the former built-in
//! `ifchange-thenchange` check, registered under the canonical id `file/ifchange`.
//! It runs inside the checkleft wasm host and reads files via the WASI filesystem
//! sandbox.
//!
//! ## What the check detects
//!
//! When a file contains `// LINT.IfChange` ... `// LINT.ThenChange(<target>)` markers,
//! and the region between the markers is modified, the check requires the named target
//! file (or labeled block within it) to also be modified in the same change.
//!
//! ## Supported comment styles
//!
//! The `LINT.IfChange` / `LINT.ThenChange` directives are recognized when preceded
//! by any of: `//`, `#`, `--`, `;`, `/*`, `*`, `<!--` (and `*/` / `-->` suffixes
//! are stripped). This covers most common source languages.
//!
//! ## Configuration
//!
//! This check has no configuration surface; all parameters are intrinsic to the
//! LINT markers in the source files.
//!
//! ## Known limitation: base-revision enforcement is not available
//!
//! The WASM sandbox exposes only the *current* file tree; it cannot read the
//! base revision of a changed file. This creates two enforcement gaps relative
//! to the former native implementation:
//!
//! 1. **Deleted files** — if a file containing `LINT.IfChange` markers is
//!    deleted as part of a change, the check cannot inspect the old content and
//!    will not flag missing target updates for the deleted file's blocks.
//!
//! 2. **Removed markers on existing files** — if a change edits the guarded
//!    region in a still-present file *and simultaneously removes* its
//!    `LINT.IfChange`/`LINT.ThenChange` markers, the check sees neither the
//!    block nor the edit in the current tree and reports nothing. The former
//!    native implementation (which read the base revision) would have caught
//!    this as a contract-removal violation.
//!
//! All other cases (modified files, new files, renamed files) are fully enforced.

use std::collections::{BTreeMap, BTreeSet};

use checkleft_check_sdk::{
    ChangeKind, ChangeSet, ChangedFile, CheckInput, DiffHunk, Finding, Location, Severity, check,
};

// ── Parsing types ────────────────────────────────────────────────────────────

#[derive(Debug)]
struct IfChangeFile {
    blocks: Vec<IfChangeBlock>,
    label_map: BTreeMap<String, usize>,
}

impl IfChangeFile {
    fn block_by_label(&self, label: &str) -> Option<&IfChangeBlock> {
        self.label_map.get(label).and_then(|i| self.blocks.get(*i))
    }
}

#[derive(Clone, Debug)]
struct IfChangeBlock {
    source_label: Option<String>,
    ifchange_line: usize,
    thenchange_line: usize,
    target: ThenChangeTarget,
}

#[derive(Clone, Debug)]
enum ThenChangeTarget {
    File { path: String },
    Block { path: String, label: String },
}

// ── Main check ───────────────────────────────────────────────────────────────

#[check(
    name = "file/ifchange",
    description = "requires linked files or blocks to change together",
    severity = error,
    access_scope = whole_repo
)]
pub fn file_ifchange_check(input: CheckInput) -> Vec<Finding> {
    let analyses: Vec<FileAnalysis> = input
        .changeset
        .changed_files
        .iter()
        .map(|f| analyze_file(f, &input.changeset))
        .collect();

    let mut findings = Vec::new();

    for analysis in &analyses {
        findings.extend(analysis.parse_findings.iter().cloned());
    }

    let mut emitted_keys: BTreeSet<String> = BTreeSet::new();

    for analysis in &analyses {
        if !analysis.parse_findings.is_empty() {
            continue;
        }
        for block in &analysis.touched_blocks {
            let key = format!("{}:{}:{}", analysis.path, block.ifchange_line, block.thenchange_line);
            if !emitted_keys.insert(key) {
                continue;
            }

            let status = target_status(block, &input.changeset, &analyses);
            match status {
                TargetStatus::Satisfied => continue,
                TargetStatus::MissingFile => findings.push(broken_target_finding(
                    &analysis.path,
                    block,
                    "linked target file does not exist in the current tree".to_owned(),
                )),
                TargetStatus::MissingLabel => findings.push(broken_target_finding(
                    &analysis.path,
                    block,
                    "linked target label does not exist in the current tree".to_owned(),
                )),
                TargetStatus::NotChanged => findings.push(broken_target_finding(
                    &analysis.path,
                    block,
                    "linked target was not updated in the same change".to_owned(),
                )),
            }
        }
    }

    findings
}

// ── Per-file analysis ─────────────────────────────────────────────────────────

struct FileAnalysis {
    path: String,
    touched_blocks: Vec<IfChangeBlock>,
    parse_findings: Vec<Finding>,
}

fn analyze_file(changed_file: &ChangedFile, changeset: &ChangeSet) -> FileAnalysis {
    let path = changed_file.path.clone();

    if changed_file.kind == ChangeKind::Deleted {
        return FileAnalysis {
            path,
            touched_blocks: vec![],
            parse_findings: vec![],
        };
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            let finding = Finding {
                severity: Severity::Error,
                message: format!("failed to read `{path}` for ifchange analysis: {e}"),
                location: Some(Location {
                    path: path.clone(),
                    line: None,
                    column: None,
                }),
                remediations: vec![],
                suggested_fix: None,
            };
            return FileAnalysis {
                path,
                touched_blocks: vec![],
                parse_findings: vec![finding],
            };
        }
    };

    let parsed = match parse_ifchange_file(&path, &content) {
        Ok(p) => p,
        Err(msg) => {
            let finding = Finding {
                severity: Severity::Error,
                message: msg,
                location: Some(Location {
                    path: path.clone(),
                    line: None,
                    column: None,
                }),
                remediations: vec![],
                suggested_fix: None,
            };
            return FileAnalysis {
                path,
                touched_blocks: vec![],
                parse_findings: vec![finding],
            };
        }
    };

    let diff = changeset.file_diffs.iter().find(|d| d.path == path);

    let touched_blocks = parsed
        .blocks
        .into_iter()
        .filter(|block| {
            diff.is_some_and(|d| {
                d.hunks
                    .iter()
                    .any(|hunk| hunk_touches_range_new(hunk, block.ifchange_line, block.thenchange_line))
            })
        })
        .collect();

    FileAnalysis {
        path,
        touched_blocks,
        parse_findings: vec![],
    }
}

// ── Target status ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetStatus {
    Satisfied,
    MissingFile,
    MissingLabel,
    NotChanged,
}

fn target_status(block: &IfChangeBlock, changeset: &ChangeSet, analyses: &[FileAnalysis]) -> TargetStatus {
    match &block.target {
        ThenChangeTarget::File { path } => {
            if !std::path::Path::new(path).exists() {
                return TargetStatus::MissingFile;
            }
            if file_changed(changeset, path) {
                TargetStatus::Satisfied
            } else {
                TargetStatus::NotChanged
            }
        }
        ThenChangeTarget::Block { path, label } => {
            if !std::path::Path::new(path).exists() {
                return TargetStatus::MissingFile;
            }
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return TargetStatus::NotChanged,
            };
            let parsed = match parse_ifchange_file(path, &content) {
                Ok(p) => p,
                Err(_) => return TargetStatus::NotChanged,
            };
            if parsed.block_by_label(label).is_none() {
                return TargetStatus::MissingLabel;
            }
            let Some(target_analysis) = analyses.iter().find(|a| a.path == *path) else {
                return TargetStatus::NotChanged;
            };
            if target_analysis
                .touched_blocks
                .iter()
                .any(|b| b.source_label.as_deref() == Some(label))
            {
                TargetStatus::Satisfied
            } else {
                TargetStatus::NotChanged
            }
        }
    }
}

fn file_changed(changeset: &ChangeSet, target_path: &str) -> bool {
    changeset
        .changed_files
        .iter()
        .any(|f| f.path == target_path || f.old_path.as_deref().is_some_and(|op| op == target_path))
}

// ── Hunk / range overlap ──────────────────────────────────────────────────────

fn hunk_touches_range_new(hunk: &DiffHunk, range_start: usize, range_end: usize) -> bool {
    hunk_touches_range(hunk.new_start as usize, hunk.new_lines as usize, range_start, range_end)
}

fn hunk_touches_range(start: usize, len: usize, range_start: usize, range_end: usize) -> bool {
    if len == 0 {
        return start >= range_start && start <= range_end.saturating_add(1);
    }
    let end = start.saturating_add(len.saturating_sub(1));
    start <= range_end && end >= range_start
}

// ── Finding construction ──────────────────────────────────────────────────────

fn broken_target_finding(source_path: &str, block: &IfChangeBlock, detail: String) -> Finding {
    Finding {
        severity: Severity::Error,
        message: format!("{}: {detail}", render_target(&block.target)),
        location: Some(Location {
            path: source_path.to_owned(),
            line: Some(block.ifchange_line as u32),
            column: Some(1),
        }),
        remediations: vec![
            "Update the linked file or block in the same change, or bypass the check with a documented reason."
                .to_owned(),
        ],
        suggested_fix: None,
    }
}

fn render_target(target: &ThenChangeTarget) -> String {
    match target {
        ThenChangeTarget::File { path } => format!("`LINT.ThenChange({path})`"),
        ThenChangeTarget::Block { path, label } => format!("`LINT.ThenChange({path}:{label})`"),
    }
}

// ── Parsing ───────────────────────────────────────────────────────────────────

fn parse_ifchange_file(path: &str, contents: &str) -> Result<IfChangeFile, String> {
    let mut blocks = Vec::new();
    let mut label_map: BTreeMap<String, usize> = BTreeMap::new();
    let mut current: Option<(Option<String>, usize)> = None;

    for (line_idx, raw_line) in contents.lines().enumerate() {
        let line_number = line_idx + 1;
        let text = normalize_directive_text(raw_line);

        if let Some(maybe_label) = parse_ifchange_directive(text) {
            if current.is_some() {
                return Err(format!(
                    "{path}:{line_number}: nested `LINT.IfChange` blocks are not supported"
                ));
            }
            if let Some(ref label) = maybe_label
                && label_map.contains_key(label.as_str())
            {
                return Err(format!(
                    "{path}:{line_number}: duplicate `LINT.IfChange({label})` label"
                ));
            }
            current = Some((maybe_label, line_number));
        } else if let Some(raw_target) = parse_thenchange_directive(text) {
            let Some((source_label, ifchange_line)) = current.take() else {
                return Err(format!(
                    "{path}:{line_number}: `LINT.ThenChange(...)` must close a preceding `LINT.IfChange` block"
                ));
            };
            let target = parse_thenchange_target(raw_target).map_err(|e| format!("{path}:{line_number}: {e}"))?;
            let block_index = blocks.len();
            if let Some(ref label) = source_label {
                label_map.insert(label.clone(), block_index);
            }
            blocks.push(IfChangeBlock {
                source_label,
                ifchange_line,
                thenchange_line: line_number,
                target,
            });
        }
    }

    if let Some((_, open_line)) = current {
        return Err(format!(
            "{path}:{open_line}: `LINT.IfChange` block is missing a closing `LINT.ThenChange(...)`"
        ));
    }

    Ok(IfChangeFile { blocks, label_map })
}

/// Strip comment markers from a source line and return the core directive text.
fn normalize_directive_text(line: &str) -> &str {
    let mut text = line.trim();
    for prefix in ["//", "#", "--", ";", "/*", "*", "<!--"] {
        if let Some(stripped) = text.strip_prefix(prefix) {
            text = stripped.trim_start();
            break;
        }
    }
    for suffix in ["*/", "-->"] {
        if let Some(stripped) = text.strip_suffix(suffix) {
            text = stripped.trim_end();
        }
    }
    text
}

/// Recognizes `LINT.IfChange` (returns `Some(None)`) or `LINT.IfChange(label)`
/// (returns `Some(Some(label))`). Returns `None` for any other text.
fn parse_ifchange_directive(text: &str) -> Option<Option<String>> {
    if text == "LINT.IfChange" {
        return Some(None);
    }
    let rest = text.strip_prefix("LINT.IfChange(")?;
    let label = rest.strip_suffix(')')?;
    if label.is_empty() || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some(Some(label.to_owned()))
}

/// Recognizes `LINT.ThenChange(<target>)` and returns the raw target string,
/// or `None` for any other text.
fn parse_thenchange_directive(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("LINT.ThenChange(")?;
    let target = rest.strip_suffix(')')?;
    if target.is_empty() || target.contains(')') {
        return None;
    }
    Some(target)
}

fn parse_thenchange_target(raw: &str) -> Result<ThenChangeTarget, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("`LINT.ThenChange(...)` target must not be empty".to_owned());
    }

    let (path_text, label) = match trimmed.rfind(':') {
        Some(pos) => {
            let maybe_label = trimmed[pos + 1..].trim();
            if !maybe_label.is_empty()
                && maybe_label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                (&trimmed[..pos], Some(maybe_label))
            } else {
                (trimmed, None)
            }
        }
        None => (trimmed, None),
    };

    validate_path(path_text.trim())?;

    Ok(match label {
        Some(l) => ThenChangeTarget::Block {
            path: path_text.trim().to_owned(),
            label: l.to_owned(),
        },
        None => ThenChangeTarget::File {
            path: path_text.trim().to_owned(),
        },
    })
}

fn validate_path(path_text: &str) -> Result<(), String> {
    let p = std::path::Path::new(path_text);
    if p.is_absolute() {
        return Err(format!(
            "path `{path_text}` is absolute: only relative paths are allowed"
        ));
    }
    for component in p.components() {
        use std::path::Component;
        if matches!(component, Component::ParentDir) {
            return Err(format!("path traversal is not allowed in `{path_text}`"));
        }
    }
    Ok(())
}

// NOTE: this crate is an rlib, NOT a standalone wasm component. The component
// ABI (`export_checks!` → `list-checks`/`run-check`) is wired ONCE in the
// aggregating `checkleft-preinstalled-bundle` crate, which links this check
// alongside the other preinstalled checks into a single multiplexed component.
// That deduplicates the shared wasm runtime baseline (std/alloc/SDK/wit-bindgen)
// across all preinstalled checks instead of duplicating it per check.

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use checkleft_check_sdk::{ChangeKind, ChangeSet, ChangedFile, CheckInput, DiffHunk, FileDiff};
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // Serialize CWD changes so parallel tests don't interfere.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_changeset(changed_files: Vec<(&str, ChangeKind)>, diffs: Vec<(&str, Vec<DiffHunk>)>) -> ChangeSet {
        ChangeSet {
            changed_files: changed_files
                .into_iter()
                .map(|(path, kind)| ChangedFile {
                    path: path.to_owned(),
                    kind,
                    old_path: None,
                })
                .collect(),
            file_diffs: diffs
                .into_iter()
                .map(|(path, hunks)| FileDiff {
                    path: path.to_owned(),
                    hunks,
                })
                .collect(),
            commit_description: None,
            pr_description: None,
            change_id: None,
            repository: None,
        }
    }

    /// A hunk touching `new_start` for `new_lines` lines (representing added lines).
    fn hunk_new(new_start: u32, new_lines: u32) -> DiffHunk {
        DiffHunk {
            old_start: 0,
            old_lines: 0,
            new_start,
            new_lines,
            added_lines: new_lines,
            removed_lines: 0,
        }
    }

    fn run_check(changeset: ChangeSet) -> Vec<Finding> {
        let input = CheckInput::__from_parts(changeset, "{}".to_owned());
        file_ifchange_check(input)
    }

    // ── File-target tests ─────────────────────────────────────────────────────

    #[test]
    fn passes_when_source_and_target_change_together() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::create_dir_all(dir.path().join("frontend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange\nschema v2\n// LINT.ThenChange(frontend/schema.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("frontend/schema.txt"), "schema view v2\n").unwrap();

        // Hunk covers line 2 (the body line inside the IfChange block at lines 1-3).
        let cs = make_changeset(
            vec![
                ("backend/schema.txt", ChangeKind::Modified),
                ("frontend/schema.txt", ChangeKind::Modified),
            ],
            vec![("backend/schema.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert!(findings.is_empty(), "unexpected findings: {findings:?}");
    }

    #[test]
    fn fails_when_linked_target_file_does_not_change() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::create_dir_all(dir.path().join("frontend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange\nschema v2\n// LINT.ThenChange(frontend/schema.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("frontend/schema.txt"), "schema view v1\n").unwrap();

        let cs = make_changeset(
            vec![("backend/schema.txt", ChangeKind::Modified)],
            vec![("backend/schema.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1, "expected 1 finding; got {findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(
            findings[0].message.contains("frontend/schema.txt"),
            "message: {}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("not updated in the same change"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn fails_when_linked_target_file_does_not_exist() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(frontend/missing.txt)\n",
        )
        .unwrap();
        // frontend/missing.txt is intentionally NOT created.

        let cs = make_changeset(
            vec![("backend/schema.txt", ChangeKind::Modified)],
            vec![("backend/schema.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("does not exist in the current tree"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn no_finding_when_block_not_touched_by_diff() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        // File has an IfChange block on lines 1-3 and some other content.
        // The hunk covers line 5 (outside the block range 1-3).
        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt)\nextra line\nchanged line\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "something\n").unwrap();

        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(5, 1)])], // hunk is outside block (lines 1-3)
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert!(
            findings.is_empty(),
            "block not touched — no finding expected; got {findings:?}"
        );
    }

    #[test]
    fn deleted_files_are_skipped_without_findings() {
        // A deleted file cannot be read, so its IfChange blocks are not enforced.
        // This is the documented WASM limitation (no base-version access).
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(dir.path().join("b.txt"), "something\n").unwrap();

        // a.txt is deleted (not on disk), b.txt is not in changeset.
        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Deleted)],
            vec![("a.txt", vec![hunk_new(1, 3)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert!(
            findings.is_empty(),
            "deleted file must produce no findings; got {findings:?}"
        );
    }

    // ── Block-target tests ────────────────────────────────────────────────────

    #[test]
    fn passes_when_linked_target_block_changes() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::create_dir_all(dir.path().join("frontend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange(schema)\nvalue=2\n// LINT.ThenChange(frontend/schema.txt:view)\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("frontend/schema.txt"),
            "// LINT.IfChange(view)\nrender=2\n// LINT.ThenChange(backend/schema.txt:schema)\n",
        )
        .unwrap();

        let cs = make_changeset(
            vec![
                ("backend/schema.txt", ChangeKind::Modified),
                ("frontend/schema.txt", ChangeKind::Modified),
            ],
            vec![
                ("backend/schema.txt", vec![hunk_new(2, 1)]),
                ("frontend/schema.txt", vec![hunk_new(2, 1)]),
            ],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert!(
            findings.is_empty(),
            "both blocks touched — no finding expected; got {findings:?}"
        );
    }

    #[test]
    fn fails_when_linked_target_block_does_not_change() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::create_dir_all(dir.path().join("frontend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange(schema)\nvalue=2\n// LINT.ThenChange(frontend/schema.txt:view)\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("frontend/schema.txt"),
            "// LINT.IfChange(view)\nrender=1\n// LINT.ThenChange(backend/schema.txt:schema)\n",
        )
        .unwrap();

        // Only backend changes; frontend file exists but is not in the changeset.
        let cs = make_changeset(
            vec![("backend/schema.txt", ChangeKind::Modified)],
            vec![("backend/schema.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1, "expected 1 finding; got {findings:?}");
        assert!(
            findings[0].message.contains("frontend/schema.txt:view"),
            "message: {}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("not updated in the same change"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn reports_missing_target_label() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("backend")).unwrap();
        fs::create_dir_all(dir.path().join("frontend")).unwrap();
        fs::write(
            dir.path().join("backend/schema.txt"),
            "// LINT.IfChange(schema)\nvalue=2\n// LINT.ThenChange(frontend/schema.txt:view)\n",
        )
        .unwrap();
        // frontend/schema.txt exists but has no "view" label.
        fs::write(dir.path().join("frontend/schema.txt"), "render=1\n").unwrap();

        let cs = make_changeset(
            vec![("backend/schema.txt", ChangeKind::Modified)],
            vec![("backend/schema.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("frontend/schema.txt:view"),
            "message: {}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("label does not exist"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn finding_location_points_to_ifchange_line() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("a.txt"),
            "prefix line\n// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "b content\n").unwrap();

        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(3, 1)])], // hunk on line 3 (body between lines 2 and 4)
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        let loc = findings[0].location.as_ref().expect("finding must have location");
        assert_eq!(loc.path, "a.txt");
        assert_eq!(loc.line, Some(2), "location must point at the IfChange line (line 2)");
        assert_eq!(loc.column, Some(1));
    }

    #[test]
    fn finding_message_contains_remediation() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "b content\n").unwrap();

        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(
            !findings[0].remediations.is_empty(),
            "finding must carry at least one remediation"
        );
    }

    // ── Deduplication ─────────────────────────────────────────────────────────

    #[test]
    fn duplicate_block_emitted_only_once() {
        // If both the "current" and some edge path would emit for the same block,
        // the key-based deduplication must suppress the duplicate.
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("a.txt"),
            "// LINT.IfChange\ncontent\n// LINT.ThenChange(b.txt)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "something\n").unwrap();

        // Two hunks both touching the same block (lines 1-3).
        let cs = make_changeset(
            vec![("a.txt", ChangeKind::Modified)],
            vec![("a.txt", vec![hunk_new(1, 1), hunk_new(2, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(
            findings.len(),
            1,
            "same block must not be emitted twice; got {findings:?}"
        );
    }

    // ── Parse error tests ─────────────────────────────────────────────────────

    #[test]
    fn reports_parse_error_for_nested_ifchange() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(
            dir.path().join("bad.txt"),
            "// LINT.IfChange\n// LINT.IfChange\n// LINT.ThenChange(other.txt)\n",
        )
        .unwrap();

        let cs = make_changeset(
            vec![("bad.txt", ChangeKind::Modified)],
            vec![("bad.txt", vec![hunk_new(1, 3)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(
            findings[0].message.contains("nested"),
            "message: {}",
            findings[0].message
        );
    }

    #[test]
    fn reports_parse_error_for_orphan_thenchange() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        fs::write(dir.path().join("bad.txt"), "// LINT.ThenChange(other.txt)\n").unwrap();

        let cs = make_changeset(
            vec![("bad.txt", ChangeKind::Modified)],
            vec![("bad.txt", vec![hunk_new(1, 1)])],
        );
        let findings = run_check(cs);

        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("must close a preceding"),
            "message: {}",
            findings[0].message
        );
    }

    // ── Parsing unit tests (no IO) ────────────────────────────────────────────

    #[test]
    fn parse_unlabeled_block_targeting_file() {
        let parsed = parse_ifchange_file(
            "backend/version.rs",
            "// LINT.IfChange\nconst VERSION: &str = \"v1\";\n// LINT.ThenChange(tools/release/version.txt)\n",
        )
        .expect("parse");
        assert_eq!(parsed.blocks.len(), 1);
        assert_eq!(parsed.blocks[0].source_label, None);
        assert_eq!(parsed.blocks[0].ifchange_line, 1);
        assert_eq!(parsed.blocks[0].thenchange_line, 3);
        match &parsed.blocks[0].target {
            ThenChangeTarget::File { path } => assert_eq!(path, "tools/release/version.txt"),
            other => panic!(
                "expected File target; got {other:?}",
                other = std::mem::discriminant(other)
            ),
        }
    }

    #[test]
    fn parse_labeled_block_targeting_labeled_block() {
        let parsed = parse_ifchange_file(
            "backend/api/user.proto",
            "# LINT.IfChange(schema)\nmessage User {}\n# LINT.ThenChange(frontend/src/types.ts:user_schema)\n",
        )
        .expect("parse");
        assert_eq!(parsed.blocks.len(), 1);
        assert_eq!(parsed.blocks[0].source_label.as_deref(), Some("schema"));
        match &parsed.blocks[0].target {
            ThenChangeTarget::Block { path, label } => {
                assert_eq!(path, "frontend/src/types.ts");
                assert_eq!(label, "user_schema");
            }
            other => panic!(
                "expected Block target; got {other:?}",
                other = std::mem::discriminant(other)
            ),
        }
        assert!(parsed.block_by_label("schema").is_some());
    }

    #[test]
    fn parse_ignores_prose_mentions_of_ifchange() {
        let parsed =
            parse_ifchange_file("docs/guide.md", "Use LINT.IfChange in comments, not in prose.\n").expect("parse");
        assert!(parsed.blocks.is_empty());
    }

    #[test]
    fn parse_rejects_duplicate_labels() {
        let err = parse_ifchange_file(
            "docs/guide.md",
            "// LINT.IfChange(shared)\n// LINT.ThenChange(other/file.md)\n// LINT.IfChange(shared)\n// LINT.ThenChange(other/file.md)\n",
        )
        .expect_err("must fail");
        assert!(err.contains("duplicate `LINT.IfChange(shared)` label"), "err: {err}");
    }

    #[test]
    fn parse_rejects_nested_ifchange_blocks() {
        let err = parse_ifchange_file(
            "docs/guide.md",
            "// LINT.IfChange\n// LINT.IfChange\n// LINT.ThenChange(other/file.md)\n",
        )
        .expect_err("must fail");
        assert!(err.contains("nested `LINT.IfChange` blocks"), "err: {err}");
    }

    #[test]
    fn parse_rejects_missing_thenchange() {
        let err =
            parse_ifchange_file("docs/guide.md", "// LINT.IfChange(orphan)\nstill open\n").expect_err("must fail");
        assert!(err.contains("missing a closing `LINT.ThenChange(...)`"), "err: {err}");
    }

    #[test]
    fn parse_rejects_thenchange_without_ifchange() {
        let err = parse_ifchange_file("docs/guide.md", "// LINT.ThenChange(other/file.md)\n").expect_err("must fail");
        assert!(
            err.contains("must close a preceding `LINT.IfChange` block"),
            "err: {err}"
        );
    }

    #[test]
    fn parse_rejects_invalid_thenchange_target() {
        let err = parse_ifchange_file("docs/guide.md", "// LINT.IfChange\n// LINT.ThenChange(../escape.md)\n")
            .expect_err("must fail");
        assert!(err.contains("path traversal is not allowed"), "err: {err}");
    }

    #[test]
    fn parse_recognizes_all_comment_styles() {
        for (prefix, suffix) in [
            ("// ", ""),
            ("# ", ""),
            ("-- ", ""),
            ("; ", ""),
            ("/* ", " */"),
            ("* ", ""),
            ("<!-- ", " -->"),
        ] {
            let content = format!("{prefix}LINT.IfChange{suffix}\nline\n{prefix}LINT.ThenChange(target.txt){suffix}\n");
            let parsed = parse_ifchange_file("file.txt", &content)
                .unwrap_or_else(|e| panic!("parse failed for prefix `{prefix}`: {e}"));
            assert_eq!(
                parsed.blocks.len(),
                1,
                "expected 1 block for prefix `{prefix}`; got {}",
                parsed.blocks.len()
            );
        }
    }

    // ── Hunk overlap unit tests ────────────────────────────────────────────────

    #[test]
    fn hunk_touching_first_line_of_block() {
        assert!(hunk_touches_range(1, 1, 1, 3));
    }

    #[test]
    fn hunk_touching_last_line_of_block() {
        assert!(hunk_touches_range(3, 1, 1, 3));
    }

    #[test]
    fn hunk_before_block_does_not_touch() {
        assert!(!hunk_touches_range(0, 0, 1, 3));
    }

    #[test]
    fn hunk_after_block_does_not_touch() {
        assert!(!hunk_touches_range(4, 1, 1, 3));
    }

    #[test]
    fn zero_len_hunk_at_block_boundary() {
        // An insertion at line 2 (between lines 1 and 2) touches range [1,3].
        assert!(hunk_touches_range(2, 0, 1, 3));
    }

    #[test]
    fn zero_len_hunk_beyond_block() {
        // An insertion at line 5 does not touch range [1,3].
        assert!(!hunk_touches_range(5, 0, 1, 3));
    }
}
