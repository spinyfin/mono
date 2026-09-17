import SwiftUI

/// Header bar shown above the document body in the async markdown viewer
/// when it is displaying a PR review guide. Reads the live `WorkTask`
/// (not a snapshot captured when the guide opened) so the merge control and
/// currentness banner always reflect the PR's current state — design:
/// "It derives availability from the live root task and the same board
/// eligibility projection, not a snapshot captured when the guide opened."
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
                    if task.boardColumn == .review {
                        MergeWhenReadyControl(onConfirm: { chatModel.mergeWhenReady(for: task) })
                    }
                }
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    if let (owner, repo, number) = Self.parsePR(task.prURL) {
                        if let url = task.prURL.flatMap(URL.init(string:)) {
                            Link("\(owner)/\(repo) #\(number)", destination: url)
                                .font(.caption)
                        } else {
                            Text("\(owner)/\(repo) #\(number)")
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
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .background(Color.secondary.opacity(0.06))
            Divider()
        }
    }

    @ViewBuilder
    private func currentnessBanner(for task: WorkTask) -> some View {
        // A refresh that completed WHILE this version stayed open: the card
        // already points at the new version, but "an already-open viewer
        // stays pinned until the user switches" (design) — offer, don't
        // force, the update.
        if task.reviewGuideLifecycle == "ready",
           let newerVersionId = task.reviewGuideReadableVersionId,
           newerVersionId != chatModel.pendingReviewGuideVersionId {
            HStack(spacing: 8) {
                Text("A newer explanation is available.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Button("Open updated guide") { chatModel.openReviewGuide(for: task) }
                    .controlSize(.small)
            }
        } else {
            let presentation = ReviewGuideCardPresentation.from(
                lifecycle: task.reviewGuideLifecycle,
                readableVersionId: task.reviewGuideReadableVersionId
            )
            switch presentation?.kind {
            case .refreshing:
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Updating explanation\u{2026}")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            case .refreshFailed:
                HStack(spacing: 8) {
                    Image(systemName: "exclamationmark.triangle")
                        .foregroundStyle(.orange)
                    Text("Explanation refresh failed \u{2014} this guide covers an older revision.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Button("Retry") { chatModel.retryReviewGuide(for: task) }
                        .controlSize(.small)
                }
            default:
                EmptyView()
            }
        }
    }

    /// Parse `https://github.com/{owner}/{repo}/pull/{number}` into its
    /// three components. Returns `nil` for any other shape (non-GitHub
    /// remote, malformed URL) rather than guessing.
    private static func parsePR(_ prURL: String?) -> (owner: String, repo: String, number: String)? {
        guard let prURL, let url = URL(string: prURL) else { return nil }
        let parts = url.pathComponents.filter { $0 != "/" }
        guard parts.count >= 4, parts[2] == "pull" else { return nil }
        return (owner: parts[0], repo: parts[1], number: parts[3])
    }
}
