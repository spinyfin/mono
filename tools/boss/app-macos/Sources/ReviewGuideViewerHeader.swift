import SwiftUI
import Textual

/// Header bar shown above the document body in the async markdown viewer
/// when it is displaying a PR review guide. Reads the live `WorkTask`
/// (not a snapshot captured when the guide opened) so the merge control and
/// currentness banner always reflect the PR's current state — design:
/// "It derives availability from the live root task and the same board
/// eligibility projection, not a snapshot captured when the guide opened."
/// That projection is `WorkTask.isMergeWhenReadyEligible` (Models.swift),
/// the same one `ChatViewModel+BoardHelpers.swift` uses to gate the card's
/// control — a task merely routed into the Review column while `blocked`
/// on a non-CI review-phase reason (`merge_conflict`, `review_feedback`)
/// must not show the button here either. CI-blocked Review cards
/// (`ci_failure` / `ci_failure_exhausted`) are eligible: Merge rebases a
/// behind-base branch.
///
/// Deliberately quiet: a guide is "a generated explanation", never an
/// approval. No checkmark, no green success wording, no merge gate tied to
/// guide freshness — a stale/refreshing/failed guide banner is informational
/// only and never disables the merge control below it.
struct ReviewGuideViewerHeader: View {
    @ObservedObject var chatModel: ChatViewModel
    @ObservedObject private var drafts: GuideCommentDrafts

    /// `findingsInitiallyExpanded` seeds the addendum disclosure. The
    /// default is collapsed so the generated explanation stays on screen;
    /// the screenshot fixture (`BossReviewGuideCapture`) passes `true` to
    /// photograph the expanded list.
    init(
        chatModel: ChatViewModel,
        rootTaskId: String,
        generatedAt: String?,
        findingsInitiallyExpanded: Bool = false,
        inlineGuide: InlineReviewGuideHeader? = nil,
        isReadOnly: Bool = false,
        isActive: Bool = true
    ) {
        self.chatModel = chatModel
        self.rootTaskId = rootTaskId
        self.generatedAt = generatedAt
        self.inlineGuide = inlineGuide
        self.isReadOnly = isReadOnly
        self.isActive = isActive
        self.drafts = chatModel.commentBridge.draftStore
        _findingsExpanded = State(initialValue: findingsInitiallyExpanded)
    }
    let rootTaskId: String
    @State private var findingsExpanded: Bool
    /// The open version's generation timestamp (RFC 3339), or `nil` while
    /// still loading.
    let generatedAt: String?
    let inlineGuide: InlineReviewGuideHeader?
    let isActive: Bool
    let isReadOnly: Bool

    var actionsDisabled: Bool { isReadOnly || !chatModel.isConnected }

    static func showsMergeControl(_ task: WorkTask) -> Bool {
        task.isMergeWhenReadyEligible && task.navigator?.root?.mergeEvidence == nil
    }

    static func sourceLabels(comparisonID: String, selectedID: String?) -> [String] {
        var labels = ["Source comparison: \(comparisonID)"]
        if let selectedID, selectedID != comparisonID {
            labels.append("This guide covers an older source revision.")
        }
        return labels
    }

    private var artifact: CommentArtifactRef? {
        if let inlineGuide { return inlineGuide.artifact }
        if case .loaded(_, _, let artifact) = chatModel.asyncMarkdownViewerVM.state { return artifact }
        return nil
    }

    private func openVersion(_ versionID: String) {
        if let inlineGuide { inlineGuide.openVersion(versionID) }
        else { chatModel.openReviewGuide(versionId: versionID, rootTaskId: rootTaskId, onOpen: false) }
    }

