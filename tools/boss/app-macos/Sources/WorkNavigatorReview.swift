import SwiftUI

/// PR review remains available while a revision runs and after it lands.
/// Section membership and the status explanation come from the engine.
struct WorkNavigatorReview: View {
    let context: NavigatorDetailContext
    @ObservedObject private var model: ChatViewModel
    @ObservedObject private var guides: NavigatorReviewGuideStore

    init(context: NavigatorDetailContext) {
        self.context = context
        model = context.model
        guides = context.model.navigatorReviewGuides
    }

    static func register(in registry: inout NavigatorDetailRegistry) {
        registry.register(.review) { AnyView(WorkNavigatorReview(context: $0)) }
    }

    private var isActive: Bool { model.navigator.tab == .review }
    var actionsDisabled: Bool { context.isReadOnly || !model.isConnected }
    private var root: WorkTask { context.root }
    private var reader: NavigatorReviewGuideStore.Reader? {
        guides.readers[root.id].flatMap { $0.prURL == root.prURL ? $0 : nil }
    }
    private var content: ReviewGuideVersionContent? { reader?.content }
    private var artifact: CommentArtifactRef? {
        content.map { .reviewGuide(seriesID: $0.seriesId, versionID: $0.id) }
    }

    var body: some View {
        GeometryReader { geometry in
            VStack(alignment: .leading, spacing: 10) {
                actions
                if geometry.size.width >= 780 {
                    HStack(alignment: .top, spacing: 12) {
                        guide.frame(maxWidth: .infinity, maxHeight: .infinity)
                        diffPlaceholder.frame(width: 220)
                    }
                } else {
                    diffPlaceholder
                    guide
                }
            }
        }
        .environmentObject(model)
        .environment(\.commentBackend, model.commentBridge)
        .environment(\.commentsReadOnly, context.isCommentsReadOnly)
        .environment(\.commentsActive, isActive)
        .environment(\.openOriginalGuide, { versionID in
            model.loadNavigatorReviewGuide(for: root, versionID: versionID)
        })
        .task(id: [root.id, root.prURL ?? "", root.reviewGuideReadableVersionId ?? "", String(isActive)]) {
            guard isActive else {
                model.reviewGuideSurfaceClosed(rootTaskID: root.id)
                return
            }
            // One visit to the tab is one open, however often these inputs change.
            let opened = model.reviewGuideSurfaceOpened(root: root)
            model.loadNavigatorReviewGuide(for: root, onOpen: opened)
        }
        .task(id: "\(root.id)|\(isActive)") {
            guard isActive else { return }
            if model.attachmentsByTaskID[root.id] == nil { model.loadAttachments(taskId: root.id) }
        }
        .accessibilityIdentifier("navigator-review")
    }

