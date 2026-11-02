//! Shared text fragments used verbatim across multiple agent-prompt
//! renderers.
//!
//! Several agent prompts (see [`crate::pr_review`],
//! [`crate::automation_triage`]) end with an identical `## Boundaries` +
//! `## Probes` block. Keeping one copy here prevents the renderers from
//! drifting apart and makes an intentional wording change a one-line edit.
//!
//! Note that not every prompt shares this fragment: renderers with
//! *intentionally* different boundaries wording keep their own copy —
//! [`crate::answer_agent`] widens the first rule to "inside or outside"
//! (a strictly read-only agent) and omits the probes section,
//! and [`crate::worker_setup`] names a concrete sibling-workspace path and
//! adds a work-taxonomy sentence. Do not fold those onto this fragment
//! without matching each site's semantics.
//!
//! [`absolute_paths_fragment`], by contrast, IS shared by every worker
//! prompt — the harness behaviour it describes is not worker-kind-specific.

/// The `## Boundaries` + `## Probes` block shared verbatim by the
/// `pr_review` and `automation_triage` agent prompts.
///
/// The string starts at the `## Boundaries` heading and ends with a trailing
/// newline after the probes sentence, so a caller embeds it right
/// after the blank line that precedes the boundaries section.
pub fn boundaries_and_probes_fragment() -> &'static str {
    "## Boundaries\n\
     \n\
     - Do not modify files outside your workspace. Other workspaces\n\
     belong to other workers.\n\
     - Do not modify cube's database, lease state, or workspace registry.\n\
     - `~/Library/Application Support/Boss/` is engine-only.\n\
     Never read, write, or touch it.\n\
     `bossctl` is not available to workers.\n\
     \n\
     ## Probes\n\
     \n\
     Messages may arrive between turns. Treat them as questions from a\n\
     human reviewer — short, specific answers.\n"
}

/// Publication-time freshness check shared by PR-producing briefs and driver rules.
pub fn prepush_rebase_fragment() -> String {
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
    format!(
        "### Before every PR branch push\n\n\
         Before the first push and every later push (`{cube} pr create` or `{cube} pr update`), fetch and compare the branch's base with its current upstream tip; do not rely on the base recorded at dispatch or an earlier check. Use `{cube} workspace local-rebase --pr <bound-pr-number> --json` for an existing PR: it resolves the actual GitHub remote and PR base, including a parent PR branch for stacked PRs. Before a PR exists, use the same flow with `{cube} workspace local-rebase --base <intended-pr-base-branch> --json`, naming the branch the PR will target (the configured integration branch, or the parent branch for a stack).\n\n\
         The command fetches that remote and rebases the preserved working-copy history onto the freshly fetched tracking ref without publishing, repositioning the working copy, or collapsing commits. Use `jj` and the returned `destination` to compare ancestry and inspect the result. If the base has advanced, the branch must include the latest base before pushing. Resolve every conflicted commit from the base upward, integrating both sides' intent; never discard upstream changes, take either side wholesale, drop commits, squash, or reword existing descriptions. Preserve published PR bookmarks and advance the execution recovery bookmarks after rebasing.\n\n\
         After a rebase, re-run the builds and tests affected by the upstream changes and conflict resolutions before pushing, even when the rebase looked clean. Verify with `jj` that no commit to be pushed has unresolved conflicts. If the rebase or resolution cannot be completed cleanly, stop and report the conflicting files and reason; never push a half-resolved branch. Only then publish with the existing PR tooling.\n\n"
    )
}

/// The one-sentence rule for a command a worker deliberately runs in the
/// background: declare the wait, and never idle in a wait-loop without one.
///
/// This is the single copy of that wording for the prompts that do not carry
/// the full `## If you are waiting on a long-running job` section (triage, the
/// conflict-resolution gate). Callers put
/// their own "prefer the foreground" sentence in front of it, since what
/// counts as a long command differs per prompt. `boss` is the worker's
/// invocation of the boss binary
/// ([`boss_engine_worker_bin::WORKER_BOSS_INVOCATION`]).
pub fn background_wait_declaration(boss: &str) -> String {
    format!(
        "If you do background a long-running command, declare it with `{boss} propose wait --reason \"<what you are waiting on>\" --duration <bound>` before ending your turn, and never idle in a wait-loop without that declaration."
    )
}

