import AppKit
import os.log
import SwiftUI
import UpdateCore

struct WorkCreateSheet: View {
    let request: WorkCreateRequest
    /// Parent product's default repo URL when the request is for a
    /// task or chore. Drives the chore/task form's repo render mode
    /// per design Q10: hidden-with-disclosure when set, shown-required
    /// when nil.
    let productDefaultRepoURL: String?
    /// Empirical known-repo set for the parent product. Powers the
    /// "Recent repos" picker. Empty when the form is for a product or
    /// project.
    let knownRepos: [String]
    let onCancel: () -> Void
    /// Callback args: `(name, description, repoRemoteURL, goal,
    /// setAsProductDefault)`. The last flag is meaningful only on
    /// task/chore submissions made against a product without a
    /// default repo where the user typed a fresh URL.
    let onCreate: (String, String, String, String, Bool) -> Void

    @State private var name = ""
    @State private var description = ""
    @State private var goal = ""
    @State private var repoFormState: WorkCreateRepoFormState

    init(
        request: WorkCreateRequest,
        productDefaultRepoURL: String?,
        knownRepos: [String],
        onCancel: @escaping () -> Void,
        onCreate: @escaping (String, String, String, String, Bool) -> Void
    ) {
        self.request = request
        self.productDefaultRepoURL = productDefaultRepoURL
        self.knownRepos = knownRepos
        self.onCancel = onCancel
        self.onCreate = onCreate
        _repoFormState = State(
            initialValue: WorkCreateRepoFormState(
                productRepoURL: productDefaultRepoURL,
                knownRepos: knownRepos
            )
        )
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(title)
                .font(.title3.weight(.semibold))

            TextField("Name", text: $name)

            switch request.kind {
            case .product:
                TextField("Description", text: $description)
                VStack(alignment: .leading, spacing: 4) {
                    // Product-create repo field is independent of the
                    // chore/task form state — same wire field, but the
                    // form mode + recent-repos picker only make sense
                    // *under* an existing product.
                    TextField(
                        ProductRepoFieldCopy.placeholder,
                        text: productCreateRepoBinding
                    )
                    Text(ProductRepoFieldCopy.helperText)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            case .project:
                TextField("Description", text: $description)
                TextField("Goal", text: $goal)
            case .task, .chore:
                TextField("Description", text: $description)
                workItemRepoField
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                Button("Create") {
                    onCreate(
                        name,
                        description,
                        submittedRepoURL,
                        goal,
                        repoFormState.shouldSetAsProductDefault
                    )
                }
                .keyboardShortcut(.defaultAction)
                .disabled(isSubmitDisabled)
            }
        }
        .padding(20)
        .frame(width: 460)
    }