    private var actions: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(Self.gateExplanation(root)).font(.callout).foregroundStyle(.secondary)
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 12) { actionButtons }
                VStack(alignment: .leading, spacing: 8) { actionButtons }
            }
        }
    }

    @ViewBuilder
    private var actionButtons: some View {
        if let versionID = reader?.versionID {
            // The visit to this tab was already counted as the open.
            Button("Open guide in window") { model.openReviewGuide(versionId: versionID, rootTaskId: root.id, onOpen: false) }
        }
        if root.boardColumn == .review || root.boardColumn == .done {
            Button("Open review terminal") { model.openReviewTerminal(for: root) }
                .disabled(context.isReadOnly || model.openingReviewTerminalIDs.contains(root.id))
        }
        if let title = root.generateReviewGuideMenuTitle {
            Button(title) { model.generateReviewGuide(for: root) }
                .disabled(actionsDisabled || model.retryingReviewGuideRootTaskIDs.contains(root.id)
                    || ["queued", "generating"].contains(root.reviewGuideLifecycle ?? ""))
        }
    }

    private var guide: some View {
        VStack(spacing: 0) {
            // Cap the metadata area so attachments and findings cannot crowd
            // the markdown out of a short or narrow detail pane.
            ScrollView {
                ReviewGuideViewerHeader(
                    chatModel: model, rootTaskId: root.id, generatedAt: content?.generatedAt,
                    inlineGuide: InlineReviewGuideHeader(
                        versionID: reader?.versionID, comparisonID: content?.comparisonId,
                        artifact: artifact,
                        openVersion: { model.loadNavigatorReviewGuide(for: root, versionID: $0) }
                    ),
                    isReadOnly: context.isReadOnly, isActive: isActive
                )
                if let reader {
                    HStack {
                        Text("Version: \(reader.versionID)").font(.caption2.monospaced()).textSelection(.enabled)
                        if reader.versions.count > 1 {
                            Menu("Viewed versions") {
                                ForEach(reader.versions, id: \.id) { version in
                                    Button("\(version.generatedAt) · \(version.id)") {
                                        model.loadNavigatorReviewGuide(for: root, versionID: version.id)
                                    }
                                }
                            }
                        }
                    }
                    .padding(.horizontal, 16)
                }
            }
            .frame(maxHeight: 210)
            guideBody
        }
    }

    @ViewBuilder
    private var guideBody: some View {
        switch reader?.state {
        case .loaded(let content):
            MarkdownViewerView(title: "Review guide", source: content.markdown, artifact: artifact)
                .id(content.id)
        case .loading:
            ProgressView("Loading review guide…").frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed(let message):
            VStack(spacing: 10) {
                Text(message).foregroundStyle(.secondary)
                Button("Retry loading") { model.loadNavigatorReviewGuide(for: root, refresh: true) }
                    .disabled(actionsDisabled)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case nil:
            VStack(spacing: 10) {
                if ["queued", "generating"].contains(root.reviewGuideLifecycle ?? "") {
                    ProgressView("Generating review guide…")
                } else if root.reviewGuideLifecycle != "failed" {
                    Text("No review guide has been generated.").foregroundStyle(.secondary)
                }
                Text("You can review the pull request on GitHub.").font(.caption).foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private var diffPlaceholder: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Diff preview", systemImage: "doc.text.magnifyingglass").font(.headline)
            Text("Not available in this view.").font(.callout).foregroundStyle(.secondary)
            if let url = Self.diffURL(root.prURL) {
                Link("Open PR diff on GitHub", destination: url)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.quaternary.opacity(0.3), in: RoundedRectangle(cornerRadius: 6))
        .accessibilityIdentifier("navigator-diff-placeholder")
    }

    static func diffURL(_ prURL: String?) -> URL? {
        guard let prURL, let (owner, repo, number) = parseGitHubPRURL(prURL) else {
            return prURL.flatMap(URL.init(string:))
        }
        return URL(string: "https://github.com/\(owner)/\(repo)/pull/\(number)/files")
    }

    static func gateExplanation(_ root: WorkTask) -> String {
        if root.navigator?.root?.mergeEvidence != nil {
            return "Merged — review guide and evidence are retained for this completed attempt."
        }
        let reason = root.navigator?.root?.reasonLabel ?? root.status.replacingOccurrences(of: "_", with: " ")
        if let queue = root.mergeQueueState {
            return "\(reason) · Merge queue: \(queue). Waiting for the engine to observe the merge."
        }
        if root.isMergeWhenReadyEligible {
            return "\(reason). Merge when ready checks the current PR gates and asks for confirmation if revisions remain open. Guide freshness does not grant approval."
        }
        return "\(reason). Merge is not currently available; the engine updates readiness as work and checks progress."
    }
}

struct NavigatorReviewHistory: View {
    let model: ChatViewModel
    let root: WorkTask
    @ObservedObject private var guides: NavigatorReviewGuideStore

    init(model: ChatViewModel, root: WorkTask) {
        self.model = model
        self.root = root
        guides = model.navigatorReviewGuides
    }

    var body: some View {
        if root.prURL != nil {
            VStack(alignment: .leading, spacing: 6) {
                Text("Review guides").font(.headline)
                if let version = root.reviewGuideReadableVersionId {
                    Button("Current readable guide") { open(version) }
                }
                if !viewedVersions.isEmpty {
                    ForEach(viewedVersions, id: \.id) { version in
                        Button("Viewed \(version.generatedAt) · \(version.id)") { open(version.id) }
                    }
                }
            }
        }
    }

    var viewedVersions: [ReviewGuideVersionContent] {
        guard let reader = guides.readers[root.id], reader.prURL == root.prURL else { return [] }
        return reader.versions
    }

    func open(_ versionID: String) {
        model.loadNavigatorReviewGuide(for: root, versionID: versionID)
        model.navigator.tab = .review
    }
}
