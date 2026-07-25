use std::collections::HashMap;
use std::path::PathBuf;

use regex::Regex;

use crate::input::{DiffHunk, FileDiff, FileLineDelta};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedPatchFileDiff {
    pub file_diff: FileDiff,
    pub line_delta: FileLineDelta,
}

pub(super) fn parse_file_diffs_from_git_patch(patch: &str) -> HashMap<PathBuf, ParsedPatchFileDiff> {
    let mut output = HashMap::new();

    let mut current_old_path: Option<PathBuf> = None;
    let mut current_new_path: Option<PathBuf> = None;
    let mut current_effective_old_path: Option<PathBuf> = None;
    let mut current_effective_new_path: Option<PathBuf> = None;
    let mut current_hunks = Vec::new();
    let mut current_delta = FileLineDelta::default();
    let mut current_added_ranges: Vec<(u32, u32)> = Vec::new();
    // Post-image line counter, advanced by context and `+` lines while walking a
    // hunk's body; `None` outside a hunk. Seeded from each hunk header's
    // `new_start` so added-line numbers are exact even though the parser also
    // sees `-U3`-context diffs (see `pending_added_run`).
    let mut new_line_counter: Option<u32> = None;
    // An in-progress run of contiguous added lines, closed (pushed into
    // `current_added_ranges`) whenever a context line or hunk boundary breaks
    // contiguity.
    let mut pending_added_run: Option<(u32, u32)> = None;

    let close_pending_run = |pending: &mut Option<(u32, u32)>, ranges: &mut Vec<(u32, u32)>| {
        if let Some(run) = pending.take() {
            ranges.push(run);
        }
    };

    let flush = |old_path: &Option<PathBuf>,
                 new_path: &Option<PathBuf>,
                 hunks: &mut Vec<DiffHunk>,
                 delta: FileLineDelta,
                 added_ranges: &mut Vec<(u32, u32)>,
                 output: &mut HashMap<PathBuf, ParsedPatchFileDiff>| {
        let path = new_path.as_ref().or(old_path.as_ref());
        let Some(path) = path else {
            hunks.clear();
            added_ranges.clear();
            return;
        };

        let file_diff = FileDiff {
            hunks: std::mem::take(hunks),
            added_line_ranges: std::mem::take(added_ranges),
        };
        output
            .entry(path.clone())
            .and_modify(|existing| {
                existing.line_delta.added_lines = existing.line_delta.added_lines.saturating_add(delta.added_lines);
                existing.line_delta.removed_lines =
                    existing.line_delta.removed_lines.saturating_add(delta.removed_lines);
                existing.file_diff.hunks.extend(file_diff.hunks.clone());
                existing
                    .file_diff
                    .added_line_ranges
                    .extend(file_diff.added_line_ranges.clone());
            })
            .or_insert(ParsedPatchFileDiff {
                file_diff,
                line_delta: delta,
            });
    };

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if current_effective_old_path.is_none() {
                current_effective_old_path = current_old_path.clone();
            }
            if current_effective_new_path.is_none() {
                current_effective_new_path = current_new_path.clone();
            }
            close_pending_run(&mut pending_added_run, &mut current_added_ranges);
            new_line_counter = None;
            flush(
                &current_effective_old_path,
                &current_effective_new_path,
                &mut current_hunks,
                current_delta,
                &mut current_added_ranges,
                &mut output,
            );
            current_delta = FileLineDelta::default();
            current_hunks.clear();
            current_effective_old_path = None;
            current_effective_new_path = None;
            (current_old_path, current_new_path) = parse_diff_git_paths(rest);
            continue;
        }

        // The `--- `/`+++ ` file-header forms only occur between a `diff --git`
        // line and the first `@@` hunk header; once inside a hunk body, a line
        // starting with `--- `/`+++ ` is content (e.g. a deleted markdown rule or
        // an added `++ ` comment), not a header, so these branches must not fire.
        let in_hunk = new_line_counter.is_some();

        if !in_hunk {
            if let Some(rest) = line.strip_prefix("--- ") {
                current_effective_old_path = parse_patch_path(rest);
                continue;
            }

            if let Some(rest) = line.strip_prefix("+++ ") {
                current_effective_new_path = parse_patch_path(rest);
                continue;
            }
        }

        if line.starts_with("@@") {
            close_pending_run(&mut pending_added_run, &mut current_added_ranges);
            if let Some(hunk) = parse_hunk_header(line) {
                new_line_counter = Some(hunk.new_start as u32);
                current_hunks.push(hunk);
            }
            continue;
        }

        if line.starts_with('+') && (in_hunk || !line.starts_with("+++")) {
            current_delta.added_lines = current_delta.added_lines.saturating_add(1);
            if let Some(hunk) = current_hunks.last_mut() {
                hunk.added_lines = hunk.added_lines.saturating_add(1);
            }
            if let Some(counter) = new_line_counter {
                match &mut pending_added_run {
                    Some((_, end)) if *end + 1 == counter => *end = counter,
                    _ => {
                        close_pending_run(&mut pending_added_run, &mut current_added_ranges);
                        pending_added_run = Some((counter, counter));
                    }
                }
                new_line_counter = Some(counter + 1);
            }
            continue;
        }

        if line.starts_with('-') && (in_hunk || !line.starts_with("---")) {
            current_delta.removed_lines = current_delta.removed_lines.saturating_add(1);
            if let Some(hunk) = current_hunks.last_mut() {
                hunk.removed_lines = hunk.removed_lines.saturating_add(1);
            }
            // A removed line consumes no post-image line number, so it neither
            // advances `new_line_counter` nor breaks a pending added-line run:
            // `-old` immediately followed by `+new` still yields a contiguous
            // added range across the replacement.
            continue;
        }

        // Any other in-hunk line (context ` `, or a `\ No newline` marker) is not
        // an added line: close any pending run, and advance the post-image
        // counter for real context lines (a context line occupies a post-image
        // line; a `\`-marker line does not).
        if new_line_counter.is_some() {
            close_pending_run(&mut pending_added_run, &mut current_added_ranges);
            if !line.starts_with('\\') {
                new_line_counter = new_line_counter.map(|counter| counter + 1);
            }
        }
    }

    close_pending_run(&mut pending_added_run, &mut current_added_ranges);
    if current_effective_old_path.is_none() {
        current_effective_old_path = current_old_path;
    }
    if current_effective_new_path.is_none() {
        current_effective_new_path = current_new_path;
    }
    flush(
        &current_effective_old_path,
        &current_effective_new_path,
        &mut current_hunks,
        current_delta,
        &mut current_added_ranges,
        &mut output,
    );
    output
}

