import AppKit
import os.log
import SwiftUI
import UpdateCore

struct RepoChipView: View {
    let presentation: RepoChipPresentation
    var emphasized: Bool = false

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "folder")
                .font(.caption2)
            Text(presentation.shortName)
                .font(.caption.weight(.semibold))
                .lineLimit(1)
                .truncationMode(.tail)
        }
        .fixedSize(horizontal: true, vertical: false)
        .foregroundStyle(Color(nsColor: .labelColor))
        .padding(.horizontal, 7)
        .padding(.vertical, 3)
        .background(Color(nsColor: .controlBackgroundColor))
        .clipShape(Capsule())
        .overlay(
            Capsule().strokeBorder(
                Color(nsColor: .separatorColor),
                lineWidth: 0.5
            )
        )
        .help(presentation.tooltip)
        .accessibilityLabel(presentation.accessibilityLabel)
    }
}

/// Upstream-link affordance rendered in the kanban card footer when
/// a work item carries an `externalRef`. Three visual states:
///
/// - **Bound** (`isStale == false`): accent-colored `↗ #N` link, opens the
///   upstream URL in the default browser.
/// - **Stale** (`isStale == true`): secondary-colored with strikethrough,
///   still clickable; tooltip explains the binding was cleared.
/// - **Absent** (`ExternalRefLinkPresentation.forTask` returns `nil`): no
///   view rendered at all (callers gate on nil).

struct ExternalRefLinkView: View {
    let presentation: ExternalRefLinkPresentation

    var body: some View {
        if let url = URL(string: presentation.url), url.scheme != nil {
            Link(destination: url) {
                labelText
            }
            .buttonStyle(.plain)
            .pointerStyle(.link)
            .help(presentation.tooltip)
        } else {
            labelText
                .help(presentation.tooltip)
        }
    }

    private var labelText: some View {
        Text(presentation.label)
            .font(.system(.caption2, design: .monospaced))
            .foregroundStyle(presentation.isStale ? Color.secondary : Color.accentColor)
            .strikethrough(presentation.isStale)
            .accessibilityLabel(presentation.isStale
                ? "Upstream issue (stale): \(presentation.label)"
                : "Open upstream issue: \(presentation.label)")
            .lineLimit(1)
            .fixedSize(horizontal: true, vertical: false)
    }
}

/// "🔧 conflict cleared" PR-card chip. Phase 5 #15 of the merge-
/// conflict design. Rendered on parent cards whose PR was the target
/// of a successful conflict-resolution attempt in the last 24h
/// (the freshness window lives on
/// [[ChatViewModel.badgeFreshnessWindow]]). The tooltip names the
/// action so a glance tells a reader *what* the engine cleared, not
/// just that something happened.

struct ConflictClearedBadge: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "checkmark.circle.fill")
                .font(.caption2.weight(.semibold))
            Text("conflict cleared")
                .font(.caption.weight(.semibold))
                .lineLimit(1)
        }
        .fixedSize(horizontal: true, vertical: false)
        .foregroundStyle(Color.green)
        .help("The engine cleared a merge conflict on this PR within the last 24 hours.")
        .accessibilityLabel("Conflict cleared by the engine")
    }
}

/// "✅ ci auto-fixed" PR-card chip. Phase 11 #37 / design Q11.
/// Parallels [[ConflictClearedBadge]] — green, 24-hour freshness
/// window — for cards whose PR was the target of a successful CI
/// auto-fix attempt.

struct CIAutoFixedBadge: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "checkmark.circle.fill")
                .font(.caption2.weight(.semibold))
            Text("ci auto-fixed")
                .font(.caption.weight(.semibold))
                .lineLimit(1)
        }
        .fixedSize(horizontal: true, vertical: false)
        .foregroundStyle(Color.green)
        .help("The engine auto-fixed a CI failure on this PR within the last 24 hours.")
        .accessibilityLabel("CI auto-fixed by the engine")
    }
}

