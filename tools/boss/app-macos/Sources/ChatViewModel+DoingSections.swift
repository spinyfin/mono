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
    static func doingSections(
        items: [WorkTask], membership: [String: DoingSection]
    ) -> [WorkBoardSection] {
        // The engine defines absent keys as Queued. This also keeps cards
        // visible while connecting to an older engine or moving optimistically.
        let grouped = Dictionary(grouping: items) { membership[$0.id] ?? .queued }
        return DoingSection.allCases.compactMap { section in
            guard let rows = grouped[section], !rows.isEmpty else { return nil }
            if section == .queued { return queuedSection(items: rows) }
            return WorkBoardSection(
                id: "doing-\(section.rawValue)", title: section.title, items: rows,
                isCollapsible: true, defaultExpanded: true
            )
        }
    }
}
