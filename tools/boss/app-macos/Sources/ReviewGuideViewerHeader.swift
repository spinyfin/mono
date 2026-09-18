import SwiftUI

/// Header bar shown above the document body in the async markdown viewer
/// when it is displaying a PR review guide. Reads the live `WorkTask`
/// (not a snapshot captured when the guide opened) so the merge control and
/// currentness banner always reflect the PR's current state — design:
/// "It derives availability from the live root task and the same board
/// eligibility projection, not a snapshot captured when the guide opened."
/// That projection is `WorkTask.isMergeWhenReadyEligible` (Models.swift),
/// the same one `ChatViewModel+BoardHelpers.swift` uses to gate the card's
/// control — a task merely routed into the Review column while `blocked`
/// (conflict/CI-failure resolution in progress, `isReviewPhaseBlocked`)
/// must not show the button here either.
///
/// Deliberately quiet: a guide is "a generated explanation", never an
/// approval. No checkmark, no green success wording, no merge gate tied to
/// guide freshness — a stale/refreshing/failed guide banner is informational
/// only and never disables the merge control below it.
struct ReviewGuideViewerHeader: View {
    @ObservedObject var chatModel: ChatViewModel
    let rootTaskId: String
    /// The open version's generation timestamp (RFC 3339), or `nil` while
    /// still loading.
    let generatedAt: String?

    var body: some View {
        if let task = chatModel.task(withID: rootTaskId) {
            VStack(alignment: .leading, spacing: 6) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text("Review guide \u{2014} generated explanation")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer(minLength: 8)
                    if task.isMergeWhenReadyEligible {
                        MergeWhenReadyControl(onConfirm: { chatModel.mergeWhenReady(for: task) })
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
                currentnessBanner(for: task)
                mergeFeedbackRow(for: task)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .background(Color.secondary.opacity(0.06))
            Divider()
        }
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
            displayedVersionId: chatModel.pendingReviewGuideVersionId,
            displayedComparisonId: chatModel.asyncMarkdownViewerVM.reviewGuideComparisonId
        )
        if currentness.showsOpenUpdatedGuide {
            HStack(spacing: 8) {
                Text("A newer explanation is available.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Button("Open updated guide") { chatModel.openReviewGuide(for: task) }
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
        case .refreshFailed(let displayedStaleSource):
            HStack(spacing: 8) {
                Image(systemName: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
                Text(
                    displayedStaleSource
                        ? "Explanation refresh failed \u{2014} this guide covers an older revision."
                        : "Explanation refresh failed."
                )
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Button("Retry") { chatModel.retryReviewGuide(for: task) }
                    .controlSize(.small)
            }
        case .none:
            EmptyView()
        }
    }
}
