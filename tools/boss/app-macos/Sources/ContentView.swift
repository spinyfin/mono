import AppKit
import os.log
import SwiftUI
import UpdateCore

private let workBoardColumnWidth: CGFloat = 280

private let workBoardColumnWidthWide: CGFloat = 340
private let workBoardColumnWidthMax: CGFloat = 420
private let workBoardWideThreshold: CGFloat = 1400
private let workBoardUltraWideThreshold: CGFloat = 1800
private let workBoardColumnSpacing: CGFloat = 12
private let workBoardHorizontalPadding: CGFloat = 20
private let workBossPanelDefaultExpandedWidth: CGFloat = 380
private let workBossPanelMinWidth: CGFloat = 280
private let workBossPanelMaxWidth: CGFloat = 600
private let workBossPanelCollapsedWidth: CGFloat = 88
private let workBossPanelDividerHitWidth: CGFloat = 12

struct ContentView: View {
    @EnvironmentObject private var model: ChatViewModel
    @EnvironmentObject private var updateModel: UpdateModel
    #if canImport(GhosttyKit)
    @StateObject private var workersWorkspace = WorkersWorkspaceModel()
    @StateObject private var bossPane = BossPaneModel()
    #endif
    @State private var isSearchExpanded: Bool = false
    @State private var workColumnVisibility: NavigationSplitViewVisibility = .all
    @Environment(\.openWindow) private var openWindow
    @AppStorage("boss.ui.standardSearch") private var useStandardSearch: Bool = false
    @AppStorage("boss.kanban.boardStyle") private var kanbanBoardStyle: KanbanBoardStyle = .classic