/// The `## Always use absolute paths` block shared verbatim by every worker
/// prompt (standard, reviewer, triage, answer-agent).
///
/// Claude Code 2.1.257 added a permission-classifier branch that refuses to
/// auto-approve a compound Bash command pairing a `cd` with a *relative* file
/// read; 2.1.259 widened the same idea to a circuit breaker (explicitly not
/// classifier-approvable) over `grep`, `egrep`, `fgrep`, `rg`, `diff`, `git`,
/// `cp` and `mv`. An unattended `--permission-mode auto` worker that writes
/// commands in that shape stalls on a dialog nobody is watching.
///
/// Boss no longer arms the first of those branches (the Boss-data-dir fence
/// emits no `Read()` deny rule — see `worker_setup::deny_rules`), but this
/// instruction is a complement to that fix, not a substitute for it: the
/// absolute-path command shape is unaffected by either branch, and by
/// whatever the next release adds in the same vein. The engine-owned
/// boundaries are enforced by hooks regardless of how a path is spelled, so
/// nothing here relaxes a guard — it only steers the worker onto the command
/// shape that runs clean.
///
/// The string starts at the `## Always use absolute paths` heading and ends
/// with a trailing newline, so a caller embeds it right after the blank line
/// that precedes it.
pub fn absolute_paths_fragment() -> &'static str {
    "## Always use absolute paths — never `cd`\n\
     \n\
     Your shell already starts in your workspace, so a `cd` buys you nothing\n\
     and costs you the run: the agent harness refuses to auto-approve a\n\
     compound command that pairs a `cd` with a relative file read (and, for\n\
     `grep`/`rg`/`diff`/`git`/`cp`/`mv`, refuses outright), so the session\n\
     stops on a permission dialog with no human watching it.\n\
     \n\
     - Do NOT write `cd <dir> && <command>`, and do not `cd` as a separate\n\
     step either.\n\
     - Address every file and directory by its absolute path:\n\
     `cat /abs/path/to/file.rs`, `rg pattern /abs/path/to/dir`,\n\
     `sed -n '1,40p' /abs/path/to/file.rs`.\n\
     - For a tool that must run from a directory, use its own flag rather\n\
     than a `cd` — e.g. `git -C <abs dir> …`, `make -C <abs dir> …`.\n\
     \n\
     This is about the command shape, not about what you may access: the\n\
     engine's guards resolve every path the same way however it is spelled,\n\
     so writing paths in full is never a way around a boundary.\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths_fragment_forbids_cd_and_ends_with_a_newline() {
        let frag = absolute_paths_fragment();
        assert!(frag.starts_with("## Always use absolute paths"));
        assert!(frag.contains("never `cd`"));
        assert!(frag.contains("absolute path"));
        assert!(frag.ends_with('\n'));
    }

    #[test]
    fn fragment_has_both_headings_and_bossctl_note() {
        let frag = boundaries_and_probes_fragment();
        assert!(frag.starts_with("## Boundaries\n"));
        assert!(frag.contains("## Probes\n"));
        assert!(frag.contains("`bossctl` is not available to workers."));
        assert!(frag.ends_with("short, specific answers.\n"));
    }
}

pub mod audit_text {
    pub const PROCESSED_MARKER: &str = "[postmortem-followups]";

    /// `[engine-reconcile]` audit-line marker, shared with
    /// [`ENGINE_AUDIT_LINE_PREFIXES`].
    pub const ENGINE_RECONCILE_MARKER: &str = "[engine-reconcile]";

    /// `[doc-detector]` audit-line marker, shared with
    /// [`ENGINE_AUDIT_LINE_PREFIXES`].
    pub const DOC_DETECTOR_MARKER: &str = "[doc-detector]";

    /// `[pr-review-skip]` audit-line marker appended by
    /// `completion::finalize_passes`'s pure-rebase skip gate, shared
    /// with [`ENGINE_AUDIT_LINE_PREFIXES`] so that gate's audit line is also
    /// stripped before brief / emptiness checks see it.
    pub const PR_REVIEW_SKIP_MARKER: &str = "[pr-review-skip]";

    /// Prefixes of engine-appended description lines. Shared by
    /// [`append_description_line`] writers and [`strip_engine_audit_lines`] so the
    /// vocabulary of audit lines lives in one place: a line whose trimmed text
    /// starts with one of these is engine-owned, not human-authored brief.
    ///
    /// Every `append_description_line` writer's marker must appear here — this
    /// list is the single source of truth for what counts as an engine audit
    /// line rather than human-authored brief text. See
    /// `engine_audit_line_prefixes_cover_every_known_writer` below, which pins
    /// each writer's marker constant against this list so a new writer that
    /// forgets to register here fails a test instead of silently poisoning
    /// empty-brief detection.
    pub const ENGINE_AUDIT_LINE_PREFIXES: &[&str] = &[
        DOC_DETECTOR_MARKER,
        ENGINE_RECONCILE_MARKER,
        crate::deferred_scope::DEFERRED_SCOPE_MARKER,
        PR_REVIEW_SKIP_MARKER,
        PROCESSED_MARKER,
    ];