    /// Repo field for chore/task creation. Renders the disclosure
    /// form in product-has-default mode and the required form in
    /// product-has-no-default mode, with the "Recent repos" picker
    /// and "Set as product default" affordance gated as the design
    /// describes.
    @ViewBuilder
    private var workItemRepoField: some View {
        switch repoFormState.mode {
        case .productHasDefault(let defaultURL):
            DisclosureGroup(
                WorkItemRepoFieldCopy.overrideDisclosureLabel,
                isExpanded: $repoFormState.overrideEnabled
            ) {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Inherits from product: \(defaultURL)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    recentReposPicker
                    TextField(
                        WorkItemRepoFieldCopy.overridePlaceholder,
                        text: $repoFormState.enteredURL
                    )
                    Text(WorkItemRepoFieldCopy.overrideHelperText)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.top, 4)
            }
        case .productHasNoDefault:
            VStack(alignment: .leading, spacing: 6) {
                recentReposPicker
                TextField(
                    WorkItemRepoFieldCopy.requiredPlaceholder,
                    text: $repoFormState.enteredURL
                )
                Text(WorkItemRepoFieldCopy.requiredHelperText)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if repoFormState.showSetAsProductDefaultCheckbox {
                    Toggle(
                        WorkItemRepoFieldCopy.setAsProductDefaultLabel,
                        isOn: $repoFormState.setAsProductDefault
                    )
                    .font(.caption)
                }
            }
        }
    }

    /// "Recent repos" picker — surfaces the product's empirical
    /// known-repo set. The first option is a no-op placeholder that
    /// the picker shows when the user hasn't picked anything yet;
    /// selecting any other entry copies its URL into the text field.
    @ViewBuilder
    private var recentReposPicker: some View {
        if !knownRepos.isEmpty {
            Picker(
                WorkItemRepoFieldCopy.recentReposLabel,
                selection: pickerSelectionBinding
            ) {
                Text("Choose…").tag(Optional<String>.none)
                ForEach(knownRepos, id: \.self) { url in
                    Text("\(shortRepoName(for: url)) — \(url)")
                        .tag(Optional<String>.some(url))
                }
            }
            .pickerStyle(.menu)
        }
    }

    /// Two-way binding between the recent-repos `Picker` and the text
    /// field. Reading reports the URL when it exactly matches a known
    /// entry; writing copies the chosen URL into the entered text.
    private var pickerSelectionBinding: Binding<String?> {
        Binding(
            get: {
                let trimmed = repoFormState.enteredURL
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                return knownRepos.contains(trimmed) ? trimmed : nil
            },
            set: { newValue in
                guard let newValue else { return }
                repoFormState.enteredURL = newValue
                repoFormState.setAsProductDefault = false
            }
        )
    }

    /// Binding for the product-create repo field. Product creation
    /// doesn't share the chore/task form state — the field is a
    /// vanilla text input — so we keep the value alongside the rest
    /// of the chore/task form state in the same `enteredURL` slot
    /// (the two cases are mutually exclusive by request kind, so the
    /// reuse is safe and avoids a parallel `@State`).
    private var productCreateRepoBinding: Binding<String> {
        $repoFormState.enteredURL
    }

    /// The URL string to forward to `onCreate`. For chore/task in
    /// `.productHasDefault` mode with the override disclosure closed,
    /// the value is the empty string — submission falls through to
    /// the product default, matching the engine's
    /// "absent field → inherit" semantics.
    private var submittedRepoURL: String {
        switch request.kind {
        case .product:
            return repoFormState.enteredURL
        case .project:
            return ""
        case .task, .chore:
            return repoFormState.submittedURL ?? ""
        }
    }

    /// Encodes the submission gate. Name is always required; the
    /// repo field adds a second gate for chore/task creation under a
    /// product with no default.
    private var isSubmitDisabled: Bool {
        if name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return true
        }
        switch request.kind {
        case .product, .project:
            return false
        case .task, .chore:
            return repoFormState.isSubmissionBlocked
        }
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

/// Shared layout metrics for the redesigned Edit Product dialog (#982).
///
/// One source of truth for the dialog's grid so the Product, External
/// Tracker, and GitHub-account sections all inset to the same left margin
/// and share one label column. The label column is sized for the longest
/// label in the form ("Worker branch prefix" / "Owner / Organization") so
/// no label clips or runs past the dialog edge the way the first
/// `Form`-based pass did.

private enum ProductDialogMetrics {
    /// Dialog width. Wide enough that a 160pt label column still leaves a
    /// comfortable field column; a deliberate, modest bump over #982's 480.
    static let width: CGFloat = 520
    /// Outer inset for the title, section stack, and footer button bar.
    /// Full-bleed dividers ignore it so the header/content/footer read as
    /// distinct bands.
    static let horizontalPadding: CGFloat = 24
    /// Fixed width of the leading label column. Fits the longest label at
    /// the body font with margin to spare, which is the whole point of the
    /// redesign — every row's field starts at the same x.
    static let labelColumnWidth: CGFloat = 160
    /// Gap between the label column and the field column.
    static let labelFieldGap: CGFloat = 12
    /// Vertical gap between rows inside one section.
    static let rowSpacing: CGFloat = 12
    /// Vertical gap between a section header and its first row.
    static let headerRowGap: CGFloat = 10
    /// Vertical gap between sections.
    static let sectionGap: CGFloat = 22
}

