use std::collections::HashMap;
use std::path::PathBuf;

use crate::input::FileLineDelta;

pub(super) fn parse_line_deltas_from_git_patch(patch: &str) -> HashMap<PathBuf, FileLineDelta> {
    let mut output = HashMap::new();

    let mut current_path: Option<PathBuf> = None;
    let mut current_delta = FileLineDelta::default();

    let flush = |path: &Option<PathBuf>,
                 delta: FileLineDelta,
                 output: &mut HashMap<PathBuf, FileLineDelta>| {
        if let Some(path) = path {
            output
                .entry(path.clone())
                .and_modify(|existing| {
                    existing.added_lines = existing.added_lines.saturating_add(delta.added_lines);
                    existing.removed_lines =
                        existing.removed_lines.saturating_add(delta.removed_lines);
                })
                .or_insert(delta);
        }
    };

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(&current_path, current_delta, &mut output);
            current_delta = FileLineDelta::default();
            current_path = parse_diff_git_new_path(rest);
            continue;
        }

        if let Some(rest) = line.strip_prefix("+++ ") {
            if let Some(path) = parse_patch_path(rest) {
                current_path = Some(path);
            }
            continue;
        }

        if line.starts_with('+') && !line.starts_with("+++") {
            current_delta.added_lines = current_delta.added_lines.saturating_add(1);
            continue;
        }

        if line.starts_with('-') && !line.starts_with("---") {
            current_delta.removed_lines = current_delta.removed_lines.saturating_add(1);
            continue;
        }
    }

    flush(&current_path, current_delta, &mut output);
    output
}

fn parse_diff_git_new_path(rest: &str) -> Option<PathBuf> {
    let mut parts = rest.split_whitespace();
    let _old = parts.next()?;
    let new = parts.next()?;
    parse_patch_path(new)
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::parse_line_deltas_from_git_patch;

    #[test]
    fn parses_line_deltas_from_git_patch() {
        let deltas = parse_line_deltas_from_git_patch(
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

        let existing = deltas
            .get(&PathBuf::from("src/lib.rs"))
            .expect("src/lib.rs delta");
        assert_eq!(existing.added_lines, 2);
        assert_eq!(existing.removed_lines, 1);

        let new_file = deltas
            .get(&PathBuf::from("src/new.rs"))
            .expect("src/new.rs delta");
        assert_eq!(new_file.added_lines, 1);
        assert_eq!(new_file.removed_lines, 0);
    }
}
