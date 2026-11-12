//! Reads the conflict markers jj writes into a working-copy file back
//! into the three versions of each conflicted region.
//!
//! jj has three marker styles (`ui.conflict-marker-style`) and all three
//! are understood here, because which one a host uses is its own
//! configuration:
//!
//! ```text
//! diff (the default)                    snapshot                         git
//! <<<<<<< Conflict 1 of 1               <<<<<<< Conflict 1 of 1          <<<<<<< Side #1 (Conflict 1 of 1)
//! %%%%%%% Changes from base to side #1  +++++++ Contents of side #1      side 1
//! -base line                            side 1                           ||||||| Base
//! +side 1                               ------- Contents of base         base line
//! +++++++ Contents of side #2           base line                        =======
//! side 2                                +++++++ Contents of side #2      side 2
//! >>>>>>> Conflict 1 of 1 ends          side 2                           >>>>>>> Side #2 (Conflict 1 of 1 ends)
//!                                       >>>>>>> Conflict 1 of 1 ends
//! ```
//!
//! In the diff style jj shows whichever side is closer to the base as the
//! diff, so the diff section can come before or after the snapshot. The
//! sides are numbered by their position, not by which one is the diff.
//!
//! The git style is also what `git merge-file --diff3` and
//! `git merge-tree` write, which is how the offline replay feeds recorded
//! conflicts through the same code.

/// One conflicted region: what the common ancestor had there, and what
/// each side has now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConflictHunk {
    /// 1-based line number of the opening marker, for decline reasons.
    pub(crate) line: usize,
    pub(crate) base: Vec<String>,
    pub(crate) side1: Vec<String>,
    pub(crate) side2: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Lines both sides agree on.
    Text(Vec<String>),
    Conflict(ConflictHunk),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConflictedText {
    pub(crate) segments: Vec<Segment>,
    pub(crate) ends_with_newline: bool,
}

const MIN_MARKER_LEN: usize = 7;

/// Length of the run of `kind` that starts `line`, when that run is a
/// marker: at least seven characters, followed by a space or the end of
/// the line.
fn marker_len(line: &str, kind: char) -> Option<usize> {
    let run = line.chars().take_while(|&c| c == kind).count();
    let rest = &line[run * kind.len_utf8()..];
    (run >= MIN_MARKER_LEN && (rest.is_empty() || rest.starts_with(' '))).then_some(run)
}

fn is_marker(line: &str, kind: char, len: usize) -> bool {
    marker_len(line, kind) == Some(len)
}

/// Splits `content` into agreed text and two-sided conflict hunks.
///
/// Fails, with a reason fit for a decline message, when a conflict has
/// more than two sides, has no base section to compare against, or the
/// markers do not form a complete block.
pub(crate) fn parse(content: &str) -> Result<ConflictedText, String> {
    let lines: Vec<&str> = content.split_terminator('\n').collect();
    let mut segments = Vec::new();
    let mut text: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(len) = marker_len(lines[i], '<') else {
            text.push(lines[i].to_owned());
            i += 1;
            continue;
        };
        let opening = i + 1;
        i += 1;
        let jj_sections = lines
            .get(i)
            .is_some_and(|next| ['%', '+', '-'].iter().any(|&kind| is_marker(next, kind, len)));
        let (hunk, next) = if jj_sections {
            parse_jj_sections(&lines, i, len, opening)?
        } else {
            parse_git_sections(&lines, i, len, opening)?
        };
        if !text.is_empty() {
            segments.push(Segment::Text(std::mem::take(&mut text)));
        }
        segments.push(Segment::Conflict(hunk));
        i = next;
    }
    if !text.is_empty() {
        segments.push(Segment::Text(text));
    }
    Ok(ConflictedText {
        segments,
        ends_with_newline: content.ends_with('\n'),
    })
}

