import AppKit
import SwiftUI
import Textual

struct ContentView: View {
    @StateObject private var model = ChatViewModel()

    var body: some View {
        NavigationSplitView {
            sidebar
        } detail: {
            detail
        }
        .frame(minWidth: 860, minHeight: 560)
        .task {
            model.startIfNeeded()
        }
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Picker("Mode", selection: Binding(
                    get: { model.navigationMode },
                    set: { model.setNavigationMode($0) }
                )) {
                    ForEach(NavigationMode.allCases) { mode in
                        Text(mode.rawValue).tag(mode)
                    }
                }
                .pickerStyle(.segmented)
                .frame(width: 170)
            }

            ToolbarItem {
                if model.navigationMode == .agents {
                    Button {
                        model.createAgent()
                    } label: {
                        Label("New Agent", systemImage: "plus")
                    }
                } else {
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
        }
        .alert(item: $model.pendingPermission) { request in
            Alert(
                title: Text("Permission Request"),
                message: Text(request.title),
                primaryButton: .default(Text("Allow")) {
                    model.respondToPendingPermission(granted: true)
                },
                secondaryButton: .destructive(Text("Deny")) {
                    model.respondToPendingPermission(granted: false)
                }
            )
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
                onCancel: { model.dismissWorkCreateRequest() },
                onCreate: { name, description, repoRemoteURL, goal in
                    model.submitWorkCreateRequest(
                        request,
                        name: name,
                        description: description,
                        repoRemoteURL: repoRemoteURL,
                        goal: goal
                    )
                }
            )
        }
        .sheet(item: $model.pendingWorkEditRequest) { request in
            WorkEditSheet(
                request: request,
                onCancel: { model.dismissWorkEditRequest() },
                onSave: { name, description, status, repoRemoteURL, goal, priority, prURL in
                    model.submitWorkEditRequest(
                        request,
                        name: name,
                        description: description,
                        status: status,
                        repoRemoteURL: repoRemoteURL,
                        goal: goal,
                        priority: priority,
                        prURL: prURL
                    )
                }
            )
        }
    }

    private var sidebar: some View {
        Group {
            if model.navigationMode == .agents {
                agentSidebar
            } else {
                workSidebar
            }
        }
        .navigationSplitViewColumnWidth(min: 200, ideal: 250, max: 340)
    }

    private var detail: some View {
        Group {
            if model.navigationMode == .agents {
                agentDetail
            } else {
                workDetail
            }
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var agentSidebar: some View {
        List(model.agents, selection: $model.selectedAgentID) { agent in
            HStack {
                Image(systemName: "person.circle")
                    .foregroundStyle(.secondary)
                VStack(alignment: .leading, spacing: 2) {
                    Text(agent.name)
                        .font(.body)
                    if !agent.isReady {
                        Text("Starting…")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    } else if agent.isSending {
                        Text("Working…")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .tag(agent.id)
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom) {
            HStack {
                Button {
                    model.createAgent()
                } label: {
                    Label("New Agent", systemImage: "plus")
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
            .padding(.vertical, 8)
        }
    }

    private var workSidebar: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                VStack(alignment: .leading, spacing: 10) {
                    workSidebarSectionTitle("Products")
                    ForEach(model.products) { product in
                        WorkSidebarFilterRow(
                            title: product.name,
                            subtitle: product.repoRemoteURL,
                            systemImage: "shippingbox",
                            isSelected: model.selectedProduct?.id == product.id,
                            trailing: product.status.capitalized
                        ) {
                            model.selectWorkProduct(product.id)
                        }
                    }
                }

                if let product = model.selectedProduct {
                    Divider()

                    VStack(alignment: .leading, spacing: 10) {
                        workSidebarSectionTitle("Projects")
                        WorkSidebarFilterRow(
                            title: "All Projects",
                            subtitle: "Show the full product board",
                            systemImage: "square.stack.3d.up",
                            isSelected: model.selectedProject == nil,
                            trailing: nil
                        ) {
                            model.selectWorkProjectFilter(nil)
                        }

                        ForEach(model.projectsForSelectedProduct) { project in
                            WorkSidebarFilterRow(
                                title: project.name,
                                subtitle: project.goal.isEmpty ? project.priority.capitalized : project.goal,
                                systemImage: "folder",
                                isSelected: model.selectedProject?.id == project.id,
                                trailing: project.status.capitalized
                            ) {
                                model.selectWorkProjectFilter(project.id)
                            }
                        }
                    }

                    if !(model.choresByProductID[product.id] ?? []).isEmpty {
                        Divider()
                    }

                    VStack(alignment: .leading, spacing: 10) {
                        workSidebarSectionTitle("Options")
                        Toggle(
                            "Show Chores",
                            isOn: Binding(
                                get: { model.includeWorkChores },
                                set: { model.setIncludeWorkChores($0) }
                            )
                        )
                        .toggleStyle(.switch)
                        .disabled(model.selectedProject != nil)

                        Toggle(
                            "Blocked Only",
                            isOn: Binding(
                                get: { model.workShowBlockedOnly },
                                set: { model.setWorkShowBlockedOnly($0) }
                            )
                        )
                        .toggleStyle(.switch)

                        if model.selectedProject != nil {
                            Text("Chores are product-level work, so they appear only in the all-projects view.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                }
            }
        }
        .padding(12)
        .searchable(text: $model.workSearchText, placement: .sidebar, prompt: "Filter board")
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
            .padding(.vertical, 8)
        }
    }

    private var agentDetail: some View {
        VStack(spacing: 0) {
            messageList
            composer
        }
    }

    private var workDetail: some View {
        Group {
            if model.products.isEmpty {
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
            } else if let product = model.selectedProduct {
                VSplitView {
                    workBoard(product: product)
                    workInspector
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

    private var messageList: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    ForEach(model.selectedAgentTimeline) { item in
                        switch item {
                        case .message(let message):
                            MessageBubble(message: message)
                                .id(item.id)
                        case .terminal(let terminal):
                            TerminalActivityCard(activity: terminal)
                                .id(item.id)
                        }
                    }
                }
                .padding(16)
            }
            .onChange(of: model.selectedAgentTimeline.count) {
                if let last = model.selectedAgentTimeline.last {
                    DispatchQueue.main.async {
                        proxy.scrollTo(last.id, anchor: .bottom)
                    }
                }
            }
        }
    }

    private var composer: some View {
        let isDraftEmpty = model.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        let canSend =
            model.selectedAgentID != nil && !isDraftEmpty && !model.isSelectedAgentSending
            && model.isSelectedAgentReady

        return VStack(spacing: 0) {
            HStack(alignment: .center, spacing: 10) {
                ComposerTextView(
                    text: $model.draft,
                    placeholder: model.isSelectedAgentReady ? "Type a message…" : "Agent starting…",
                    autoFocus: true,
                    focusTrigger: model.selectedAgentID
                ) {
                    model.sendDraft()
                }
                .frame(height: 36)
                .frame(maxWidth: .infinity)

                Button {
                    model.sendDraft()
                } label: {
                    Image(systemName: "paperplane.fill")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(canSend ? .primary : .secondary)
                        .frame(width: 18, height: 18)
                }
                .buttonStyle(.plain)
                .keyboardShortcut(.return, modifiers: [.command])
                .disabled(!canSend)
                .help("Send")
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(Color(nsColor: .controlBackgroundColor))
            .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
            )
            .padding(.horizontal, 16)
            .padding(.bottom, 12)
            .padding(.top, 4)

            if model.isSelectedAgentSending {
                HStack(spacing: 6) {
                    ProgressView()
                        .controlSize(.mini)
                    Text("Working…")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
            }
        }
    }

    @ViewBuilder
    private func workSectionHeader(
        title: String,
        subtitle: String,
        actions: [(String, () -> Void)]
    ) -> some View {
        HStack(alignment: .top) {
            VStack(alignment: .leading, spacing: 4) {
                Text(title)
                    .font(.title2.weight(.semibold))
                Text(subtitle)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            HStack {
                ForEach(Array(actions.enumerated()), id: \.offset) { _, action in
                    Button(action.0, action: action.1)
                }
            }
        }
    }

    @ViewBuilder
    private func workMetadataRow(_ label: String, value: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(label)
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(value)
                .font(.body)
        }
    }

    private func workBoard(product: WorkProduct) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(product.name)
                        .font(.title2.weight(.semibold))
                    Text(model.selectedProject?.name ?? "All projects")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if let remote = product.repoRemoteURL, !remote.isEmpty {
                    Text(remote)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            .padding(.horizontal, 24)
            .padding(.top, 20)

            ScrollView(.horizontal) {
                HStack(alignment: .top, spacing: 16) {
                    ForEach(WorkBoardColumnKey.allCases) { column in
                        workColumn(column)
                    }
                }
                .padding(.horizontal, 24)
                .padding(.bottom, 24)
            }
        }
    }

    private func workColumn(_ column: WorkBoardColumnKey) -> some View {
        let items = model.workItems(in: column)

        return VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(column.title)
                    .font(.headline)
                Spacer()
                Text("\(items.count)")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(Color(nsColor: .quaternaryLabelColor).opacity(0.12))
                    .clipShape(Capsule())
            }

            if column == .backlog {
                HStack(spacing: 8) {
                    Button("New Task") {
                        model.presentCreateTask()
                    }
                    .disabled(model.selectedProject == nil || !model.isConnected)

                    Button("New Chore") {
                        model.presentCreateChore()
                    }
                    .disabled(model.selectedProduct == nil || !model.isConnected)
                }
                .font(.caption)
            }

            Divider()

            if items.isEmpty {
                Text("No items")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 80, alignment: .topLeading)
            } else {
                VStack(alignment: .leading, spacing: 10) {
                    ForEach(items) { task in
                        Button {
                            model.selectWorkCard(task.id)
                        } label: {
                            WorkBoardCardView(
                                task: task,
                                projectName: task.isChore ? nil : model.projectName(for: task.projectID),
                                isSelected: model.selectedTask?.id == task.id
                            )
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
        }
        .frame(width: 260, alignment: .topLeading)
        .padding(14)
        .background(Color(nsColor: .controlBackgroundColor))
        .clipShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
        )
        .dropDestination(for: String.self) { items, _ in
            guard let taskID = items.first else { return false }
            model.moveTask(taskID, to: column)
            return true
        }
    }

    private var workInspector: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                if let task = model.selectedTask {
                    workSectionHeader(
                        title: task.name,
                        subtitle: task.isChore ? "Chore" : "Task",
                        actions: [("Edit", model.presentEditSelectedWorkItem)]
                    )
                    if !task.description.isEmpty {
                        Text(task.description)
                    }
                    if let projectName = model.projectName(for: task.projectID) {
                        workMetadataRow("Project", value: projectName)
                    }
                    workMetadataRow("Status", value: task.status.capitalized)
                    if let ordinal = task.ordinal, !task.isChore {
                        workMetadataRow("Phase", value: "\(ordinal)")
                    }
                    workMetadataRow("PR", value: task.prURL ?? "Not set")
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Move")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        HStack {
                            ForEach(WorkBoardColumnKey.allCases) { column in
                                Button(column.title) {
                                    model.moveTask(task.id, to: column)
                                }
                                .disabled(task.boardColumn == column && task.status != "blocked")
                            }
                        }
                    }
                    HStack {
                        if task.status == "active" || task.status == "blocked" {
                            Button(task.status == "blocked" ? "Unblock" : "Mark Blocked") {
                                model.toggleBlocked(for: task.id)
                            }
                        }
                        if !task.isChore {
                            Button("Move Up") {
                                model.moveSelectedTask(offset: -1)
                            }
                            Button("Move Down") {
                                model.moveSelectedTask(offset: 1)
                            }
                        }
                        Button("Delete", role: .destructive) {
                            model.deleteSelectedWorkItem()
                        }
                    }
                } else if let project = model.selectedProject {
                    workSectionHeader(
                        title: project.name,
                        subtitle: "Project",
                        actions: [
                            ("Edit", model.presentEditSelectedWorkItem),
                            ("New Task", model.presentCreateTask),
                        ]
                    )
                    if !project.description.isEmpty {
                        Text(project.description)
                    }
                    workMetadataRow("Status", value: project.status.capitalized)
                    workMetadataRow("Priority", value: project.priority.capitalized)
                    workMetadataRow("Goal", value: project.goal.isEmpty ? "Not set" : project.goal)
                    let tasks = model.tasksByProjectID[project.id] ?? []
                    if !tasks.isEmpty {
                        Divider()
                        Text("Phases")
                            .font(.headline)
                        ForEach(
                            tasks.sorted { lhs, rhs in
                                switch (lhs.ordinal, rhs.ordinal) {
                                case let (left?, right?) where left != right:
                                    return left < right
                                default:
                                    if lhs.createdAt == rhs.createdAt {
                                        return lhs.name.localizedCaseInsensitiveCompare(rhs.name)
                                            == .orderedAscending
                                    }
                                    return lhs.createdAt < rhs.createdAt
                                }
                            }
                        ) { task in
                            VStack(alignment: .leading, spacing: 2) {
                                Text(task.name)
                                    .font(.body.weight(.medium))
                                Text(task.status.capitalized)
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.vertical, 4)
                        }
                    }
                } else if let product = model.selectedProduct {
                    workSectionHeader(
                        title: product.name,
                        subtitle: "Product",
                        actions: [
                            ("Edit", model.presentEditSelectedWorkItem),
                            ("New Project", model.presentCreateProject),
                            ("New Chore", model.presentCreateChore),
                        ]
                    )
                    if !product.description.isEmpty {
                        Text(product.description)
                    }
                    workMetadataRow("Status", value: product.status.capitalized)
                    workMetadataRow("Remote", value: product.repoRemoteURL ?? "Not set")
                    workMetadataRow("Projects", value: "\(model.projectsForSelectedProduct.count)")
                    workMetadataRow("Visible Cards", value: "\(model.visibleWorkItems.count)")
                }
            }
            .padding(24)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(minHeight: 220)
        .background(Color(nsColor: .controlBackgroundColor))
    }

    @ViewBuilder
    private func workSidebarSectionTitle(_ title: String) -> some View {
        Text(title)
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
            .textCase(.uppercase)
    }
}

private struct WorkSidebarFilterRow: View {
    let title: String
    let subtitle: String?
    let systemImage: String
    let isSelected: Bool
    let trailing: String?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: systemImage)
                    .foregroundStyle(isSelected ? .primary : .secondary)
                    .frame(width: 16)
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(.body.weight(isSelected ? .semibold : .regular))
                        .foregroundStyle(.primary)
                    if let subtitle, !subtitle.isEmpty {
                        Text(subtitle)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                    }
                }
                Spacer(minLength: 8)
                if let trailing {
                    WorkStatusBadge(text: trailing)
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .background(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill(isSelected ? Color.accentColor.opacity(0.12) : Color.clear)
            )
        }
        .buttonStyle(.plain)
    }
}

