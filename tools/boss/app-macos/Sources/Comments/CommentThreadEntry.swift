import Foundation

/// Mirrors the engine's `comment_thread_entries.entry_kind` values
/// (`THREAD_ENTRY_KIND_NUDGE` / `_ANSWER` / `_OPERATOR_FOLLOWUP`,
/// `boss-protocol/src/types.rs`). See
/// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
/// § "Reply/link mechanics".
enum ThreadEntryKind: String, Equatable {
    case nudge
    case answer
    case operatorFollowup = "operator_followup"
}

/// Client mirror of the engine's `CommentThreadEntry` (`comment_thread_entries`
/// table) — an engine-authored (or operator-authored follow-up) entry in a
/// comment's conversation thread. Always a child of exactly one `Comment`,
/// rendered inline beneath it in chronological order.
///
/// `id` is the engine's `comment_thread_entries.id` for a persisted entry, or a
/// fresh `UUID` string for an entry synthesised by a local stub.
struct CommentThreadEntry: Identifiable, Equatable {
    let id: String
    let entryKind: ThreadEntryKind
    /// `"engine"` for nudge/answer entries; operator identity for follow-ups.
    let author: String
    let body: String
    /// Set on a `nudge` entry once a `[Revise]` batch actually claims the
    /// comment — may postdate the entry itself (design § "Reply/link
    /// mechanics").
    var reviseTaskId: String? = nil
    let createdAt: Date

    /// Map an engine wire entry into the UI type, keeping the engine's stable id.
    static func from(_ wire: WireCommentThreadEntry) -> CommentThreadEntry {
        CommentThreadEntry(
            id: wire.id,
            entryKind: ThreadEntryKind(rawValue: wire.entryKind) ?? .nudge,
            author: wire.author,
            body: wire.body,
            reviseTaskId: wire.reviseTaskId,
            createdAt: Comment.parseWireTimestamp(wire.createdAt)
        )
    }
}