fn parse_jj_sections(
    lines: &[&str],
    mut i: usize,
    len: usize,
    opening: usize,
) -> Result<(ConflictHunk, usize), String> {
    let mut sides: Vec<Vec<String>> = Vec::new();
    let mut bases: Vec<Vec<String>> = Vec::new();
    loop {
        let Some(&header) = lines.get(i) else {
            return Err(format!("conflict at line {opening} has no closing marker"));
        };
        i += 1;
        if is_marker(header, '>', len) {
            break;
        }
        let mut body = Vec::new();
        // Newer jj splits the diff header over two lines; the second one
        // starts with a run of backslashes.
        if is_marker(header, '%', len) && lines.get(i).is_some_and(|line| is_marker(line, '\\', len)) {
            i += 1;
        }
        while let Some(&line) = lines.get(i) {
            if ['%', '+', '-', '>'].iter().any(|&kind| is_marker(line, kind, len)) {
                break;
            }
            body.push(line);
            i += 1;
        }
        if is_marker(header, '%', len) {
            let mut base = Vec::new();
            let mut side = Vec::new();
            for line in body {
                if let Some(added) = line.strip_prefix('+') {
                    side.push(added.to_owned());
                } else if let Some(removed) = line.strip_prefix('-') {
                    base.push(removed.to_owned());
                } else if let Some(kept) = line.strip_prefix(' ') {
                    base.push(kept.to_owned());
                    side.push(kept.to_owned());
                } else if line.is_empty() {
                    // An unchanged empty line whose single leading space
                    // was trimmed away.
                    base.push(String::new());
                    side.push(String::new());
                } else {
                    return Err(format!(
                        "conflict at line {opening} has a diff line that starts with neither `+`, `-` nor a space"
                    ));
                }
            }
            bases.push(base);
            sides.push(side);
        } else if is_marker(header, '+', len) {
            sides.push(body.into_iter().map(str::to_owned).collect());
        } else if is_marker(header, '-', len) {
            bases.push(body.into_iter().map(str::to_owned).collect());
        } else {
            return Err(format!("conflict at line {opening} has an unrecognised section header"));
        }
    }
    if sides.len() != 2 || bases.len() != 1 {
        return Err(format!(
            "conflict at line {opening} has {} sides; only two-sided conflicts are handled",
            sides.len()
        ));
    }
    let side2 = sides.pop().unwrap_or_default();
    let side1 = sides.pop().unwrap_or_default();
    let hunk = ConflictHunk {
        line: opening,
        base: bases.pop().unwrap_or_default(),
        side1,
        side2,
    };
    Ok((hunk, i))
}

