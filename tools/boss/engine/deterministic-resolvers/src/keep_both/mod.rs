//! Keep-both resolution of same-spot insertions.
//!
//! The most common mechanical conflict is two branches each adding
//! something new at the same place: two tests at the end of one module,
//! two `mod` lines, two names in one `use` list. Neither side touched what
//! the other wrote, so the resolution is to keep both. This module decides
//! when that is safe to do without a model, and does it.
//!
//! A file is resolved only when **every** conflict in it qualifies:
//!
//! - the common ancestor had nothing at that spot (both sides inserted),
//!   or the spot is inside a `use` list where each side only added names;
//! - each side's insertion is self-contained: brackets balanced, not
//!   ending inside a string or comment;
//! - under [`KeepBothScope::ItemsAndImports`], each insertion is nothing
//!   but whole Rust items and import declarations;
//! - the two sides did not add the same thing.
//!
//! Anything else is refused with a reason, and the file is left for a
//! resolver that can exercise judgement. The result is never one side's
//! version: it always contains both insertions, side #1's first.
//!
//! Keeping both does not prove the result is right. Whether it compiles
//! and passes its tests is decided by the checks every pushed resolution
//! already goes through.

mod lexer;
mod markers;
mod rust_items;
mod use_list;

use std::collections::BTreeSet;

use lexer::Syntax;
use markers::{ConflictHunk, Segment};
use rust_items::{Item, Refusal};

/// How much a keep-both resolution is allowed to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepBothScope {
    /// Whole Rust items and import declarations only. Their order within
    /// a module does not change meaning, so keeping both is safe wherever
    /// the result compiles.
    ItemsAndImports,
    /// Any self-contained run of whole lines, which adds statements, list
    /// entries, fields, variants and match arms. Their order can matter,
    /// so this scope is for paths an operator has named.
    WholeLines,
}

/// What kind of insertion one resolved conflict was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InsertionKind {
    /// Definitions: functions, types, impls, constants, inline modules.
    Items,
    /// `use`, `mod name;` and `extern crate` declarations.
    Imports,
    /// Names added to a brace-wrapped `use` list.
    ImportList,
    /// Whole lines that are not items (statements, list entries).
    Lines,
}

impl InsertionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            InsertionKind::Items => "items",
            InsertionKind::Imports => "imports",
            InsertionKind::ImportList => "import_list",
            InsertionKind::Lines => "lines",
        }
    }
}

/// A file with every conflict replaced by both sides' insertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepBothResolution {
    /// The resolved content, not yet reformatted.
    pub content: String,
    /// One entry per conflict, in file order.
    pub insertions: Vec<InsertionKind>,
}

/// Resolves every conflict in `content` (a file with conflict markers, at
/// repository path `path`) by keeping both sides' insertions.
///
/// Returns the reason for refusing when any conflict does not qualify; a
/// refusal never resolves part of the file.
pub fn keep_both_insertions(path: &str, content: &str, scope: KeepBothScope) -> Result<KeepBothResolution, String> {
    let syntax = Syntax::for_path(path);
    if scope == KeepBothScope::ItemsAndImports && syntax != Syntax::Rust {
        return Err("whole items and imports are only recognised in Rust source".to_owned());
    }
    let mut parsed = markers::parse(content)?;
    if !parsed
        .segments
        .iter()
        .any(|segment| matches!(segment, Segment::Conflict(_)))
    {
        return Err("no conflict markers found".to_owned());
    }
    if syntax == Syntax::Rust {
        parsed.segments = realign(parsed.segments);
    }

    let mut out: Vec<String> = Vec::new();
    let mut insertions = Vec::new();
    let mut added = AddedItems::default();
    for segment in parsed.segments {
        let hunk = match segment {
            Segment::Text(lines) => {
                out.extend(lines);
                continue;
            }
            Segment::Conflict(hunk) => hunk,
        };
        let at = |reason: String| format!("conflict at line {}: {reason}", hunk.line);
        let use_list_path = if syntax == Syntax::Rust {
            use_list::enclosing_use_list(&out)
        } else {
            None
        };
        if let Some(list_path) = use_list_path {
            let merged = use_list::merge_entries(&hunk).map_err(at)?;
            added.record_list_entries(&list_path, &merged.added1, &merged.added2);
            out.push(merged.line);
            insertions.push(InsertionKind::ImportList);
            continue;
        }
        if !hunk.base.is_empty() {
            return Err(at(
                "both sides changed lines that were already there; only same-spot insertions are kept".to_owned(),
            ));
        }
        let (lines, kind) = union_insertion(&hunk, syntax, scope, &mut added).map_err(at)?;
        out.extend(lines);
        insertions.push(kind);
    }
    added.check()?;

    let mut resolved = out.join("\n");
    if parsed.ends_with_newline {
        resolved.push('\n');
    }
    Ok(KeepBothResolution {
        content: resolved,
        insertions,
    })
}

