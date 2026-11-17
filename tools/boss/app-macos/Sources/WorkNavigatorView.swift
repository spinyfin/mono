import SwiftUI

struct WorkNavigatorView: View {
    @ObservedObject var model: ChatViewModel
    var registry = NavigatorDetailRegistry()
    @Environment(\.openWindow) private var openWindow
    @FocusState private var focus: Focus?
    private enum Focus: Hashable {
        case row(String), section(NavigatorSection), detail
    }

    var body: some View {
        VStack(spacing: 0) {
            if !model.isConnected {
                Text("Disconnected — showing the last snapshot. Changes are unavailable until reconnect.")
                    .font(.callout).foregroundStyle(.secondary).padding(8)
            }
            if let message = model.navigatorCompatibilityMessage {
                VStack(spacing: 12) {
                    Text(message)
                    Button("Use Kanban") { model.setWorkViewMode(.kanban) }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                HSplitView {
                    navigator.frame(minWidth: 260, idealWidth: 360, maxWidth: 520)
                    WorkNavigatorDetail(model: model, registry: registry)
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
    }

    private var navigator: some View {
        let scope = model.navigatorScopeKey
        let sections = model.navigatorSections
        return VStack(spacing: 8) {
            scopePicker
            TextField("Search tasks and revision asks", text: $model.workSearchText)
                .textFieldStyle(.roundedBorder).padding(.horizontal, 8)
            GeometryReader { geometry in
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 2) {
                            ForEach(sections) { section in
                                sectionHeader(section)
                                if !model.navigatorCollapsed.contains(section.section) {
                                    if section.rows.isEmpty {
                                        Text("No tasks").font(.caption).foregroundStyle(.secondary).padding(8)
                                    }
                                    ForEach(section.rows) { row in
                                        Button {
                                            model.selectWorkCard(row.id)
                                            model.selectNavigatorRoot(row.id, askID: row.matchingAskID)
                                            focus = .row(row.id)
                                        } label: {
                                            WorkNavigatorRow(row: row,
                                                project: model.visibleSelectedProjectFilterIDs.count == 1 ? nil : model.projectName(for: row.task.projectID),
                                                width: geometry.size.width,
                                                isSelected: model.navigator.selectedRootID == row.id)
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
                    .onChange(of: model.navigator.revealGeneration) { _, _ in
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

    private var scopePicker: some View {
        HStack {
            Picker("Scope", selection: Binding(
                get: {
                    if model.filterToChoresOnly { return "unprojected" }
                    if model.visibleSelectedProjectFilterIDs.count > 1 { return "multiple" }
                    return model.visibleSelectedProjectFilterIDs.first ?? ""
                },
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
                Text(model.selectedProduct?.name ?? "All projects").tag("")
                Text("No project").tag("unprojected")
                if model.visibleSelectedProjectFilterIDs.count > 1 {
                    Text("\(model.visibleSelectedProjectFilterIDs.count) projects").tag("multiple")
                }
                ForEach(model.projectsForSelectedProduct) { project in Text(project.name).tag(project.id) }
            }
            .labelsHidden()
            if let project = model.selectedProject {
                Button { model.pendingWorkEditRequest = WorkEditRequest(item: .project(project)) } label: { Image(systemName: "info.circle") }
                    .help("Project information").disabled(!model.isConnected)
            }
            if model.navigatorProjectAttentionCount > 0 {
                Button { openWindow(id: "attentions") } label: {
                    Label("\(model.navigatorProjectAttentionCount)", systemImage: "bell.badge")
                }
                .help("Open project notifications")
                .accessibilityLabel("\(model.navigatorProjectAttentionCount) project notifications")
            }
        }
        .padding(.horizontal, 8)
    }

    private func sectionHeader(_ section: NavigatorSectionRows) -> some View {
        let expanded = !model.navigatorCollapsed.contains(section.section)
        return Button {
            model.setNavigatorSection(section.section, expanded: !expanded)
            focus = .section(section.section)
        } label: {
            HStack {
                Image(systemName: expanded ? "chevron.down" : "chevron.right").frame(width: 12)
                Text(section.section.title).fontWeight(.semibold)
                Spacer()
                Text(model.isWorkSearchActive ? "\(section.rows.count) / \(section.total)" : "\(section.total)")
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
