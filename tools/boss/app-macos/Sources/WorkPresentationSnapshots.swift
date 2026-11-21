import Foundation

/// Layout equality intentionally excludes row payloads. A renamed card or a
/// worker tick belongs to the row, not the column's membership/layout.
struct WorkBoardSectionLayout: Equatable {
    let section: WorkBoardSection
    let itemIDs: [String]

    init(_ section: WorkBoardSection) {
        self.section = section
        itemIDs = section.items.map(\.id)
    }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.itemIDs == rhs.itemIDs
            && lhs.section.id == rhs.section.id
            && lhs.section.title == rhs.section.title
            && lhs.section.isCollapsible == rhs.section.isCollapsible
            && lhs.section.defaultExpanded == rhs.section.defaultExpanded
            && lhs.section.projectID == rhs.section.projectID
            && lhs.section.groupKey == rhs.section.groupKey
            && lhs.section.queueBannerText == rhs.section.queueBannerText
            && lhs.section.countLabel == rhs.section.countLabel
    }
}

struct WorkBoardLayout: Equatable {
    let columns: [WorkBoardColumnKey: [WorkBoardSectionLayout]]
    let projects: [String: WorkProject]
    let revealTarget: String?
    let revealGeneration: UUID

    @MainActor
    init(model: ChatViewModel) {
        columns = Dictionary(uniqueKeysWithValues: WorkBoardColumnKey.allCases.map {
            ($0, model.workSections(in: $0).map(WorkBoardSectionLayout.init))
        })
        projects = Dictionary(model.projectsForSelectedProduct.map { ($0.id, $0) }, uniquingKeysWith: { _, new in new })
        revealTarget = model.revealScrollTarget
        revealGeneration = model.revealGeneration
    }
}

struct WorkSidebarSnapshot: Equatable {
    let products: [WorkProduct]
    let selectedProduct: WorkProduct?
    let projects: [WorkProject]
    let counts: WorkSidebarCounts
    let selectedProjectIDs: Set<String>
    let archivingProjectIDs: Set<String>
    let attentionItems: [WorkAttentionItem]
    let isConnected: Bool
    let editorialControlsEnabled: Bool
    let choresOnly: Bool
    let hasProjectFilters: Bool
    let includeChores: Bool
    let blockedOnly: Bool
    let showParked: Bool
    let showArchived: Bool

    @MainActor
    init(model: ChatViewModel) {
        products = model.activeProducts
        selectedProduct = model.selectedProduct
        projects = model.projectsForSelectedProduct
        counts = model.sidebarCounts
        selectedProjectIDs = model.selectedProjectFilterIDs
        archivingProjectIDs = model.archivingProjectIDs
        attentionItems = model.selectedProductOpenAttentionItems
        isConnected = model.isConnected
        editorialControlsEnabled = model.isEditorialControlsEnabled
        choresOnly = model.filterToChoresOnly
        hasProjectFilters = model.hasProjectFilters
        includeChores = model.includeChores
        blockedOnly = model.showBlockedOnly
        showParked = model.showParked
        showArchived = model.showArchivedProjects
    }
}
