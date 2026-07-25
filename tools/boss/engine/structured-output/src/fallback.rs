//! Fallback-channel vocabulary: recovering a structured payload from free
//! text when the [file contract](crate) produced nothing.
//!
//! The file channel is primary and driver-agnostic. When it is empty — a
//! remote worker whose artifact was written on the far host, an older
//! in-flight run, a worker that simply ignored the instruction — the engine
//! asks the *driver* to recover the payload from the worker's final assistant
//! message. That producer is driver-specific (it knows the CLI's transcript
//! and final-message conventions), but the shapes it deals in are not, and
//! live here: a [`FallbackCandidate`] carries a payload in exactly the wire
//! form the file contract uses, so one parser serves both channels.

use boss_engine_utils::json_extract::extract_balanced_object;

/// One recovered payload to try, in the wire form of its
/// [`StructuredOutputKind`](crate::StructuredOutputKind).
///
/// A driver returns these most-preferred-first; the caller validates each in
/// order and keeps the first that parses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackCandidate {
    /// The payload text, in the file contract's wire form for its kind.
    pub payload: String,
    /// `true` when the worker clearly *intended* this text as structured
    /// output — a fenced code block, or a sentinel-introduced payload — rather
    /// than it being incidental JSON found in prose. Callers use this to
    /// decide whether a parse failure is worth surfacing back to the worker:
    /// a malformed explicit payload is a real mistake to report, a
    /// non-validating scrap of prose JSON is noise.
    pub explicit: bool,
}

impl FallbackCandidate {
    /// A payload the worker plainly meant as structured output.
    pub fn explicit(payload: impl Into<String>) -> Self {
        Self {
            payload: payload.into(),
            explicit: true,
        }
    }

    /// A payload inferred from prose, where a parse failure may just mean
    /// "this wasn't the payload".
    pub fn inferred(payload: impl Into<String>) -> Self {
        Self {
            payload: payload.into(),
            explicit: false,
        }
    }
}

/// Every JSON object in `text` that could be a structured payload, in the
/// order a caller should try them:
///
/// 1. ` ```json ` fenced blocks, in document order (explicit).
/// 2. Plain ` ``` ` fenced blocks with no language tag, in document order
///    (explicit).
/// 3. Bare/unfenced balanced `{…}` objects, in **reverse** document order
///    (inferred) — the last object in the message is the likeliest payload,
///    since the conventional shape is prose followed by trailing JSON.
///
/// The caller parses each candidate against its schema and keeps the first
/// that validates, which reproduces the original scraper's "first fenced block
/// that parses, else the last bare object that parses" behaviour.
pub fn json_object_candidates(text: &str) -> Vec<FallbackCandidate> {
    let mut candidates = Vec::new();

    // 1: ```json fenced blocks.
    let mut rest = text;
    while let Some(fence_start) = rest.find("```json") {
        let after_fence = &rest[fence_start + 7..];
        let trimmed = after_fence.trim_start_matches('\n');
        if let Some(end) = trimmed.find("```") {
            candidates.push(FallbackCandidate::explicit(trimmed[..end].trim()));
        }
        rest = &rest[fence_start + 7..];
    }

    // 2: plain ``` fenced blocks (no language tag).
    let mut rest = text;
    while let Some(fence_start) = rest.find("```") {
        let after_fence = &rest[fence_start + 3..];
        let trimmed = after_fence.trim_start_matches('\n');
        // Skip ```json / ```jsonc blocks — already collected above.
        if !trimmed.starts_with("json")
            && let Some(end) = trimmed.find("```")
        {
            candidates.push(FallbackCandidate::explicit(trimmed[..end].trim()));
        }
        rest = &rest[fence_start + 3..];
    }

    // 3: bare balanced objects, latest first.
    let mut bare = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(object) = extract_balanced_object(&text[i..])
        {
            bare.push(FallbackCandidate::inferred(object));
            // Advance past this object so nested objects are not offered
            // separately.
            i += object.len();
            continue;
        }
        i += 1;
    }
    bare.reverse();
    candidates.extend(bare);

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_json_blocks_come_first_in_document_order() {
        let text = "prose\n```json\n{\"a\":1}\n```\nmore\n```json\n{\"b\":2}\n```\n";
        let candidates = json_object_candidates(text);
        assert_eq!(candidates[0], FallbackCandidate::explicit("{\"a\":1}"));
        assert_eq!(candidates[1], FallbackCandidate::explicit("{\"b\":2}"));
        assert!(candidates[0].explicit, "a fenced block is an explicit payload");
    }

    #[test]
    fn plain_fenced_block_is_collected_but_json_fenced_is_not_duplicated() {
        let text = "```\n{\"plain\":true}\n```";
        let candidates = json_object_candidates(text);
        assert!(
            candidates.iter().any(|c| c.payload == "{\"plain\":true}" && c.explicit),
            "plain fenced block must be offered: {candidates:?}",
        );

        let json_fenced = "```json\n{\"a\":1}\n```";
        let fenced_count = json_object_candidates(json_fenced)
            .iter()
            .filter(|c| c.payload == "{\"a\":1}" && c.explicit)
            .count();
        assert_eq!(
            fenced_count, 1,
            "a ```json block must not also be collected as a plain block"
        );
    }

    #[test]
    fn bare_objects_are_offered_last_and_latest_first() {
        let text = "first {\"n\":1} then {\"n\":2} done";
        let candidates = json_object_candidates(text);
        let bare: Vec<_> = candidates.iter().filter(|c| !c.explicit).collect();
        assert_eq!(bare.len(), 2, "both bare objects offered: {candidates:?}");
        assert_eq!(bare[0].payload, "{\"n\":2}", "latest bare object is tried first");
        assert_eq!(bare[1].payload, "{\"n\":1}");
    }

    #[test]
    fn nested_objects_are_not_offered_separately() {
        let text = "{\"outer\":{\"inner\":1}}";
        let bare: Vec<_> = json_object_candidates(text)
            .into_iter()
            .filter(|c| !c.explicit)
            .collect();
        assert_eq!(bare.len(), 1, "only the outermost object is a candidate: {bare:?}");
        assert_eq!(bare[0].payload, "{\"outer\":{\"inner\":1}}");
    }

    #[test]
    fn no_json_yields_no_candidates() {
        assert!(json_object_candidates("just prose, nothing structured").is_empty());
    }

    #[test]
    fn unterminated_fence_does_not_panic_and_falls_through_to_bare_scan() {
        let text = "```json\n{\"a\":1}\nno closing fence";
        let candidates = json_object_candidates(text);
        assert!(
            candidates.iter().any(|c| c.payload == "{\"a\":1}"),
            "the object is still recoverable as a bare candidate: {candidates:?}",
        );
    }
}
