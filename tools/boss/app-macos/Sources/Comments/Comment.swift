import Foundation

/// Mirrors the engine's `INTENT_DIRECTIVE` / `INTENT_QUESTION` /
/// `INTENT_LARGER_CHANGE` constants (`boss-protocol/src/types.rs`). A
/// `directive`/`largerChange` classification routes to the revision/
/// update-task path; `question` routes to the read-only answer agent.
/// See `tools/boss/docs/designs/comment-triggered-document-revisions.md`.
enum CommentIntent: String, CaseIterable, Equatable {
    case directive
    case question
    case largerChange = "larger_change"

    var displayName: String {
        switch self {
        case .directive: return "Directive"
        case .question: return "Question"
        case .largerChange: return "Larger Change"
        }
    }

    var symbolName: String {
        switch self {
        case .directive: return "arrow.right.circle"
        case .question: return "questionmark.circle"
        case .largerChange: return "shippingbox.circle"
        }
    }
}

/// Identifies the exact occurrence of a quoted-text span a comment is anchored to.
///
/// Two comments on two different occurrences of the same word have the same
/// `quotedText` but different `occurrenceIndex` values, letting the highlight
/// renderer paint each one independently.
struct CommentAnchor: Equatable {
    /// The verbatim selected text.
    let quotedText: String
    /// 0-based index: which occurrence of `quotedText` in the plain-text
    /// projection this anchor targets.
    let occurrenceIndex: Int
}

/// A single in-memory comment attached to a markdown viewer.
///
/// Phase 1 anchoring is naive: the quoted text and its occurrence index are
/// stored verbatim in memory and lost when the viewer window closes.
/// Resilient TextQuoteSelector anchoring lives in Phase 2.
struct Comment: Identifiable, Equatable {
    let id: UUID
    /// The text the user had selected when they created this comment.
    let quotedText: String
    /// Which occurrence of `quotedText` in the rendered plain text this
    /// comment targets (0-based). Captured at selection time.
    let occurrenceIndex: Int
    /// The comment body the user typed.
    let body: String
    let createdAt: Date
    /// `nil` while classification is pending (or, in this Phase 1 thin client,
    /// indefinitely — no engine RPC classifies comments yet; the badge shows
    /// "classifying…" and the operator can set this directly via the override
    /// control). Set by `CommentsClassify` once engine connectivity lands, or
    /// by a manual override (`CommentsSetIntent`) either way.
    var intent: CommentIntent? = nil
    /// `true` once the operator has manually reclassified this comment via the
    /// badge's override control, mirroring the engine's `intent_overridden_by`
    /// audit trail.
    var intentOverriddenByUser: Bool = false

    var anchor: CommentAnchor {
        CommentAnchor(quotedText: quotedText, occurrenceIndex: occurrenceIndex)
    }
}