/// In-flight / exhausted CI-failure chip. Design Q11 calls for two
/// visual states:
///  - 🟧 `ci failing (used/budget)` while the engine still has budget
///    and a worker is/was in flight.
///  - 🛑 `ci failing (exhausted)` once the engine has given up; the
///    user is the next actor (`boss engine ci retry`).

struct CIFailureChip: View {
    let badge: CiFailureBadge

    private var label: String {
        switch badge.state {
        case .inFlight:
            if badge.budget > 0 {
                return "ci failing (\(badge.attemptsUsed)/\(badge.budget))"
            }
            return "ci failing"
        case .exhausted:
            return "ci failing (exhausted)"
        }
    }

    private var color: Color {
        switch badge.state {
        case .inFlight: return .orange
        case .exhausted: return .red
        }
    }

    private var icon: String {
        switch badge.state {
        case .inFlight: return "exclamationmark.triangle.fill"
        case .exhausted: return "octagon.fill"
        }
    }

    private var tooltip: String {
        switch badge.state {
        case .inFlight:
            return "The engine is auto-fixing a CI failure on this PR. \(badge.attemptsUsed) of \(badge.budget) attempts used."
        case .exhausted:
            return "The engine has exhausted its CI auto-fix budget for this PR. Run `boss engine ci retry <work-item>` to try again."
        }
    }

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: icon)
                .font(.caption2.weight(.semibold))
            Text(label)
                .font(.caption.weight(.semibold))
                .lineLimit(1)
        }
        .fixedSize(horizontal: true, vertical: false)
        .foregroundStyle(color)
        .help(tooltip)
        .accessibilityLabel(tooltip)
    }
}

/// CI status indicator shown on Review-lane cards. Four visual states:
/// in-progress (yellow clock), success (green checkmark), fail (red X),
/// and unknown / nil (also rendered as in-progress). The unknown state
/// means the first poll is still pending — showing in-progress is truthful
/// ("we haven't checked yet") and keeps the icon slot occupied so it
/// doesn't pop in later.

struct PrCiIndicator: View {
    let state: String
    var detail: String? = nil

    var body: some View {
        if let icon = systemImage {
            Image(systemName: icon)
                .font(.caption2.weight(.semibold))
                .foregroundStyle(tint)
                .help(tooltipText)
                .accessibilityLabel(tooltipText)
        }
    }

    private var systemImage: String? {
        switch state {
        case "success": return "checkmark.circle.fill"
        case "fail":    return "xmark.circle.fill"
        default:        return "clock.fill"
        }
    }

    private var tint: Color {
        switch state {
        case "success": return .green
        case "fail":    return .red
        default:        return .yellow
        }
    }

    private var tooltipText: String {
        switch state {
        case "success":
            return "All required CI checks passed"
        case "fail":
            if let detail, let checks = parseCheckNames(from: detail), !checks.isEmpty {
                return "Required CI check(s) failed: \(checks.joined(separator: ", "))"
            }
            return "Required CI check(s) failed"
        default:
            return "Required CI checks in progress"
        }
    }

    private func parseCheckNames(from json: String) -> [String]? {
        guard let data = json.data(using: .utf8),
              let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]]
        else { return nil }
        return arr.compactMap { $0["name"] as? String }
    }
}

/// Parsed form of `WorkTask.mergeQueueDetail` — the JSON sub-state blob
/// (`{"position", "state", "enqueued_at", "section_order"}`) the merge
/// poller writes while a PR sits in GitHub's merge queue or has Merge When
/// Ready armed. Kept free of SwiftUI so the parsing contract can be
/// unit-tested without hosting a view (mirrors `AutomationTime`).