fn parse_diff_git_paths(rest: &str) -> (Option<PathBuf>, Option<PathBuf>) {
    let mut parts = rest.split_whitespace();
    let old = parts.next().and_then(parse_patch_path);
    let new = parts.next().and_then(parse_patch_path);
    (old, new)
}

fn parse_patch_path(raw: &str) -> Option<PathBuf> {
    if raw == "/dev/null" {
        return None;
    }
    if let Some(stripped) = raw.strip_prefix("a/") {
        return Some(PathBuf::from(stripped));
    }
    if let Some(stripped) = raw.strip_prefix("b/") {
        return Some(PathBuf::from(stripped));
    }
    Some(PathBuf::from(raw))
}

fn parse_hunk_header(line: &str) -> Option<DiffHunk> {
    let pattern = Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").expect("valid hunk regex");
    let captures = pattern.captures(line)?;

    Some(DiffHunk {
        old_start: captures.get(1)?.as_str().parse().ok()?,
        old_lines: captures
            .get(2)
            .and_then(|value| value.as_str().parse().ok())
            .unwrap_or(1),
        new_start: captures.get(3)?.as_str().parse().ok()?,
        new_lines: captures
            .get(4)
            .and_then(|value| value.as_str().parse().ok())
            .unwrap_or(1),
        added_lines: 0,
        removed_lines: 0,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::parse_file_diffs_from_git_patch;

    #[test]
    fn parses_file_diffs_from_git_patch() {
        let diffs = parse_file_diffs_from_git_patch(
            r#"
diff --git a/src/lib.rs b/src/lib.rs
index 0000000..1111111 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@
-old
+new
+more
 same
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1 @@
+created
"#,
        );

        let existing = diffs.get(&PathBuf::from("src/lib.rs")).expect("src/lib.rs delta");
        assert_eq!(existing.line_delta.added_lines, 2);
        assert_eq!(existing.line_delta.removed_lines, 1);
        assert_eq!(existing.file_diff.hunks.len(), 1);
        assert_eq!(existing.file_diff.hunks[0].old_start, 1);
        assert_eq!(existing.file_diff.hunks[0].old_lines, 2);
        assert_eq!(existing.file_diff.hunks[0].new_start, 1);
        assert_eq!(existing.file_diff.hunks[0].new_lines, 3);
        // `-old` immediately followed by `+new`/`+more` yields one contiguous
        // added range (1..=2), even though the replaced `-old` line sits between
        // the hunk header and the first `+` line.
        assert_eq!(existing.file_diff.added_line_ranges, vec![(1, 2)]);

        let new_file = diffs.get(&PathBuf::from("src/new.rs")).expect("src/new.rs delta");
        assert_eq!(new_file.line_delta.added_lines, 1);
        assert_eq!(new_file.line_delta.removed_lines, 0);
        assert_eq!(new_file.file_diff.hunks[0].old_start, 0);
        assert_eq!(new_file.file_diff.hunks[0].old_lines, 0);
        assert_eq!(new_file.file_diff.added_line_ranges, vec![(1, 1)]);
    }

    #[test]
    fn added_line_ranges_break_on_context_lines_and_span_hunks() {
        let diffs = parse_file_diffs_from_git_patch(
            r#"
diff --git a/src/lib.rs b/src/lib.rs
index 0000000..1111111 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,5 +1,6 @@
 unchanged1
+added2
+added3
 unchanged4
+added5
 unchanged6
@@ -20,3 +21,4 @@
 unchanged21
+added22
 unchanged23
"#,
        );

        let diff = diffs.get(&PathBuf::from("src/lib.rs")).expect("src/lib.rs diff");
        assert_eq!(diff.file_diff.added_line_ranges, vec![(2, 3), (5, 5), (22, 22)]);
    }

    #[test]
    fn renamed_unchanged_file_has_no_added_line_ranges() {
        let diffs = parse_file_diffs_from_git_patch(
            r#"
diff --git a/src/old_name.rs b/src/new_name.rs
similarity index 100%
rename from src/old_name.rs
rename to src/new_name.rs
"#,
        );

        let diff = diffs.get(&PathBuf::from("src/new_name.rs")).expect("renamed file diff");
        assert!(diff.file_diff.added_line_ranges.is_empty());
        assert!(diff.file_diff.hunks.is_empty());
    }

    #[test]
    fn binary_file_hunk_is_skipped_text_hunks_still_parsed() {
        // A patch with a binary file followed by a text file: the binary entry
        // produces no line-delta (no @@ headers), and the text file is still parsed.
        let diffs = parse_file_diffs_from_git_patch(
            r#"
diff --git a/data.bin b/data.bin
new file mode 100644
index 0000000..1234567
Binary files /dev/null and b/data.bin differ
diff --git a/src/lib.rs b/src/lib.rs
index 0000000..1111111 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1,2 @@
 existing
+new line
"#,
        );

        assert!(
            !diffs.contains_key(&PathBuf::from("data.bin"))
                || diffs
                    .get(&PathBuf::from("data.bin"))
                    .is_some_and(|d| d.line_delta.added_lines == 0 && d.file_diff.hunks.is_empty()),
            "binary file should have no line-delta or no hunks"
        );

        let text_diff = diffs.get(&PathBuf::from("src/lib.rs")).expect("text file diff");
        assert_eq!(text_diff.line_delta.added_lines, 1);
        assert_eq!(text_diff.line_delta.removed_lines, 0);
    }

    #[test]
    fn parses_deleted_file_patch_under_old_path() {
        let diffs = parse_file_diffs_from_git_patch(
            r#"
diff --git a/src/old.rs b/src/old.rs
deleted file mode 100644
index 1111111..0000000
--- a/src/old.rs
+++ /dev/null
@@ -1 +0,0 @@
-gone
"#,
        );

        let deleted = diffs.get(&PathBuf::from("src/old.rs")).expect("deleted file diff");
        assert_eq!(deleted.line_delta.added_lines, 0);
        assert_eq!(deleted.line_delta.removed_lines, 1);
        assert_eq!(deleted.file_diff.hunks[0].old_start, 1);
        assert_eq!(deleted.file_diff.hunks[0].new_start, 0);
        assert!(deleted.file_diff.added_line_ranges.is_empty());
    }

    #[test]
    fn removed_dashes_line_does_not_shift_added_line_ranges() {
        // Deleting a line whose content is exactly `---` (a markdown horizontal
        // rule / YAML frontmatter separator) becomes the patch line `----`, which
        // starts with `---`. It must still be classified as a removal (by the
        // in-hunk leading `-`), not mistaken for a `--- ` file header or folded
        // into the context branch, or `added` would be reported one line early.
        let diffs = parse_file_diffs_from_git_patch(
            r#"
diff --git a/notes.md b/notes.md
index 0000000..1111111 100644
--- a/notes.md
+++ b/notes.md
@@ -1,3 +1,3 @@
-title
----
 body
+added
"#,
        );

        let diff = diffs.get(&PathBuf::from("notes.md")).expect("notes.md diff");
        assert_eq!(diff.line_delta.added_lines, 1);
        assert_eq!(diff.line_delta.removed_lines, 2);
        // The added line is post-image line 2 (the `body` context line is
        // post-image line 1); if the `----` removal were miscounted as context,
        // this would come out as (3, 3) instead.
        assert_eq!(diff.file_diff.added_line_ranges, vec![(2, 2)]);
    }

    #[test]
    fn added_plus_plus_line_is_not_mistaken_for_file_header() {
        // Adding a line whose content is `++ foo` becomes the patch line
        // `+++ foo`, which starts with `+++`. Inside a hunk body this must still
        // be classified as an addition, not mistaken for a `+++ ` file header
        // (which would silently mis-key `current_effective_new_path`).
        let diffs = parse_file_diffs_from_git_patch(
            r#"
diff --git a/src/lib.rs b/src/lib.rs
index 0000000..1111111 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,1 +1,2 @@
 unchanged
+++ foo
"#,
        );

        let diff = diffs.get(&PathBuf::from("src/lib.rs")).expect("src/lib.rs diff");
        assert_eq!(diff.line_delta.added_lines, 1);
        assert_eq!(diff.file_diff.added_line_ranges, vec![(2, 2)]);
    }

    #[test]
    fn deletion_only_hunk_has_no_added_line_ranges() {
        let diffs = parse_file_diffs_from_git_patch(
            r#"
diff --git a/src/lib.rs b/src/lib.rs
index 0000000..1111111 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,1 @@
 kept
-gone1
-gone2
"#,
        );

        let diff = diffs.get(&PathBuf::from("src/lib.rs")).expect("src/lib.rs diff");
        assert_eq!(diff.line_delta.added_lines, 0);
        assert_eq!(diff.line_delta.removed_lines, 2);
        assert!(diff.file_diff.added_line_ranges.is_empty());
    }
}