    var body: some View {
        if let task = chatModel.task(withID: rootTaskId) {
            VStack(alignment: .leading, spacing: 6) {
                HStack(alignment: .center, spacing: 8) {
                    Text("Review guide \u{2014} generated explanation")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer(minLength: 8)
                    if Self.showsMergeControl(task) {
                        MergeWhenReadyControl(
                            isRequesting: chatModel.mergingWhenReadyIDs.contains(task.id),
                            style: .regular,
                            onConfirm: {
                                chatModel.mergeWhenReady(for: task, origin: inlineGuide == nil ? .reviewGuideViewer : .board)
                            }
                        )
                        .disabled(actionsDisabled)
                    }
                }
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    if let (org, repo, number) = parseGitHubPRURL(task.prURL ?? "") {
                        if let url = task.prURL.flatMap(URL.init(string:)) {
                            Link("\(org)/\(repo) #\(number)", destination: url)
                                .font(.caption)
                        } else {
                            Text("\(org)/\(repo) #\(number)")
                                .font(.caption)
                        }
                    }
                    if let generatedAt {
                        Text("Generated \(AutomationTime.relative(generatedAt, now: Date()))")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                if let artifact {
                    ForEach(drafts.byVersion.keys.sorted(), id: \.self) { versionId in
                        if versionId != artifact.guideVersionId {
                            ForEach(drafts.drafts(for: versionId).filter { $0.seriesId == artifact.id }, id: \.composerId) { draft in
                                Button("Resume draft: " + String(draft.body.prefix(32))) {
                                    drafts.pendingResumeVersionId = versionId
                                    drafts.pendingResumeComposerId = draft.composerId
                                    openVersion(versionId)
                                }
                                .controlSize(.small)
                            }
                        }
                    }
                }
                if let seriesId = displayedSeriesId,
                   let findings = chatModel.reviewGuideFindingsBySeriesID[seriesId] {
                    Text(findings.statusText)
                        .font(.caption)
                        .accessibilityIdentifier("review-guide-findings-status")
                    VStack(alignment: .leading, spacing: 4) {
                        Button {
                            findingsExpanded.toggle()
                        } label: {
                            HStack(spacing: 4) {
                                Image(systemName: "chevron.right")
                                    .font(.caption2.weight(.semibold))
                                    .rotationEffect(.degrees(findingsExpanded ? 90 : 0))
                                    .frame(width: 10)
                                Text("AI review findings addendum")
                                Spacer(minLength: 0)
                            }
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .accessibilityAddTraits(.isHeader)
                        .accessibilityValue(findingsExpanded ? "expanded" : "collapsed")
                        .accessibilityIdentifier("review-guide-findings-toggle")
                        .onHover { hovering in
                            if hovering { NSCursor.pointingHand.push() } else { NSCursor.pop() }
                        }
                        if findingsExpanded {
                            ReviewGuideFindingsAddendumList(markdown: findings.addendumMarkdown)
                        }
                    }
                    .font(.caption)
                }
                currentnessBanner(for: task)
                mergeFeedbackRow(for: task)
                ReviewGuideAttachments(chatModel: chatModel, rootTaskId: rootTaskId)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .background(Color.secondary.opacity(0.06))
            .task(id: "\(rootTaskId)|\(displayedSeriesId ?? "")|\(isActive)") {
                // Skip until the guide on screen has a series id, then
                // re-poll immediately when that series changes (same root,
                // replacement PR, or the artifact id arriving after load).
                guard isActive, let seriesId = displayedSeriesId else { return }
                while !Task.isCancelled {
                    chatModel.engine.sendGetReviewGuideSummary(rootTaskId: rootTaskId, seriesId: seriesId)
                    do { try await Task.sleep(for: .seconds(5)) }
                    catch { return }
                }
            }
            Divider()
        }
    }

    /// The PR series id of the guide actually on screen, or `nil` while
    /// still loading or if the shared viewer is showing something else
    /// entirely (a design doc, a task description). Findings polling must
    /// scope to this, not to `rootTaskId` alone, so a replacement PR on the
    /// same root cannot swap findings under a still-open older guide.
    private var displayedSeriesId: String? {
        guard artifact?.kind == WireArtifactKind.reviewGuide else { return nil }
        return artifact?.id
    }

    /// Surfaces the same accept/error feedback the board card shows for a
    /// merge action, scoped to this viewer's root task — the merge action
    /// fired from here has otherwise been invisible to a reader who is not
    /// also watching the board window (`mergeFeedbackNotice` renders only on
    /// `WorkBoardCard`, and merge failures land only in `ContentView`'s
    /// modal alert, which is never presented over this separate viewer
    /// window).
    @ViewBuilder
    private func mergeFeedbackRow(for task: WorkTask) -> some View {
        if let notice = chatModel.mergeFeedbackNotice, notice.taskID == task.id {
            WorkMergeFeedbackBanner(message: notice.message) {
                chatModel.clearMergeFeedback()
            }
        } else if let errorMessage = chatModel.mergeErrorNoticesByTaskID[task.id] {
            HStack(spacing: 8) {
                Image(systemName: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
                Text(errorMessage)
                    .font(.caption)
                    .foregroundStyle(.primary)
                Spacer(minLength: 4)
                Button(action: { chatModel.clearMergeError(for: task.id) }) {
                    Image(systemName: "xmark.circle.fill")
                        .foregroundStyle(.secondary)
                        .font(.caption)
                }
                .buttonStyle(.plain)
                .help("Dismiss")
                .accessibilityLabel("Dismiss merge error")
            }
        }
    }

    @ViewBuilder
    private func currentnessBanner(for task: WorkTask) -> some View {
        // Derive currentness from the version on screen, not from the
        // series' current readable version. The viewer stays pinned until
        // the user switches, so `task.reviewGuideStaleSource` (computed for
        // the readable pointer) can disagree with what this window shows.
        let currentness = ReviewGuideViewerCurrentness.from(
            lifecycle: task.reviewGuideLifecycle,
            readableVersionId: task.reviewGuideReadableVersionId,
            selectedComparisonId: task.reviewGuideSelectedComparisonId,
            displayedVersionId: inlineGuide != nil ? inlineGuide?.versionID : chatModel.pendingReviewGuideVersionId,
            displayedComparisonId: inlineGuide != nil ? inlineGuide?.comparisonID : chatModel.asyncMarkdownViewerVM.reviewGuideComparisonId,
            error: task.reviewGuideError
        )
        let comparisonID = inlineGuide != nil ? inlineGuide?.comparisonID : chatModel.asyncMarkdownViewerVM.reviewGuideComparisonId
        if let comparisonID {
            Text(Self.sourceLabels(comparisonID: comparisonID, selectedID: task.reviewGuideSelectedComparisonId)[0])
                .font(.caption2.monospaced()).textSelection(.enabled)
            if let stale = Self.sourceLabels(comparisonID: comparisonID, selectedID: task.reviewGuideSelectedComparisonId).dropFirst().first {
                Label(stale, systemImage: "clock")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        if currentness.showsOpenUpdatedGuide {
            HStack(spacing: 8) {
                Text("A newer explanation is available.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Button("Open updated guide") {
                    if let versionID = task.reviewGuideReadableVersionId { openVersion(versionID) }
                }
                    .controlSize(.small)
            }
        }
        switch currentness.status {
        case .refreshing:
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text("Updating explanation\u{2026}")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        case .refreshFailed(let displayedStaleSource, let error):
            failedGuideBanner(
                for: task,
                headline: displayedStaleSource
                    ? "Explanation refresh failed \u{2014} this guide covers an older revision."
                    : "Explanation refresh failed.",
                error: error
            )
        case .failed(let error):
            failedGuideBanner(for: task, headline: "Review guide failed to generate.", error: error)
        case .none:
            EmptyView()
        }
    }

    @ViewBuilder
    private func failedGuideBanner(for task: WorkTask, headline: String, error: String?) -> some View {
        let summary = ReviewGuideCardPresentation.summaryLine(of: error)
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.triangle")
                .foregroundStyle(.orange)
            VStack(alignment: .leading, spacing: 2) {
                Text(headline)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if let summary {
                    Text(summary)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
            }
            Button("Retry") { chatModel.retryReviewGuide(for: task) }
                .controlSize(.small)
                .disabled(actionsDisabled || chatModel.retryingReviewGuideRootTaskIDs.contains(task.id))
        }
        .help(error ?? headline)
    }
}

/// The expanded addendum list: as tall as its content, scrolling only once
/// that passes `maxHeight`.
///
/// A bare `ScrollView` takes every point its parent offers, so capping it
/// with `.frame(maxHeight:)` alone makes the cap a fixed height. The list
/// instead measures its content and offers the scroll view
/// `min(content, maxHeight)`: short lists fit, long ones scroll. Collapsing
/// or re-expanding recreates this view, so the measurement never carries
/// over from an earlier, larger list.
struct ReviewGuideFindingsAddendumList: View {
    let markdown: String
    /// Longest the list gets before it scrolls.
    static let maxHeight: CGFloat = 200
    @State private var contentHeight: CGFloat?

    var body: some View {
        ScrollView {
            StructuredText(markdown: markdown)
                .bossMarkdown()
                .textual.textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                // The content lays out at its natural height inside the
                // scroll view whatever height the scroll view itself has,
                // and that height depends only on the width the parent
                // sets, so feeding it back into the frame cannot oscillate.
                .onGeometryChange(for: CGFloat.self, of: \.size.height) { height in
                    contentHeight = height
                }
        }
        .frame(maxHeight: min(contentHeight ?? 0, Self.maxHeight))
    }
}

/// Display identity and navigation belong to the reader that owns this header.
struct InlineReviewGuideHeader {
    let versionID: String?
    let comparisonID: String?
    let artifact: CommentArtifactRef?
    let openVersion: (String) -> Void
}
