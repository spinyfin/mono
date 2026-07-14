use std::path::Path;

use async_trait::async_trait;

use crate::{ConflictClass, ConflictedFile, DeterministicResolver, ResolveOutcome};

const CONFLICT_START_PREFIX: &str = "<<<<<<< Conflict ";
const DIFF_HEADER: &str = "%%%%%%% Changes from base to side #1";
const SNAPSHOT_HEADER: &str = "+++++++ Contents of side #2";
const CONFLICT_END_PREFIX: &str = ">>>>>>> Conflict ";

/// Resolves the "pure-append registry union" formulaic class named in
/// rung 0's future built-ins (`mod.rs` / `lib.rs` registries, the
/// highest-volume append/registry hotspot per the design's conflict
/// data): both branches, diverged from a common base, add distinct new
/// declaration lines (`mod foo;`, `pub use bar::Baz;`, ...) at the same
/// point. jj's structural merge already auto-resolves everything except
/// the genuinely overlapping region (rung 1 runs first); what's left is
/// a two-sided marker block. When side #1's "changes from base" are
/// pure `+` lines (no context, no removals), the base contributed
/// nothing to that region on either side, so side #1's additions and
/// side #2's full content are both non-overlapping insertions that a
/// line-oriented 3-way merge only flagged because of ordering
/// ambiguity — safe to union.
///
/// Declines (leaving the file untouched) whenever: any conflict block
/// doesn't match this exact jj two-sided diff/snapshot shape (e.g. a
/// 3+-sided conflict, or a diff line that is a removal or unchanged
/// context — meaning the base *did* contribute something, so this
/// isn't a pure append), or any resulting line isn't a bare
/// `mod`/`use` declaration. Deliberately conservative: it only fires on
/// the shape the class name promises, never on anything resembling real
/// code.
pub struct RegistryAppendUnionResolver;

impl RegistryAppendUnionResolver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RegistryAppendUnionResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DeterministicResolver for RegistryAppendUnionResolver {
    fn class(&self) -> ConflictClass {
        ConflictClass::RegistryAppendUnion
    }

    fn applies_to(&self, file: &ConflictedFile) -> bool {
        matches!(
            Path::new(&file.path).file_name().and_then(|name| name.to_str()),
            Some("mod.rs") | Some("lib.rs")
        )
    }

    async fn resolve(&self, workspace_path: &Path, file: &ConflictedFile) -> ResolveOutcome {
        let full_path = workspace_path.join(&file.path);
        let content = match std::fs::read_to_string(&full_path) {
            Ok(content) => content,
            Err(e) => {
                return ResolveOutcome::Declined {
                    reason: format!("failed to read {}: {e}", file.path),
                };
            }
        };

        match union_pure_appends(&content) {
            Ok(resolved) => match std::fs::write(&full_path, resolved) {
                Ok(()) => ResolveOutcome::Resolved {
                    summary: format!("unioned pure-append registry conflict(s) in {}", file.path),
                },
                Err(e) => ResolveOutcome::Declined {
                    reason: format!("failed to write resolved {}: {e}", file.path),
                },
            },
            Err(reason) => ResolveOutcome::Declined { reason },
        }
    }
}

/// Whether `line` is a bare `mod`/`use` declaration: an optional
/// visibility modifier (`pub`, `pub(crate)`, `pub(super)`, `pub(in
/// ...)`), then `mod ...;` or `use ...;`. Anything else (inline module
/// bodies, attributes, arbitrary statements) is rejected — this
/// resolver only ever produces lines that look like the ones it
/// consumed.
fn is_registry_declaration_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.ends_with(';') {
        return false;
    }
    let rest = strip_visibility(trimmed);
    rest.starts_with("mod ") || rest.starts_with("use ")
}

fn strip_visibility(s: &str) -> &str {
    if let Some(after_pub) = s.strip_prefix("pub") {
        let after_pub = after_pub.trim_start();
        if let Some(rest) = s.strip_prefix("pub(")
            && let Some(close) = rest.find(')')
        {
            return rest[close + 1..].trim_start();
        }
        return after_pub;
    }
    s
}