private struct WorkBoardCardView: View {
    let task: WorkTask
    let projectName: String?
    let isSelected: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .top) {
                Text(task.name)
                    .font(.body.weight(.medium))
                    .foregroundStyle(.primary)
                    .multilineTextAlignment(.leading)
                Spacer(minLength: 8)
                Image(systemName: task.isChore ? "wrench.and.screwdriver" : "circle.hexagongrid")
                    .foregroundStyle(.secondary)
            }

            HStack {
                if let projectName, !projectName.isEmpty {
                    WorkStatusBadge(text: projectName)
                } else {
                    WorkStatusBadge(text: "Chore")
                }
                if task.status == "blocked" {
                    WorkStatusBadge(text: "Blocked")
                }
                Spacer()
                Text(task.status.replacingOccurrences(of: "_", with: " ").capitalized)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            if let prURL = task.prURL, !prURL.isEmpty {
                Text(prURL)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(cardBackground)
        .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .stroke(borderColor, lineWidth: isSelected ? 2 : 1)
        )
        .draggable(task.id)
    }

    private var cardBackground: Color {
        if isSelected {
            return Color.accentColor.opacity(0.08)
        }
        if task.status == "blocked" {
            return Color.orange.opacity(0.08)
        }
        return Color(nsColor: .windowBackgroundColor)
    }

    private var borderColor: Color {
        if isSelected {
            return .accentColor
        }
        if task.status == "blocked" {
            return .orange
        }
        return Color(nsColor: .separatorColor)
    }
}