/// Both sides' insertions for one conflict whose base is empty.
fn union_insertion(
    hunk: &ConflictHunk,
    syntax: Syntax,
    scope: KeepBothScope,
    added: &mut AddedItems,
) -> Result<(Vec<String>, InsertionKind), String> {
    if syntax == Syntax::Rust {
        let side = |lines: &[String], number: usize| {
            rust_items::split_items(lines).map_err(|refusal| (format!("side #{number} {}", refusal.reason()), refusal))
        };
        match (side(&hunk.side1, 1), side(&hunk.side2, 2)) {
            (Ok(items1), Ok(items2)) => {
                let kind = if items1.iter().chain(&items2).all(|item| item.is_import) {
                    InsertionKind::Imports
                } else {
                    InsertionKind::Items
                };
                let lines = join_items(&hunk.side1, &items1, &hunk.side2, &items2);
                added.record(items1, items2);
                return Ok((lines, kind));
            }
            // A fragment cut out of the middle of something cannot be kept
            // under any scope; one that merely is not items can still be
            // kept as plain lines where the scope allows.
            (Err((why, Refusal::NotSelfContained(_))), _) | (_, Err((why, Refusal::NotSelfContained(_)))) => {
                return Err(why);
            }
            (Err((why, Refusal::NotItems(_))), _) | (_, Err((why, Refusal::NotItems(_)))) => {
                if scope == KeepBothScope::ItemsAndImports {
                    return Err(why);
                }
            }
        }
    }
    union_lines(hunk, syntax)
}

/// Side #1's items, then side #2's. Blank lines either side put at the
/// seam become one separator; so does the gap between two items when at
/// least one of them spans several lines.
fn join_items(side1: &[String], items1: &[Item], side2: &[String], items2: &[Item]) -> Vec<String> {
    let blank = |line: &&String| line.trim().is_empty();
    let trailing_blanks = side1.iter().rev().take_while(blank).count();
    let leading_blanks = side2.iter().take_while(blank).count();
    // Two one-line declarations (`mod a;`, `use b;`) sit on adjacent lines.
    let compact =
        items1.last().is_some_and(|item| item.single_line) && items2.first().is_some_and(|item| item.single_line);
    let mut out: Vec<String> = side1[..side1.len() - trailing_blanks].to_vec();
    if trailing_blanks + leading_blanks > 0 || !compact {
        out.push(String::new());
    }
    out.extend_from_slice(&side2[leading_blanks..]);
    out
}

/// Keeps both sides as plain lines, for insertions that are not items.
fn union_lines(hunk: &ConflictHunk, syntax: Syntax) -> Result<(Vec<String>, InsertionKind), String> {
    let mut starts: Vec<Vec<String>> = Vec::new();
    for (side, lines) in [(1, &hunk.side1), (2, &hunk.side2)] {
        let label = |why: String| format!("side #{side} {why}");
        let scanned = lexer::scan(&lines.join("\n"), syntax).map_err(label)?;
        let code = scanned.code();
        lexer::check_balanced(&code).map_err(label)?;
        let text = scanned.without_comments();
        if text.trim().is_empty() {
            return Err(label("adds no code".to_owned()));
        }
        starts.push(statement_starts(&code, &text));
    }
    if let Some(shared) = starts[0].iter().find(|line| starts[1].contains(line)) {
        return Err(format!("both sides added the same line `{shared}`"));
    }
    let lines = hunk.side1.iter().chain(&hunk.side2).cloned().collect();
    Ok((lines, InsertionKind::Lines))
}

