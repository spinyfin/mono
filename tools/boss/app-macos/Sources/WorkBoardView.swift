import SwiftUI

private let workBoardColumnWidth: CGFloat = 280

private let workBoardColumnWidthWide: CGFloat = 340
private let workBoardColumnWidthMax: CGFloat = 420
private let workBoardWideThreshold: CGFloat = 1400
private let workBoardUltraWideThreshold: CGFloat = 1800
private let workBoardColumnSpacing: CGFloat = 12
private let workBoardHorizontalPadding: CGFloat = 20

struct WorkBoardView: View, @MainActor Equatable {
    let model: ChatViewModel
    let kanbanBoardStyle: KanbanBoardStyle
    @StateObject private var layout: WorkPresentationValue<WorkBoardLayout>

    init(model: ChatViewModel, style: KanbanBoardStyle) {
        self.model = model
        kanbanBoardStyle = style
        _layout = StateObject(wrappedValue: WorkPresentationValue(updates: model.presentationUpdates) {
            WorkBoardLayout(model: model)
        })
    }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.model === rhs.model && lhs.kanbanBoardStyle == rhs.kanbanBoardStyle
    }

    var body: some View {
        // Read at the board boundary so a layout/reveal change refreshes the
        // scroll readers as well as their ForEach children and captured proxies.
        let value = layout.value
        GeometryReader { geometry in
            let columnWidth: CGFloat = {
                if geometry.size.width >= workBoardUltraWideThreshold {
                    return workBoardColumnWidthMax
                } else if geometry.size.width >= workBoardWideThreshold {
                    return workBoardColumnWidthWide
                } else {
                    return workBoardColumnWidth
                }
            }()
            let columnSpacing: CGFloat = kanbanBoardStyle == .minimal ? 24 : workBoardColumnSpacing
            ScrollViewReader { boardProxy in
                ScrollView(.horizontal) {
                    HStack(alignment: .top, spacing: columnSpacing) {
                        ForEach(WorkBoardColumnKey.allCases) { column in
                            workColumn(column, layout: value, width: columnWidth, boardProxy: boardProxy)
                                .id(column)
                        }
                    }
                    .padding(.horizontal, workBoardHorizontalPadding)
                    .padding(.top, workBoardHorizontalPadding)
                    .frame(maxHeight: .infinity, alignment: .top)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(RevealBoardViewport(model: model))
            }
        }
        .environment(\.kanbanBoardStyle, kanbanBoardStyle)
    }

    private func workColumn(_ column: WorkBoardColumnKey, layout: WorkBoardLayout, width: CGFloat = workBoardColumnWidth, boardProxy: ScrollViewProxy) -> some View {
        let sections = layout.columns[column]?.map(\.section) ?? []
        let itemCount = sections.reduce(0) { $0 + $1.items.count }

        return VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(column.title)
                    .font(.headline)
                    .contextMenu {
                        Button("Copy Board Diagnostics") { model.copyBoardDiagnostics() }
                    }
                Spacer()
                Text("\(itemCount)")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(Color(nsColor: .quaternaryLabelColor).opacity(0.12))
                    .clipShape(Capsule())
            }

            if kanbanBoardStyle == .classic {
                Divider()
            }

            if itemCount == 0 {
                Text("No items")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 80, alignment: .topLeading)
                Spacer(minLength: 0)
            } else {
                WorkBoardColumnScrollView(
                    column: column,
                    cardIDs: sections.flatMap { $0.items.map(\.id) },
                    revealTarget: layout.revealTarget,
                    revealGeneration: layout.revealGeneration,
                    boardProxy: boardProxy,
                    isRevealStillPending: { [model] target in model.revealScrollTarget == target }
                ) {
                    ForEach(sections) { section in
                        workSectionView(section, column: column, layout: layout)
                    }
                }
            }
        }
        .modifier(BoardDiagnosticColumn(trace: model.boardDiagnostics, column: column.rawValue,
            sections: sections))
        .padding(14)
        .frame(width: width, alignment: .topLeading)
        .frame(maxHeight: .infinity, alignment: .topLeading)
        .background(columnBackground)
        .clipShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .stroke(columnBorderColor, lineWidth: 1)
        )
        // Column-level fallback: a drop that missed every section — the
        // column's padding, or a column with no groups. Reported with no
        // group, which carries strictly less intent than a group-qualified
        // drop; the engine will not read it as a transition for a card
        // already in this column. Sections install their own, narrower drop
        // targets (see `workSectionView`) and SwiftUI prefers the innermost,
        // so a drop that does land on a group is group-qualified.
        .dropDestination(for: String.self) { items, _ in
            guard let taskID = items.first else { return false }
            return model.attemptDrop(taskID, onColumn: column, group: nil)
        }
    }

    private var columnBackground: Color {
        switch kanbanBoardStyle {
        case .classic:
            return Color(nsColor: .controlBackgroundColor)
        case .airy, .elevated:
            return Color(nsColor: .quaternaryLabelColor).opacity(0.06)
        case .minimal:
            return Color(nsColor: .separatorColor).opacity(0.12)
        }
    }

    private var columnBorderColor: Color {
        switch kanbanBoardStyle {
        case .classic:
            return Color(nsColor: .separatorColor)
        case .airy, .elevated, .minimal:
            return .clear
        }
    }

    @ViewBuilder
    private func workSectionView(_ section: WorkBoardSection, column: WorkBoardColumnKey, layout: WorkBoardLayout) -> some View {
        let sectionProject = section.projectID.flatMap { layout.projects[$0] }
        Group {
            if section.isCollapsible {
                CollapsibleWorkBoardSection(
                    sectionID: section.id,
                    title: section.title,
                    count: section.items.count,
                    defaultExpanded: section.defaultExpanded,
                    countLabel: section.countLabel,
                    shortIDLabel: sectionProject?.shortID.map { "P" + String($0) },
                    banner: section.queueBannerText,
                    revealGeneration: section.items.contains { $0.id == layout.revealTarget }
                        ? layout.revealGeneration : nil,
                    onDrop: { items, expanded in
                        guard let taskID = items.first else { return false }
                        return model.attemptDrop(
                            taskID, onColumn: column, group: expanded ? section.groupKey : nil
                        )
                    }
                ) {
                    if let sectionProject {
                        HStack(spacing: 6) {
                            ProjectDesignDocAffordance(model: model, project: sectionProject)
                            PlannerRunAffordance(model: model, project: sectionProject)
                        }
                    }
                } content: {
                    workSectionItems(section.items, column: column)
                }
            } else {
                workSectionItems(section.items, column: column)
            }
        }
        // Group-qualified drop target. `section.dropGroupKey` is non-nil only
        // for sections that are real named groups (Done's "Merging" and its
        // completion buckets) *and* currently expanded; whole-column and
        // project-rollup sections carry no status meaning, and a collapsed
        // section shows no cards to aim between, so both report `nil` and
        // behave exactly like the column-level fallback above. Naming the
        // group is what lets the engine tell a reorder inside Done ▸ Merging
        // (a no-op) from a genuine Merging → completed transition (a
        // completion), without either gesture having to be disabled.
        //
        // Read inside the closure, not captured: the disclosure state can
        // change between laying the section out and the drop landing on it.
        //
        // SwiftUI routes a drop to the innermost matching destination, so this
        // wins over the column's whenever the pointer is actually over a
        // section.
        .dropDestination(for: String.self) { items, _ in
            guard let taskID = items.first else { return false }
            return model.attemptDrop(taskID, onColumn: column, group: section.dropGroupKey)
        }
    }

    @ViewBuilder
    private func workSectionItems(_ items: [WorkTask], column: WorkBoardColumnKey) -> some View {
        // Membership changes reach this container; mounted rows observe their
        // own render values independently of column layout and other rows.
        WorkBoardSectionItemsView(
            items: items,
            column: column,
            boardStyle: kanbanBoardStyle,
            model: model
        )
    }

}
