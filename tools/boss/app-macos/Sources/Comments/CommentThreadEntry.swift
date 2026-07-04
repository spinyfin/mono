import Foundation

/// Mirrors the engine's `comment_thread_entries.entry_kind` values
/// (`THREAD_ENTRY_KIND_NUDGE` / `_ANSWER` / `_OPERATOR_FOLLOWUP`,
/// `boss-protocol/src/types.rs`). Only `nudge` is produced anywhere today
/// (Phase 2b); `answer`/`operatorFollowup` are later bucket-2 phases, but the
/// type carries all three so the thread view doesn't need to change shape
/// when those land. See
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
struct CommentThreadEntry: Identifiable, Equatable {
    let id: UUID
    let entryKind: ThreadEntryKind
    /// `"engine"` for nudge/answer entries; operator identity for follow-ups.
    let author: String
    let body: String
    /// Set on a `nudge` entry once a `[Revise]` batch actually claims the
    /// comment — may postdate the entry itself (design § "Reply/link
    /// mechanics").
    var reviseTaskId: String? = nil
    let createdAt: Date
}