    /// True when `line` is an engine-appended audit line rather than
    /// human-authored brief text.
    pub fn is_engine_audit_line(line: &str) -> bool {
        let trimmed = line.trim_start();
        ENGINE_AUDIT_LINE_PREFIXES
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
    }

    /// Strip engine-appended audit lines from a work-item description so
    /// emptiness checks and reviewer-brief rendering see only the
    /// human-authored body.
    ///
    /// `[deferred-scope]` lines are still collected into
    /// `deferred_scope_declarations` from the original description; this helper
    /// only removes them from the brief / revision-ask text.
    pub fn strip_engine_audit_lines(description: &str) -> String {
        description
            .lines()
            .filter(|line| !is_engine_audit_line(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(test)]
    pub(crate) fn append_description_line(db: &crate::work::WorkDb, id: &str, line: &str) -> anyhow::Result<()> {
        db.append_work_item_description_line(id, line)
    }
}

pub mod breakdown {
    /// One discrete unit from a design doc's implementation task breakdown.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BreakdownEntry {
        /// Entry title (`###` heading text, or the first line of a numbered item).
        pub title: String,
        /// Remainder of the entry (Scope, Effort hint, Dependencies, free prose).
        pub body: String,
    }

    /// Headings that open a design-doc implementation breakdown section.
    const BREAKDOWN_HEADINGS: &[&str] = &[
        "proposed implementation task breakdown",
        "follow-up implementation chores",
        "implementation plan",
        "implementation task breakdown",
    ];

    /// Locate the breakdown section body (text after the matching `##` heading
    /// until the next `##`-or-shallower heading, or EOF). Returns `None` when no
    /// recognised breakdown heading is present.
    ///
    /// Uses [`crate::pr_template::parse_all_headings`], which tracks fenced code
    /// blocks, so a `## `-prefixed line inside a fenced example (e.g. a Markdown
    /// snippet in the design doc) is never mistaken for a section boundary.
    pub fn extract_breakdown_section(doc: &str) -> Option<&str> {
        let headings = crate::pr_template::parse_all_headings(doc);
        let heading_idx = headings.iter().position(|h| {
            h.level == 2
                && BREAKDOWN_HEADINGS
                    .iter()
                    .any(|b| h.title.trim().to_ascii_lowercase() == *b)
        })?;
        let heading = &headings[heading_idx];
        let start_byte = doc[heading.start..]
            .find('\n')
            .map(|off| heading.start + off + 1)
            .unwrap_or(doc.len());
        let end_byte = headings[heading_idx + 1..]
            .iter()
            .find(|next| next.level <= heading.level)
            .map(|next| next.start)
            .unwrap_or(doc.len());
        if start_byte > end_byte {
            return None;
        }
        Some(doc.get(start_byte..end_byte).unwrap_or("").trim())
    }

    /// Parse discrete breakdown entries from a design doc.
    ///
    /// Prefers `###` ATX headings inside the breakdown section (the modern
    /// design-directive shape). Falls back to top-level numbered list items
    /// (`1. …`, optionally `**1. …**`) when the section has no `###` entries,
    /// then to `- **Name.** …` bold-bullet entries. Returns an empty vec when no
    /// breakdown section or no entries are found — the planner still receives
    /// the full doc and can fall back to free-form extraction, but that fallback
    /// is a silent degradation from the hard entry boundary this function
    /// exists to provide, so callers that build the authoritative-entries prompt
    /// block log when a recognised section yields zero entries.
    pub fn extract_breakdown_entries(doc: &str) -> Vec<BreakdownEntry> {
        let Some(section) = extract_breakdown_section(doc) else {
            return Vec::new();
        };
        let heading_entries = parse_hash_entries(section);
        if !heading_entries.is_empty() {
            return heading_entries;
        }
        let numbered_entries = parse_numbered_entries(section);
        if !numbered_entries.is_empty() {
            return numbered_entries;
        }
        let bullet_entries = parse_bullet_entries(section);
        if bullet_entries.is_empty() {
            tracing::warn!(
                "planner: design doc has a recognised breakdown section but no entries parsed \
             from it (no ### headings, numbered items, or bold bullets) — falling back to \
             free-form extraction from prose"
            );
        }
        bullet_entries
    }

    /// True when `line`, trimmed, opens or closes a fenced code block (``` or
    /// ~~~, at least 3 chars). Callers toggle an `in_fence` flag on this so a
    /// `### `/numbered-looking line inside a fence is never mistaken for an
    /// entry marker — mirrors the fence handling in
    /// `boss_engine_editorial::split_markdown_segments`.
    fn is_fence_delimiter(line: &str) -> bool {
        let trimmed = line.trim();
        trimmed.starts_with("```") || trimmed.starts_with("~~~")
    }

    /// Split a breakdown section on `###` headings into entries. Skips a leading
    /// `Breakdown size: N entries …` prose line (not an entry), and ignores
    /// anything inside a fenced code block.
    ///
    /// Uses the shared fence-aware ATX tokenizer (`boss_pr_template::parse_all_headings`)
    /// so heading detection cannot drift from the review-brief section locator.
    pub fn parse_hash_entries(section: &str) -> Vec<BreakdownEntry> {
        let headings: Vec<_> = crate::pr_template::parse_all_headings(section)
            .into_iter()
            .filter(|heading| heading.level == 3)
            .collect();
        let mut entries = Vec::with_capacity(headings.len());
        for (i, heading) in headings.iter().enumerate() {
            let end = headings[i + 1..]
                .iter()
                .find(|next| next.level <= heading.level)
                .map(|next| next.start)
                .unwrap_or(section.len());
            let after_heading = section[heading.start..end]
                .find('\n')
                .map(|n| heading.start + n + 1)
                .unwrap_or(end);
            let body = section.get(after_heading..end).unwrap_or("").to_owned();
            entries.push(BreakdownEntry {
                title: heading.title.clone(),
                body,
            });
        }
        entries
    }

    /// Fallback: numbered list items (`1. Title` / `1. Title — scope`, optionally
    /// wrapped in `**…**`/`__…__` emphasis) as entries. An indented line (a
    /// sub-step nested under a real entry) is never promoted to a top-level
    /// entry; a fenced code block's contents are never scanned for markers.
    pub fn parse_numbered_entries(section: &str) -> Vec<BreakdownEntry> {
        let mut entries = Vec::new();
        let mut current_title: Option<String> = None;
        let mut body_lines: Vec<&str> = Vec::new();
        let mut in_fence = false;

        for line in section.lines() {
            if is_fence_delimiter(line) {
                in_fence = !in_fence;
                if current_title.is_some() {
                    body_lines.push(line);
                }
                continue;
            }
            if in_fence {
                if current_title.is_some() {
                    body_lines.push(line);
                }
                continue;
            }
            let is_indented = line.starts_with(' ') || line.starts_with('\t');
            let trimmed = line.trim();
            if !is_indented && let Some(rest) = strip_numbered_item(trimmed) {
                if let Some(title) = current_title.take() {
                    entries.push(BreakdownEntry {
                        title,
                        body: body_lines.join("\n"),
                    });
                    body_lines.clear();
                }
                current_title = Some(rest.to_owned());
                continue;
            }
            if current_title.is_some() {
                body_lines.push(line);
            }
        }
        if let Some(title) = current_title {
            entries.push(BreakdownEntry {
                title,
                body: body_lines.join("\n"),
            });
        }
        entries
    }

    /// Second fallback: `- **Name.** rest of line…` bullet entries, e.g.
    /// `- **6f-4: protocol additions.** protocol additions are described...`.
    /// The bold span is the title; text after the closing emphasis on the same
    /// line, plus any indented continuation lines, becomes the body.
    pub fn parse_bullet_entries(section: &str) -> Vec<BreakdownEntry> {
        let mut entries = Vec::new();
        let mut current_title: Option<String> = None;
        let mut body_lines: Vec<String> = Vec::new();
        let mut in_fence = false;

        for line in section.lines() {
            if is_fence_delimiter(line) {
                in_fence = !in_fence;
                if current_title.is_some() {
                    body_lines.push(line.to_owned());
                }
                continue;
            }
            if in_fence {
                if current_title.is_some() {
                    body_lines.push(line.to_owned());
                }
                continue;
            }
            let is_indented = line.starts_with(' ') || line.starts_with('\t');
            let trimmed = line.trim();
            if !is_indented && let Some((title, rest)) = strip_bullet_bold_item(trimmed) {
                if let Some(prev_title) = current_title.take() {
                    entries.push(BreakdownEntry {
                        title: prev_title,
                        body: body_lines.join("\n"),
                    });
                    body_lines.clear();
                }
                current_title = Some(title.to_owned());
                if !rest.trim().is_empty() {
                    body_lines.push(rest.trim().to_owned());
                }
                continue;
            }
            if current_title.is_some() && !trimmed.is_empty() {
                body_lines.push(trimmed.to_owned());
            }
        }
        if let Some(title) = current_title {
            entries.push(BreakdownEntry {
                title,
                body: body_lines.join("\n"),
            });
        }
        entries
    }

    /// If `line` is a `- **Bold title.** rest…` bullet, return `(title, rest)`.
    pub fn strip_bullet_bold_item(line: &str) -> Option<(&str, &str)> {
        let after_dash = line.strip_prefix("- ")?;
        let after_open = after_dash.strip_prefix("**")?;
        let close_idx = after_open.find("**")?;
        let title = after_open[..close_idx].trim().trim_end_matches('.').trim();
        if title.is_empty() {
            return None;
        }
        let rest = &after_open[close_idx + 2..];
        Some((title, rest))
    }

    /// If `line` is a numbered list item (`1. foo` / `12. foo`), optionally
    /// wrapped in leading `*`/`_` emphasis markers (`**1. foo**`), return the
    /// text after the marker (with a matching trailing emphasis marker
    /// stripped); otherwise `None`. Requires a literal space after the `.` so
    /// prose like `2.5x faster than before` never parses as an entry.
    pub fn strip_numbered_item(line: &str) -> Option<&str> {
        let mut rest_line = line;
        while let Some(stripped) = rest_line.strip_prefix('*').or_else(|| rest_line.strip_prefix('_')) {
            rest_line = stripped;
        }
        let bytes = rest_line.as_bytes();
        if bytes.is_empty() || !bytes[0].is_ascii_digit() {
            return None;
        }
        let mut i = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == 0 || i >= bytes.len() || bytes[i] != b'.' {
            return None;
        }
        if bytes.get(i + 1) != Some(&b' ') {
            return None;
        }
        let rest = rest_line[i + 1..].trim_start();
        if rest.is_empty() {
            return None;
        }
        let rest = rest
            .strip_suffix("**")
            .or_else(|| rest.strip_suffix("__"))
            .unwrap_or(rest);
        let rest = rest.trim_end();
        if rest.is_empty() { None } else { Some(rest) }
    }
}

pub mod execution_bookmark_recovery {
    use boss_protocol::WorkExecution;