private struct WorkCreateSheet: View {
    let request: WorkCreateRequest
    let onCancel: () -> Void
    let onCreate: (String, String, String, String) -> Void

    @State private var name = ""
    @State private var description = ""
    @State private var repoRemoteURL = ""
    @State private var goal = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(title)
                .font(.title3.weight(.semibold))

            TextField("Name", text: $name)

            switch request.kind {
            case .product:
                TextField("Description", text: $description)
                TextField("Remote URL", text: $repoRemoteURL)
            case .project:
                TextField("Description", text: $description)
                TextField("Goal", text: $goal)
            case .task, .chore:
                TextField("Description", text: $description)
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                Button("Create") {
                    onCreate(name, description, repoRemoteURL, goal)
                }
                .keyboardShortcut(.defaultAction)
                .disabled(name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 420)
    }

    private var title: String {
        switch request.kind {
        case .product:
            return "New Product"
        case .project:
            return "New Project"
        case .task:
            return "New Task"
        case .chore:
            return "New Chore"
        }
    }
}

private struct WorkEditSheet: View {
    let request: WorkEditRequest
    let onCancel: () -> Void
    let onSave: (String, String, String, String, String, String, String) -> Void

    @State private var name: String
    @State private var description: String
    @State private var status: String
    @State private var repoRemoteURL: String
    @State private var goal: String
    @State private var priority: String
    @State private var prURL: String