/// The lines that begin a top-level statement or entry of a fragment,
/// whitespace-normalised: the units two sides would have in common if
/// they added the same thing. `code` (literals and comments blanked) is
/// read for structure, `text` (comments blanked) for what the line says.
fn statement_starts(code: &str, text: &str) -> Vec<String> {
    let mut starts = Vec::new();
    let mut depth = 0i64;
    let mut previous_ended_unit = true;
    for (code_line, text_line) in code.lines().zip(text.lines()) {
        let said = text_line.trim();
        if said.is_empty() {
            continue;
        }
        if depth == 0 && previous_ended_unit && said.chars().any(char::is_alphanumeric) {
            starts.push(said.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        for ch in code_line.chars() {
            match ch {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                _ => {}
            }
        }
        previous_ended_unit = code_line.trim_end().ends_with([';', ',', '{', '}']);
    }
    starts
}

/// Every item each side added anywhere in the file, to refuse a file in
/// which both sides added the same one.
#[derive(Default)]
struct AddedItems {
    side1: Vec<Item>,
    side2: Vec<Item>,
    /// The full path of everything each side imported, whether through a
    /// `use` declaration of its own or a name added to an existing list.
    imports1: BTreeSet<String>,
    imports2: BTreeSet<String>,
}

impl AddedItems {
    fn record(&mut self, items1: Vec<Item>, items2: Vec<Item>) {
        for (items, imports) in [(&items1, &mut self.imports1), (&items2, &mut self.imports2)] {
            for declaration in items
                .iter()
                .filter_map(|item| item.key.as_deref()?.strip_prefix("use:"))
            {
                imports.extend(use_list::imported_paths("", declaration));
            }
        }
        self.side1.extend(items1);
        self.side2.extend(items2);
    }

    /// Records the names each side added to the `use` list at `list_path`.
    fn record_list_entries(&mut self, list_path: &str, added1: &[String], added2: &[String]) {
        for (entries, imports) in [(added1, &mut self.imports1), (added2, &mut self.imports2)] {
            for entry in entries {
                imports.extend(use_list::imported_paths(list_path, entry));
            }
        }
    }

    fn check(&self) -> Result<(), String> {
        let keys1: BTreeSet<&str> = self.side1.iter().filter_map(|item| item.key.as_deref()).collect();
        if let Some(key) = self
            .side2
            .iter()
            .filter_map(|item| item.key.as_deref())
            .find(|key| keys1.contains(key))
        {
            let name = key.split_once(':').map_or(key, |(_, name)| name);
            return Err(format!("both sides added the same item `{name}`"));
        }
        // The same import can arrive in two spellings or two conflicts: a
        // name added to a list on one side and a declaration of its own on
        // the other, or two names added at different places in one list.
        if let Some(path) = self.imports1.intersection(&self.imports2).next() {
            return Err(format!("both sides added the same import `{path}`"));
        }
        for first in &self.side1 {
            let Some(shape) = first.shape.as_deref() else { continue };
            if let Some(second) = self.side2.iter().find(|item| item.shape.as_deref() == Some(shape)) {
                let name = |item: &Item| {
                    item.key
                        .as_deref()
                        .and_then(|key| key.split_once(':'))
                        .map_or("_", |(_, name)| name)
                        .to_owned()
                };
                return Err(format!(
                    "both sides added the same definition under two names (`{}` and `{}`)",
                    name(first),
                    name(second)
                ));
            }
        }
        Ok(())
    }
}

/// Moves a conflict's boundaries when the diff that produced it was
/// aligned one or more lines off an item boundary.
///
/// Appending a function after another one is commonly reported as
/// inserting "`}`, blank line, the new function without its closing
/// brace", with the closing brace that follows left as context. Both
/// sides then start with the same lines, and those lines are repeated
/// right after the conflict. Rotating them out of the conflict changes
/// nothing about either side's file but leaves each insertion a whole
/// item. The mirror case shares trailing lines with the text before it.
fn realign(segments: Vec<Segment>) -> Vec<Segment> {
    const MAX_SHIFT: usize = 8;
    // Give every conflict a text segment on both sides, empty if need be,
    // so shifted lines always have somewhere to go.
    let mut padded: Vec<Segment> = Vec::with_capacity(segments.len() * 2 + 1);
    for segment in segments {
        if matches!(segment, Segment::Conflict(_)) && !matches!(padded.last(), Some(Segment::Text(_))) {
            padded.push(Segment::Text(Vec::new()));
        }
        padded.push(segment);
    }
    if !matches!(padded.last(), Some(Segment::Text(_))) {
        padded.push(Segment::Text(Vec::new()));
    }

    for index in 1..padded.len().saturating_sub(1) {
        let [Segment::Text(before), Segment::Conflict(hunk), Segment::Text(after)] = &mut padded[index - 1..=index + 1]
        else {
            continue;
        };
        if !hunk.base.is_empty() || whole_items(&hunk.side1, &hunk.side2) {
            continue;
        }
        let limit = MAX_SHIFT.min(hunk.side1.len()).min(hunk.side2.len());

        // Shift down: the sides' shared leading lines also follow the conflict.
        let down = (1..=limit.min(after.len()))
            .take_while(|&k| hunk.side1[..k] == after[..k] && hunk.side2[..k] == after[..k])
            .find(|&k| whole_items(&rotated_left(&hunk.side1, k), &rotated_left(&hunk.side2, k)));
        if let Some(k) = down {
            before.extend(after.drain(..k));
            hunk.side1.rotate_left(k);
            hunk.side2.rotate_left(k);
            continue;
        }

        // Shift up: the sides' shared trailing lines also precede the conflict.
        let up = (1..=limit.min(before.len()))
            .take_while(|&k| {
                let tail = &before[before.len() - k..];
                hunk.side1[hunk.side1.len() - k..] == *tail && hunk.side2[hunk.side2.len() - k..] == *tail
            })
            .find(|&k| whole_items(&rotated_right(&hunk.side1, k), &rotated_right(&hunk.side2, k)));
        if let Some(k) = up {
            let moved = before.split_off(before.len() - k);
            after.splice(0..0, moved);
            hunk.side1.rotate_right(k);
            hunk.side2.rotate_right(k);
        }
    }
    padded
}

fn whole_items(side1: &[String], side2: &[String]) -> bool {
    rust_items::split_items(side1).is_ok() && rust_items::split_items(side2).is_ok()
}

fn rotated_left(lines: &[String], k: usize) -> Vec<String> {
    let mut rotated = lines.to_vec();
    rotated.rotate_left(k);
    rotated
}

fn rotated_right(lines: &[String], k: usize) -> Vec<String> {
    let mut rotated = lines.to_vec();
    rotated.rotate_right(k);
    rotated
}

#[cfg(test)]
mod tests;