fn parse_git_sections(
    lines: &[&str],
    mut i: usize,
    len: usize,
    opening: usize,
) -> Result<(ConflictHunk, usize), String> {
    let mut sections: [Vec<String>; 3] = Default::default();
    // side 1, then base after `|||||||`, then side 2 after `=======`.
    let mut current = 0;
    loop {
        let Some(&line) = lines.get(i) else {
            return Err(format!("conflict at line {opening} has no closing marker"));
        };
        i += 1;
        if current == 0 && is_marker(line, '|', len) {
            current = 1;
        } else if current == 0 && is_marker(line, '=', len) {
            return Err(format!(
                "conflict at line {opening} has no base section, so an insertion cannot be told from an edit"
            ));
        } else if current == 1 && is_marker(line, '=', len) {
            current = 2;
        } else if current == 2 && is_marker(line, '>', len) {
            break;
        } else if is_marker(line, '<', len) {
            return Err(format!(
                "conflict at line {opening} is not closed before the next one opens"
            ));
        } else {
            sections[current].push(line.to_owned());
        }
    }
    let [side1, base, side2] = sections;
    Ok((
        ConflictHunk {
            line: opening,
            base,
            side1,
            side2,
        },
        i,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn only_hunk(content: &str) -> ConflictHunk {
        let parsed = parse(content).unwrap();
        let mut hunks = parsed.segments.into_iter().filter_map(|segment| match segment {
            Segment::Conflict(hunk) => Some(hunk),
            Segment::Text(_) => None,
        });
        let hunk = hunks.next().expect("one conflict");
        assert!(hunks.next().is_none(), "expected exactly one conflict");
        hunk
    }

    #[test]
    fn reads_the_diff_style_with_the_diff_section_first() {
        let hunk = only_hunk(
            "mod a;\n<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n+mod c;\n+++++++ Contents of side #2\nmod d;\n>>>>>>> Conflict 1 of 1 ends\nmod z;\n",
        );
        assert_eq!(hunk.line, 2);
        assert!(hunk.base.is_empty());
        assert_eq!(hunk.side1, lines(&["mod c;"]));
        assert_eq!(hunk.side2, lines(&["mod d;"]));
    }

    #[test]
    fn reads_the_diff_style_with_the_snapshot_section_first() {
        // jj shows the side closer to the base as the diff. When side #2's
        // change is the smaller one the snapshot of side #1 comes first.
        let hunk = only_hunk(
            "<<<<<<< Conflict 1 of 1\n+++++++ Contents of side #1\nmod one;\nmod two;\n%%%%%%% Changes from base to side #2\n+mod pr;\n>>>>>>> Conflict 1 of 1 ends\n",
        );
        assert!(hunk.base.is_empty());
        assert_eq!(hunk.side1, lines(&["mod one;", "mod two;"]));
        assert_eq!(hunk.side2, lines(&["mod pr;"]));
    }

    #[test]
    fn rebuilds_the_base_from_a_diff_section() {
        let hunk = only_hunk(
            "<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n     Alpha, Beta,\n-    Delta,\n+    Charlie, Delta,\n\n+++++++ Contents of side #2\n    Alpha, Beta,\n    Delta, Echo,\n\n>>>>>>> Conflict 1 of 1 ends\n",
        );
        assert_eq!(hunk.base, lines(&["    Alpha, Beta,", "    Delta,", ""]));
        assert_eq!(hunk.side1, lines(&["    Alpha, Beta,", "    Charlie, Delta,", ""]));
        assert_eq!(hunk.side2, lines(&["    Alpha, Beta,", "    Delta, Echo,", ""]));
    }

    #[test]
    fn reads_the_snapshot_style() {
        let hunk = only_hunk(
            "<<<<<<< Conflict 1 of 1\n+++++++ Contents of side #1\nmod c;\n------- Contents of base\nmod old;\n+++++++ Contents of side #2\nmod d;\n>>>>>>> Conflict 1 of 1 ends\n",
        );
        assert_eq!(hunk.base, lines(&["mod old;"]));
        assert_eq!(hunk.side1, lines(&["mod c;"]));
        assert_eq!(hunk.side2, lines(&["mod d;"]));
    }

    #[test]
    fn reads_the_git_style_with_any_labels() {
        let hunk = only_hunk("a\n<<<<<<< 1f2e3d\nmod c;\n||||||| 9a8b7c\n=======\nmod d;\n>>>>>>> 4c5b6a\nz\n");
        assert!(hunk.base.is_empty());
        assert_eq!(hunk.side1, lines(&["mod c;"]));
        assert_eq!(hunk.side2, lines(&["mod d;"]));
    }

    #[test]
    fn skips_the_second_line_of_a_two_line_diff_header() {
        let hunk = only_hunk(
            "<<<<<<< conflict 1 of 1\n%%%%%%% diff from: base\n\\\\\\\\\\\\\\        to: side #1\n+mod c;\n+++++++ side #2\nmod d;\n>>>>>>> conflict 1 of 1 ends\n",
        );
        assert_eq!(hunk.side1, lines(&["mod c;"]));
        assert_eq!(hunk.side2, lines(&["mod d;"]));
    }

    #[test]
    fn keeps_surrounding_text_and_the_trailing_newline_flag() {
        let parsed = parse("a\n<<<<<<< x\n1\n||||||| b\n=======\n2\n>>>>>>> y\nz").unwrap();
        assert!(!parsed.ends_with_newline);
        assert_eq!(parsed.segments.len(), 3);
        assert_eq!(parsed.segments[0], Segment::Text(lines(&["a"])));
        assert_eq!(parsed.segments[2], Segment::Text(lines(&["z"])));
    }

    #[test]
    fn longer_markers_are_matched_by_their_own_length() {
        // jj lengthens its markers when the content already has lines that
        // look like seven-character markers.
        let hunk = only_hunk(
            "<<<<<<<<< Conflict 1 of 1\n+++++++++ Contents of side #1\n+++++++ not a marker\n%%%%%%%%% Changes from base to side #2\n+two\n>>>>>>>>> Conflict 1 of 1 ends\n",
        );
        assert_eq!(hunk.side1, lines(&["+++++++ not a marker"]));
        assert_eq!(hunk.side2, lines(&["two"]));
    }

    #[test]
    fn rejects_a_conflict_with_more_than_two_sides() {
        let err = parse(
            "<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n+a\n%%%%%%% Changes from base to side #2\n+b\n+++++++ Contents of side #3\nc\n>>>>>>> Conflict 1 of 1 ends\n",
        )
        .unwrap_err();
        assert!(err.contains("3 sides"), "{err}");
    }

    #[test]
    fn rejects_markers_without_a_base_section() {
        let err = parse("<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n").unwrap_err();
        assert!(err.contains("no base section"), "{err}");
    }

    #[test]
    fn rejects_an_unterminated_conflict() {
        assert!(parse("<<<<<<< Conflict 1 of 1\n+++++++ Contents of side #1\na\n").is_err());
        assert!(parse("<<<<<<< ours\na\n||||||| base\n").is_err());
    }

    #[test]
    fn text_without_markers_parses_as_plain_text() {
        let parsed = parse("mod a;\n=======\nmod b;\n").unwrap();
        assert_eq!(parsed.segments.len(), 1);
    }
}
