import AppKit
import os.log
import SwiftUI
import UpdateCore

struct WorkDispatchFailureBanner: View {
    let reason: String
    let errorText: String?

    /// Mirrors `boss_engine_core::work::DELIBERATE_PARK_DISPATCH_FAILED_REASON`.
    /// A row carrying this reason is not broken — the worker did its job and
    /// escalated correctly (it declared itself blocked, or the auto-nudge
    /// breaker gave up waiting for a response) — so it must not share the
    /// red "Failed to start" treatment below, which reads as the engine
    /// having given up on a dispatch it could not get running. Both reasons
    /// share this one banner (same `dispatch_failed_reason` field, same
    /// Backlog placement, same clear-on-next-run behavior) because the
    /// underlying halted-state surface is deliberately the same; only the
    /// framing differs: the row is not broken, so it must not read as a failure.
    private static let deliberateParkReason = "deliberate_park"

    /// Mirrors `boss_engine_core::work::DELIBERATE_PARK_CHURN_COMBINED_MARKER`.
    /// A deliberately parked row that has ALSO tripped the churn guard
    /// carries this exact substring in its `dispatch_failed_error` body
    /// (`WorkDb::deliberate_park_text`). The engine unit test
    /// `combined_park_churn_marker_matches_swift_banner` `include_str!`s
    /// this file and asserts that constant appears here verbatim, so a
    /// wording change on either side fails that test instead of silently
    /// dropping the churn half of the card headline.
    static let combinedParkChurnMarker = "tripping the churn guard on top of the park"

    private var isDeliberatePark: Bool {
        reason == Self.deliberateParkReason
    }

    private var reasonLabel: String {
        // Combined parks retain the human-only-clearable reason. Their stored
        // diagnostic text also names the churn guard, including older rows.
        if isDeliberatePark, errorText?.contains(Self.combinedParkChurnMarker) == true {
            return "deliberate park + churn guard"
        }
        return reason.replacingOccurrences(of: "_", with: " ")
    }

    var headline: String {
        isDeliberatePark ? "Waiting on you — \(reasonLabel)" : "Failed to start — \(reasonLabel)"
    }

    var summary: String? {
        isDeliberatePark ? "Review the open attention item, then drag to Doing to resume." : errorText
    }

    /// Parked cards must show the full resume instruction; dispatch-failure
    /// cards cap the stored diagnostic at three lines. Consumed by `body`
    /// and asserted from tests so a hard 1-line cap cannot hide behind a
    /// structurally-unlike control view.
    var summaryLineLimit: Int? {
        isDeliberatePark ? nil : 3
    }

    private var tint: Color {
        isDeliberatePark ? .blue : .red
    }

    private var iconName: String {
        isDeliberatePark ? "person.fill.questionmark" : "exclamationmark.triangle.fill"
    }

    var body: some View {
        HStack(alignment: .top, spacing: 6) {
            Image(systemName: iconName)
                .foregroundStyle(tint)
                .font(.caption)
                .padding(.top, 1)
            VStack(alignment: .leading, spacing: 2) {
                Text(headline)
                    .font(.caption.weight(.medium))
                    .foregroundStyle(.primary)
                    .fixedSize(horizontal: false, vertical: true)
                if let summary, !summary.isEmpty {
                    Text(summary)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(summaryLineLimit)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 4)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(tint.opacity(0.12))
                .overlay(
                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                        .strokeBorder(tint.opacity(0.4), lineWidth: 1)
                )
        )
        .accessibilityElement(children: .combine)
        .help(errorText ?? reasonLabel)
    }
}

struct WorkDragRefusalBanner: View {
    let message: String
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 6) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .font(.caption)
                .padding(.top, 1)
            Text(message)
                .font(.caption)
                .foregroundStyle(.primary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 4)
            Button(action: onDismiss) {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(.secondary)
                    .font(.caption)
            }
            .buttonStyle(.plain)
            .help("Dismiss")
            .accessibilityLabel("Dismiss drag refusal")
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(Color.orange.opacity(0.12))
                .overlay(
                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                        .strokeBorder(Color.orange.opacity(0.4), lineWidth: 1)
                )
        )
        .accessibilityElement(children: .combine)
    }
}

/// Inline confirmation banner shown on the card whose
/// `merge_when_ready_accepted` reply just arrived (`ChatViewModel.mergeFeedbackNotice`),
/// e.g. "Submitted to Trunk merge queue" — typically already routed into the
/// Merging section by the engine's optimistic queue-state write rather than
/// still sitting in Review (see `mergeFeedbackNotice`'s doc comment). Auto-
/// dismissed after 5s; the close button lets the user clear it early. Mirrors
/// `WorkDragRefusalBanner`'s shape with a positive (green, checkmark)
/// treatment instead of a warning.

struct WorkMergeFeedbackBanner: View {
    let message: String
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 6) {
            Image(systemName: "checkmark.circle.fill")
                .foregroundStyle(.green)
                .font(.caption)
                .padding(.top, 1)
            Text(message)
                .font(.caption)
                .foregroundStyle(.primary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 4)
            Button(action: onDismiss) {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(.secondary)
                    .font(.caption)
            }
            .buttonStyle(.plain)
            .help("Dismiss")
            .accessibilityLabel("Dismiss merge confirmation")
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(Color.green.opacity(0.12))
                .overlay(
                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                        .strokeBorder(Color.green.opacity(0.4), lineWidth: 1)
                )
        )
        .accessibilityElement(children: .combine)
    }
}

/// Persistent banner shown across the top of the kanban whenever a search
/// filter is active. Non-matching cards are hidden while a search is in
/// effect, and without a standing indicator a stale query reads as an
/// empty or complete board — a card looks deleted when it is merely
/// filtered out (issue #1248). The banner states the view is filtered,
/// echoes the active query, and offers a one-click Clear affordance.

struct WorkFilterBanner: View {
    let query: String
    let onClear: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "line.3.horizontal.decrease.circle.fill")
                .foregroundStyle(Color.accentColor)
                .font(.callout)
            Text("Filtered view — showing matches for \(Text("“\(query)”").foregroundStyle(.primary).fontWeight(.semibold))")
            .foregroundStyle(.secondary)
            .font(.callout)
            .lineLimit(1)
            .truncationMode(.middle)
            Spacer(minLength: 8)
            Button(action: onClear) {
                Label("Clear filter", systemImage: "xmark.circle.fill")
                    .font(.callout.weight(.medium))
            }
            .buttonStyle(.plain)
            .foregroundStyle(Color.accentColor)
            .help("Clear the search filter and show all cards")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity)
        .background(Color.accentColor.opacity(0.12))
        .overlay(alignment: .bottom) {
            Rectangle()
                .fill(Color.accentColor.opacity(0.35))
                .frame(height: 1)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Board is filtered by search query \(query). Activate to clear the filter.")
    }
}

/// A wrapping horizontal stack: lays subviews left-to-right and wraps to a
/// new line as soon as the next subview would overflow the proposed width.
///
/// Used for the kanban card's metadata/badge cluster. A plain `HStack` of
/// `.fixedSize` chips (effort, CI status, repo, work-item id, agent/action
/// chips) cannot compress and has no wrap behaviour, so a full badge set on
/// a card with a long title overflows past the lane's right edge and gets
/// clipped — the recurring regression in #1172. Flowing the cluster instead
/// constrains every chip to the lane width: the row grows downward rather
/// than running off the side.