struct MergeQueueDetail: Equatable {
    /// 1-indexed queue position. `nil` while the PR is only Merge-When-Ready
    /// armed (not yet queued), or when GitHub/Trunk didn't report one.
    var position: Int?
    /// The entry's raw state — GitHub's `mergeQueueEntry.state` (e.g.
    /// `"AWAITING_CHECKS"`, `"MERGEABLE"`, `"LOCKED"`, `"QUEUED"`,
    /// `"UNMERGEABLE"`) for a GitHub-native entry, or Trunk's lowercase
    /// `TrunkPrState` (`"pending"`, `"testing"`, `"tests_passed"`,
    /// `"not_ready"`, `"failed"`, `"cancelled"`, `"pending_failure"`) when
    /// `source == "trunk"`. `nil` while not queued, or when neither
    /// reported one.
    var state: String?
    /// RFC 3339 timestamp of when the PR entered the queue. `nil` when
    /// GitHub/Trunk didn't report one.
    var enqueuedAt: String?
    /// Engine-computed sort key for the kanban "Merging" section — ascending
    /// order matches the real merge-queue order, with Merge-When-Ready cards
    /// (no queue position) always sorting below every queued card. `nil`
    /// only for a malformed/legacy payload; callers should sort those last.
    var sectionOrder: Int64?
    /// Which mechanism wrote this entry: `"trunk"` for a Trunk-queue
    /// product's `TrunkQueueProbe`, `nil` for the GitHub-native path (the
    /// GitHub probe never writes this key — see `trunk_queue_poller.rs`'s
    /// `live_entry_detail_json`). Disambiguates `state`'s vocabulary, since
    /// GitHub and Trunk use different (and overlapping-looking) strings.
    var source: String?
    /// Trunk's queue-level state (`"RUNNING"`, `"PAUSED"`, `"DRAINING"`,
    /// `"SWITCHING_MODES"`), distinct from this entry's own `state`. `nil`
    /// for a GitHub-native entry, which has no equivalent concept.
    var queueState: String?

    /// Parse the engine's JSON blob. Returns `nil` for `nil`/empty/
    /// unparseable input so the caller can fall back to a sane default
    /// rather than propagate a parse error into the view.
    static func parse(_ json: String?) -> MergeQueueDetail? {
        guard let json, let data = json.data(using: .utf8) else { return nil }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        return MergeQueueDetail(
            position: (obj["position"] as? NSNumber)?.intValue,
            state: obj["state"] as? String,
            enqueuedAt: obj["enqueued_at"] as? String,
            sectionOrder: (obj["section_order"] as? NSNumber)?.int64Value,
            source: obj["source"] as? String,
            queueState: obj["queue_state"] as? String
        )
    }

    /// Human-readable form of `state` for tooltips (e.g.
    /// `"AWAITING_CHECKS"` → `"awaiting checks"`). Falls back to a
    /// lowercased, underscore-stripped rendering of any unrecognised value
    /// so a future GitHub enum addition still reads sensibly. GitHub-path
    /// only — Trunk entries render through `trunkBadgeText` instead.
    var displayState: String? {
        guard let state, !state.isEmpty else { return nil }
        switch state.uppercased() {
        case "AWAITING_CHECKS": return "awaiting checks"
        case "MERGEABLE": return "mergeable"
        case "LOCKED": return "locked"
        case "QUEUED": return "queued"
        case "UNMERGEABLE": return "unmergeable"
        default: return state.lowercased().replacingOccurrences(of: "_", with: " ")
        }
    }

    /// Whether this entry was written by the Trunk queue poller rather
    /// than the GitHub-native probe.
    var isTrunk: Bool { source == "trunk" }

    /// Badge text for a Trunk-sourced entry, mapping `TrunkPrState` to the
    /// short label `MergeQueueBadge` renders: `pending` shows the queue
    /// position (`"#3"`, same convention as the GitHub path) or, during the
    /// optimistic-submit window before the poller has attached a position
    /// (engine writes `{"source":"trunk","state":"pending"}` with no
    /// `position` the instant the merge click lands), falls back to
    /// `"Queued"` so the card never reads as if it were still on the
    /// GitHub/Merge-When-Ready path. `testing`/`tests_passed` show a short
    /// verb phrase, `not_ready` explains the stall (readiness, not a
    /// queue-mechanics problem — design doc §"not_ready is not a failure").
    /// Any other/future state renders Trunk's raw string rather than hiding
    /// the card — but only for states not otherwise known: `cancelled`/
    /// `merged` are handled by `isTrunkTerminal` instead, same as
    /// `failed`/`pending_failure`, since all four are retired states rather
    /// than in-flight ones. `nil` for `failed`/`pending_failure`/
    /// `cancelled`/`merged` (see `isTrunkTerminal`) or a non-Trunk entry.
    var trunkBadgeText: String? {
        guard isTrunk, let state else { return nil }
        switch state {
        case "pending": return position.map { "#\($0)" } ?? "Queued"
        case "testing": return "Testing"
        case "tests_passed": return "Merging…"
        case "not_ready": return "Waiting on readiness"
        case "failed", "pending_failure", "cancelled", "merged": return nil
        default: return state
        }
    }

