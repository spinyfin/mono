import SwiftUI
import Textual

/// Fixed-width (280 pt) right-side panel listing the comments for the
/// currently open markdown doc (engine-backed and persisted when the doc has
/// an artifact identity, otherwise in-memory). Appears whenever the layer is
/// engine-backed, or once at least one comment exists. Clicking a row jumps
/// to its anchored text (flashes the highlighted span orange for ~900 ms).
/// Dismiss button is at the top-right of each card.
struct CommentSidebar: View {
    @ObservedObject var layer: CommentLayer
    /// Collapses the sidebar back to the rail. `nil` hides the collapse control
    /// (e.g. hosting tests that render the sidebar in isolation).
    var onCollapse: (() -> Void)? = nil

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            if layer.bannerState.revisable || layer.bannerState.inRevisionCount > 0 {
                ReviseBanner(layer: layer)
                Divider()
            }
            if let message = layer.reviseDocMessage {
                Text(message)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 6)
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
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text("Comments")
                    .font(.callout.weight(.semibold))
                Spacer()
                Text("\(layer.comments.count)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
                if let onCollapse {
                    Button(action: onCollapse) {
                        Image(systemName: "chevron.right")
                            .font(.caption.weight(.semibold))
                    }
                    .buttonStyle(.plain)
                    .help("Collapse comments")
                    .accessibilityIdentifier("comment-sidebar-collapse")
                }
            }
            // Soft-dismiss "show resolved" toggle (P529 Phase 2). Only meaningful
            // on an engine-backed viewer, where resolved comments are retained.
            if layer.isEngineBacked {
                Toggle("Show resolved", isOn: $layer.showResolved)
                    .toggleStyle(.checkbox)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
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

/// Renders a comment's `comment_thread_entries` inline, oldest first.
/// `nudge` (bucket 1&3) and `answer` (bucket 2, engine-authored) share a
/// neutral background; `operator_followup` gets an accent tint so the
/// back-and-forth of a bucket-2 conversation reads like a thread rather than
/// a flat list.
private struct ThreadEntriesView: View {
    let entries: [CommentThreadEntry]

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(entries) { entry in
                HStack(alignment: .top, spacing: 4) {
                    Image(systemName: symbolName(for: entry.entryKind))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                    StructuredText(markdown: entry.body)
                        .bossMarkdown()
                        // Scale the shared 17pt-body-relative theme down to
                        // match this view's previous `.caption` (12pt) size.
                        .textual.fontScale(12.0 / 17.0)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.horizontal, 8)
                .padding(.vertical, 4)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(
                    RoundedRectangle(cornerRadius: 4)
                        .fill(backgroundColor(for: entry.entryKind))
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

    private func backgroundColor(for kind: ThreadEntryKind) -> Color {
        switch kind {
        case .nudge, .answer: return Color.secondary.opacity(0.08)
        case .operatorFollowup: return Color.accentColor.opacity(0.1)
        }
    }
}

/// The bucket-2 "other party is typing" indicator, shown under a comment
/// while its answer agent run is `.answering` (design § "Bucket 2" —
/// "Thinking indicator"). Pulses via opacity rather than a canned SF Symbol
/// content-transition effect so it doesn't require a newer macOS deployment
/// target.
private struct ThinkingIndicatorView: View {
    @State private var isPulsing = false

    var body: some View {
        Label("Thinking…", systemImage: "ellipsis.bubble")
            .font(.caption2)
            .foregroundStyle(.secondary)
            .opacity(isPulsing ? 0.35 : 1.0)
            .onAppear {
                withAnimation(.easeInOut(duration: 0.8).repeatForever(autoreverses: true)) {
                    isPulsing = true
                }
            }
    }
}

/// Shown when a comment's bucket-2 answer-agent spawn never made it to
/// `running` (or the run itself errored out) — `comment.answerAgentFailed`.
/// Distinguishes a genuine terminal failure from [`ThinkingIndicatorView`]'s
/// still-in-flight pulse, so a spawn skip (e.g. an unresolvable doc-owner
/// repo) reads as "this stopped" rather than an indicator that silently
/// runs forever with nothing behind it.
private struct AnswerFailedIndicatorView: View {
    var body: some View {
        Label("Couldn't answer", systemImage: "exclamationmark.bubble")
            .font(.caption2)
            .foregroundStyle(.orange)
            .help("The answer agent failed to start or run for this comment. Edit the comment or retry later.")
    }
}

/// The reply box under an `answered` bucket-2 thread — the UI half of the
/// engine's `CommentsPostFollowup` RPC (design § "Follow-up loop").
private struct FollowupComposer: View {
    let comment: Comment
    @ObservedObject var layer: CommentLayer
    @State private var text: String = ""

    var body: some View {
        HStack(spacing: 6) {
            TextField("Reply…", text: $text)
                .textFieldStyle(.roundedBorder)
                .font(.caption)
                .onSubmit(send)
            Button(action: send) {
                Image(systemName: "arrow.up.circle.fill")
            }
            .buttonStyle(.plain)
            .disabled(text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
    }

    private func send() {
        guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        layer.postFollowup(body: text, for: comment)
        text = ""
    }
}

/// Shown while a posted follow-up awaits the engine's async reclassifier
/// (design § "Follow-up loop"): the outcome — loop back into bucket 2
/// (`.answering`) or bridge onto the bucket-1&3 track (`.active`) — arrives
/// on the artifact's comment-topic push and reloads the layer; there is no
/// operator action here.
private struct FollowupClassifyingIndicator: View {
    var body: some View {
        Label("Reclassifying…", systemImage: "ellipsis.circle")
            .font(.caption2)
            .foregroundStyle(.tertiary)
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

/// The anchor-resolution glyph on a comment row: a ⚠ when the engine could only
/// re-anchor the comment via fuzzy match (so the highlight may sit on slightly
/// shifted text), or an "anchor lost" badge when the anchor could not be
/// resolved at all (the comment paints no doc highlight). Nothing when the anchor
/// resolved exactly. Mirrors `work_comments.last_resolved_with` (design §
/// "Re-anchoring on load").
private struct AnchorStatusBadge: View {
    let comment: Comment

    var body: some View {
        if comment.isOrphaned {
            Label("anchor lost", systemImage: "mappin.slash")
                .font(.caption2)
                .foregroundStyle(.orange)
                .help("This comment's anchor could not be found in the current document.")
        } else if comment.isFuzzyAnchored {
            Label("fuzzy", systemImage: "exclamationmark.triangle")
                .font(.caption2)
                .foregroundStyle(.yellow)
                .help("Re-anchored by fuzzy match — the document changed near this comment; double-check the highlighted text.")
        }
    }
}

/// The small grey elapsed-time chip in a comment row's footer. Coarse
/// ("2 minutes ago") rather than SwiftUI's built-in `Text(_:style:.relative)`,
/// which ticks at ~1s granularity and reads as ongoing activity even for a
/// long-`resolved` comment.
///
/// Static everywhere except `isLiveAnswering`, the one state where elapsed
/// time is genuinely informative (a live answer agent is working). There it
/// refreshes on a slow cadence — mirrors `WorkerWaitingIndicator`'s
/// `TimelineView(.periodic(from:by:))` — rather than SwiftUI's 1s tick.
private struct CommentAgeChip: View {
    let comment: Comment
    let isLiveAnswering: Bool

    var body: some View {
        if isLiveAnswering {
            TimelineView(.periodic(from: .now, by: 30)) { context in
                chipText(now: context.date)
            }
        } else {
            chipText(now: Date())
        }
    }

    private func chipText(now: Date) -> some View {
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .full
        return Text(formatter.localizedString(for: comment.createdAt, relativeTo: now))
            .font(.caption2)
            .foregroundStyle(.tertiary)
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

    /// The bucket-2 thread-level indicators (design § "Comment/thread state
    /// machine"): a thinking indicator while the answer agent actually runs
    /// (`answer_agent_running`, not just the `.answering` status — see
    /// `Comment.answerAgentRunning`), an "Answered" checkmark + follow-up
    /// composer once it's replied, or a passive "reclassifying" indicator
    /// while a just-posted follow-up awaits the engine's async reclassifier.
    /// `resolved`/`inRevision`/`orphaned`/`dismissed` render nothing here —
    /// that track has its own `RevisionChip` instead.
    ///
    /// `answerAgentFailed` (`Comment.answerAgentFailed`) takes priority over
    /// each status's default rendering in `.answering`/`.awaitingFollowup`/
    /// `.active`: those are exactly the statuses a failed spawn can leave a
    /// `question`-classified comment sitting in (see
    /// `record_answer_agent_spawn_failure` in the engine), and without this
    /// check they'd render either nothing or a perpetual in-progress
    /// indicator for a run that already gave up.
    @ViewBuilder
    private var bucketTwoTrack: some View {
        switch comment.status {
        case .answering:
            if comment.answerAgentFailed {
                AnswerFailedIndicatorView()
            } else if comment.answerAgentRunning {
                ThinkingIndicatorView()
            }
        case .answered:
            VStack(alignment: .leading, spacing: 4) {
                Label("Answered", systemImage: "checkmark.circle")
                    .font(.caption2)
                    .foregroundStyle(.green)
                FollowupComposer(comment: comment, layer: layer)
            }
        case .awaitingFollowup:
            if comment.answerAgentFailed {
                AnswerFailedIndicatorView()
            } else {
                FollowupClassifyingIndicator()
            }
        case .active:
            if comment.answerAgentFailed {
                AnswerFailedIndicatorView()
            }
        case .resolved, .inRevision, .orphaned, .dismissed:
            EmptyView()
        }
    }

    /// Whether this comment has a genuinely live answer agent running — the
    /// one case where the age chip's elapsed time is still informative.
    /// Mirrors the `bucketTwoTrack` `.answering` predicate above.
    private var isLiveAnswering: Bool {
        comment.status == .answering && !comment.answerAgentFailed && comment.answerAgentRunning
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

                StructuredText(markdown: comment.body)
                    .bossMarkdown()
                    // Scale the shared 17pt-body-relative theme down to match
                    // this row's previous `.callout` (13pt) size, so replies
                    // don't jump in size inside the narrow sidebar bubble.
                    .textual.fontScale(13.0 / 17.0)
                    .textual.textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)

                if !comment.threadEntries.isEmpty {
                    ThreadEntriesView(entries: comment.threadEntries)
                }

                bucketTwoTrack

                HStack(spacing: 8) {
                    IntentBadge(comment: comment, layer: layer)
                    if let chipState = comment.revisionChipState {
                        RevisionChip(state: chipState)
                    }
                    AnchorStatusBadge(comment: comment)
                    CommentAgeChip(comment: comment, isLiveAnswering: isLiveAnswering)
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