    pub fn worker_instructions(execution: &WorkExecution) -> String {
        let bookmark = format!("boss-recovery/{}", execution.id);
        let publication = format!("boss/{}", execution.id);
        let text = format!(
            "## Execution recovery bookmark\n\nThe engine created and owns recovery bookmark `{bookmark}` and publication bookmark `{publication}` in the shared jj store for this execution, including revisions. Advance both existing bookmarks to `@` before editing a new change (`jj bookmark set {bookmark} {publication} -r @`). jj follows rewrites of that change automatically. After every `jj new`, `jj commit`, split, squash, rebase, or checkout, advance it again before editing; checkpoint file edits with `jj status` and keep the bookmark at the latest work before ending a turn or stopping. Never delete or push the recovery bookmark. Its separate namespace retains work even when cube cleans up a merged publication bookmark. A revision still publishes only to its existing PR branch; its execution bookmark is a local recovery pointer.\n\nThe engine has already positioned this lease. Inspect its current changes before any checkout; do not reset it to main or re-run PR positioning from generic setup instructions. If you produce nothing, leave the engine-created bookmark in place so an empty run remains distinguishable from a missing recovery pointer.\n\n"
        );
        text
    }

    /// `reconciliation_note` is the persisted [`Reconciliation::note`] when
    /// dispatch had to re-anchor the preserved pointers before recovering them.
    ///
    /// [`Reconciliation::note`]: boss_engine_recovery::execution_bookmark::Reconciliation::note
    pub fn recovery_instructions(
        recovery: Option<&(String, bool)>,
        report: Option<&boss_engine_recovery::execution_bookmark::RestoreReport>,
        reconciliation_note: Option<&str>,
    ) -> String {
        let note = reconciliation_note.unwrap_or_default();
        if let Some(report) = report {
            return format!("{}{note}", report.instructions());
        }
        let Some((predecessor, has_work)) = recovery else {
            return note.to_owned();
        };
        let state = if *has_work {
            "Unpublished changes and their history were recovered into this lease. Stay at `@`; do not reset or repeat PR checkout instructions."
        } else {
            "The predecessor has no unpublished changes to recover. The engine positioned this lease normally."
        };
        format!(
            "## EXECUTION BOOKMARK RECOVERY\n\nThe engine resolved predecessor `{predecessor}` through its recorded bookmark in the shared jj store. {state} The old workspace was not used. Inspect `jj status`, `jj diff --stat`, and the inherited history. Re-run the required build and tests in your own leased workspace before publishing; earlier validation does not satisfy this run's gate.\n\n{note}"
        )
    }
}