    /// Whether a Trunk-sourced entry has left the queue on a test failure,
    /// or otherwise retired without merging (`cancelled`) or completed
    /// (`merged`). All four states retire the merge intent — `failed`/
    /// `pending_failure` snap the card back to Review, `cancelled` does the
    /// same (design doc §"Trunk queue poller" task 5, §"Failure surfacing"),
    /// and `merged` means the card is about to leave the board entirely —
    /// before the badge would ever redraw with them, but the badge checks
    /// this defensively so a stale render never shows a retired entry as if
    /// it were still progressing.
    var isTrunkTerminal: Bool {
        guard isTrunk, let state else { return false }
        return ["failed", "pending_failure", "cancelled", "merged"].contains(state)
    }

    /// "Trunk queue paused/draining" banner text for the Merging section
    /// header (`ChatViewModel.mergingSection`). `nil` while the queue is
    /// `RUNNING` (the healthy default), for a non-Trunk entry, or when
    /// Trunk didn't report a queue state.
    var queueStateBanner: String? {
        guard isTrunk, let queueState, queueState != "RUNNING" else { return nil }
        switch queueState {
        case "PAUSED": return "Trunk queue paused"
        case "DRAINING": return "Trunk queue draining"
        case "SWITCHING_MODES": return "Trunk queue switching modes"
        default: return "Trunk queue \(queueState.lowercased())"
        }
    }
}

/// Compact queue badge for a card in the kanban's "Merging" section (Done
/// column, above "Today"). Shows the queue position (`"#3"`) plus an icon
/// for mergeable / unmergeable / checks-running — replacing the old verbose
/// "merging — #1, awaiting checks" text chip. A Merge-When-Ready card that
/// hasn't reached the queue yet has no position to show, so it renders the
/// readiness icon alone (mono#1939: the old chip's unbounded text
/// truncated the PR number off review cards; this one keeps the same
/// `.layoutPriority(-1)` / single-line shape so the PR link — laid out at
/// `.layoutPriority(1)` — always wins the available width first).

struct MergeQueueBadge: View {
    var mergeQueueState: String
    var detail: String?
    /// Required-CI state (`WorkTask.ciRequiredState`) for the
    /// not-yet-queued Merge-When-Ready case, where there is no
    /// `mergeQueueEntry.state` to read readiness from.
    var ciRequiredState: String?

    @Environment(\.colorScheme) private var colorScheme

    private var parsed: MergeQueueDetail? { MergeQueueDetail.parse(detail) }

    private enum Readiness: Equatable {
        case mergeable
        case unmergeable
        case checksRunning

        var systemImage: String {
            switch self {
            case .mergeable: return "checkmark.circle.fill"
            case .unmergeable: return "xmark.circle.fill"
            case .checksRunning: return "clock.fill"
            }
        }

        var label: String {
            switch self {
            case .mergeable: return "mergeable"
            case .unmergeable: return "unmergeable"
            case .checksRunning: return "checks running"
            }
        }
    }

    private var isTrunk: Bool { parsed?.isTrunk ?? false }