    init(
        request: WorkEditRequest,
        onCancel: @escaping () -> Void,
        onSave: @escaping (String, String, String, String, String, String, String) -> Void
    ) {
        self.request = request
        self.onCancel = onCancel
        self.onSave = onSave

        switch request.item {
        case .product(let product):
            _name = State(initialValue: product.name)
            _description = State(initialValue: product.description)
            _status = State(initialValue: product.status)
            _repoRemoteURL = State(initialValue: product.repoRemoteURL ?? "")
            _goal = State(initialValue: "")
            _priority = State(initialValue: "")
            _prURL = State(initialValue: "")
        case .project(let project):
            _name = State(initialValue: project.name)
            _description = State(initialValue: project.description)
            _status = State(initialValue: project.status)
            _repoRemoteURL = State(initialValue: "")
            _goal = State(initialValue: project.goal)
            _priority = State(initialValue: project.priority)
            _prURL = State(initialValue: "")
        case .task(let task), .chore(let task):
            _name = State(initialValue: task.name)
            _description = State(initialValue: task.description)
            _status = State(initialValue: task.status)
            _repoRemoteURL = State(initialValue: "")
            _goal = State(initialValue: "")
            _priority = State(initialValue: "")
            _prURL = State(initialValue: task.prURL ?? "")
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(title)
                .font(.title3.weight(.semibold))

            TextField("Name", text: $name)
            TextField("Description", text: $description)

            switch request.item {
            case .product:
                Picker("Status", selection: $status) {
                    ForEach(["active", "paused", "archived"], id: \.self) { status in
                        Text(status.capitalized).tag(status)
                    }
                }
                TextField("Remote URL", text: $repoRemoteURL)
            case .project:
                Picker("Status", selection: $status) {
                    ForEach(["planned", "active", "blocked", "done", "archived"], id: \.self) { status in
                        Text(status.capitalized).tag(status)
                    }
                }
                Picker("Priority", selection: $priority) {
                    ForEach(["low", "medium", "high"], id: \.self) { priority in
                        Text(priority.capitalized).tag(priority)
                    }
                }
                TextField("Goal", text: $goal)
            case .task, .chore:
                Picker("Status", selection: $status) {
                    ForEach(["todo", "active", "blocked", "in_review", "done"], id: \.self) { status in
                        Text(status.replacingOccurrences(of: "_", with: " ").capitalized).tag(status)
                    }
                }
                TextField("PR URL", text: $prURL)
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                Button("Save") {
                    onSave(name, description, status, repoRemoteURL, goal, priority, prURL)
                }
                .keyboardShortcut(.defaultAction)
                .disabled(name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 440)
    }

    private var title: String {
        switch request.item {
        case .product:
            return "Edit Product"
        case .project:
            return "Edit Project"
        case .task:
            return "Edit Task"
        case .chore:
            return "Edit Chore"
        }
    }
}

private struct WorkStatusBadge: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(.secondary)
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(Color(nsColor: .controlBackgroundColor))
            .clipShape(Capsule())
    }
}

