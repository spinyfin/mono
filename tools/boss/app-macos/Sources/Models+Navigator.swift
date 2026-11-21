import Foundation

enum WorkViewMode: String, CaseIterable, Identifiable {
    case kanban, navigator
    var id: String { rawValue }
    var title: String { self == .kanban ? "Kanban" : "Navigator" }
}

enum NavigatorSection: String, CaseIterable, Identifiable, Decodable {
    case working, needsAttention = "needs_attention", backlog, landed
    var id: String { rawValue }
    var title: String {
        switch self {
        case .working: "Working"
        case .needsAttention: "Needs attention"
        case .backlog: "Backlog"
        case .landed: "Landed"
        }
    }
}

enum NavigatorDetailSlot: String, CaseIterable, Identifiable {
    case activity, review, attention, history
    var id: String { rawValue }
    var title: String { rawValue.capitalized }
    func title(actionCount: Int) -> String {
        if self == .attention, actionCount > 0 { return "Attention \(actionCount)" }
        return title
    }
}

/// Unknown enum strings remain decodable. Only section/version compatibility
/// gates the navigator; an unfamiliar reason still has an engine-authored label.
struct NavigatorProjection: Decodable, Hashable {
    let version: Int
    let rootTaskId: String
    let root: NavigatorRoot?

    static func parse(_ value: Any?) -> Self? {
        guard let value, JSONSerialization.isValidJSONObject(value),
              let data = try? JSONSerialization.data(withJSONObject: value) else { return nil }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try? decoder.decode(Self.self, from: data)
    }
}

struct NavigatorRoot: Decodable, Hashable {
    let section: String
    let sortKey: String
    let defaultDetail: String
    let reason: NavigatorReason
    let reasonLabel: String
    let revisions: NavigatorRevisions
    var actions: [NavigatorAction] { actionValues ?? [] }
    var activities: [NavigatorActivity] { activityValues ?? [] }
    var commands: [NavigatorCommand] { commandValues ?? [] }
    let mergeEvidence: NavigatorMergeEvidence?
    private let actionValues: [NavigatorAction]?
    private let activityValues: [NavigatorActivity]?
    private let commandValues: [NavigatorCommand]?

    enum CodingKeys: String, CodingKey {
        case section, sortKey, defaultDetail, reason, reasonLabel, revisions, mergeEvidence
        case actionValues = "actions", activityValues = "activities", commandValues = "commands"
    }
    var knownSection: NavigatorSection? { NavigatorSection(rawValue: section) }
    var questionCount: Int { actions.filter { $0.kind == "question" || $0.type == "open_input" }.count }
}

struct NavigatorReason: Decodable, Hashable {
    let type: String
    let activity: String?
    let action: String?
    let wait: NavigatorWait?
    let evidence: NavigatorMergeEvidence?
}

struct NavigatorWait: Decodable, Hashable {
    let kind: String
    let at: String?
    let blocker: String?
    let label: String?
}

struct NavigatorRevisions: Decodable, Hashable {
    let open: Int
    let total: Int
    let currentTaskId: String?
}

struct NavigatorActivity: Decodable, Hashable {
    let executionId: String
    let taskId: String
    let runId: String?
}

struct NavigatorAction: Decodable, Hashable {
    let type: String
    let attentionId: String?
    let kind: String?
    let taskId: String?
    let activity: NavigatorActivity?
}

struct NavigatorCommand: Decodable, Hashable {
    let kind: String
    let taskId: String
}

struct NavigatorMergeEvidence: Decodable, Hashable {
    let prUrl: String
    let observedAt: String
    let mergedAt: String?
}

struct NavigatorRow: Identifiable, Equatable {
    let task: WorkTask
    let projection: NavigatorRoot
    let matchingAskID: String?
    var id: String { task.id }
}

struct NavigatorSectionRows: Identifiable, Equatable {
    let section: NavigatorSection
    let rows: [NavigatorRow]
    let total: Int
    var id: NavigatorSection { section }
}

/// Original brief vs the current revision ask. Identity is the document's
/// task id, never the navigator section or a live execution.
enum NavigatorBriefDocument: String, Hashable {
    case original
    case currentAsk
    case historical
}

/// Per-document dirty text. Background work-tree updates never overwrite it.
struct NavigatorBriefDraft: Equatable {
    var text: String
    var baseline: String
    var isEditing: Bool
}

/// Presentation state is independent of row location and worker execution.
struct WorkNavigatorState {
    var mode: WorkViewMode = .kanban
    var kanbanCardOnExit: String?
    var selectedRootID: String?
    var retainedTask: WorkTask?
    var selectedAtSection: NavigatorSection?
    var tab: NavigatorDetailSlot = .activity
    var tabs: Set<NavigatorDetailSlot> = [.activity, .history]
    var askID: String?
    var historicalDocumentByRoot: [String: String] = [:]
    var collapsedByScope: [String: Set<NavigatorSection>] = [:]
    var scrollByScope: [String: String] = [:]
    var revealGeneration = UUID()
    /// Explicit per-root choice (picker, History, reveal). Absent means the
    /// default derived from the projection; see `navigatorDocument(for:)`.
    var draftDocumentByRoot: [String: String] = [:]
    var automaticDocumentPins: Set<String> = []
    var documentByRoot: [String: NavigatorBriefDocument] = [:]
    /// Keyed by root id; holds the selected activity so a new run cannot replace it.
    var activityByRoot: [String: NavigatorActivity] = [:]
    /// Dirty markdown keyed by the document's task id.
    var briefDrafts: [String: NavigatorBriefDraft] = [:]
}
