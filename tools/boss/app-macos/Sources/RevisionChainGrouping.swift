import Foundation

// ===========================================================================
// Shared revision-chain grouping algorithm for [[TranscriptViewerView]] and
// [[AttachmentViewerView]]: both group a flat list of chain-scoped rows
// (executions, attachments) by the task that owns them, labelled "Original"
// / "R<seq>" / "Revision". Extracted so the two viewers over the same chain
// structurally cannot disagree about labelling — see AttachmentViewerView's
// header doc comment.
// ===========================================================================

/// Conformance required to run [[revisionChainGroups(_:rootTaskId:)]] over a
/// flat list of chain-scoped rows.
protocol RevisionChainItem: Identifiable {
    /// The task/chore id this row is filed under — the chain root's id for
    /// rows contributed before any revision, or a revision task's own id.
    var workItemId: String { get }
}

/// One task's worth of chain-scoped rows, labelled for display. Generic
/// replacement for the former `ExecutionGroup` / `AttachmentGroup` pair.
struct RevisionChainGroup<Item: RevisionChainItem>: Identifiable {
    let taskId: String
    /// Human-readable label: "Original" for the chain root, "R1", "R2", …
    /// for revision tasks in sequence order, or "" when the whole list is
    /// from the chain root (the common pre-revision case — callers should
    /// render an unlabelled section in that case).
    let label: String
    let items: [Item]

    var id: String { taskId }
}

/// Groups `items` by `workItemId`, ordered root-first then by revision
/// sequence, and labelled "Original" / "R<seq>" / "Revision". Returns a
/// single unnamed group when every item is from `rootTaskId` itself.
func revisionChainGroups<Item: RevisionChainItem>(
    _ items: [Item],
    rootTaskId: String,
    revisions: [WorkTask]
) -> [RevisionChainGroup<Item>] {
    guard !items.isEmpty else { return [] }

    // Partition by workItemId while preserving the chronological order
    // within each bucket. The chain root's rows always go first; revision
    // buckets follow in revisionSeq order.
    var byTask: [String: [Item]] = [:]
    for item in items {
        byTask[item.workItemId, default: []].append(item)
    }

    // Build the ordered task list: root first, then revisions by seq.
    var orderedTaskIds: [String] = [rootTaskId]
    orderedTaskIds.append(contentsOf: revisions.map { $0.id })
    // Append any task ids not covered above (defensive, shouldn't happen).
    for taskId in byTask.keys where !orderedTaskIds.contains(taskId) {
        orderedTaskIds.append(taskId)
    }

    let hasRevisionItems = orderedTaskIds.dropFirst().contains {
        byTask[$0] != nil
    }

    return orderedTaskIds.compactMap { taskId -> RevisionChainGroup<Item>? in
        guard let rows = byTask[taskId], !rows.isEmpty else { return nil }
        let label: String
        if taskId == rootTaskId {
            label = hasRevisionItems ? "Original" : ""
        } else {
            let seq = revisions.first(where: { $0.id == taskId })?.revisionSeq
            label = seq.map { "R\($0)" } ?? "Revision"
        }
        return RevisionChainGroup(taskId: taskId, label: label, items: rows)
    }
}