private struct ComposerTextView: NSViewRepresentable {
    @Binding var text: String
    let placeholder: String
    let autoFocus: Bool
    var focusTrigger: String?
    let onSubmit: () -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(parent: self)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = NSScrollView()
        scrollView.drawsBackground = false
        scrollView.borderType = .noBorder
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.scrollerStyle = .overlay

        let textView = ComposerNSTextView()
        textView.delegate = context.coordinator
        textView.isEditable = true
        textView.isSelectable = true
        textView.isRichText = false
        textView.importsGraphics = false
        textView.allowsUndo = true
        textView.font = .preferredFont(forTextStyle: .body)
        textView.textColor = .labelColor
        textView.backgroundColor = .clear
        textView.drawsBackground = false
        textView.focusRingType = .none
        textView.textContainer?.lineFragmentPadding = 0
        textView.isHorizontallyResizable = false
        textView.isVerticallyResizable = true
        textView.autoresizingMask = [.width]
        textView.maxSize = NSSize(
            width: CGFloat.greatestFiniteMagnitude,
            height: CGFloat.greatestFiniteMagnitude
        )
        textView.minSize = NSSize(width: 0, height: 0)
        textView.textContainer?.widthTracksTextView = true
        textView.submitHandler = onSubmit
        textView.placeholder = placeholder
        textView.string = text

