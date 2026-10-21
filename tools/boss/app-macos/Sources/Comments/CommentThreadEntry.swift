import Foundation

/// The retired `entry_kind` value the engine no longer writes
/// (`THREAD_ENTRY_KIND_NUDGE`, `boss-protocol/src/types.rs`). The engine's
/// `list_comment_thread_entries` already excludes rows carrying it; this
/// constant lets `Comment.from` filter defensively on the wire value before
/// mapping, without giving `ThreadEntryKind` a case for a kind nothing
/// should ever render.
let retiredNudgeEntryKindWireValue = "nudge"

/// Mirrors the engine's `comment_thread_entries.entry_kind` values
/// (`THREAD_ENTRY_KIND_ANSWER` / `_OPERATOR_FOLLOWUP`, `boss-protocol/src/types.rs`).
/// See `tools/boss/docs/designs/comment-triggered-document-revisions.md`
/// § "Reply/link mechanics".
enum ThreadEntryKind: String, Equatable {
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
    /// Unused by any entry kind this type is constructed with — `Comment.from`
    /// filters the retired `nudge` kind out before mapping, so no surviving
    /// entry ever carries a value here. See `Comment.reopenedAt` for the
    /// signal that actually drives the sidebar's `Reopened` chip today.
    var reviseTaskId: String? = nil
    var proposedEdit: AnswerProposedEdit? = nil
    let createdAt: Date

    /// Map an engine wire entry into the UI type, keeping the engine's stable id.
    /// An entry_kind the client doesn't recognize (a value from a newer
    /// engine, or corrupt data) falls back to `.answer` rather than being
    /// dropped, so it still reaches `ThreadEntriesView` instead of vanishing
    /// silently — only the retired `nudge` kind, filtered by `Comment.from`
    /// before this is ever called, is meant to disappear.
    static func from(_ wire: WireCommentThreadEntry) -> CommentThreadEntry {
        CommentThreadEntry(
            id: wire.id,
            entryKind: ThreadEntryKind(rawValue: wire.entryKind) ?? .answer,
            author: wire.author,
            body: wire.body,
            reviseTaskId: wire.reviseTaskId,
            proposedEdit: wire.proposedEdit,
            createdAt: Comment.parseWireTimestamp(wire.createdAt)
        )
    }
}

import SwiftUI

struct ProposedEditInput: Codable, Equatable, Sendable {
    let version: Int
    let title: String
    let instruction: String
    let replacements: [ProposedTextReplacement]
}

struct ProposedTextReplacement: Codable, Equatable, Sendable {
    let path: String
    let oldText: String
    let newText: String

    enum CodingKeys: String, CodingKey {
        case path
        case oldText = "old_text"
        case newText = "new_text"
    }
}

struct AnswerProposedEdit: Codable, Equatable, Sendable {
    let proposal: ProposedEditInput
    let state: String
    let revisionID: String?
    let revisionStatus: String?

    var canAct: Bool { state == "proposed" && proposal.version == 1 }
    var stateLabel: String {
        switch state {
        case "proposed": return proposal.version == 1 ? "Proposed edit" : "Unsupported proposal version"
        case "approved": return "Approved · \(revisionStatus?.replacingOccurrences(of: "_", with: " ").capitalized ?? "Revision created")"
        case "dismissed": return "Dismissed"
        case "superseded": return "Superseded by a later conversation"
        default: return "Unavailable"
        }
    }

    enum CodingKeys: String, CodingKey {
        case proposal, state
        case revisionID = "revision_id"
        case revisionStatus = "revision_status"
    }
}

/// All actions and editing stay in the comment thread; the engine owns the
/// transition and returns the durable state through comment invalidation.
struct ProposedEditCard: View {
    let entryID: String
    let edit: AnswerProposedEdit
    @ObservedObject var layer: CommentLayer
    @State private var editing = false
    @State private var instruction = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(edit.stateLabel).font(.caption).foregroundStyle(.secondary)
            Text(edit.proposal.title).font(.headline).fixedSize(horizontal: false, vertical: true)
            Text(edit.proposal.instruction).fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
            ForEach(Array(edit.proposal.replacements.enumerated()), id: \.offset) { _, replacement in
                VStack(alignment: .leading, spacing: 4) {
                    Text(replacement.path).font(.caption)
                    Text("− \(replacement.oldText)").foregroundStyle(.red)
                    Text("+ \(replacement.newText)").foregroundStyle(.green)
                }
                .font(.system(.caption, design: .monospaced))
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
            }
            if edit.canAct {
                if editing {
                    TextEditor(text: $instruction).frame(minHeight: 80)
                    HStack {
                        Button("Approve edited instruction") {
                            if layer.approveEdit(entryID: entryID, instruction: instruction) { editing = false }
                        }
                        .disabled(instruction.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                        Button("Cancel") { editing = false }
                    }
                } else {
                    VStack(alignment: .leading, spacing: 6) {
                        Button("Approve as revision") { _ = layer.approveEdit(entryID: entryID, instruction: nil) }
                        Button("Edit then approve") {
                            instruction = edit.proposal.instruction
                            editing = true
                        }
                        Button("Dismiss") { layer.dismissEdit(entryID: entryID) }
                    }
                }
            }
            if edit.state == "approved", let id = edit.revisionID {
                Button("Open revision") { layer.revealRevision(id) }.buttonStyle(.link)
            }
        }
        .font(.caption)
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 6).fill(Color.accentColor.opacity(0.08)))
    }
}