    private var readiness: Readiness {
        if isTrunk {
            switch parsed?.state {
            case "tests_passed": return .mergeable
            case "failed", "pending_failure": return .unmergeable
            default: return .checksRunning // pending, testing, not_ready, unknown, nil
            }
        }
        if mergeQueueState == "queued" {
            switch parsed?.state?.uppercased() {
            case "MERGEABLE": return .mergeable
            case "UNMERGEABLE": return .unmergeable
            default: return .checksRunning // AWAITING_CHECKS, QUEUED, LOCKED, unknown
            }
        }
        switch ciRequiredState {
        case "success": return .mergeable
        case "fail": return .unmergeable
        default: return .checksRunning // in_progress, unknown, nil
        }
    }

    private var queuePosition: Int? {
        guard mergeQueueState == "queued" else { return nil }
        return parsed?.position
    }

    /// Primary badge label: Trunk entries render their mapped state text
    /// (`trunkBadgeText`, e.g. `"Testing"`/`"Merging…"`); everything else
    /// keeps the plain `"#n"` queue-position rendering.
    private var badgeText: String? {
        if isTrunk {
            return parsed?.trunkBadgeText
        }
        return queuePosition.map { "#\($0)" }
    }

    private var tooltipText: String {
        var parts: [String]
        if isTrunk {
            parts = ["PR is in the Trunk merge queue."]
            if parsed?.state == "pending" {
                // `trunkBadgeText` shows a queue position for this state,
                // not a state name — phrase the tooltip to match instead
                // of "State: #3.".
                parts.append(parsed?.position.map { "Queue position \($0)." } ?? "Queued.")
            } else if let badgeText {
                parts.append("State: \(badgeText).")
            }
        } else if mergeQueueState == "queued" {
            parts = ["PR is in the merge queue."]
            if let queuePosition {
                parts.append("Queue position \(queuePosition).")
            }
        } else {
            parts = ["Merge When Ready is armed — PR will merge automatically once required checks pass."]
        }
        parts.append("Status: \(readiness.label).")
        if let enqueuedAt = parsed?.enqueuedAt {
            parts.append("Enqueued \(AutomationTime.relative(enqueuedAt, now: Date())).")
        }
        return parts.joined(separator: " ")
    }

    private var accessibilityLabel: String {
        if isTrunk, let badgeText {
            return "\(badgeText), \(readiness.label)"
        }
        let subject = queuePosition.map { "Queue position \($0)" } ?? "Merge when ready"
        return "\(subject), \(readiness.label)"
    }

    var body: some View {
        // A Trunk entry that left the queue on a test failure has already
        // retired its merge intent and snapped the card back to Review —
        // rendering nothing here (rather than a stale terminal label) is
        // the correct behavior if a redraw ever races that transition.
        if parsed?.isTrunkTerminal == true {
            EmptyView()
        } else {
            HStack(spacing: 3) {
                if let badgeText {
                    Text(badgeText)
                        .font(.caption.weight(.semibold))
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                Image(systemName: readiness.systemImage)
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(readiness == .mergeable ? Color.green : Color.white)
            }
            .foregroundStyle(Color.white)
            .padding(.horizontal, 6)
            .padding(.vertical, 3)
            .background(backgroundColor)
            .clipShape(Capsule())
            .help(tooltipText)
            .accessibilityLabel(accessibilityLabel)
        }
    }

    private var backgroundColor: Color {
        switch colorScheme {
        case .light:
            return Color(red: 165/255, green: 107/255, blue: 0/255)
        case .dark:
            return Color(red: 158/255, green: 106/255, blue: 3/255)
        @unknown default:
            return Color(red: 165/255, green: 107/255, blue: 0/255)
        }
    }
}

/// Warning indicator shown on the PR card of a chain root when at least one
/// descendant revision is still `todo` or `active`. Signals that new commits
/// are incoming and the PR should not be merged yet.

struct PrInRevisionIndicator: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "exclamationmark.triangle.fill")
                .font(.caption2.weight(.semibold))
            Text("in revision")
                .font(.caption.weight(.semibold))
                .lineLimit(1)
        }
        .foregroundStyle(Color.white)
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .background(Color.orange)
        .clipShape(Capsule())
        .fixedSize()
        .help("A revision is in progress — do not merge this PR yet")
        .accessibilityLabel("In revision — do not merge")
    }
}

