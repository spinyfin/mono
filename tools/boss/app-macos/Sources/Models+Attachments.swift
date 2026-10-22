import Foundation

// ===========================================================================
// Screenshot-evidence viewer. View models for `work_attachments` rows plus
// the window-open payload and on-disk blob resolution the viewer needs.
// Split out of Models.swift to keep that file under the repo's file-size
// check, mirroring Models+Transcript.swift for the near-identical surface.
// ===========================================================================

/// Window-open payload for the `"attachment-viewer"` scene. Keyed by
/// `taskId` only (custom `Hashable`/`Equatable`) so re-invoking "View
/// screenshots" for the same task focuses the existing window rather than
/// spawning a second one.
struct AttachmentViewerRef: Codable, Hashable {
    var taskId: String

    func hash(into hasher: inout Hasher) { hasher.combine(taskId) }
    static func == (lhs: Self, rhs: Self) -> Bool { lhs.taskId == rhs.taskId }
}

/// One `work_attachments` row — a stored screenshot and its metadata.
/// Mirrors `boss_protocol::WorkAttachment` on the wire.
struct AttachmentVM: Identifiable, Hashable, RevisionChainItem {
    let id: String
    /// The execution that submitted it.
    let executionId: String
    /// The row this attachment is filed under. For a revision this is the
    /// revision's own id, never the chain root's — see the corrected doc
    /// comment on `boss_protocol::WorkAttachment::work_item_id`.
    let workItemId: String
    /// Worker-supplied one-line description. Empty when none was given —
    /// the viewer falls back to `sourceName`.
    let caption: String
    /// Lowercase hex sha256 of the stored bytes; also the blob's on-disk
    /// filename stem.
    let contentDigest: String
    /// Epoch seconds, as a string.
    let createdAt: String
    /// `"png"` or `"jpeg"` — the wire's `AttachmentMediaType` discriminant.
    let mediaType: String
    let pixelWidth: Int
    let pixelHeight: Int
    let sizeBytes: Int64
    /// Basename of the path the worker submitted; the caption fallback.
    let sourceName: String
    /// Set when retention reclaimed the bytes — the row survives as a
    /// tombstone so a stale reference can explain itself rather than
    /// rendering a broken image.
    let reclaimedAt: String?

    var isReclaimed: Bool { reclaimedAt != nil }

    /// The extension the content-addressed blob is stored under. Mirrors
    /// `AttachmentMediaType::extension()` — note this differs from the wire
    /// discriminant itself (`"jpeg"` stores as `.jpg`, not `.jpeg`).
    var fileExtension: String {
        mediaType == "jpeg" ? "jpg" : "png"
    }

    /// What to show as the item's title: the caption, falling back to the
    /// source filename, matching the gallery's own fallback rule.
    var displayTitle: String {
        let trimmed = caption.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? sourceName : trimmed
    }
}

/// Keep explicitly captioned comparisons together, before then after. Only
/// matching captions containing one phase word participate; unrelated evidence
/// retains the engine's order. Match across revisions as well as within a run.
enum ReviewGuideAttachmentOrdering {
    private struct Phase {
        let subject: String
        let isAfter: Bool
    }

    private static func phase(_ attachment: AttachmentVM) -> Phase? {
        let words = attachment.caption.lowercased().components(separatedBy: CharacterSet.alphanumerics.inverted)
            .filter { !$0.isEmpty }
        let phases = words.filter { $0 == "before" || $0 == "after" }
        guard phases.count == 1 else { return nil }
        return Phase(subject: words.filter { $0 != "before" && $0 != "after" }.joined(separator: " "),
                     isAfter: phases[0] == "after")
    }

    static func arrange(_ attachments: [AttachmentVM]) -> [AttachmentVM] {
        let phases = attachments.map(phase)
        var groups: [String: [Int]] = [:]
        for (index, phase) in phases.enumerated() {
            if let phase { groups[phase.subject, default: []].append(index) }
        }
        var emitted = Set<String>()
        var result: [AttachmentVM] = []
        for (index, attachment) in attachments.enumerated() {
            guard let phase = phases[index], let group = groups[phase.subject],
                  group.contains(where: { phases[$0]?.isAfter == true }),
                  group.contains(where: { phases[$0]?.isAfter == false }) else {
                result.append(attachment)
                continue
            }
            guard emitted.insert(phase.subject).inserted else { continue }
            let before = group.filter { phases[$0]?.isAfter == false }
            let after = group.filter { phases[$0]?.isAfter == true }
            // Repeated captions still form adjacent pairs, preserving each
            // phase's input order and retaining any unmatched extra images.
            for offset in 0..<max(before.count, after.count) {
                if offset < before.count { result.append(attachments[before[offset]]) }
                if offset < after.count { result.append(attachments[after[offset]]) }
            }
        }
        return result
    }
}

// MARK: - Blob path resolution

/// Resolves an [[AttachmentVM]] to its on-disk blob path under the engine's
/// content-addressed attachment store, without going through the loopback
/// HTTP gallery (which only resolves on the machine that produced it — see
/// `SubmitAttachment`'s worker-prompt guidance against pasting that URL
/// anywhere).
///
/// Mirrors the engine's own `AttachmentStore::blob_path` (Rust):
/// `<state_root>/attachments/<first-2-hex-chars-of-digest>/<digest>.<ext>`.
/// Reuses [[DispatchEventsPaths.stateRoot()]] rather than re-deriving the
/// state root a third time (a second, isolation-unaware copy already exists
/// on `LogViewerPaths` — this one intentionally does not repeat that gap).
enum AttachmentBlobPaths {
    static func blobURL(for attachment: AttachmentVM) -> URL {
        let shard = String(attachment.contentDigest.prefix(2))
        return DispatchEventsPaths.stateRoot()
            .appendingPathComponent("attachments", isDirectory: true)
            .appendingPathComponent(shard, isDirectory: true)
            .appendingPathComponent("\(attachment.contentDigest).\(attachment.fileExtension)")
    }
}