        scrollView.documentView = textView
        context.coordinator.textView = textView
        context.coordinator.didAutoFocus = false
        return scrollView
    }

    func updateNSView(_ nsView: NSScrollView, context: Context) {
        context.coordinator.parent = self
        guard let textView = context.coordinator.textView else {
            return
        }

        textView.submitHandler = onSubmit
        textView.placeholder = placeholder
        if textView.string != text {
            textView.string = text
            textView.needsDisplay = true
        }

        let shouldFocus: Bool
        if !context.coordinator.didAutoFocus, autoFocus {
            context.coordinator.didAutoFocus = true
            shouldFocus = true
        } else if focusTrigger != context.coordinator.lastFocusTrigger {
            context.coordinator.lastFocusTrigger = focusTrigger
            shouldFocus = true
        } else {
            shouldFocus = false
        }

        if shouldFocus {
            DispatchQueue.main.async {
                guard let window = textView.window else {
                    return
                }
                window.makeFirstResponder(textView)
            }
        }
    }

    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: ComposerTextView
        weak var textView: ComposerNSTextView?
        var didAutoFocus = false
        var lastFocusTrigger: String?

        init(parent: ComposerTextView) {
            self.parent = parent
        }

        func textDidChange(_ notification: Notification) {
            guard let textView = notification.object as? NSTextView else {
                return
            }
            parent.text = textView.string
            textView.needsDisplay = true
        }
    }
}

private final class ComposerNSTextView: NSTextView {
    var submitHandler: (() -> Void)?
    var placeholder: String = "" {
        didSet {
            needsDisplay = true
        }
    }

    override func layout() {
        super.layout()
        guard let layoutManager, let textContainer, let scrollView = enclosingScrollView else { return }
        layoutManager.ensureLayout(for: textContainer)
        let textHeight = layoutManager.usedRect(for: textContainer).height
        let visibleHeight = scrollView.contentSize.height
        let topInset = max(0, (visibleHeight - textHeight) / 2)
        if abs(textContainerInset.height - topInset) > 0.5 {
            textContainerInset = NSSize(width: 0, height: topInset)
        }
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)

        guard string.isEmpty, !placeholder.isEmpty, let font else {
            return
        }