    var body: some View {
        // Work and Agents are kept alive via opacity + hit-testing so SwiftUI
        // doesn't tear down the libghostty NSViews on tab switches (teardown
        // would force ghostty_surface_new and restart every claude session).
        // DesignsView is structurally conditional because it contains its own
        // NavigationSplitView: two NSVs mounted concurrently share the same
        // NSWindow toolbar namespace and AppKit deduplicates their toggle
        // items, causing position thrash and a missing Designs sidebar. Only
        // one NSV may live in the tree at a time. Designs remounts cheaply
        // (filesystem reads only) so structural conditional is safe here.
        ZStack {
            // boss.ui.standardSearch OFF (default): custom WorkSearchToolbarItem below.
            // boss.ui.standardSearch ON: SwiftUI .searchable() owns placement/focus/clear.
            // Both branches keep the same opacity/hitTesting treatment so navigation-mode
            // switching never tears down NSViews in agentsView (see comment above).
            if useStandardSearch {
                NavigationSplitView(columnVisibility: $workColumnVisibility) {
                    sidebar
                } detail: {
                    detail
                }
                // Remove the system sidebarToggle only on non-Work tabs. On the Work
                // tab, the system-provided toggle handles both expanded and collapsed
                // states natively, giving exactly one toggle button in either state
                // without a state-conditional custom button (the root cause of the
                // T479/T612 recurrence: suppressing one button in one collapse state
                // always left the other button visible in the opposite state).
                .toolbar(removing: model.navigationMode == .work ? nil : .sidebarToggle)
                .opacity(model.navigationMode == .work ? 1 : 0)
                .allowsHitTesting(model.navigationMode == .work)
                .searchable(
                    text: $model.workSearchText,
                    placement: .toolbar,
                    prompt: "Search tasks…"
                )
            } else {
                NavigationSplitView(columnVisibility: $workColumnVisibility) {
                    sidebar
                } detail: {
                    detail
                }
                .toolbar(removing: model.navigationMode == .work ? nil : .sidebarToggle)
                .opacity(model.navigationMode == .work ? 1 : 0)
                .allowsHitTesting(model.navigationMode == .work)
            }

            agentsView
                .opacity(model.navigationMode == .agents ? 1 : 0)
                .allowsHitTesting(model.navigationMode == .agents)

            if model.navigationMode == .designs {
                DesignsView(chat: model)
            }

            if model.navigationMode == .automations {
                AutomationsView(model: model)
                    .background(Color(nsColor: .windowBackgroundColor).ignoresSafeArea())
            }

        }
        .safeAreaInset(edge: .top, spacing: 0) {
            // Persistent chrome-level signal that the engine socket is
            // down. Only shown after we've connected at least once so
            // the banner doesn't flash on launch during the normal
            // initial-connect window. Replaces the previous behavior
            // where every reconnect attempt re-popped a "Work Error"
            // modal (#698) — transport errors are now routed away from
            // `workErrorMessage` in `ChatViewModel.handle`. Also gated on
            // `showConnectionLostBanner` rather than the raw `isConnected`
            // flag: a drop that self-heals within `connectionLostBannerDelay`
            // (reconnect backoff starts at 0.5s) never surfaces this at
            // all, so a brief blip reads as silent self-healing rather than
            // a user-visible connection error.
            VStack(spacing: 0) {
                if model.showConnectionLostBanner {
                    EngineUnreachableBanner(
                        isRestarting: model.isRestartingEngine,
                        onRestart: { model.restartEngine() }
                    )
                    .transition(.move(edge: .top).combined(with: .opacity))
                }
                // Connection is up but the engine reports a degraded
                // condition (missing ANTHROPIC_API_KEY, dispatch paused,
                // syspolicyd wedged, etc.). Surface as a first-class
                // affordance so operators can't miss it (#699).
                if model.isConnected, !model.engineHealthIssues.isEmpty {
                    EngineHealthBanner(
                        issues: model.engineHealthIssues,
                        onUnpauseDispatch: { model.resumeDispatch() }
                    )
                    .transition(.move(edge: .top).combined(with: .opacity))
                }
            }
        }
        .animation(.easeInOut(duration: 0.15), value: model.showConnectionLostBanner)
        .animation(.easeInOut(duration: 0.15), value: model.engineHealthIssues)
        #if canImport(GhosttyKit)
        .task {
            // Wire the SwiftPM-only pane allocator into ChatViewModel
            // so EngineRequest events from the engine route through to
            // WorkersWorkspaceModel. Bazel builds without GhosttyKit
            // leave the handlers nil; ChatViewModel responds with
            // EngineToAppError::Internal in that path.
            model.paneSpawnHandler = { [workspace = workersWorkspace] request in
                workspace.spawnWorkerPane(request)
            }
            model.paneReleaseHandler = { [workspace = workersWorkspace] slotId, killGrace in
                workspace.releaseWorkerPane(slotId: slotId, killGraceSeconds: killGrace)
            }
            model.paneSendHandler = { [workspace = workersWorkspace] slotId, text in
                workspace.sendToPane(slotId: slotId, text: text)
            }
            model.paneFocusHandler = { [workspace = workersWorkspace] slotId in
                workspace.focusWorkerPane(slotId: slotId)
            }
            model.paneInterruptHandler = { [workspace = workersWorkspace] slotId in
                workspace.interruptWorkerPane(slotId: slotId)
            }
            model.paneListHostedHandler = { [workspace = workersWorkspace] in
                workspace.listHostedPanes()
            }
            // Forward pool-config pushes from the engine so WorkersWorkspaceModel
            // always uses the engine's live pool sizes rather than independently-
            // maintained constants that drift when pool sizes change.
            // Also forward coordinatorModel to BossPaneModel so the Boss pane
            // tracks whatever effort=max resolves to in the engine.
            model.panePoolConfigHandler = { [workspace = workersWorkspace, boss = bossPane] workerSlots, automationSlots, reviewSlots, coordinatorModel in
                workspace.configureSlots(workerCount: workerSlots, automationCount: automationSlots, reviewCount: reviewSlots)
                boss.updateCoordinatorModel(coordinatorModel)
            }
            // Install the Boss-pane shell-pid provider so the engine can
            // authenticate Boss-tier RPCs (e.g. `bossctl agents reap`).
            // The closure is re-evaluated on every call, so it picks up
            // the current surface pid after a Boss-pane restart.
            model.bossPaneShellPidProvider = { [boss = bossPane] in
                boss.session.shellPid
            }
            // Fire whenever the surface is (re-)attached — covers initial
            // creation and restarts after the coordinator session exits.
            bossPane.session.onSurfaceAttached = { [model] in
                model.bossPaneShellPidAvailable()
            }
            // Handle the race where the surface was attached before this
            // task ran (most common at startup).
            if bossPane.session.terminalReady {
                model.bossPaneShellPidAvailable()
            }
            // Forward worker-pane shell pids to the engine once surfaces
            // attach. WorkersWorkspaceModel fires onShellPidAvailable after
            // ghostty_surface_foreground_pid returns a valid pid so the
            // engine can wire process tracking for reviewer and other panes.
            workersWorkspace.onShellPidAvailable = { [model] runId, shellPid in
                model.workerPaneShellPidAvailable(runId: runId, shellPid: shellPid)
            }
            // Forward worker-pane deaths (surface failed to attach, or the
            // shell process exited) to the engine immediately so it can
            // reap the backing execution instead of waiting for the
            // periodic dead-pid sweep.
            workersWorkspace.onPaneDied = { [model] runId in
                model.workerPaneDied(runId: runId)
            }
            // Report sleep/wake recovery to the engine so a worker-pane
            // spawn stranded by the sleep redispatches immediately
            // instead of waiting for the next periodic sweep.
            GhosttyRuntime.shared.onDisplaysDidWake = { [model] in
                model.spawnCapabilityRestored()
            }
            // Forward surface-creation failures (no shell came up — the
            // post-sleep "no active display" condition) so the engine fails
            // the spawn fast instead of waiting out its 60s spawn-ack timeout.
            workersWorkspace.onSpawnFailed = { [model] runId, reason in
                model.workerPaneSpawnFailed(runId: runId, reason: reason)
            }
        }
        #endif
        .frame(minWidth: 860, minHeight: 560)
        .navigationTitle(model.selectedProduct?.name ?? "Boss")
        .task {
            // Hand the SwiftUI `openWindow` action to the view model
            // so its design-doc dispatch can open the in-app renderer
            // window. The view model can't reach `@Environment` from
            // its own scope; injecting via a closure is how all the
            // other view-model boundaries (pane allocator above,
            // urlOpener) cross the same line.
            model.designRendererOpener = { [openWindow] content in
                openWindow(id: "design-renderer", value: content)
            }
            model.markdownViewerOpener = { [openWindow] content in
                openWindow(id: "markdown-viewer", value: content)
            }
            model.asyncMarkdownViewerOpener = { [openWindow] in
                openWindow(id: "async-markdown-viewer")
            }
            model.reviewTerminalOpener = { [openWindow] in
                openWindow(id: "review-terminal")
            }
            // Register capabilities compiled into this build so the engine can
            // detect flag ↔ capability mismatches and surface a warning badge.
            CapabilityRegistry.shared.register("toolbar_search_standard")
            model.startIfNeeded()
        }
        .toolbar {
            ToolbarItem(placement: .navigation) {
                Picker("Mode", selection: Binding(
                    get: { model.navigationMode },
                    set: { model.setNavigationMode($0) }
                )) {
                    ForEach(NavigationMode.allCases) { mode in
                        Text(mode.rawValue).tag(mode)
                    }
                }
                .pickerStyle(.segmented)
                .frame(width: 360)
            }

            ToolbarItem {
                if model.navigationMode == .work {
                    Menu {
                        Button("New Product") {
                            model.presentCreateProduct()
                        }
                        .disabled(!model.isConnected)

                        Button("New Project") {
                            model.presentCreateProject()
                        }
                        .disabled(model.selectedProduct == nil || !model.isConnected)

                        Button("New Task") {
                            model.presentCreateTask()
                        }
                        .disabled(model.selectedProject == nil || !model.isConnected)

                        Button("New Chore") {
                            model.presentCreateChore()
                        }
                        .disabled(model.selectedProduct == nil || !model.isConnected)
                    } label: {
                        Label("New", systemImage: "plus")
                    }
                }
            }

            ToolbarItemGroup(placement: .primaryAction) {
                if model.navigationMode == .work {
                    WorkProjectFilterToolbarButton(model: model)
                    WorkGroupToolbarMenu(model: model)
                    if !useStandardSearch {
                        WorkSearchToolbarItem(
                            model: model,
                            isExpanded: $isSearchExpanded
                        )
                    }
                }
            }

            ToolbarItem(placement: .primaryAction) {
                NotificationsToolbarButton(model: model)
            }

            ToolbarItem(placement: .primaryAction) {
                UpdateBadgeToolbarButton(updateModel: updateModel)
            }
        }
        .onChange(of: model.navigationMode) { _, newMode in
            isSearchExpanded = false
            if useStandardSearch && newMode != .work {
                model.workSearchText = ""
            }
        }
        .alert(
            "Work Error",
            isPresented: Binding(
                get: { model.workErrorMessage != nil },
                set: { newValue in
                    if !newValue {
                        model.workErrorMessage = nil
                    }
                }
            ),
            actions: {
                Button("OK", role: .cancel) {}
            },
            message: {
                Text(model.workErrorMessage ?? "")
            }
        )
        .sheet(item: $model.pendingWorkCreateRequest) { request in
            WorkCreateSheet(
                request: request,
                productDefaultRepoURL: productDefaultRepoURL(for: request),
                knownRepos: knownRepos(for: request),
                onCancel: { model.dismissWorkCreateRequest() },
                onCreate: { name, description, repoRemoteURL, goal, setAsDefault in
                    model.submitWorkCreateRequest(
                        request,
                        name: name,
                        description: description,
                        repoRemoteURL: repoRemoteURL,
                        goal: goal,
                        setAsProductDefault: setAsDefault
                    )
                }
            )
        }
        .sheet(isPresented: Binding(
            get: { model.plannerInspectorProjectID != nil },
            set: { if !$0 { model.closePlannerInspector() } }
        )) {
            if let projectID = model.plannerInspectorProjectID,
               let project = model.project(withID: projectID) {
                PlannerRunInspectorView(model: model, project: project)
            }
        }
        .sheet(isPresented: Binding(
            get: { model.editorialControlsProductID != nil && model.isEditorialControlsEnabled },
            set: { if !$0 { model.editorialControlsProductID = nil } }
        )) {
            if let productID = model.editorialControlsProductID {
                EditorialControlsSheet(
                    model: model,
                    productID: productID,
                    onDismiss: { model.editorialControlsProductID = nil }
                )
            }
        }
        .sheet(item: $model.pendingWorkEditRequest) { request in
            WorkEditSheet(
                request: request,
                onCancel: { model.dismissWorkEditRequest() },
                onSave: { name, description, status, repoRemoteURL, goal, priority, prURL, workerBranchPrefix, docsRepo in
                    model.submitWorkEditRequest(
                        request,
                        name: name,
                        description: description,
                        status: status,
                        repoRemoteURL: repoRemoteURL,
                        goal: goal,
                        priority: priority,
                        prURL: prURL,
                        workerBranchPrefix: workerBranchPrefix,
                        docsRepo: docsRepo
                    )
                },
                onSetTracker: { kind, org, repo, projectNumber, reverseClose in
                    if case .product(let product) = request.item {
                        model.setProductExternalTracker(
                            productId: product.id,
                            kind: kind,
                            org: org,
                            repo: repo,
                            projectNumber: projectNumber,
                            reverseClose: reverseClose
                        )
                    }
                },
                onUnsetTracker: {
                    if case .product(let product) = request.item {
                        model.unsetProductExternalTracker(productId: product.id)
                    }
                },
                onSetMergeMechanism: { mechanism in
                    if case .product(let product) = request.item {
                        model.setProductMergeMechanism(productId: product.id, mechanism: mechanism)
                    }
                }
            )
            // Re-inject the model so the nested GitHubAccountSection (inside
            // ExternalTrackerSection) can read it via @EnvironmentObject;
            // sheet content does not always inherit the presenter's
            // environment objects.
            .environmentObject(model)
        }
        .sheet(isPresented: Binding(
            get: { updateModel.showUpdateSheet },
            set: { updateModel.showUpdateSheet = $0 }
        )) {
            UpdateResultSheet()
                .environmentObject(updateModel)
        }
        .overlay(alignment: .topTrailing) {
            if let feedback = updateModel.manualCheckFeedback {
                UpdateStatusToast(feedback: feedback)
                    .padding(.top, 52)
                    .padding(.trailing, 16)
                    .transition(.opacity.combined(with: .offset(y: -8)))
            }
        }
        .animation(.easeInOut(duration: 0.2), value: updateModel.manualCheckFeedback)
    }