/// `LabeledContentStyle` that lays every form row out on the shared grid:
/// a fixed-width, leading-aligned label column followed by a field column
/// that fills the remaining width. Applied once to the whole form so all
/// rows — across all three sections — line up. A label-less row
/// (`LabeledContent { ... } label: { EmptyView() }`) still reserves the
/// column, which is how the Reverse-close toggle and Unset button align to
/// the field column instead of floating.

private struct ProductFixedLabelStyle: LabeledContentStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: ProductDialogMetrics.labelFieldGap) {
            configuration.label
                .frame(width: ProductDialogMetrics.labelColumnWidth, alignment: .leading)
            configuration.content
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

/// A titled group in the Edit Product dialog: a real, left-aligned section
/// header (the first pass rendered these as centered, stray-looking labels)
/// over a consistently-spaced stack of rows. All three sections use this so
/// their headers and content share one alignment grid.

private struct ProductFormSection<Content: View>: View {
    private let title: String
    private let content: Content

    init(_ title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: ProductDialogMetrics.headerRowGap) {
            Text(title)
                .font(.headline)
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityAddTraits(.isHeader)
            VStack(alignment: .leading, spacing: ProductDialogMetrics.rowSpacing) {
                content
            }
        }
    }
}

struct WorkEditSheet: View {
    @EnvironmentObject private var model: ChatViewModel

    let request: WorkEditRequest
    let onCancel: () -> Void
    let onSave: (String, String, String, String, String, String, String, String, String) -> Void
    let onSetTracker: ((String, String, String, Int, Bool) -> Void)?
    let onUnsetTracker: (() -> Void)?
    let onSetMergeMechanism: ((String) -> Void)?

    @State private var name: String
    @State private var description: String
    @State private var status: String
    @State private var repoRemoteURL: String
    @State private var goal: String
    @State private var priority: String
    @State private var prURL: String
    @State private var workerBranchPrefix: String
    @State private var docsRepo: String

    // External tracker state (product only)
    @State private var trackerKind: String
    @State private var trackerOrg: String
    @State private var trackerRepo: String
    @State private var trackerProjectNumber: String
    @State private var trackerReverseClose: Bool
    // True if the product had a tracker bound when the sheet opened.
    private let initialTrackerBound: Bool

    // Merge mechanism state (product only). `"direct"` or `"trunk_queue"`;
    // mirrors `Product.merge_mechanism` with `nil` resolved to `"direct"`.
    @State private var mergeMechanism: String
    private let initialMergeMechanism: String