/// Review-gating indicator for Review-lane cards. Four states:
/// required (empty checklist — awaiting review), approved (green
/// checkmark — all required reviews in), changes_requested (exclamation
/// — at least one reviewer requested changes), unknown (hidden).

struct PrReviewIndicator: View {
    let state: String
    var detail: String? = nil

    var body: some View {
        if let icon = systemImage {
            Image(systemName: icon)
                .font(.caption2.weight(.semibold))
                .foregroundStyle(tint)
                .help(tooltipText)
                .accessibilityLabel(tooltipText)
        }
    }

    private var systemImage: String? {
        switch state {
        case "required":           return "checklist"
        case "approved":           return "checkmark.seal.fill"
        case "changes_requested":  return "exclamationmark.circle.fill"
        default:                   return nil
        }
    }

    private var tint: Color {
        switch state {
        case "required":           return .secondary
        case "approved":           return .green
        case "changes_requested":  return .orange
        default:                   return .secondary
        }
    }

    private var tooltipText: String {
        let reviewers = reviewerNames(from: detail)
        switch state {
        case "required":
            return "Awaiting required review"
        case "approved":
            if reviewers.isEmpty { return "Approved" }
            return "Approved by \(reviewers.joined(separator: ", "))"
        case "changes_requested":
            if reviewers.isEmpty { return "Changes requested" }
            return "Changes requested by \(reviewers.joined(separator: ", "))"
        default:
            return "Review state unknown"
        }
    }

    private func reviewerNames(from json: String?) -> [String] {
        guard let json,
              let data = json.data(using: .utf8),
              let arr = try? JSONSerialization.jsonObject(with: data) as? [String]
        else { return [] }
        return arr
    }
}

/// "resolving conflicts" PR-card chip. Rendered on cards that have been
/// routed to the Doing column because a merge-resolution worker is
/// actively running against them. Signals to the user that the active
/// work is conflict resolution rather than the original task scope.

struct ResolvingConflictsBadge: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "arrow.triangle.2.circlepath")
                .font(.caption2)
            Text("resolving conflicts")
                .font(.caption.weight(.semibold))
                .foregroundStyle(Color.orange)
                .lineLimit(1)
                .truncationMode(.tail)
        }
        // The icon keeps its intrinsic size, but the label is allowed to
        // truncate so a wide badge yields footer width to the fixed-size
        // repo chip and short-id rather than pushing them off the card's
        // right edge. The full text stays reachable via the tooltip and
        // accessibility label. `.layoutPriority(-1)` makes this badge the
        // first element the footer HStack squeezes when space is tight.
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .background(Color.orange.opacity(0.12))
        .clipShape(Capsule())
        .layoutPriority(-1)
        .help("A worker is actively resolving a merge conflict on this PR.")
        .accessibilityLabel("Resolving merge conflict")
    }
}

/// "resolving CI failure" PR-card chip. Rendered on cards routed to the
/// Doing column because a CI-remediation worker is actively running
/// against them. Symmetric to [[ResolvingConflictsBadge]] — same visual
/// vocabulary, same orange tint, different icon and label.

struct ResolvingCIFailureBadge: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "arrow.triangle.2.circlepath")
                .font(.caption2)
            Text("resolving CI failure")
                .font(.caption.weight(.semibold))
                .foregroundStyle(Color.orange)
                .lineLimit(1)
                .truncationMode(.tail)
        }
        // See [[ResolvingConflictsBadge]]: the label truncates so this
        // wider badge can't clip the trailing repo chip / short-id off the
        // card's right edge. Full text remains in the tooltip and a11y
        // label, and `.layoutPriority(-1)` makes it yield space first.
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .background(Color.orange.opacity(0.12))
        .clipShape(Capsule())
        .layoutPriority(-1)
        .help("A worker is actively resolving a CI failure on this PR.")
        .accessibilityLabel("Resolving CI failure")
    }
}

/// "AI reviewing" card chip. Rendered on Doing-column cards held in `active`
/// while a `pr_review` reviewer execution is in flight (P992). The badge
/// distinguishes a card that is intentionally waiting for the AI review pass
/// from one that appears stuck with no explanation.