/// Parses `content` for jj two-sided conflict blocks and, if every
/// block is a pure-append shape, returns the file with each block
/// replaced by its unioned resolution. Returns `Err` with a decline
/// reason (and leaves the caller's copy of `content` untouched) the
/// moment any block fails to qualify.
fn union_pure_appends(content: &str) -> Result<String, String> {
    let ends_with_newline = content.ends_with('\n');
    let lines: Vec<&str> = content.lines().collect();

    let mut out: Vec<String> = Vec::new();
    let mut saw_conflict = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if !line.starts_with(CONFLICT_START_PREFIX) {
            out.push(line.to_owned());
            i += 1;
            continue;
        }

        saw_conflict = true;
        i += 1;
        if lines.get(i) != Some(&DIFF_HEADER) {
            return Err(format!(
                "conflict block at line {} does not have the expected two-sided diff header (not a pure-append shape, or has more than two sides)",
                i
            ));
        }
        i += 1;

        let mut side1_added = Vec::new();
        loop {
            match lines.get(i) {
                Some(&SNAPSHOT_HEADER) => {
                    i += 1;
                    break;
                }
                Some(&diff_line) => {
                    let Some(added) = diff_line.strip_prefix('+') else {
                        return Err(format!(
                            "conflict block's side #1 diff contains a non-addition line at {i} \
                             (removal or unchanged context) — base contributed content here, so this isn't a pure append"
                        ));
                    };
                    side1_added.push(added.to_owned());
                    i += 1;
                }
                None => return Err("conflict block ended before side #2's header".to_owned()),
            }
        }

        let mut side2_lines = Vec::new();
        loop {
            match lines.get(i) {
                Some(&candidate) if candidate.starts_with(CONFLICT_END_PREFIX) => {
                    i += 1;
                    break;
                }
                Some(&content_line) => {
                    side2_lines.push(content_line.to_owned());
                    i += 1;
                }
                None => return Err("conflict block ended before its closing marker".to_owned()),
            }
        }

        for candidate in side1_added.iter().chain(side2_lines.iter()) {
            if !is_registry_declaration_line(candidate) {
                return Err(format!(
                    "conflict block contains a non-declaration line ({candidate:?}); only bare `mod`/`use` lines qualify for registry union"
                ));
            }
        }

        for l in side1_added {
            out.push(l);
        }
        for l in side2_lines {
            if !out.contains(&l) {
                out.push(l);
            }
        }
    }

    if !saw_conflict {
        return Err("no jj conflict markers found".to_owned());
    }

    let mut resolved = out.join("\n");
    if ends_with_newline {
        resolved.push('\n');
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> ConflictedFile {
        ConflictedFile {
            path: path.to_owned(),
            marker_count: Some(1),
            shape: "content".to_owned(),
        }
    }

    #[test]
    fn applies_to_matches_mod_rs_and_lib_rs_only() {
        let resolver = RegistryAppendUnionResolver::new();
        assert!(resolver.applies_to(&file("mod.rs")));
        assert!(resolver.applies_to(&file("tools/boss/engine/deterministic-resolvers/src/resolvers/mod.rs")));
        assert!(resolver.applies_to(&file("lib.rs")));
        assert!(!resolver.applies_to(&file("main.rs")));
        assert!(!resolver.applies_to(&file("Cargo.lock")));
    }

    #[tokio::test]
    async fn unions_a_pure_append_conflict_at_end_of_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mod.rs"),
            "mod a;\n\
             mod b;\n\
             <<<<<<< Conflict 1 of 1\n\
             %%%%%%% Changes from base to side #1\n\
             +mod c;\n\
             +++++++ Contents of side #2\n\
             mod d;\n\
             >>>>>>> Conflict 1 of 1 ends\n",
        )
        .unwrap();

        let resolver = RegistryAppendUnionResolver::new();
        let outcome = resolver.resolve(dir.path(), &file("mod.rs")).await;

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }), "{outcome:?}");
        let resolved = std::fs::read_to_string(dir.path().join("mod.rs")).unwrap();
        assert_eq!(resolved, "mod a;\nmod b;\nmod c;\nmod d;\n");
    }

    #[tokio::test]
    async fn unions_a_conflict_between_surrounding_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "mod a;\n\
             mod b;\n\
             <<<<<<< Conflict 1 of 1\n\
             %%%%%%% Changes from base to side #1\n\
             +mod q;\n\
             +++++++ Contents of side #2\n\
             mod r;\n\
             >>>>>>> Conflict 1 of 1 ends\n\
             mod c;\n",
        )
        .unwrap();

        let resolver = RegistryAppendUnionResolver::new();
        let outcome = resolver.resolve(dir.path(), &file("lib.rs")).await;

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }), "{outcome:?}");
        let resolved = std::fs::read_to_string(dir.path().join("lib.rs")).unwrap();
        assert_eq!(resolved, "mod a;\nmod b;\nmod q;\nmod r;\nmod c;\n");
    }

    #[tokio::test]
    async fn dedupes_an_identical_line_added_by_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mod.rs"),
            "<<<<<<< Conflict 1 of 1\n\
             %%%%%%% Changes from base to side #1\n\
             +pub use shared::Thing;\n\
             +++++++ Contents of side #2\n\
             pub use shared::Thing;\n\
             >>>>>>> Conflict 1 of 1 ends\n",
        )
        .unwrap();

        let resolver = RegistryAppendUnionResolver::new();
        let outcome = resolver.resolve(dir.path(), &file("mod.rs")).await;

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }), "{outcome:?}");
        let resolved = std::fs::read_to_string(dir.path().join("mod.rs")).unwrap();
        assert_eq!(resolved, "pub use shared::Thing;\n");
    }

    #[tokio::test]
    async fn handles_multiple_independent_conflict_blocks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mod.rs"),
            "<<<<<<< Conflict 1 of 2\n\
             %%%%%%% Changes from base to side #1\n\
             +mod a1;\n\
             +++++++ Contents of side #2\n\
             mod a2;\n\
             >>>>>>> Conflict 1 of 2 ends\n\
             mod shared;\n\
             <<<<<<< Conflict 2 of 2\n\
             %%%%%%% Changes from base to side #1\n\
             +mod b1;\n\
             +++++++ Contents of side #2\n\
             mod b2;\n\
             >>>>>>> Conflict 2 of 2 ends\n",
        )
        .unwrap();

        let resolver = RegistryAppendUnionResolver::new();
        let outcome = resolver.resolve(dir.path(), &file("mod.rs")).await;

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }), "{outcome:?}");
        let resolved = std::fs::read_to_string(dir.path().join("mod.rs")).unwrap();
        assert_eq!(resolved, "mod a1;\nmod a2;\nmod shared;\nmod b1;\nmod b2;\n");
    }

    #[tokio::test]
    async fn declines_when_side_one_diff_contains_a_removal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mod.rs"),
            "<<<<<<< Conflict 1 of 1\n\
             %%%%%%% Changes from base to side #1\n\
             -mod old;\n\
             +mod new;\n\
             +++++++ Contents of side #2\n\
             mod other;\n\
             >>>>>>> Conflict 1 of 1 ends\n",
        )
        .unwrap();

        let resolver = RegistryAppendUnionResolver::new();
        let outcome = resolver.resolve(dir.path(), &file("mod.rs")).await;

        match outcome {
            ResolveOutcome::Declined { reason } => assert!(reason.contains("isn't a pure append")),
            other => panic!("expected Declined, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("mod.rs")).unwrap(),
            "<<<<<<< Conflict 1 of 1\n\
             %%%%%%% Changes from base to side #1\n\
             -mod old;\n\
             +mod new;\n\
             +++++++ Contents of side #2\n\
             mod other;\n\
             >>>>>>> Conflict 1 of 1 ends\n",
            "declined file must be left untouched"
        );
    }

    #[tokio::test]
    async fn declines_on_a_three_sided_conflict() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mod.rs"),
            "<<<<<<< Conflict 1 of 1\n\
             %%%%%%% Changes from base to side #1\n\
             +mod a;\n\
             %%%%%%% Changes from base to side #2\n\
             +mod b;\n\
             +++++++ Contents of side #3\n\
             mod c;\n\
             >>>>>>> Conflict 1 of 1 ends\n",
        )
        .unwrap();

        let resolver = RegistryAppendUnionResolver::new();
        let outcome = resolver.resolve(dir.path(), &file("mod.rs")).await;

        assert!(matches!(outcome, ResolveOutcome::Declined { .. }), "{outcome:?}");
    }

    #[tokio::test]
    async fn declines_when_a_line_is_not_a_bare_declaration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mod.rs"),
            "<<<<<<< Conflict 1 of 1\n\
             %%%%%%% Changes from base to side #1\n\
             +fn added() {}\n\
             +++++++ Contents of side #2\n\
             mod other;\n\
             >>>>>>> Conflict 1 of 1 ends\n",
        )
        .unwrap();

        let resolver = RegistryAppendUnionResolver::new();
        let outcome = resolver.resolve(dir.path(), &file("mod.rs")).await;

        match outcome {
            ResolveOutcome::Declined { reason } => assert!(reason.contains("non-declaration line")),
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn declines_when_no_conflict_markers_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mod.rs"), "mod a;\nmod b;\n").unwrap();

        let resolver = RegistryAppendUnionResolver::new();
        let outcome = resolver.resolve(dir.path(), &file("mod.rs")).await;

        assert!(matches!(outcome, ResolveOutcome::Declined { .. }), "{outcome:?}");
    }

    #[test]
    fn declaration_line_recognizes_visibility_modifiers() {
        assert!(is_registry_declaration_line("mod foo;"));
        assert!(is_registry_declaration_line("pub mod foo;"));
        assert!(is_registry_declaration_line("pub(crate) mod foo;"));
        assert!(is_registry_declaration_line("pub(super) use foo::Bar;"));
        assert!(is_registry_declaration_line("  use std::fmt;  "));
        assert!(!is_registry_declaration_line("fn foo() {}"));
        assert!(!is_registry_declaration_line("mod foo {}"));
        assert!(!is_registry_declaration_line(""));
    }
}