        let origin = textContainerOrigin
        let x = origin.x + (textContainer?.lineFragmentPadding ?? 0)
        let y = origin.y
        let attrs: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: NSColor.placeholderTextColor,
        ]
        (placeholder as NSString).draw(at: NSPoint(x: x, y: y), withAttributes: attrs)
    }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        guard event.type == .keyDown else {
            return super.performKeyEquivalent(with: event)
        }

        let modifiers = event.modifierFlags.intersection([.command, .shift, .option, .control])
        guard modifiers == [.command], let chars = event.charactersIgnoringModifiers else {
            return super.performKeyEquivalent(with: event)
        }

        switch chars.lowercased() {
        case "a":
            selectAll(nil)
            return true
        case "c":
            copy(nil)
            return true
        case "v":
            paste(nil)
            return true
        case "x":
            cut(nil)
            return true
        case "z":
            undoManager?.undo()
            return true
        default:
            return super.performKeyEquivalent(with: event)
        }
    }

    override func doCommand(by selector: Selector) {
        let isNewlineCommand = selector == #selector(insertNewline(_:))
            || selector == #selector(insertLineBreak(_:))
            || selector == #selector(insertNewlineIgnoringFieldEditor(_:))
        guard isNewlineCommand, !hasMarkedText() else {
            super.doCommand(by: selector)
            return
        }

        let modifiers = NSApp.currentEvent?.modifierFlags.intersection([
            .shift,
            .control,
            .option,
            .command,
        ]) ?? []

        if modifiers == [.shift] {
            insertNewline(nil)
            return
        }

        if modifiers.isEmpty {
            submitHandler?()
            return
        }

        super.doCommand(by: selector)
    }
}

private struct MessageBubble: View {
    let message: ChatMessage

    var body: some View {
        switch message.role {
        case .assistant:
            assistantText
        case .user:
            userBubble
        case .system:
            systemText
        }
    }

    private var assistantText: some View {
        HStack {
            StructuredText(markdown: message.text)
                .textual.textSelection(.enabled)
                .frame(maxWidth: 720, alignment: .leading)
            Spacer(minLength: 60)
        }
    }

    private var userBubble: some View {
        HStack {
            Spacer(minLength: 80)
            Text(message.text)
                .font(.body)
                .textSelection(.enabled)
                .padding(12)
                .frame(maxWidth: 560, alignment: .leading)
                .background(.blue.opacity(0.18))
                .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
        }
    }

    private var systemText: some View {
        HStack {
            Text(message.text)
                .font(.caption)
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .frame(maxWidth: 720, alignment: .leading)
            Spacer(minLength: 60)
        }
    }
}

private struct TerminalActivityCard: View {
    let activity: TerminalActivity

    @State private var isExpanded: Bool = false
    @State private var isHovering: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if isExpanded {
                VStack(spacing: 0) {
                    terminalHeader
                        .padding(.horizontal, 12)
                        .padding(.vertical, 10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(terminalHeaderBackground)

                    Divider()
                        .overlay(Color(nsColor: .separatorColor))

                    TerminalOutputPane(activity: activity, background: terminalOutputBackground)
                }
                .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
                .overlay(
                    RoundedRectangle(cornerRadius: 12, style: .continuous)
                        .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
                )
            } else {
                terminalHeader
                    .padding(.horizontal, 12)
                    .padding(.vertical, 10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(terminalHeaderBackground)
                    .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
                    .overlay(
                        RoundedRectangle(cornerRadius: 12, style: .continuous)
                            .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
                    )
            }
        }
        .onHover { hovering in
            isHovering = hovering
        }
    }

    private var commandPrefix: String {
        if isFailed {
            return "Failed"
        }
        if isSuccessful {
            return "Success"
        }
        return "Running"
    }

    private var command: String {
        let command = activity.command.isEmpty ? "<command unavailable>" : activity.command
        return command
    }

    private var isSuccessful: Bool {
        activity.status == "Done"
    }

    private var isFailed: Bool {
        activity.status.hasPrefix("Failed") || activity.status.hasPrefix("Terminated")
    }

