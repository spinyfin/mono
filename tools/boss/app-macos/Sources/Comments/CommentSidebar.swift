import SwiftUI

/// Fixed-width (280 pt) right-side panel listing the in-memory comments for
/// the currently open markdown doc. Appears only when at least one comment
/// exists. Clicking a row jumps to its anchored text (flashes the highlighted
/// span orange for ~900 ms). Dismiss button is at the top-right of each card.
struct CommentSidebar: View {
    @ObservedObject var layer: CommentLayer

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            if layer.bannerState.revisable || layer.bannerState.inRevisionCount > 0 {
                ReviseBanner(layer: layer)
                Divider()
            }
            if layer.comments.isEmpty {
                Spacer()
                Text("No comments yet.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Spacer()
            } else {
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(layer.comments) { comment in
                            CommentRow(comment: comment, layer: layer)
                            Divider()
                        }
                    }
                }
            }
            addCommentRow
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text("Comments")
                    .font(.callout.weight(.semibold))
                Spacer()
                Text("\(layer.comments.count)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
            Text("Comments not yet persisted — Phase 1 preview")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    private var addCommentRow: some View {
        Button {
            layer.requestNewComment()
        } label: {
            Label("Add Comment", systemImage: "plus.bubble")
                .font(.callout)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .buttonStyle(.borderless)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

/// The `[Revise]` banner: `"{n} unresolved comment(s). [Revise]"`, plus an
/// `in_revision` count when a batch is already in flight. Shown whenever
/// there's something to revise or something already revising — design §
/// "Buckets 1 & 3 — unified" / "2d. Banner state on the comment read path".
private struct ReviseBanner: View {
    @ObservedObject var layer: CommentLayer

    var body: some View {
        let state = layer.bannerState
        VStack(alignment: .leading, spacing: 4) {
            if state.revisable {
                HStack(spacing: 8) {
                    Text(unresolvedText(state.unresolvedCount))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                    Button("Revise") {
                        layer.reviseDoc()
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                }
            }
            if state.inRevisionCount > 0 {
                Label(inRevisionText(state.inRevisionCount), systemImage: "arrow.triangle.2.circlepath")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private func unresolvedText(_ count: Int) -> String {
        count == 1 ? "1 unresolved comment." : "\(count) unresolved comments."
    }

    private func inRevisionText(_ count: Int) -> String {
        count == 1 ? "1 comment in revision" : "\(count) comments in revision"
    }
}

/// The `[Revise]`-track state chip on a single comment row — the "current
/// state" counterpart to the thread entries' "conversational trace of why"
/// (design § "Reply/link mechanics").
private struct RevisionChip: View {
    let state: RevisionChipState

    var body: some View {
        Label(text, systemImage: symbolName)
            .font(.caption2)
            .foregroundStyle(color)
            .help(help)
    }

    private var text: String {
        switch state {
        case .nudged: return "Nudge sent"
        case .inRevision: return "In revision"
        case .resolved: return "Resolved"
        case .reopened: return "Reopened"
        }
    }

    private var symbolName: String {
        switch state {
        case .nudged: return "bell.badge"
        case .inRevision: return "arrow.triangle.2.circlepath"
        case .resolved: return "checkmark.circle"
        case .reopened: return "arrow.uturn.backward.circle"
        }
    }

    private var color: Color {
        switch state {
        case .nudged: return .orange
        case .inRevision: return .blue
        case .resolved: return .green
        case .reopened: return .red
        }
    }

    private var help: String {
        switch state {
        case .nudged:
            return "Classified as wanting a doc change — click [Revise] to start one"
        case .inRevision(let taskId):
            return "Addressed by \(taskId)"
        case .resolved(let taskId):
            return "Resolved by \(taskId)"
        case .reopened:
            return "The task addressing this comment was abandoned — back on the [Revise] banner"
        }
    }
}

/// Renders a comment's `comment_thread_entries` inline, oldest first —
/// today this is just the nudge (bucket 1&3); `answer`/`operator_followup`
/// entries (bucket 2) render generically once those phases land.
private struct ThreadEntriesView: View {
    let entries: [CommentThreadEntry]

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(entries) { entry in
                HStack(alignment: .top, spacing: 4) {
                    Image(systemName: symbolName(for: entry.entryKind))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                    Text(entry.body)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.horizontal, 8)
                .padding(.vertical, 4)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(
                    RoundedRectangle(cornerRadius: 4)
                        .fill(Color.secondary.opacity(0.08))
                )
            }
        }
    }

    private func symbolName(for kind: ThreadEntryKind) -> String {
        switch kind {
        case .nudge: return "bell"
        case .answer: return "text.bubble"
        case .operatorFollowup: return "arrowshape.turn.up.left"
        }
    }
}

/// Classification badge for a single comment: shows "classifying…" while
/// `comment.intent` is nil, or the classified intent's icon + label once set.
/// Always clickable — opens a menu the operator uses to manually (re)classify
/// the comment, mirroring the engine's `CommentsSetIntent` override RPC.
private struct IntentBadge: View {
    let comment: Comment
    @ObservedObject var layer: CommentLayer

    var body: some View {
        Menu {
            ForEach(CommentIntent.allCases, id: \.self) { intent in
                Button {
                    layer.setIntent(intent, for: comment)
                } label: {
                    if comment.intent == intent {
                        Label(intent.displayName, systemImage: "checkmark")
                    } else {
                        Text(intent.displayName)
                    }
                }
            }
        } label: {
            badgeLabel
        }
        .menuStyle(.borderlessButton)
        .fixedSize()
        .help(comment.intentOverriddenByUser
            ? "Manually classified as \(comment.intent?.displayName ?? "?") — click to reclassify"
            : "Click to classify this comment")
    }

    @ViewBuilder
    private var badgeLabel: some View {
        if let intent = comment.intent {
            Label(intent.displayName, systemImage: intent.symbolName)
                .font(.caption2)
                .foregroundStyle(.secondary)
        } else {
            Label("classifying…", systemImage: "ellipsis.circle")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        }
    }
}

private struct CommentRow: View {
    let comment: Comment
    @ObservedObject var layer: CommentLayer

    var body: some View {
        // Entire row is tappable to jump to the comment's anchored span.
        Button {
            layer.jumpTo(comment)
        } label: {
            rowContent
        }
        .buttonStyle(.plain)
    }

    private var rowContent: some View {
        ZStack(alignment: .topTrailing) {
            VStack(alignment: .leading, spacing: 8) {
                // Leave room for the dismiss button in the top-right.
                Color.clear.frame(height: 0)
                    .padding(.trailing, 28)

                if !comment.quotedText.isEmpty {
                    Text(comment.quotedText)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(3)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 4)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(
                            RoundedRectangle(cornerRadius: 4)
                                .fill(Color.accentColor.opacity(0.08))
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 4)
                                .stroke(Color.accentColor.opacity(0.25), lineWidth: 0.5)
                        )
                }

                Text(comment.body)
                    .font(.callout)
                    .fixedSize(horizontal: false, vertical: true)

                if !comment.threadEntries.isEmpty {
                    ThreadEntriesView(entries: comment.threadEntries)
                }

                HStack(spacing: 8) {
                    IntentBadge(comment: comment, layer: layer)
                    if let chipState = comment.revisionChipState {
                        RevisionChip(state: chipState)
                    }
                    Text(comment.createdAt, style: .relative)
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 10)

            // Action buttons: dismiss, stacked at top-right.
            VStack(spacing: 4) {
                Button {
                    layer.dismiss(comment)
                } label: {
                    Label("Dismiss", systemImage: "xmark.circle")
                        .labelStyle(.iconOnly)
                        .font(.caption)
                }
                .buttonStyle(.borderless)
                .foregroundStyle(.secondary)
                .help("Dismiss comment")
            }
            .padding(.top, 8)
            .padding(.trailing, 8)
        }
    }
}
