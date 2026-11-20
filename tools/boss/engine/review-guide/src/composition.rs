//! Shared composition and textual-source rules for engine and standalone guides.

use boss_pr_review_sources::SourcePacket;

use crate::{GuideContext, render_guide_context, render_source_context};

/// Append the same ordered context blocks to a primary, update, or evaluation template.
/// Callers must obtain task context before composing a prompt.
pub fn compose_prompt(template: String, packet: &SourcePacket, context: &GuideContext, screenshots: &str) -> String {
    format!(
        "{template}\n\n{}\n{}\n### Registered screenshots\n\n{screenshots}",
        render_source_context(packet),
        render_guide_context(context),
    )
}

/// Repository-relative paths cannot escape the pinned source tree.
pub fn valid_linked_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Validate a linked range independently of the object transport.
/// A zero/zero range denotes the whole textual file, including an empty file.
pub fn linked_range_ok(path: &str, start: u32, end: u32, blob: &[u8]) -> bool {
    if !valid_linked_path(path) || blob.len() > 16 * 1024 * 1024 {
        return false;
    }
    let Ok(text) = std::str::from_utf8(blob) else {
        return false;
    };
    !text.contains('\0')
        && ((start == 0 && end == 0) || (start > 0 && end >= start && end as usize <= text.lines().count()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_require_text_valid_paths_and_existing_lines() {
        assert!(linked_range_ok("src/a.rs", 1, 2, b"a\nb\n"));
        assert!(linked_range_ok("empty", 0, 0, b""));
        for (start, end) in [(0, 1), (2, 1), (1, 3)] {
            assert!(!linked_range_ok("a", start, end, b"a\nb\n"));
        }
        for path in ["", "/a", "../a", "a/./b", "a//b", "a\0"] {
            assert!(!linked_range_ok(path, 0, 0, b"text"));
        }
        assert!(!linked_range_ok("a", 0, 0, b"a\0"));
        assert!(!linked_range_ok("a", 0, 0, &[255]));
        assert!(!linked_range_ok("a", 0, 0, &vec![b'a'; 16 * 1024 * 1024 + 1]));
    }
}