    private var terminalHeader: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 6) {
                if let cwd = activity.cwd, !cwd.isEmpty {
                    Text(cwd)
                        .font(.system(.footnote, design: .monospaced))
                        .foregroundStyle(.secondary)
                }

                commandLineText
                    .font(.system(.callout, design: .monospaced))
                    .textSelection(.enabled)
            }

            Spacer(minLength: 12)

            Button {
                isExpanded.toggle()
            } label: {
                Image(systemName: isExpanded ? "chevron.up" : "chevron.down")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .frame(width: 22, height: 22)
                    .background(Color(nsColor: .quaternaryLabelColor).opacity(0.22))
                    .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
            }
            .buttonStyle(.plain)
            .help(isExpanded ? "Hide output" : "Show output")
            .opacity(isHovering ? 1 : 0)
            .allowsHitTesting(isHovering)
            .animation(.easeInOut(duration: 0.12), value: isHovering)
        }
    }

    private var statusWordColor: Color {
        if isFailed {
            return .red
        }
        if isSuccessful {
            return .green
        }
        return .primary
    }

    private var commandLineText: Text {
        Text(commandPrefix).foregroundColor(statusWordColor)
            + Text(" \(command)").foregroundColor(.primary)
    }

    private var terminalHeaderBackground: Color {
        Color(nsColor: .controlBackgroundColor)
    }

    private var terminalOutputBackground: Color {
        Color(nsColor: .textBackgroundColor)
    }
}

private struct TerminalOutputPane: View {
    let activity: TerminalActivity
    let background: Color

    @State private var isPinnedToBottom: Bool = true
    @State private var suppressOffsetTracking: Bool = false
    @State private var contentFrame: CGRect = .zero
    @State private var viewportHeight: CGFloat = 0

    private let bottomThreshold: CGFloat = 6

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    Text(activity.output.isEmpty ? "" : activity.output)
                        .font(.system(.callout, design: .monospaced))
                        .frame(maxWidth: .infinity, alignment: .topLeading)
                        .textSelection(.enabled)
                        .padding(12)
                    Color.clear
                        .frame(height: 1)
                        .id(outputBottomID)
                }
                .background(
                    GeometryReader { geo in
                        Color.clear.preference(
                            key: TerminalContentFramePreferenceKey.self,
                            value: geo.frame(in: .named(scrollSpaceID))
                        )
                    }
                )
            }
            .coordinateSpace(name: scrollSpaceID)
            .background(
                GeometryReader { geo in
                    Color.clear.preference(
                        key: TerminalViewportHeightPreferenceKey.self,
                        value: geo.size.height
                    )
                }
            )
            .frame(minHeight: 120, maxHeight: 240)
            .background(background)
            .onPreferenceChange(TerminalContentFramePreferenceKey.self) { frame in
                contentFrame = frame
                refreshPinnedState()
            }
            .onPreferenceChange(TerminalViewportHeightPreferenceKey.self) { height in
                viewportHeight = height
                refreshPinnedState()
            }
            .onAppear {
                scrollToBottom(proxy, animated: false)
                isPinnedToBottom = true
            }
            .onChange(of: activity.output.count) { _, _ in
                guard isPinnedToBottom else {
                    return
                }

                suppressOffsetTracking = true
                scrollToBottom(proxy, animated: true)

                DispatchQueue.main.asyncAfter(deadline: .now() + 0.12) {
                    isPinnedToBottom = true
                    suppressOffsetTracking = false
                }
            }
        }
    }

    private var outputBottomID: String {
        "terminal-output-bottom-\(activity.id)"
    }

    private var scrollSpaceID: String {
        "terminal-scroll-space-\(activity.id)"
    }

    private func scrollToBottom(_ proxy: ScrollViewProxy, animated: Bool) {
        if animated {
            withAnimation(.easeOut(duration: 0.12)) {
                proxy.scrollTo(outputBottomID, anchor: .bottom)
            }
        } else {
            proxy.scrollTo(outputBottomID, anchor: .bottom)
        }
    }

    private func refreshPinnedState() {
        guard !suppressOffsetTracking else {
            return
        }

        let bottomDistance = max(0, contentFrame.height + contentFrame.minY - viewportHeight)
        isPinnedToBottom = bottomDistance <= bottomThreshold
    }
}

private struct TerminalContentFramePreferenceKey: PreferenceKey {
    static let defaultValue: CGRect = .zero

    static func reduce(value: inout CGRect, nextValue: () -> CGRect) {
        value = nextValue()
    }
}

private struct TerminalViewportHeightPreferenceKey: PreferenceKey {
    static let defaultValue: CGFloat = 0

    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = nextValue()
    }
}