struct ReviewingAIBadge: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "brain")
                .font(.caption2)
            Text("AI reviewing")
                .font(.caption.weight(.semibold))
                .foregroundStyle(Color.accentColor)
                .lineLimit(1)
                .truncationMode(.tail)
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .background(Color.accentColor.opacity(0.10))
        .clipShape(Capsule())
        .layoutPriority(-1)
        .help("An AI reviewer pass is running on this PR. The card will move to Review once the pass completes (typically within a minute).")
        .accessibilityLabel("AI reviewing PR")
    }
}

/// Badge for a task filed as deferred / future scope (`deferred == true`).
/// Deliberately muted (secondary tint, "moon" glyph) so parked future work
/// reads as "set aside" at a glance and is visually distinct from a
/// genuinely-queued backlog card. Distinct from the AI `[deferred-scope]`
/// attention marker (`DeferredScopeCardBadge`) — this reflects the durable
/// per-row `deferred` classification, not a transient AI flag.

struct FutureScopeBadge: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "moon.zzz.fill")
                .font(.caption2)
            Text("Future")
                .font(.caption.weight(.semibold))
                .lineLimit(1)
                .truncationMode(.tail)
        }
        .foregroundStyle(Color.secondary)
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .background(Color.secondary.opacity(0.12))
        .clipShape(Capsule())
        .layoutPriority(-1)
        .help("Filed as future scope — parked until explicitly approved. The engine keeps it visible and unblocks its dependencies, but never auto-dispatches it. Approve with a drag to Doing, `bossctl work start`, or `boss task update --deferred false`.")
        .accessibilityLabel("Deferred: future scope")
    }
}

struct WorkStatusBadge: View {
    let text: String
    var emphasized: Bool = false
    /// Overrides the `.help()` hover tooltip. `nil` (the default) falls
    /// back to `text` itself, matching every pre-existing call site
    /// unchanged. Pass this when the visible label is a short/transformed
    /// stand-in for something longer — e.g. the blocked pill's verbatim
    /// detail.
    var tooltip: String? = nil
    /// Shows a small dot so a hovering user can tell there's more before
    /// the system tooltip delay fires, rather than discovering it by
    /// accident. Only meaningful when `tooltip` differs from `text`.
    var hasMoreInfo: Bool = false

    var body: some View {
        HStack(spacing: 3) {
            Text(text)
                .lineLimit(1)
                .truncationMode(.tail)
            if hasMoreInfo {
                Circle()
                    .fill(foregroundColor.opacity(0.55))
                    .frame(width: 4, height: 4)
                    .accessibilityHidden(true)
            }
        }
        .font(.caption.weight(.semibold))
        .foregroundStyle(foregroundColor)
        .padding(.horizontal, 8)
        .padding(.vertical, 3)
        .background(backgroundColor)
        .clipShape(Capsule())
        .help(tooltip ?? text)
        .accessibilityHint(hasMoreInfo ? (tooltip ?? "") : "")
    }

    private var foregroundColor: Color {
        if emphasized {
            return .accentColor
        }
        return Color(nsColor: .labelColor)
    }

    private var backgroundColor: Color {
        if emphasized {
            return Color.white.opacity(0.96)
        }
        return Color(nsColor: .controlBackgroundColor)
    }
}

/// Compact count chip for the navigator project row. Shows the number
/// of unblocked (green `▶ N`) or dependency-blocked (red `⏸ N`) tasks
/// for a project. Color + symbol ensures the chip is meaningful for
/// color-blind users. Visual weight deliberately subordinate to the
/// project name — matches the `T<n>` / `P<n>` chip treatment.

struct ProjectTaskCountChip: View {
    enum Kind {
        case unblocked
        case blocked
    }

    let count: Int
    let kind: Kind

    var body: some View {
        Text(label)
            .font(.caption.weight(.semibold))
            .foregroundStyle(foregroundColor)
            .lineLimit(1)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(backgroundColor)
            .clipShape(Capsule())
            .help(helpText)
    }

