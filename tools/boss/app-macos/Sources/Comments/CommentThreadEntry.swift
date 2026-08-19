import Foundation

/// Mirrors the engine's `comment_thread_entries.entry_kind` values
/// (`THREAD_ENTRY_KIND_ANSWER` / `_OPERATOR_FOLLOWUP`, plus the retired
/// `THREAD_ENTRY_KIND_NUDGE`, `boss-protocol/src/types.rs`). See
/// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
/// § "Reply/link mechanics".
///
/// `nudge` is retired — the engine no longer writes it — but stays a case
/// here so a pre-existing thread's `nudge` row still decodes instead of
/// falling through to the "unrecognized" sentinel; `Comment.from` filters it
/// out before it ever reaches the sidebar.
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
    /// `"engine"` for an engine-authored entry (answer, or the
    /// no-reply-posted apology); operator identity for follow-ups.
    let author: String
    let body: String
    /// Unused by any entry kind currently written (`answer` /
    /// `operator_followup`); a pre-existing `nudge` row may carry a value
    /// here, but that row is filtered out before it reaches this type in
    /// practice. See `Comment.reopenedAt` for the signal that actually
    /// drives the sidebar's `Reopened` chip today.
    var reviseTaskId: String? = nil
    let createdAt: Date

    /// Map an engine wire entry into the UI type, keeping the engine's stable id.
    /// An entry_kind the client doesn't recognize (a value from a newer
    /// engine, or corrupt data) also falls back to `.nudge` — inert, since
    /// `Comment.from` filters that kind out of what's actually rendered.
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
