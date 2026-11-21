import SwiftUI

struct WorkSidebarView: View, @MainActor Equatable {
    let model: ChatViewModel
    @StateObject private var state: WorkPresentationValue<WorkSidebarSnapshot>

    init(model: ChatViewModel) {
        self.model = model
        _state = StateObject(wrappedValue: WorkPresentationValue(updates: model.presentationUpdates) {
            WorkSidebarSnapshot(model: model)
        })
    }

    static func == (lhs: Self, rhs: Self) -> Bool { lhs.model === rhs.model }

    var body: some View {
        let value = state.value
        List {
            if !value.products.isEmpty {
                Section {
                    ZStack(alignment: .trailing) {
                        SidebarProductPicker(
                            selection: workProductSelection,
                            products: value.products
                        )
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.trailing, 56)

                        HStack(spacing: 4) {
                            if value.editorialControlsEnabled {
                                Button {
                                    if let productID = model.selectedProduct?.id {
                                        model.openEditorialControls(productID: productID)
                                    }
                                } label: {
                                    Image(systemName: "doc.text.magnifyingglass")
                                        .frame(width: 16, height: 16)
                                }
                                .buttonStyle(.borderless)
                                .help("Editorial Rules")
                                .disabled(value.selectedProduct == nil || !value.isConnected)
                            }

                            Button {
                                model.presentEditSelectedProduct()
                            } label: {
                                Image(systemName: "square.and.pencil")
                                    .frame(width: 16, height: 16)
                            }
                            .buttonStyle(.borderless)
                            .padding(.trailing, -2)
                            .help("Edit Product")
                            .disabled(value.selectedProduct == nil || !value.isConnected)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .listRowInsets(EdgeInsets(top: 3, leading: -8, bottom: 3, trailing: 0))

                    let attentionItems = value.attentionItems
                    if !attentionItems.isEmpty {
                        ExternalTrackerSyncBanner(items: attentionItems)
                            .listRowInsets(EdgeInsets(top: 0, leading: 0, bottom: 0, trailing: 0))
                            .listRowBackground(Color.clear)
                    }
                } header: {
                    workSidebarSectionTitle("Product")
                }
            }

            if value.selectedProduct != nil {
                Section {
                    WorkSidebarFilterRow(
                        title: "All Projects",
                        subtitle: nil,
                        systemImage: "square.stack.3d.up",
                        isSelected: !value.hasProjectFilters,
                        trailing: nil,
                        showsCheckbox: false
                    )
                    .listRowInsets(EdgeInsets(top: 3, leading: 8, bottom: 3, trailing: 8))
                    .listRowBackground(Color.clear)
                    .contentShape(Rectangle())
                    .onTapGesture {
                        model.clearProjectFilters()
                    }

                    let choresUnblocked = value.counts.products[value.selectedProduct?.id ?? "", default: .init()].unblocked
                    let choresBlocked = value.counts.products[value.selectedProduct?.id ?? "", default: .init()].blocked
                    WorkSidebarFilterRow(
                        title: "No Project",
                        subtitle: nil,
                        systemImage: "tray",
                        isSelected: value.choresOnly,
                        trailing: nil,
                        showsCheckbox: false,
                        unblockedCount: choresUnblocked > 0 ? choresUnblocked : nil,
                        blockedCount: choresBlocked > 0 ? choresBlocked : nil
                    )
                    .listRowInsets(EdgeInsets(top: 3, leading: 8, bottom: 3, trailing: 8))
                    .listRowBackground(Color.clear)
                    .contentShape(Rectangle())
                    .onTapGesture {
                        model.setFilterToChoresOnly(!model.filterToChoresOnly)
                    }

                    ForEach(value.projects) { project in
                        let isOn = value.selectedProjectIDs.contains(project.id)
                        let isArchived = project.status == "archived"
                        let isArchiving = value.archivingProjectIDs.contains(project.id)
                        let isParked = project.isParked
                        let parkedCount = value.counts.projects[project.id, default: .init()].parked
                        let unblocked = value.counts.projects[project.id, default: .init()].unblocked
                        let blocked = value.counts.projects[project.id, default: .init()].blocked
                        let docPresentation = ProjectDesignDocAffordancePresentation.from(
                            project: project
                        )
                        WorkSidebarFilterRow(
                            title: project.name,
                            subtitle: isArchiving ? "Archiving…" : project.sidebarSubtitle(parkedItemCount: parkedCount),
                            systemImage: project.sidebarSystemImage,
                            isSelected: isOn,
                            trailing: nil,
                            showsCheckbox: true,
                            isCheckboxOn: isOn,
                            dimmed: isArchived || isArchiving || isParked,
                            unblockedCount: unblocked > 0 ? unblocked : nil,
                            blockedCount: blocked > 0 ? blocked : nil,
                            designDocPresentation: docPresentation,
                            onOpenDesignDoc: docPresentation != nil ? { model.openProjectDesignDoc(project) } : nil
                        )
                        .listRowInsets(EdgeInsets(top: 3, leading: 8, bottom: 3, trailing: 8))
                        .listRowBackground(Color.clear)
                        .contentShape(Rectangle())
                        .onTapGesture {
                            model.toggleProjectFilter(project.id)
                        }
                        .contextMenu {
                            if isParked {
                                Button("Unpark") {
                                    model.unpark(id: project.id)
                                }
                            } else if !isArchived && project.status != "done" {
                                Button("Park…") {
                                    model.requestPark(project: project)
                                }
                            }
                            if !isArchived {
                                Button("Archive") {
                                    model.archiveProject(id: project.id)
                                }
                                .disabled(isArchiving)
                            }
                        }
                    }
                } header: {
                    workSidebarSectionTitle("Projects")
                }

                Section {
                    Toggle("Include chores", isOn: Binding(
                        get: { model.includeChores },
                        set: { model.setIncludeChores($0) }
                    ))
                    .listRowInsets(EdgeInsets(top: 4, leading: 8, bottom: 4, trailing: 8))
                    .listRowBackground(Color.clear)

                    Toggle("Show blocked only", isOn: Binding(
                        get: { model.showBlockedOnly },
                        set: { model.setShowBlockedOnly($0) }
                    ))
                    .listRowInsets(EdgeInsets(top: 4, leading: 8, bottom: 4, trailing: 8))
                    .listRowBackground(Color.clear)

                    Toggle("Show parked", isOn: Binding(
                        get: { model.showParked },
                        set: { model.setShowParked($0) }
                    ))
                    .help("Add parked tasks and projects' tasks back to the board. Un-parked work is never hidden by this.")
                    .listRowInsets(EdgeInsets(top: 4, leading: 8, bottom: 4, trailing: 8))
                    .listRowBackground(Color.clear)

                    Toggle("Show archived projects", isOn: Binding(
                        get: { model.showArchivedProjects },
                        set: { model.setShowArchivedProjects($0) }
                    ))
                    .listRowInsets(EdgeInsets(top: 4, leading: 8, bottom: 4, trailing: 8))
                    .listRowBackground(Color.clear)
                } header: {
                    workSidebarSectionTitle("Options")
                }
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom) {
            HStack {
                Button {
                    model.refreshWork()
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                Spacer()
                if !value.isConnected {
                    Label("Disconnected", systemImage: "circle.fill")
                        .foregroundStyle(.red)
                        .font(.caption)
                }
            }
            .padding(.horizontal, 12)
            .padding(.top, 8)
        }
    }

    private var workProductSelection: Binding<String?> {
        Binding(
            get: {
                model.selectedProduct?.id ?? model.activeProducts.first?.id
            },
            set: { newValue in
                guard let productID = newValue else { return }
                model.selectWorkProduct(productID)
            }
        )
    }

    @ViewBuilder
    private func workSidebarSectionTitle(_ title: String) -> some View {
        Text(title)
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
            .textCase(.uppercase)
    }
}