    private var label: String {
        switch kind {
        case .unblocked: return "▶ \(count)"
        case .blocked: return "⏸ \(count)"
        }
    }

    private var helpText: String {
        switch kind {
        case .unblocked: return "\(count) unblocked task\(count == 1 ? "" : "s") ready to dispatch"
        case .blocked: return "\(count) task\(count == 1 ? "" : "s") gated by a dependency"
        }
    }

    private var foregroundColor: Color {
        switch kind {
        case .unblocked: return Color(nsColor: .systemGreen)
        case .blocked: return Color(nsColor: .systemRed)
        }
    }

    private var backgroundColor: Color {
        switch kind {
        case .unblocked: return Color(nsColor: .systemGreen).opacity(0.12)
        case .blocked: return Color(nsColor: .systemRed).opacity(0.12)
        }
    }
}

/// Color-coded chip for the kanban card footer. Reads as `H`/`M`/`L`
/// to keep the chip narrow at typical column widths; the full label
/// surfaces in the tooltip and detail popover. We render every
/// priority (medium included) rather than hiding the default so the
/// field is always visible — invisible defaults are exactly what
/// pushed authors to stuff `[MEDIUM]` into the name in the first
/// place.

struct PriorityChip: View {
    let priority: WorkPriority

    var body: some View {
        Text(letter)
            .font(.caption.weight(.bold))
            .foregroundStyle(foregroundColor)
            .frame(minWidth: 18)
            .padding(.horizontal, 6)
            .padding(.vertical, 3)
            .background(backgroundColor)
            .clipShape(Capsule())
            .help("Priority: \(priority.label)")
            .accessibilityLabel("Priority \(priority.label)")
    }

    private var letter: String {
        switch priority {
        case .high: return "H"
        case .medium: return "M"
        case .low: return "L"
        }
    }

    private var backgroundColor: Color {
        switch priority {
        case .high: return Color.red.opacity(0.18)
        case .medium: return Color.gray.opacity(0.18)
        case .low: return Color.blue.opacity(0.14)
        }
    }

    private var foregroundColor: Color {
        switch priority {
        case .high: return .red
        case .medium: return Color(nsColor: .secondaryLabelColor)
        case .low: return .blue
        }
    }
}

/// Effort-level chip rendered on kanban cards. Only shown when the
/// task carries a non-nil effort_level — unset rows must not masquerade
/// as medium.

struct EffortChip: View {
    let effortLevel: String

    var body: some View {
        Text(letter)
            .font(.caption.weight(.bold))
            .foregroundStyle(foregroundColor)
            .padding(.horizontal, 6)
            .padding(.vertical, 3)
            .background(backgroundColor)
            .clipShape(Capsule())
            .help("Effort: \(label)")
            .accessibilityLabel("Effort \(label)")
    }

    private var letter: String {
        switch effortLevel {
        case "trivial": return "XS"
        case "small": return "S"
        case "medium": return "M"
        case "large": return "L"
        case "max": return "XL"
        default: return effortLevel.prefix(1).uppercased()
        }
    }

    private var label: String {
        switch effortLevel {
        case "trivial": return "Trivial"
        case "small": return "Small"
        case "medium": return "Medium"
        case "large": return "Large"
        case "max": return "Max"
        default: return effortLevel.capitalized
        }
    }

    private var backgroundColor: Color {
        switch effortLevel {
        case "trivial": return Color.blue.opacity(0.12)
        case "small": return Color.green.opacity(0.14)
        case "medium": return Color.gray.opacity(0.18)
        case "large": return Color.orange.opacity(0.18)
        case "max": return Color.red.opacity(0.14)
        default: return Color.gray.opacity(0.18)
        }
    }

    private var foregroundColor: Color {
        switch effortLevel {
        case "trivial": return .blue
        case "small": return Color(nsColor: .systemGreen)
        case "medium": return Color(nsColor: .secondaryLabelColor)
        case "large": return .orange
        case "max": return .red
        default: return Color(nsColor: .secondaryLabelColor)
        }
    }
}
