//! Keeps both sides' additions to a brace-wrapped `use` list.
//!
//! rustfmt packs the names of a long `use a::{…}` list to the line width,
//! so adding one name shifts every later line and two unrelated additions
//! collide as "both sides changed the same lines". The lines differ but
//! the list is a set: when each side only added names, the merged list is
//! the base's names plus both sides' additions, and rustfmt lays it out
//! again.

use std::collections::BTreeMap;

use super::markers::ConflictHunk;

/// The path of the `use` list the line after `preceding` sits directly
/// inside: `crate::protocol::` after `use crate::protocol::{`. `None` when
/// that line comes after the declaration, is inside a nested group, or is
/// not in a `use` declaration at all.
pub(crate) fn enclosing_use_list(preceding: &[String]) -> Option<String> {
    let mut closed = 0usize;
    for line in preceding.iter().rev() {
        let code = line.split("//").next().unwrap_or("");
        for (index, ch) in code.char_indices().rev() {
            match ch {
                '}' => closed += 1,
                '{' if closed == 0 => return use_list_path(&code[..index]),
                '{' => closed -= 1,
                ';' if closed == 0 => return None,
                _ => {}
            }
        }
    }
    None
}

/// The path written on a `use` declaration's first line before its brace
/// list opens. `None` when the line does not start a `use` declaration, or
/// the brace it leaves open belongs to a nested group.
fn use_list_path(head: &str) -> Option<String> {
    let head = head.trim_start();
    let head = match head.strip_prefix("pub") {
        Some(rest) => match rest.trim_start().strip_prefix('(') {
            Some(scope) => scope.split_once(')').map_or("", |(_, after)| after),
            None => rest,
        },
        None => head,
    };
    let path = head.trim_start().strip_prefix("use ")?;
    if path.contains('{') {
        return None;
    }
    Some(path.chars().filter(|c| !c.is_whitespace()).collect())
}

/// The full paths an import brings into scope: `b::{C, d::E}` under the
/// prefix `a::` gives `a::b::C` and `a::b::d::E`. `prefix` is the path of
/// the list the entry sits in, and is empty for a whole declaration.
pub(crate) fn imported_paths(prefix: &str, entry: &str) -> Vec<String> {
    let entry: String = entry.chars().filter(|c| !c.is_whitespace()).collect();
    let mut paths = Vec::new();
    expand(prefix, &entry, &mut paths);
    paths
}

fn expand(prefix: &str, entry: &str, paths: &mut Vec<String>) {
    let group = entry
        .split_once('{')
        .and_then(|(head, rest)| Some((head, rest.strip_suffix('}')?)));
    let Some((head, inner)) = group else {
        if !entry.is_empty() {
            paths.push(format!("{prefix}{entry}"));
        }
        return;
    };
    let prefix = format!("{prefix}{head}");
    let mut depth = 0usize;
    let mut start = 0;
    for (index, ch) in inner.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                expand(&prefix, &inner[start..index], paths);
                start = index + 1;
            }
            _ => {}
        }
    }
    expand(&prefix, &inner[start..], paths);
}

/// A region of a `use` list with both sides' additions kept.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MergedEntries {
    /// One line holding the base's entries and both sides' additions, for
    /// rustfmt to wrap and sort.
    pub(crate) line: String,
    /// The entries side #1 added.
    pub(crate) added1: Vec<String>,
    /// The entries side #2 added.
    pub(crate) added2: Vec<String>,
}

/// Merges the three versions of a region of a `use` list.
///
/// Fails when a side removed or rewrote an entry (that is an edit, not an
/// insertion) or when both sides added the same entry.
pub(crate) fn merge_entries(hunk: &ConflictHunk) -> Result<MergedEntries, String> {
    let base = entries(&hunk.base)?;
    let side1 = entries(&hunk.side1)?;
    let side2 = entries(&hunk.side2)?;
    let added1 = added(&base, &side1, 1)?;
    let added2 = added(&base, &side2, 2)?;
    if let Some(shared) = added1.iter().find(|entry| added2.contains(entry)) {
        return Err(format!("both sides added the same import `{shared}`"));
    }
    let indent: String = [&hunk.side1, &hunk.side2, &hunk.base]
        .into_iter()
        .flatten()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.chars().take_while(|c| c.is_whitespace()).collect())
        .unwrap_or_default();
    let merged: Vec<&str> = side1.iter().chain(&added2).map(String::as_str).collect();
    Ok(MergedEntries {
        line: format!("{indent}{},", merged.join(", ")),
        added1,
        added2,
    })
}

/// The comma-separated entries on `lines`, each a path, a glob, a rename
/// or a complete nested group.
fn entries(lines: &[String]) -> Result<Vec<String>, String> {
    let mut joined = String::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains("//") || line.contains("/*") {
            return Err("an import list region contains a comment".to_owned());
        }
        if !line.ends_with(',') {
            return Err(format!(
                "an import list region has a line that is not a run of complete entries (`{line}`)"
            ));
        }
        joined.push_str(line);
        joined.push(' ');
    }
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for ch in joined.chars() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or("an import list region closes a group it did not open")?;
            }
            ',' if depth == 0 => {
                push_entry(&mut entries, &current)?;
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    if depth != 0 {
        return Err("an import list region leaves a nested group open".to_owned());
    }
    push_entry(&mut entries, &current)?;
    Ok(entries)
}

fn push_entry(entries: &mut Vec<String>, raw: &str) -> Result<(), String> {
    let entry = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if entry.is_empty() {
        return Ok(());
    }
    let plausible = entry
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '{' | '}' | ',' | '*' | ' '));
    if !plausible {
        return Err(format!("`{entry}` is not an import list entry"));
    }
    entries.push(entry);
    Ok(())
}

