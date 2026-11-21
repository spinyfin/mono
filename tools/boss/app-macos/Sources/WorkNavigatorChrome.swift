import Combine
import Foundation

/// Everything the navigator list and its scope picker derive from the model,
/// as one value. `WorkNavigatorView` renders from this behind a
/// `WorkPresentationValue`, so a model publish that leaves it unchanged (a
/// worker live-status tick, an unrelated notice) never re-evaluates the list.
struct NavigatorChromeValue: Equatable {
    let isConnected: Bool
    let compatibilityMessage: String?
    let scopeKey: String
    let sections: [NavigatorSectionRows]
    let collapsed: Set<NavigatorSection>
    let selectedRootID: String?
    let revealGeneration: UUID
    let isSearchActive: Bool
    let searchText: String
    /// Row id → project name; empty when the scope is a single project.
    let projectNames: [String: String]
    let scopeSelection: String
    let selectedProductName: String?
    let visibleProjectCount: Int
    let projects: [WorkProject]
    let selectedProject: WorkProject?
    let projectAttentionCount: Int

    @MainActor
    init(model: ChatViewModel) {
        isConnected = model.isConnected
        compatibilityMessage = model.navigatorCompatibilityMessage
        scopeKey = model.navigatorScopeKey
        sections = model.navigatorSections
        collapsed = model.navigatorCollapsed
        selectedRootID = model.navigator.selectedRootID
        revealGeneration = model.navigator.revealGeneration
        isSearchActive = model.isWorkSearchActive
        searchText = model.workSearchText
        let visible = model.visibleSelectedProjectFilterIDs
        var names: [String: String] = [:]
        if visible.count != 1 {
            for section in sections {
                for row in section.rows {
                    if let name = model.projectName(for: row.task.projectID) { names[row.id] = name }
                }
            }
        }
        projectNames = names
        if model.filterToChoresOnly {
            scopeSelection = "unprojected"
        } else if visible.count > 1 {
            scopeSelection = "multiple"
        } else {
            scopeSelection = visible.first ?? ""
        }
        selectedProductName = model.selectedProduct?.name
        visibleProjectCount = visible.count
        projects = model.projectsForSelectedProduct
        selectedProject = model.selectedProject
        projectAttentionCount = model.navigatorProjectAttentionCount
    }
}

/// One navigator row's live "worker is working" bit. The row observes this
/// instead of the whole `LiveWorkerStateStore`, so a tick from any other
/// worker never invalidates it. Only rows in the Working section track live
/// state, and the value is published only when it flips.
@MainActor
final class NavigatorRowLiveState: ObservableObject {
    @Published private(set) var isWorkerActive = false
    private var row: NavigatorRow
    private weak var model: ChatViewModel?
    private var subscription: AnyCancellable?

    init(model: ChatViewModel, row: NavigatorRow) {
        self.model = model
        self.row = row
        refresh()
        subscription = model.presentationUpdates.cards.sink { [weak self] _ in self?.refresh() }
    }

    func setRow(_ row: NavigatorRow) {
        self.row = row
        refresh()
    }

    private func refresh() {
        guard let model else { return }
        let next = row.projection.knownSection == .working
            && NavigatorRowPresentation.isWorkerActive(row: row, liveStates: model.liveWorkerStates.byRunID)
        if next != isWorkerActive { isWorkerActive = next }
    }
}
