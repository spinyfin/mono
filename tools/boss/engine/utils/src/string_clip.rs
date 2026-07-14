//! Shared UTF-8-safe string clipping.
//!
//! Several engine subsystems bound a string to at most `max` bytes for
//! storage or display — an error snippet in a planner outcome, a transcript
//! line in a live-status prompt, a clipped stderr in a dispatch event, a
//! recorded editorial command. They all want the same core behavior: cut on a
//! UTF-8 char boundary (never mid-codepoint) and mark the cut with a trailing
//! marker. This module owns that logic so callers compose their own pre/post
//! processing around it instead of each re-deriving the boundary walk.

/// Return the largest UTF-8 char-boundary byte index `<= max` in `s`.
///
/// `max` may land inside a multi-byte codepoint; this walks back to the
/// nearest boundary so a subsequent `&s[..idx]` never panics and no partial
/// codepoint leaks through. `max` is clamped to `s.len()` first, so callers
/// may pass a `max` past the end of the string.
pub fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Clip `s` to at most `max` bytes on a UTF-8 char boundary, appending `…`
/// when the string is truncated. A string already within budget is returned
/// unchanged (no ellipsis).
pub fn clip_to_bytes(s: &str, max: usize) -> String {
    clip_to_bytes_with_suffix(s, max, "…")
}

/// Like [`clip_to_bytes`] but appends a caller-supplied `suffix` in place of
/// the default ellipsis when the string is truncated. Used where the cut
/// marker differs (e.g. `"… (clipped)"`).
pub fn clip_to_bytes_with_suffix(s: &str, max: usize, suffix: &str) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    // `max` may land inside a multi-byte codepoint; walk back to the nearest
    // char boundary so the slice below never panics and no partial codepoint
    // leaks through.
    let end = floor_char_boundary(s, max);
    let mut out = s[..end].to_owned();
    out.push_str(suffix);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_string_returned_verbatim_without_ellipsis() {
        assert_eq!(clip_to_bytes("tiny", 100), "tiny");
        // Exactly at the limit is the `<=` boundary case: unchanged, no marker.
        let s = "a".repeat(10);
        let out = clip_to_bytes(&s, 10);
        assert_eq!(out, s);
        assert!(!out.contains('…'));
    }

    #[test]
    fn overlong_ascii_truncated_with_ellipsis() {
        let s = "x".repeat(1000);
        let out = clip_to_bytes(&s, 10);
        assert!(out.ends_with('…'));
        let prefix = out.strip_suffix('…').unwrap();
        assert_eq!(prefix, "x".repeat(10));
        assert!(prefix.len() <= 10);
    }

    #[test]
    fn truncation_walks_back_to_char_boundary() {
        // '世' is 3 bytes; max = 8 lands inside the third char. Must back off
        // to the boundary at 6 and must not panic on the slice.
        let s = "世".repeat(10);
        let out = clip_to_bytes(&s, 8);
        assert!(out.ends_with('…'));
        assert_eq!(out.strip_suffix('…').unwrap(), "世世");

        // 'é' is 2 bytes; an odd max lands mid-codepoint and must also back off.
        let s = "é".repeat(10);
        let out = clip_to_bytes(&s, 5);
        assert!(out.ends_with('…'));
        assert_eq!(out.strip_suffix('…').unwrap(), "éé");
    }

    #[test]
    fn output_byte_length_stays_bounded() {
        let s = "a".repeat(500);
        let out = clip_to_bytes(&s, 100);
        // Retained prefix is <= max; total is at most max + the 3-byte ellipsis.
        assert!(out.len() <= 100 + '…'.len_utf8());
    }

    #[test]
    fn custom_suffix_is_appended_on_truncation_only() {
        let s = "y".repeat(50);
        let out = clip_to_bytes_with_suffix(&s, 10, "… (clipped)");
        assert!(out.ends_with("… (clipped)"));
        assert!(out.starts_with("yyyyyyyyyy"));

        // Within budget: suffix is not appended.
        assert_eq!(clip_to_bytes_with_suffix("short", 100, "… (clipped)"), "short");
    }
}