    private var sidebar: some View {
        workSidebar
            .navigationSplitViewColumnWidth(min: 220, ideal: 280, max: 360)
            .overlay(alignment: .trailing) {
                // Cursor feedback only; native NSSplitView splitter handles drag.
                Color.clear
                    .frame(width: 6)
                    .pointerStyle(.frameResize(position: .trailing))
            }
    }

    /// Look up the parent product's default repo URL for a pending
    /// create request, used by `WorkCreateSheet` to pick the repo
    /// field's render mode (design Q10). Product / project requests
    /// have no parent-product-default context that's relevant to the
    /// repo field, so we return `nil` there.
    private func productDefaultRepoURL(for request: WorkCreateRequest) -> String? {
        switch request.kind {
        case .product, .project:
            return nil
        case .task(let productID, _), .chore(let productID):
            return model.productDefaultRepoURL(productID)
        }
    }

    /// Empirical known-repo set for the parent product of a pending
    /// create request. Empty for product / project requests — neither
    /// form surfaces a recent-repos picker.
    private func knownRepos(for request: WorkCreateRequest) -> [String] {
        switch request.kind {
        case .product, .project:
            return []
        case .task(let productID, _), .chore(let productID):
            return model.knownReposForProduct(productID)
        }
    }

    private var detail: some View {
        workDetail
            .background(Color(nsColor: .windowBackgroundColor))
    }

