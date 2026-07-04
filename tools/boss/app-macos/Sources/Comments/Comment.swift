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

/// Mirrors the engine's `work_comments.status` values
/// (`boss-protocol/src/types.rs` `COMMENT_STATUS_*`): `active`/`resolved`/
/// `inRevision` drive the bucket-1&3 `[Revise]`-track chip; `answering`/
/// `answered`/`awaitingFollowup` drive the bucket-2 thread's thinking
/// indicator and follow-up composer (design § "Comment/thread state
/// machine"). `orphaned`/`dismissed` aren't rendered at all so they're left
/// out of this thin-client enum; a dismissed comment is already removed from
/// `CommentLayer.comments` entirely.
enum CommentStatus: String, Equatable {
    case active
    case resolved
    case inRevision = "in_revision"
    /// The answer agent is running for this comment (design § "Bucket 2" —
    /// "Thinking indicator").
    case answering
    /// The answer agent posted its reply; awaiting an operator follow-up.
    case answered
    /// An operator follow-up was posted and awaits reclassification —
    /// loops back to `.answering` (another question) or bridges to
    /// `.active` (directive/larger_change).
    case awaitingFollowup = "awaiting_followup"
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

    /// Mirrors `work_comments.status`. Defaults to `active`, same as the
    /// engine's `default_comment_status`.
    var status: CommentStatus = .active
    /// Mirrors `work_comments.revise_task_id`: the revision/chore that this
    /// comment's `[Revise]` batch was dispatched to. `nil` unless `status ==
    /// .inRevision` (or a `.resolved` comment keeping it as provenance).
    var reviseTaskId: String? = nil
    /// Mirrors `work_comments.status_actor`. `"engine"` marks a transition
    /// driven by (stubbed) reconciliation rather than a direct user action —
    /// the signal `revisionChipState` uses to tell a freshly-reopened
    /// comment apart from one that was never addressed.
    var statusActor: String? = nil
    /// Engine-authored nudge/answer/follow-up entries, oldest first. Mirrors
    /// `comment_thread_entries` (design § "Reply/link mechanics").
    var threadEntries: [CommentThreadEntry] = []

    var anchor: CommentAnchor {
        CommentAnchor(quotedText: quotedText, occurrenceIndex: occurrenceIndex)
    }

    /// The `[Revise]`-track chip state, derived from `status` /
    /// `reviseTaskId` / thread history — mirrors the engine's comment state
    /// machine (design § "Comment/thread state machine"). `nil` when the
    /// comment isn't on the directive/larger_change track at all (no nudge
    /// posted yet).
    var revisionChipState: RevisionChipState? {
        switch status {
        case .inRevision:
            guard let taskId = reviseTaskId else { return nil }
            return .inRevision(taskId: taskId)
        case .resolved:
            guard let taskId = reviseTaskId else { return nil }
            return .resolved(taskId: taskId)
        case .active:
            // A comment only passes back through `.active` via engine-driven
            // reconciliation after a nudge entry's `revise_task_id` was
            // filled in by a `[Revise]` batch — that combination is what
            // distinguishes "reopened" from "never addressed."
            let wasInRevision = threadEntries.contains { $0.entryKind == .nudge && $0.reviseTaskId != nil }
            if wasInRevision, statusActor == "engine" { return .reopened }
            let hasNudge = threadEntries.contains { $0.entryKind == .nudge }
            return hasNudge ? .nudged : nil
        case .answering, .answered, .awaitingFollowup:
            // Bucket-2 track states never show the bucket-1&3 chip — they
            // render their own thread-level indicators instead (design §
            // "Comment/thread state machine").
            return nil
        }
    }
}

/// The four `[Revise]`-track chip states 2f renders, per the design's
/// comment/thread state machine.
enum RevisionChipState: Equatable {
    /// Classified `directive`/`larger_change`; nudge posted, `[Revise]` not
    /// yet clicked.
    case nudged
    case inRevision(taskId: String)
    case resolved(taskId: String)
    /// The claiming task was abandoned; the comment is back on the banner.
    case reopened
}

/// Client mirror of the engine's `CommentsBannerState`
/// (`boss-protocol/src/types.rs`) — a read-only summary driving the
/// `[Revise]` banner. `docKind` is omitted: the client only ever renders
/// this banner inside a design/investigation doc viewer, so the engine-side
/// `resolve_doc_owner` scope guard has no thin-client equivalent to mirror.
struct CommentsBannerState: Equatable {
    let revisable: Bool
    let unresolvedCount: Int
    let inRevisionCount: Int
}