    init(
        request: WorkEditRequest,
        onCancel: @escaping () -> Void,
        onSave: @escaping (String, String, String, String, String, String, String, String, String) -> Void,
        onSetTracker: ((String, String, String, Int, Bool) -> Void)? = nil,
        onUnsetTracker: (() -> Void)? = nil,
        onSetMergeMechanism: ((String) -> Void)? = nil
    ) {
        self.request = request
        self.onCancel = onCancel
        self.onSave = onSave
        self.onSetTracker = onSetTracker
        self.onUnsetTracker = onUnsetTracker
        self.onSetMergeMechanism = onSetMergeMechanism

        switch request.item {
        case .product(let product):
            _name = State(initialValue: product.name)
            _description = State(initialValue: product.description)
            _status = State(initialValue: product.status)
            _repoRemoteURL = State(initialValue: product.repoRemoteURL ?? "")
            _goal = State(initialValue: "")
            _priority = State(initialValue: "")
            _prURL = State(initialValue: "")
            _workerBranchPrefix = State(initialValue: product.workerBranchPrefix ?? "")
            _docsRepo = State(initialValue: product.docsRepo ?? "")
            let resolvedMergeMechanism = product.mergeMechanism ?? "direct"
            _mergeMechanism = State(initialValue: resolvedMergeMechanism)
            initialMergeMechanism = resolvedMergeMechanism

            if let kind = product.externalTrackerKind,
               let configJSON = product.externalTrackerConfig,
               let configData = configJSON.data(using: .utf8),
               let config = try? JSONSerialization.jsonObject(with: configData) as? [String: Any] {
                _trackerKind = State(initialValue: kind)
                _trackerOrg = State(initialValue: config["org"] as? String ?? "")
                _trackerRepo = State(initialValue: config["repo"] as? String ?? "")
                let projectNum = config["project_number"]
                if let n = projectNum as? Int {
                    _trackerProjectNumber = State(initialValue: String(n))
                } else if let n = projectNum as? Double {
                    _trackerProjectNumber = State(initialValue: String(Int(n)))
                } else {
                    _trackerProjectNumber = State(initialValue: "")
                }
                _trackerReverseClose = State(initialValue: config["reverse_close"] as? Bool ?? false)
                initialTrackerBound = true
            } else {
                _trackerKind = State(initialValue: "github")
                _trackerOrg = State(initialValue: "")
                _trackerRepo = State(initialValue: "")
                _trackerProjectNumber = State(initialValue: "")
                _trackerReverseClose = State(initialValue: false)
                initialTrackerBound = false
            }

        case .project(let project):
            _name = State(initialValue: project.name)
            _description = State(initialValue: project.description)
            _status = State(initialValue: project.status)
            _repoRemoteURL = State(initialValue: "")
            _goal = State(initialValue: project.goal)
            _priority = State(initialValue: project.priority)
            _prURL = State(initialValue: "")
            _workerBranchPrefix = State(initialValue: "")
            _docsRepo = State(initialValue: "")
            _trackerKind = State(initialValue: "github")
            _trackerOrg = State(initialValue: "")
            _trackerRepo = State(initialValue: "")
            _trackerProjectNumber = State(initialValue: "")
            _trackerReverseClose = State(initialValue: false)
            initialTrackerBound = false
            _mergeMechanism = State(initialValue: "direct")
            initialMergeMechanism = "direct"
        case .task(let task), .chore(let task):
            _name = State(initialValue: task.name)
            _description = State(initialValue: task.description)
            _status = State(initialValue: task.status)
            _repoRemoteURL = State(initialValue: "")
            _goal = State(initialValue: "")
            _priority = State(initialValue: task.priority)
            _prURL = State(initialValue: task.prURL ?? "")
            _workerBranchPrefix = State(initialValue: "")
            _docsRepo = State(initialValue: "")
            _trackerKind = State(initialValue: "github")
            _trackerOrg = State(initialValue: "")
            _trackerRepo = State(initialValue: "")
            _trackerProjectNumber = State(initialValue: "")
            _trackerReverseClose = State(initialValue: false)
            initialTrackerBound = false
            _mergeMechanism = State(initialValue: "direct")
            initialMergeMechanism = "direct"
        }
    }

    var body: some View {
        if case .product = request.item {
            productBody
                .onAppear {
                    model.refreshTrunkStatus()
                }
        } else {
            sharedBody
        }
    }

