import Foundation

/// Display order only. Membership arrives in the engine's live-worker board
/// payload; the app does not infer it from worker kinds or task/run status.
enum DoingSection: String, CaseIterable {
    case coding
    case reviewing
    case queued

    var title: String {
        switch self {
        case .coding: return "Coding"
        case .reviewing: return "Reviewing"
        case .queued: return "Queued"
        }
    }
}

extension ChatViewModel {
    /// Needs Attention (operator decisions, hidden when empty), then the
    /// engine's Coding / Reviewing membership, then Queued. A decision is a
    /// card the engine placed in Queued (no live worker) that is parked on an
    /// operator answer; the dispatcher never takes those, so they do not
    /// belong in the queue's order.
    static func doingSections(
        items: [WorkTask], membership: [String: DoingSection], dispatch: [String: QueuedDispatch] = [:]
    ) -> [WorkBoardSection] {
        // The engine defines absent keys as Queued. This also keeps cards
        // visible while connecting to an older engine or moving optimistically.
        let grouped = Dictionary(grouping: items) { membership[$0.id] ?? .queued }
        let waiting = grouped[.queued] ?? []
        let decisions = waiting.filter(\.isAwaitingOperatorAnswer)
        var sections = [needsAttentionSection(items: decisions)].compactMap { $0 }
        for section in DoingSection.allCases {
            guard let rows = grouped[section], !rows.isEmpty else { continue }
            if section == .queued {
                if let queued = queuedSection(items: rows.filter { !$0.isAwaitingOperatorAnswer }, dispatch: dispatch) {
                    sections.append(queued)
                }
                continue
            }
            sections.append(WorkBoardSection(
                id: "doing-\(section.rawValue)", title: section.title, items: rows,
                isCollapsible: true, defaultExpanded: true
            ))
        }
        return sections
    }
}
