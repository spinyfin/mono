import SwiftUI

struct WorkNavigatorView: View, @MainActor Equatable {
    let model: ChatViewModel
    var workspace: WorkersWorkspaceModel? = nil
    @StateObject private var state: WorkPresentationValue<NavigatorChromeValue>
    var registry: NavigatorDetailRegistry? = nil
    @Environment(\.openWindow) private var openWindow
    @FocusState private var focus: Focus?
    private enum Focus: Hashable {
        case row(String), section(NavigatorSection), detail
    }

    init(model: ChatViewModel, workspace: WorkersWorkspaceModel? = nil, registry: NavigatorDetailRegistry? = nil) {
        self.model = model
        self.workspace = workspace
        self.registry = registry
        _state = StateObject(wrappedValue: WorkPresentationValue(updates: model.presentationUpdates) {
            NavigatorChromeValue(model: model)
        })
    }

    static func == (lhs: Self, rhs: Self) -> Bool { lhs.model === rhs.model && lhs.workspace === rhs.workspace }

    var body: some View {
        let value = state.value
        VStack(spacing: 0) {
            if !value.isConnected {
                Text("Disconnected — showing the last snapshot. Changes are unavailable until reconnect.")
                    .font(.callout).foregroundStyle(.secondary).padding(8)
            }
            if let message = value.compatibilityMessage {
                VStack(spacing: 12) {
                    Text(message)
                    Button("Use Kanban") { model.setWorkViewMode(.kanban) }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                HSplitView {
                    navigator(value).frame(minWidth: 260, idealWidth: 360, maxWidth: 520)
                    WorkNavigatorDetail(model: model, registry: registry ?? .navigator(workspace: workspace))
                        .frame(minWidth: 280, maxWidth: .infinity, maxHeight: .infinity)
                        .focusable().focused($focus, equals: .detail)
                        .onKeyPress(.escape) {
                            guard let id = model.navigator.selectedRootID else { return .ignored }
                            if model.navigatorVisibleRows.contains(where: { $0.id == id }) {
                                focus = .row(id)
                            } else if let section = model.navigatorSelectedTask?.navigator?.root?.knownSection {
                                focus = .section(section)
                            }
                            return .handled
                        }
                }
            }
        }
        .background(Color(nsColor: .windowBackgroundColor))
        .environmentObject(model)
    }

    private func navigator(_ value: NavigatorChromeValue) -> some View {
        let scope = value.scopeKey
        let sections = value.sections
        return VStack(spacing: 8) {
            scopePicker(value)
            TextField("Search tasks and revision asks", text: Binding(
                get: { value.searchText }, set: { model.workSearchText = $0 }))
                .textFieldStyle(.roundedBorder).padding(.horizontal, 8)
            GeometryReader { geometry in
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 2) {
                            ForEach(sections) { section in
                                sectionHeader(section, value: value)
                                if !value.collapsed.contains(section.section) {
                                    if section.rows.isEmpty {
                                        Text("No tasks").font(.caption).foregroundStyle(.secondary).padding(8)
                                    }
                                    ForEach(section.rows) { row in
                                        Button {
                                            model.selectWorkCard(row.id)
                                            model.selectNavigatorRoot(row.id, askID: row.matchingAskID)
                                            focus = .row(row.id)
                                        } label: {
                                            WorkNavigatorObservedRow(row: row,
                                                project: value.projectNames[row.id],
                                                width: geometry.size.width,
                                                isSelected: value.selectedRootID == row.id,
                                                model: model)
                                        }
                                        .buttonStyle(.plain)
                                        .focusable()
                                        .focused($focus, equals: .row(row.id))
                                        .onKeyPress(.return) { focus = .detail; return .handled }
                                        .id(row.id)
                                    }
                                }
                            }
                        }
                        .scrollTargetLayout().padding(.horizontal, 4)
                    }
                    .scrollPosition(id: Binding(
                        get: { model.navigator.scrollByScope[scope] },
                        set: { model.navigator.scrollByScope[scope] = $0 }
                    ), anchor: .top)
                    .onAppear {
                        if let id = model.navigatorRevealRootID {
                            proxy.scrollTo(id)
                            model.confirmNavigatorReveal(rootID: id)
                        }
                    }
                    .onChange(of: value.revealGeneration) { _, _ in
                        if let id = model.navigator.selectedRootID {
                            proxy.scrollTo(id)
                            model.confirmNavigatorReveal(rootID: id)
                        }
                    }
                    .onKeyPress(keys: [.upArrow, .downArrow, .home, .end, .leftArrow, .rightArrow, .return]) { press in
                        handleKey(press.key)
                    }
                }
            }
            .id(scope)
        }
        .padding(.vertical, 8)
    }

    private func scopePicker(_ value: NavigatorChromeValue) -> some View {
        HStack {
            Picker("Scope", selection: Binding(
                get: { value.scopeSelection },
                set: { id in
                    guard id != "multiple" else { return }
                    model.changeNavigatorScope {
                        model.selectedProjectFilterIDs = id.isEmpty || id == "unprojected" ? [] : [id]
                        model.filterToChoresOnly = id == "unprojected"
                        model.defaults.set(model.filterToChoresOnly, forKey: model.filterToChoresOnlyDefaultsKey)
                        model.persistProjectFilterIDs()
                    }
                }
            )) {
                Text(value.selectedProductName ?? "All projects").tag("")
                Text("No project").tag("unprojected")
                if value.visibleProjectCount > 1 {
                    Text("\(value.visibleProjectCount) projects").tag("multiple")
                }
                ForEach(value.projects) { project in Text(project.name).tag(project.id) }
            }
            .labelsHidden()
            if let project = value.selectedProject {
                Button { model.pendingWorkEditRequest = WorkEditRequest(item: .project(project)) } label: { Image(systemName: "info.circle") }
                    .help("Project information").disabled(!value.isConnected)
            }
            if value.projectAttentionCount > 0 {
                Button { openWindow(id: "attentions") } label: {
                    Label("\(value.projectAttentionCount)", systemImage: "bell.badge")
                }
                .help("Open project notifications")
                .accessibilityLabel("\(value.projectAttentionCount) project notifications")
            }
        }
        .padding(.horizontal, 8)
    }

    private func sectionHeader(_ section: NavigatorSectionRows, value: NavigatorChromeValue) -> some View {
        let expanded = !value.collapsed.contains(section.section)
        return Button {
            model.setNavigatorSection(section.section, expanded: !expanded)
            focus = .section(section.section)
        } label: {
            HStack {
                Image(systemName: expanded ? "chevron.down" : "chevron.right").frame(width: 12)
                Text(section.section.title).fontWeight(.semibold)
                Spacer()
                Text(value.isSearchActive ? "\(section.rows.count) / \(section.total)" : "\(section.total)")
                    .monospacedDigit().foregroundStyle(.secondary)
            }
            .padding(8).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .focusable()
        .focused($focus, equals: .section(section.section))
        .accessibilityLabel("\(section.section.title), \(section.rows.count) shown of \(section.total), \(expanded ? "expanded" : "collapsed")")
    }

    private func handleKey(_ key: KeyEquivalent) -> KeyPress.Result {
        guard let focus, focus != .detail else { return .ignored }
        if key == .return { self.focus = .detail; return .handled }
        if key == .leftArrow || key == .rightArrow {
            let section: NavigatorSection?
            if case .section(let value) = focus { section = value }
            else { section = model.navigatorSelectedTask?.navigator?.root?.knownSection }
            guard let section else { return .ignored }
            self.focus = .section(section)
            model.setNavigatorSection(section, expanded: key == .rightArrow)
        } else {
            model.moveNavigatorSelection(by: key == .upArrow || key == .home ? -1 : 1,
                                         edge: key == .home || key == .end)
            if let id = model.navigator.selectedRootID { self.focus = .row(id) }
        }
        return .handled
    }
}