    private var agentsView: some View {
        // Agents is the only top-level mode that isn't a NavigationSplitView,
        // so its content frame stops at the safe-area inset below the title
        // bar. The Work mode's sidebar uses the sidebar material that bleeds
        // up into that title bar region; with `.opacity(0)` the SwiftUI layer
        // is hidden but the title-bar strip directly above the sidebar
        // column is still visible chrome. Painting the agents background
        // through the safe area covers that strip so the Work sidebar's
        // top sliver doesn't show through when Agents is active.
        #if canImport(GhosttyKit)
        WorkersDetailView(
            workspace: workersWorkspace,
            liveStates: model.liveWorkerStates,
            liveStatusModel: model
        )
            .background(Color(nsColor: .windowBackgroundColor).ignoresSafeArea())
        #else
        VStack(alignment: .leading, spacing: 12) {
            Text("Agents mode requires GhosttyKit.")
                .font(.title3.weight(.semibold))
            Text("Run `tools/boss/app-macos/scripts/bootstrap-ghosttykit.sh` and rebuild with SwiftPM.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer()
        }
        .padding(20)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color(nsColor: .windowBackgroundColor).ignoresSafeArea())
        #endif
    }

    private var workSidebar: some View {
        List {
            if !model.activeProducts.isEmpty {
                Section {
                    ZStack(alignment: .trailing) {
                        SidebarProductPicker(
                            selection: workProductSelection,
                            products: model.activeProducts
                        )
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.trailing, 56)

                        HStack(spacing: 4) {
                            if model.isEditorialControlsEnabled {
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
                                .disabled(model.selectedProduct == nil || !model.isConnected)
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
                            .disabled(model.selectedProduct == nil || !model.isConnected)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .listRowInsets(EdgeInsets(top: 3, leading: -8, bottom: 3, trailing: 0))

                    let attentionItems = model.selectedProductOpenAttentionItems
                    if !attentionItems.isEmpty {
                        ExternalTrackerSyncBanner(items: attentionItems)
                            .listRowInsets(EdgeInsets(top: 0, leading: 0, bottom: 0, trailing: 0))
                            .listRowBackground(Color.clear)
                    }
                } header: {
                    workSidebarSectionTitle("Product")
                }
            }

            if model.selectedProduct != nil {
                Section {
                    WorkSidebarFilterRow(
                        title: "All Projects",
                        subtitle: nil,
                        systemImage: "square.stack.3d.up",
                        isSelected: !model.hasProjectFilters,
                        trailing: nil,
                        showsCheckbox: false
                    )
                    .listRowInsets(EdgeInsets(top: 3, leading: 8, bottom: 3, trailing: 8))
                    .listRowBackground(Color.clear)
                    .contentShape(Rectangle())
                    .onTapGesture {
                        model.clearProjectFilters()
                    }

                    let choresUnblocked = model.unblockedChoreCount
                    let choresBlocked = model.blockedChoreCount
                    WorkSidebarFilterRow(
                        title: "No Project",
                        subtitle: nil,
                        systemImage: "tray",
                        isSelected: model.filterToChoresOnly,
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

                    ForEach(model.projectsForSelectedProduct) { project in
                        let isOn = model.selectedProjectFilterIDs.contains(project.id)
                        let isArchived = project.status == "archived"
                        let unblocked = model.unblockedTaskCount(forProjectID: project.id)
                        let blocked = model.blockedTaskCount(forProjectID: project.id)
                        let docPresentation = ProjectDesignDocAffordancePresentation.from(
                            state: model.designDocStateByProjectID[project.id] ?? .notSet
                        )
                        WorkSidebarFilterRow(
                            title: project.name,
                            subtitle: project.shortID.map { "P" + String($0) },
                            systemImage: isArchived ? "archivebox" : "folder",
                            isSelected: isOn,
                            trailing: nil,
                            showsCheckbox: true,
                            isCheckboxOn: isOn,
                            dimmed: isArchived,
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
                            if !isArchived {
                                Button("Archive") {
                                    model.archiveProject(id: project.id)
                                }
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
                if !model.isConnected {
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

    private var workDetail: some View {
        HStack(spacing: 0) {
            workMainContent
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            workBossPanel
        }
    }

    private var workMainContent: some View {
        Group {
            if model.activeProducts.isEmpty {
                VStack(alignment: .leading, spacing: 10) {
                    Text("No work items yet")
                        .font(.title2.weight(.semibold))
                    Text("Create a product to start organizing projects, tasks, and chores.")
                        .foregroundStyle(.secondary)
                    Button("New Product") {
                        model.presentCreateProduct()
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
                .padding(24)
            } else if model.selectedProduct != nil {
                VStack(spacing: 0) {
                    if let query = model.activeWorkSearchQuery {
                        WorkFilterBanner(query: query) {
                            model.workSearchText = ""
                            isSearchExpanded = false
                        }
                    }
                    workBoard()
                }
            } else {
                VStack(alignment: .leading, spacing: 10) {
                    Text("Select a product")
                        .font(.title3.weight(.semibold))
                    Text("Choose a product from the sidebar to open its board.")
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
                .padding(24)
            }
        }
    }

    private var workBossPanel: some View {
        let isCollapsed = model.isBossPanelCollapsed
        let expandedWidth = model.bossPanelWidth

        return VStack(spacing: 0) {
            bossAgentHeader(isCollapsed: isCollapsed)

            ZStack(alignment: .leading) {
                // The boss terminal is always mounted, even while the
                // panel is collapsed. Two things would otherwise reset
                // the boss claude session:
                //
                //   1. A structural `if`/`else` that excludes
                //      BossPaneTerminalView in the collapsed branch
                //      deinits GhosttyTerminalHostView; its deinit
                //      calls ghostty_surface_free, killing the PTY
                //      child and so the boss claude process. Same
                //      failure mode the Agents↔Work toggle avoids in
                //      `body` above.
                //   2. Shrinking the surface to the 88pt collapsed
                //      strip width would SIGWINCH claude to ~10
                //      columns and reflow its TUI; the session
                //      survives but the visible buffer comes back
                //      mangled. Pinning the terminal's frame to the
                //      expanded width and clipping the outer panel
                //      keeps the surface size stable across collapse.
                #if canImport(GhosttyKit)
                BossPaneTerminalView(boss: bossPane)
                    .frame(width: expandedWidth)
                    .frame(maxHeight: .infinity)
                    .opacity(isCollapsed ? 0 : 1)
                    .allowsHitTesting(!isCollapsed)
                #else
                VStack(alignment: .leading, spacing: 8) {
                    Text("Boss pane requires GhosttyKit.")
                        .font(.callout.weight(.medium))
                    Text("Run `tools/boss/app-macos/scripts/bootstrap-ghosttykit.sh` and rebuild with SwiftPM.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer()
                }
                .padding(14)
                .frame(width: expandedWidth)
                .frame(maxHeight: .infinity, alignment: .topLeading)
                .opacity(isCollapsed ? 0 : 1)
                .allowsHitTesting(!isCollapsed)
                #endif

                if isCollapsed {
                    VStack {
                        Spacer(minLength: 0)
                        Text("Picard")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.secondary)
                            .rotationEffect(.degrees(-90))
                        Spacer(minLength: 0)
                    }
                    .frame(width: workBossPanelCollapsedWidth)
                    .frame(maxHeight: .infinity)
                }
            }
            .frame(maxHeight: .infinity)
            .clipped()
        }
        .frame(width: isCollapsed ? workBossPanelCollapsedWidth : expandedWidth)
        .frame(maxHeight: .infinity)
        .background(Color(nsColor: .windowBackgroundColor))
        .overlay(alignment: .leading) {
            if !isCollapsed {
                ResizeDivider(
                    currentWidth: model.bossPanelWidth,
                    minWidth: workBossPanelMinWidth,
                    maxWidth: workBossPanelMaxWidth,
                    onWidthChanged: { newWidth in
                        model.setBossPanelWidth(newWidth)
                    }
                )
                // Constrain the overlay to a narrow grab strip at
                // the leading edge of the Boss pane. Without this,
                // SwiftUI's overlay fills the whole pane and the
                // divider's tracking area covers everything: cursor
                // stays resize-left-right everywhere and clicks
                // intercept the libghostty surface so the Boss pane
                // never gains keyboard focus.
                //
                // The strip can't extend left of the Boss pane's
                // bounds — those clicks would land on the workMain
                // sibling instead of bubbling down to this overlay
                // (NSView hit testing is bounded by parent bounds).
                // 12pt wide on the Boss-pane side gives a much
                // easier-to-grip target than 6pt while still being
                // a small fraction of the panel.
                .frame(width: workBossPanelDividerHitWidth)
            } else {
                Rectangle()
                    .fill(Color(nsColor: .separatorColor))
                    .frame(width: 1)
            }
        }
        .animation(.snappy(duration: 0.18), value: model.isBossPanelCollapsed)
    }

    @ViewBuilder
    private func bossAgentHeader(isCollapsed: Bool) -> some View {
        HStack(alignment: .center, spacing: 10) {
            if let portrait = TrekIconAssets.image(.picard, size: .small) {
                Image(nsImage: portrait)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fit)
                    .frame(width: 22, height: 28)
                    .clipShape(RoundedRectangle(cornerRadius: 3, style: .continuous))
            } else {
                Image(systemName: "person.crop.circle.badge.checkmark")
                    .foregroundStyle(Color.accentColor)
                    .font(.system(size: 13, weight: .semibold))
                    .frame(width: 22, height: 28)
            }

            if !isCollapsed {
                Text("Picard")
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(.primary)
                    .lineLimit(1)

                Spacer(minLength: 8)
            } else {
                Spacer(minLength: 0)
            }

            Button {
                model.toggleBossPanelCollapsed()
            } label: {
                Image(systemName: "sidebar.right")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(.secondary)
                    .frame(width: 22, height: 22)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(isCollapsed ? "Expand Boss panel" : "Collapse Boss panel")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial)
        .overlay(alignment: .bottom) {
            Rectangle()
                .fill(Color(nsColor: .separatorColor).opacity(0.6))
                .frame(height: 0.5)
        }
    }

    private func workBoard() -> some View {
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
            ScrollView(.horizontal) {
                HStack(alignment: .top, spacing: columnSpacing) {
                    ForEach(WorkBoardColumnKey.allCases) { column in
                        workColumn(column, width: columnWidth)
                    }
                }
                .padding(.horizontal, workBoardHorizontalPadding)
                .padding(.top, workBoardHorizontalPadding)
                .frame(maxHeight: .infinity, alignment: .top)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .environment(\.kanbanBoardStyle, kanbanBoardStyle)
    }

    private func workColumn(_ column: WorkBoardColumnKey, width: CGFloat = workBoardColumnWidth) -> some View {
        let sections = model.workSections(in: column)
        let itemCount = sections.reduce(0) { $0 + $1.items.count }

        return VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(column.title)
                    .font(.headline)
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
                ScrollViewReader { proxy in
                    ScrollView(.vertical) {
                        VStack(alignment: .leading, spacing: 12) {
                            ForEach(sections) { section in
                                workSectionView(section, column: column)
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .topLeading)
                    }
                    .frame(maxHeight: .infinity)
                    .onChange(of: model.revealScrollTarget) { _, target in
                        guard let target else { return }
                        let columnIDs = sections.flatMap { $0.items.map(\.id) }
                        guard columnIDs.contains(target) else { return }
                        withAnimation { proxy.scrollTo(target, anchor: .center) }
                    }
                }
            }
        }
        .padding(14)
        .frame(width: width, alignment: .topLeading)
        .frame(maxHeight: .infinity, alignment: .topLeading)
        .background(columnBackground)
        .clipShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .stroke(columnBorderColor, lineWidth: 1)
        )
        .dropDestination(for: String.self) { items, _ in
            guard let taskID = items.first else { return false }
            return model.attemptMoveTask(taskID, to: column)
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
    private func workSectionView(_ section: WorkBoardSection, column: WorkBoardColumnKey) -> some View {
        let sectionProject = section.projectID.flatMap { model.project(withID: $0) }
        if section.isCollapsible {
            CollapsibleWorkBoardSection(
                sectionID: section.id,
                title: section.title,
                count: section.items.count,
                defaultExpanded: section.defaultExpanded,
                shortIDLabel: sectionProject?.shortID.map { "P" + String($0) },
                banner: section.queueBannerText
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

    @ViewBuilder
    private func workSectionItems(_ items: [WorkTask], column: WorkBoardColumnKey) -> some View {
        let selectedID = model.selectedTask?.id
        let highlightID = model.revealHighlightID
        let frontierIDs = model.depFrontierHighlightIDs
        let revisionIDs = model.revisionHighlightIDs
        let selectedRevisionParentID = model.selectedRevisionParentID
        // Lazy so off-screen cards aren't instantiated/hit-tested at all — with
        // the default (ungrouped) board layout each column is a single section,
        // so this was the actual eagerly-built list of every card in the
        // column regardless of scroll position. Combined with the whole-model
        // `@Published` invalidation that hover badges trigger (any card's
        // `onDepBadgeHover`/`onRevisionBadgeHover` re-renders every card in
        // every column), a plain `VStack` here meant hovering one badge while
        // scrolling re-evaluated and re-hit-tested every card on the board,
        // not just the visible ones. `LazyVStack` + `ScrollViewReader` +
        // `.id(task.id)` below is the supported combo for reveal-scroll, so
        // this doesn't change that behavior.
        LazyVStack(alignment: .leading, spacing: 10) {
            ForEach(items) { task in
                let isSelected = selectedID == task.id
                let isRevealed = highlightID == task.id
                let isFrontierHighlighted = frontierIDs.contains(task.id) || revisionIDs.contains(task.id) || selectedRevisionParentID == task.id
                WorkBoardCardItem(
                    task: task,
                    projectName: model.cardProjectBadge(for: task),
                    column: column,
                    runtime: column == .doing ? model.taskRuntime(for: task.id) : nil,
                    isSelected: isSelected,
                    isRevealed: isRevealed,
                    isFrontierHighlighted: isFrontierHighlighted,
                    model: model,
                    liveStates: model.liveWorkerStates
                )
                .id(task.id)
            }
        }
    }

    @ViewBuilder
    private func workSidebarSectionTitle(_ title: String) -> some View {
        Text(title)
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
            .textCase(.uppercase)
    }
}
