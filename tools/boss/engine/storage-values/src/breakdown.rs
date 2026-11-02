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
/// Uses [`boss_pr_template::parse_all_headings`], which tracks fenced code
/// blocks, so a `## `-prefixed line inside a fenced example (e.g. a Markdown
/// snippet in the design doc) is never mistaken for a section boundary.
pub fn extract_breakdown_section(doc: &str) -> Option<&str> {
    let headings = boss_pr_template::parse_all_headings(doc);
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
    let headings: Vec<_> = boss_pr_template::parse_all_headings(section)
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