/// The entries `side` has beyond `base`, in order. Fails when `side`
/// lacks an entry the base had.
fn added(base: &[String], side: &[String], side_number: usize) -> Result<Vec<String>, String> {
    let mut remaining: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in base {
        *remaining.entry(entry.as_str()).or_default() += 1;
    }
    let mut added = Vec::new();
    for entry in side {
        match remaining.get_mut(entry.as_str()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => added.push(entry.clone()),
        }
    }
    match remaining.into_iter().find(|(_, count)| *count > 0) {
        Some((missing, _)) => Err(format!(
            "side #{side_number} removed or rewrote the import `{missing}`, which is an edit rather than an insertion"
        )),
        None => Ok(added),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn hunk(base: &[&str], side1: &[&str], side2: &[&str]) -> ConflictHunk {
        ConflictHunk {
            line: 1,
            base: lines(base),
            side1: lines(side1),
            side2: lines(side2),
        }
    }

    fn list_path(preceding: &[&str]) -> Option<String> {
        enclosing_use_list(&lines(preceding))
    }

    #[test]
    fn finds_the_path_of_the_use_list_a_position_is_directly_inside() {
        assert_eq!(
            list_path(&["use crate::protocol::{", "    Alpha, Beta,"]).as_deref(),
            Some("crate::protocol::")
        );
        assert_eq!(
            list_path(&["pub(crate) use crate::protocol::{"]).as_deref(),
            Some("crate::protocol::")
        );
        assert_eq!(
            list_path(&["pub use a::{", "    nested::{One, Two},", "    Three, // note"]).as_deref(),
            Some("a::")
        );
        assert_eq!(list_path(&["use {"]).as_deref(), Some(""));
    }

    #[test]
    fn rejects_positions_outside_a_use_list() {
        // After a finished declaration.
        assert_eq!(list_path(&["use crate::protocol::{", "    Alpha,", "};"]), None);
        assert_eq!(list_path(&["use crate::a::{B, C};"]), None);
        // Inside something that is not a `use`.
        assert_eq!(list_path(&["let v = vec![", "    Alpha,"]), None);
        assert_eq!(list_path(&["enum Stage {", "    Alpha,"]), None);
        // Inside a nested group of a `use`, on its own line or on the
        // declaration's first line.
        assert_eq!(list_path(&["use a::{", "    nested::{", "        One,"]), None);
        assert_eq!(list_path(&["use a::{B, nested::{"]), None);
        assert_eq!(list_path(&[]), None);
    }

    #[test]
    fn expands_an_import_into_the_paths_it_brings_in() {
        assert_eq!(imported_paths("", "std::fmt"), ["std::fmt"]);
        assert_eq!(imported_paths("crate::a::", "B"), ["crate::a::B"]);
        assert_eq!(
            imported_paths("a::", "b::{C, d::{E, F}, self}"),
            ["a::b::C", "a::b::d::E", "a::b::d::F", "a::b::self"]
        );
        assert_eq!(imported_paths("", "a::{B,C,}"), ["a::B", "a::C"]);
        assert_eq!(imported_paths("a::", "*"), ["a::*"]);
    }

    #[test]
    fn merges_additions_from_both_sides_of_a_reflowed_list() {
        let merged = merge_entries(&hunk(
            &["    Alpha, Beta,", "    Delta,"],
            &["    Alpha, Beta, Charlie,", "    Delta,"],
            &["    Alpha, Beta,", "    Delta, Echo,"],
        ))
        .unwrap();
        assert_eq!(merged.line, "    Alpha, Beta, Charlie, Delta, Echo,");
        assert_eq!(merged.added1, ["Charlie"]);
        assert_eq!(merged.added2, ["Echo"]);
    }

    #[test]
    fn merges_pure_insertions_with_an_empty_base() {
        let merged = merge_entries(&hunk(&[], &["    One,"], &["    two::{A, B},", "    Three as T,"])).unwrap();
        assert_eq!(merged.line, "    One, two::{A, B}, Three as T,");
        assert_eq!(merged.added2, ["two::{A, B}", "Three as T"]);
    }

    #[test]
    fn refuses_when_a_side_removed_an_entry() {
        let err = merge_entries(&hunk(
            &["    Alpha, Beta,"],
            &["    Alpha, Charlie,"],
            &["    Alpha, Beta, Delta,"],
        ))
        .unwrap_err();
        assert!(err.contains("side #1 removed or rewrote the import `Beta`"), "{err}");
    }

    #[test]
    fn refuses_when_both_sides_added_the_same_entry() {
        let err = merge_entries(&hunk(
            &["    Alpha,"],
            &["    Alpha, HashMap,"],
            &["    Alpha, HashMap, Zeta,"],
        ))
        .unwrap_err();
        assert!(err.contains("both sides added the same import `HashMap`"), "{err}");
    }

    #[test]
    fn refuses_regions_that_are_not_complete_entries() {
        let not_entries = merge_entries(&hunk(&[], &["    One,"], &["};"])).unwrap_err();
        assert!(not_entries.contains("not a run of complete entries"), "{not_entries}");
        let comment = merge_entries(&hunk(&[], &["    One, // why"], &["    Two,"])).unwrap_err();
        assert!(comment.contains("comment"), "{comment}");
        let open_group = merge_entries(&hunk(&[], &["    nested::{One,"], &["    Two,"])).unwrap_err();
        assert!(open_group.contains("leaves a nested group open"), "{open_group}");
        let code = merge_entries(&hunk(&[], &["    foo(),"], &["    Two,"])).unwrap_err();
        assert!(code.contains("not an import list entry"), "{code}");
    }
}
