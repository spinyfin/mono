import Foundation

/// Window-open payload for the `"transcript-viewer"` scene. Keyed by
/// `taskId` only (custom `Hashable`/`Equatable`) so re-invoking "View
/// transcripts" for the same task focuses the existing window rather
/// than spawning a second one with a different preselection.
struct TranscriptViewerRef: Codable, Hashable {
    var taskId: String
    var preselectExecutionId: String?

    func hash(into hasher: inout Hasher) { hasher.combine(taskId) }
    static func == (lhs: Self, rhs: Self) -> Bool { lhs.taskId == rhs.taskId }
}

// MARK: - Transcript rendering (transcript-viewer.md impl task 4)

/// Role/origin of a rendered transcript segment. Mirrors
/// `boss_protocol::SegmentRole`; the wire form is snake_case (`user`,
/// `assistant`, `thinking`, `tool`, `system`). The app renders one
/// segment per `List` row, so the role drives only header colour/labels.
enum SegmentRoleVM: String, Codable, Hashable {
    case user
    case assistant
    case thinking
    case tool
    case system
}

/// Truncation metadata for an over-long `tool_result`. Mirrors
/// `boss_protocol::TruncationInfo`. The engine truncates the body and
/// hands the renderer the byte counts so the row can show a
/// "showing N of M" affordance — the markdown body itself does not carry
/// the note (that is the flat-document CLI path).
struct TruncationInfoVM: Codable, Hashable {
    let shownBytes: Int
    let totalBytes: Int

    enum CodingKeys: String, CodingKey {
        case shownBytes = "shown_bytes"
        case totalBytes = "total_bytes"
    }
}

/// One rendered transcript segment from the `execution_transcript` RPC,
/// mirroring `boss_protocol::TranscriptSegment`. Each segment maps to one
/// JSONL event (user turn, assistant turn, tool call, tool result, …) and
/// carries its own pre-rendered markdown body, so the renderer builds
/// `StructuredText` ASTs one visible row at a time (the laziness goal).
struct TranscriptSegmentVM: Identifiable, Codable, Hashable {
    let seq: Int
    let role: SegmentRoleVM
    /// Short human-readable label, e.g. `"User"`, `"⚙ Bash"`, `"↳ result"`,
    /// `"💭 Thinking"`, `"🔗 PR"`. Engine-supplied; the app shows it verbatim.
    let label: String
    let timestamp: String?
    let model: String?
    /// Pre-rendered markdown body for this one event.
    let markdown: String
    let collapsible: Bool
    let defaultCollapsed: Bool
    let truncated: TruncationInfoVM?

    /// `seq` is unique within a transcript and orders the conversation, so
    /// it doubles as the stable `List`/`ScrollViewReader` identity.
    var id: Int { seq }

    enum CodingKeys: String, CodingKey {
        case seq
        case role
        case label
        case timestamp
        case model
        case markdown
        case collapsible
        case defaultCollapsed = "default_collapsed"
        case truncated
    }
}

/// A fully-loaded transcript for one execution: the ordered segments plus
/// the engine's live/complete flags.
struct TranscriptDoc: Hashable {
    let executionId: String
    let segments: [TranscriptSegmentVM]
    /// True while the execution is still running — the transcript is a
    /// partial snapshot and the viewer offers a Refresh.
    let isLive: Bool
    /// True once the execution reached a terminal status (complement of
    /// `isLive`).
    let complete: Bool
}

/// Load state for one execution's transcript, keyed by execution id in
/// [[ChatViewModel.transcriptsByExecutionID]]. `nil` (absent) means the
/// transcript has not been requested yet.
enum TranscriptLoadState: Hashable {
    case loading
    case loaded(TranscriptDoc)
    /// The transcript file is gone (rotated/GC'd), never recorded, or the
    /// worker never started a Claude session. `reason` is engine-supplied.
    case unavailable(reason: String)
}