    @ViewBuilder
    private var productBody: some View {
        VStack(alignment: .leading, spacing: 0) {
            // Header band.
            Text("Edit Product")
                .font(.title3.weight(.semibold))
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, ProductDialogMetrics.horizontalPadding)
                .padding(.top, ProductDialogMetrics.horizontalPadding)
                .padding(.bottom, 12)

            Divider()

            // Content band: three sections on one shared label/field grid.
            VStack(alignment: .leading, spacing: ProductDialogMetrics.sectionGap) {
                ProductFormSection("Product") {
                    LabeledContent("Name") {
                        TextField("", text: $name, prompt: Text("Product name"))
                    }
                    LabeledContent("Description") {
                        TextField("", text: $description, prompt: Text("Optional"))
                    }
                    LabeledContent("Status") {
                        Picker("Status", selection: $status) {
                            ForEach(["active", "paused", "archived"], id: \.self) { s in
                                Text(s.capitalized).tag(s)
                            }
                        }
                        .labelsHidden()
                        .frame(maxWidth: 200, alignment: .leading)
                    }
                    LabeledContent("Repository URL") {
                        TextField(
                            "", text: $repoRemoteURL,
                            prompt: Text("https://github.com/org/repo")
                        )
                    }
                    LabeledContent("Worker branch prefix") {
                        VStack(alignment: .leading, spacing: 4) {
                            TextField("", text: $workerBranchPrefix, prompt: Text("e.g. bduff/"))
                            Text(
                                "Optional. Workers push to <prefix>exec_<id>. " +
                                "Leave blank to use the default prefix boss/. " +
                                "Trailing / is conventional."
                            )
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                    LabeledContent("Docs repo") {
                        VStack(alignment: .leading, spacing: 4) {
                            TextField(
                                "", text: $docsRepo,
                                prompt: Text("owner/repo")
                            )
                            Text(
                                "Optional. Investigation and design writeups open PRs here. " +
                                "Leave blank to use the user-level BOSS_USER_DOCS_REPO default."
                            )
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                }

                ProductFormSection("Merge Mechanism") {
                    LabeledContent("On approval, merge via") {
                        VStack(alignment: .leading, spacing: 4) {
                            Picker("Merge mechanism", selection: $mergeMechanism) {
                                Text("Direct merge").tag("direct")
                                Text("Trunk merge queue").tag("trunk_queue")
                            }
                            .labelsHidden()
                            .pickerStyle(.segmented)
                            .frame(maxWidth: 320, alignment: .leading)
                            Text(
                                "Direct merges an approved PR immediately (gh pr merge --auto --squash). " +
                                "Trunk merge queue submits it to the product's Trunk queue instead."
                            )
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)

                            if mergeMechanism == "trunk_queue" && model.trunkTokenConfigured == false {
                                HStack(alignment: .top, spacing: 6) {
                                    Image(systemName: "exclamationmark.triangle.fill")
                                        .foregroundStyle(.orange)
                                    VStack(alignment: .leading, spacing: 4) {
                                        Text("No Trunk API token is configured — merges on this product will fail until one is set.")
                                            .font(.caption)
                                            .foregroundStyle(.orange)
                                            .fixedSize(horizontal: false, vertical: true)
                                        SettingsLink {
                                            Text("Open Settings…")
                                                .font(.caption)
                                        }
                                    }
                                }
                                .padding(.top, 2)
                            }
                        }
                    }
                }

                ProductFormSection("External Tracker") {
                    LabeledContent("Kind") {
                        Picker("Kind", selection: $trackerKind) {
                            Text("GitHub").tag("github")
                        }
                        .labelsHidden()
                        .frame(maxWidth: 200, alignment: .leading)
                    }
                    if trackerKind == "github" {
                        LabeledContent("Owner / Organization") {
                            TextField("", text: $trackerOrg, prompt: Text("e.g. spinyfin"))
                        }
                        LabeledContent("Repository") {
                            TextField("", text: $trackerRepo, prompt: Text("e.g. mono"))
                        }
                        LabeledContent("Project number") {
                            TextField("", text: $trackerProjectNumber, prompt: Text("e.g. 7"))
                        }
                        // Label-less row: toggle + Unset align to the field
                        // column rather than floating under the fields.
                        LabeledContent {
                            VStack(alignment: .leading, spacing: 8) {
                                Toggle("Reverse-close", isOn: $trackerReverseClose)
                                    .help(
                                        "When a work item is marked done without a merged PR, " +
                                        "close the upstream GitHub issue."
                                    )
                                if initialTrackerBound {
                                    Button("Unset", role: .destructive) {
                                        onUnsetTracker?()
                                    }
                                }
                            }
                        } label: {
                            EmptyView()
                        }
                    }
                }

                ProductFormSection("GitHub account") {
                    GitHubAccountSection()
                }
            }
            .labeledContentStyle(ProductFixedLabelStyle())
            .padding(.horizontal, ProductDialogMetrics.horizontalPadding)
            .padding(.vertical, ProductDialogMetrics.sectionGap)

            Divider()

            // Footer band: dialog-level actions.
            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                Button("Save") {
                    onSave(
                        name, description, status, repoRemoteURL, "", "", "", workerBranchPrefix,
                        docsRepo
                    )
                    if trackerFormValid,
                       let num = Int(trackerProjectNumber.trimmingCharacters(in: .whitespacesAndNewlines)) {
                        onSetTracker?(trackerKind, trackerOrg, trackerRepo, num, trackerReverseClose)
                    }
                    if mergeMechanism != initialMergeMechanism {
                        onSetMergeMechanism?(mergeMechanism)
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(
                    name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ||
                    trackerFieldsEntered && !trackerFormValid
                )
            }
            .padding(.horizontal, ProductDialogMetrics.horizontalPadding)
            .padding(.vertical, 16)
        }
        .frame(width: ProductDialogMetrics.width)
    }

    @ViewBuilder
    private var sharedBody: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(title)
                .font(.title3.weight(.semibold))

            TextField("Name", text: $name)
            TextField("Description", text: $description)

            switch request.item {
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
                    ForEach(["todo", "active", "blocked", "in_review", "done", "archived"], id: \.self) { status in
                        Text(status.replacingOccurrences(of: "_", with: " ").capitalized).tag(status)
                    }
                }
                Picker("Priority", selection: $priority) {
                    ForEach(["low", "medium", "high"], id: \.self) { priority in
                        Text(priority.capitalized).tag(priority)
                    }
                }
                TextField("PR URL", text: $prURL)
            case .product:
                EmptyView()
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                Button("Save") {
                    onSave(name, description, status, repoRemoteURL, goal, priority, prURL, workerBranchPrefix, docsRepo)
                }
                .keyboardShortcut(.defaultAction)
                .disabled(name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 440)
    }

    private var trackerFieldsEntered: Bool {
        let org = trackerOrg.trimmingCharacters(in: .whitespacesAndNewlines)
        let repo = trackerRepo.trimmingCharacters(in: .whitespacesAndNewlines)
        let project = trackerProjectNumber.trimmingCharacters(in: .whitespacesAndNewlines)
        return !org.isEmpty || !repo.isEmpty || !project.isEmpty
    }

    private var trackerFormValid: Bool {
        guard trackerKind == "github" else { return false }
        let org = trackerOrg.trimmingCharacters(in: .whitespacesAndNewlines)
        let repo = trackerRepo.trimmingCharacters(in: .whitespacesAndNewlines)
        let project = trackerProjectNumber.trimmingCharacters(in: .whitespacesAndNewlines)
        return !org.isEmpty && !repo.isEmpty && Int(project) != nil
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

/// "GitHub account" subsection of the external-tracker settings — drives
/// and renders the engine-owned OAuth device flow (OAuth device-flow design
/// §4/§7/§8). All flow logic lives in the engine; the display mapping lives
/// in `GitHubAuthPresentation`. This view is a thin renderer over
/// `model.gitHubAuthState` plus button wiring to the `gitHubAuth*` bridges.
///
/// The auth state is global (one github.com token shared across all
/// GitHub-bound products), so this subsection shows the same state in every
/// product's settings sheet.

struct GitHubAccountSection: View {
    @EnvironmentObject private var model: ChatViewModel

    private var presentation: GitHubAuthPresentation {
        GitHubAuthPresentation.forState(model.gitHubAuthState)
    }

    var body: some View {
        // The enclosing `ProductFormSection("GitHub account")` now renders the
        // header and supplies the section's spacing, so this view is just the
        // account status/flow content.
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                if presentation.isBusy {
                    ProgressView()
                        .controlSize(.small)
                } else {
                    Image(systemName: presentation.statusIcon)
                        .foregroundStyle(.secondary)
                }
                Text(presentation.statusLine)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            if let prompt = presentation.pendingPrompt {
                pendingPromptView(prompt)
            }

            ForEach(Array(presentation.banners.enumerated()), id: \.offset) { _, banner in
                bannerView(banner)
            }

            if !presentation.actions.isEmpty {
                HStack(spacing: 8) {
                    ForEach(presentation.actions, id: \.self) { action in
                        actionButton(action)
                    }
                    Spacer()
                }
            }
        }
    }

    @ViewBuilder
    private func actionButton(_ action: GitHubAuthPresentation.Action) -> some View {
        switch action {
        case .connect:
            Button(presentation.connectIsRestart ? "Start over" : "Connect") {
                model.gitHubAuthConnect()
            }
        case .cancel:
            Button("Cancel") {
                model.gitHubAuthCancel()
            }
        case .disconnect:
            Button("Disconnect", role: .destructive) {
                model.gitHubAuthDisconnect()
            }
        case .reauthorize:
            Button("Re-authorize") {
                model.gitHubAuthReauthorize()
            }
        }
    }

    @ViewBuilder
    private func pendingPromptView(_ prompt: GitHubAuthPresentation.PendingPrompt) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Text("Code")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Text(prompt.userCode)
                    .font(.system(.title3, design: .monospaced).weight(.semibold))
                    .textSelection(.enabled)
            }
            HStack(spacing: 8) {
                if let url = URL(string: prompt.openURL) {
                    Link("Open in browser", destination: url)
                }
                Text(prompt.verificationURL)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
            Text("Enter the code at the verification URL to authorize Boss for issue sync.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(8)
        .background(Color(nsColor: .controlBackgroundColor))
        .clipShape(RoundedRectangle(cornerRadius: 6))
    }

    @ViewBuilder
    private func bannerView(_ banner: GitHubAuthPresentation.Banner) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 6) {
                Image(systemName: bannerIcon(banner.kind))
                    .foregroundStyle(bannerColor(banner.kind))
                Text(banner.message)
                    .font(.caption)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if banner.actionURL != nil || banner.offersRecheck {
                HStack(spacing: 8) {
                    if let urlString = banner.actionURL,
                       let label = banner.actionLabel,
                       let url = URL(string: urlString) {
                        Link(label, destination: url)
                    }
                    if banner.offersRecheck {
                        Button("Re-check") {
                            model.gitHubAuthRecheck()
                        }
                    }
                }
                .font(.caption)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(8)
        .background(bannerColor(banner.kind).opacity(0.12))
        .clipShape(RoundedRectangle(cornerRadius: 6))
    }

    private func bannerIcon(_ kind: GitHubAuthPresentation.Banner.Kind) -> String {
        switch kind {
        case .needsOrgApproval: return "building.2"
        case .needsSso: return "lock.shield"
        case .unknownOrg: return "questionmark.circle"
        case .limitedScopes: return "exclamationmark.triangle"
        case .expired: return "clock.badge.exclamationmark"
        case .denied: return "hand.raised"
        case .error: return "exclamationmark.octagon"
        }
    }

    private func bannerColor(_ kind: GitHubAuthPresentation.Banner.Kind) -> Color {
        switch kind {
        case .needsOrgApproval, .needsSso, .unknownOrg, .limitedScopes, .expired:
            return .orange
        case .denied, .error:
            return .red
        }
    }
}

/// Capsule chip surfacing a repo's short name on a kanban card or
/// product header. Hover tooltip carries the full URL plus the
/// provenance string ("Inherited from product" vs "Repo set on this
/// card") so the reader can tell where the URL came from without
/// digging into the popover. Pure view — all the mode/provenance
/// logic lives on `RepoChipPresentation` so tests don't need a
/// SwiftUI host.
///
/// The chip renders in a neutral style matching `WorkStatusBadge`
/// (used by `Blocked` and the project tag). Earlier the override
/// variant carried an accent-blue tint, but the color had no stable
/// meaning to readers — and with per-card chips now only appearing
/// on rows that carry their own repo, the "this row is special" signal
/// is already conveyed by the chip's mere presence on the card.
