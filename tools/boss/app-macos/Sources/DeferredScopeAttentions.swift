import SwiftUI

/// First-class presentation for a `deferred_scope` attention item — a
/// proper label/icon and parsed summary/reason, instead of falling into a
/// generic fallback row alongside unrelated attention kinds. Parses the
/// `[deferred-scope] summary="…" reason="…"` marker line embedded verbatim
/// in `item.bodyMarkdown`'s fenced code block, mirroring the engine's
/// `crate::deferred_scope::summary_and_reason`.
struct DeferredScopeAttentionPresentation: Equatable {
    /// `boss_protocol::DEFERRED_SCOPE_ATTENTION_KIND` wire value.
    static let kind = "deferred_scope"

    let summary: String
    let reason: String
    let isOpen: Bool

    static func forItem(_ item: WorkAttentionItem) -> DeferredScopeAttentionPresentation? {
        guard item.kind == kind else { return nil }
        let (summary, reason) = parseMarker(item.bodyMarkdown)
        return DeferredScopeAttentionPresentation(
            summary: summary ?? "(summary not parseable — see item detail)",
            reason: reason ?? "(reason not parseable — see item detail)",
            isOpen: item.status == "open"
        )
    }

    private static func parseMarker(_ bodyMarkdown: String) -> (String?, String?) {
        guard let markerLine = bodyMarkdown
            .components(separatedBy: "\n")
            .map({ $0.trimmingCharacters(in: .whitespaces) })
            .first(where: { $0.hasPrefix("[deferred-scope]") })
        else {
            return (nil, nil)
        }
        return (extractQuoted(markerLine, key: "summary"), extractQuoted(markerLine, key: "reason"))
    }

    private static func extractQuoted(_ text: String, key: String) -> String? {
        let needle = "\(key)=\""
        guard let range = text.range(of: needle) else { return nil }
        let rest = text[range.upperBound...]
        guard let endQuote = rest.firstIndex(of: "\"") else { return nil }
        return String(rest[rest.startIndex..<endQuote])
    }
}

/// Compact badge shown on a Review-lane kanban card with open
/// `deferred_scope` attention items, so they are visible without opening
/// the card. A single icon+count chip rather than a full label, because
/// card rows are already tight — adding wider chrome here truncates the
/// merge-queue badge. Clicking opens a popover listing every item with
/// per-item actions.
struct DeferredScopeCardBadge: View {
    let items: [DeferredScopeAttention]
    /// Attention item ids with an accept/create-task request in flight —
    /// see `ChatViewModel.deferredScopeActionInFlightIDs`. Threaded through
    /// as data (rather than observing the view model directly) so this row
    /// hierarchy stays a plain, closure-driven view like its siblings.
    let actionInFlightIDs: Set<String>
    let onAccept: (String) -> Void
    let onCreateTask: (String) -> Void

    @State private var isPopoverPresented = false

    var body: some View {
        Button {
            isPopoverPresented.toggle()
        } label: {
            HStack(spacing: 3) {
                Image(systemName: "scissors")
                Text("\(items.count)")
            }
            .font(.caption2.weight(.semibold))
            .foregroundStyle(.blue)
            .padding(.horizontal, 5)
            .padding(.vertical, 2)
            .background(Capsule().fill(Color.blue.opacity(0.14)))
        }
        .buttonStyle(.plain)
        .help(items.count == 1 ? "1 deferred scope item" : "\(items.count) deferred scope items")
        .accessibilityLabel(items.count == 1 ? "1 deferred scope item" : "\(items.count) deferred scope items")
        .popover(isPresented: $isPopoverPresented, arrowEdge: .trailing) {
            DeferredScopePopover(
                items: items,
                actionInFlightIDs: actionInFlightIDs,
                onAccept: onAccept,
                onCreateTask: onCreateTask
            )
        }
    }
}

struct DeferredScopePopover: View {
    /// Trailing inset = the 16pt leading content inset plus ~6pt of gutter
    /// for the macOS overlay scrollbar, so it doesn't sit on top of text.
    private static let trailingScrollbarGutter: CGFloat = 22

    let items: [DeferredScopeAttention]
    let actionInFlightIDs: Set<String>
    let onAccept: (String) -> Void
    let onCreateTask: (String) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Deferred Scope")
                .font(.headline)
                .padding(.horizontal, 16)
                .padding(.top, 14)
                .padding(.bottom, 16)

            Divider()

            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(items) { entry in
                        DeferredScopeAttentionRow(
                            entry: entry,
                            isActing: actionInFlightIDs.contains(entry.item.id),
                            onAccept: onAccept,
                            onCreateTask: onCreateTask
                        )
                        if entry.id != items.last?.id {
                            Divider()
                        }
                    }
                }
                .padding(.leading, 16)
                .padding(.trailing, Self.trailingScrollbarGutter)
                .padding(.vertical, 16)
            }
        }
        .frame(minWidth: 340, maxWidth: 420, minHeight: 80, maxHeight: 440)
    }
}

struct DeferredScopeAttentionRow: View {
    /// Icon column width + icon-to-text spacing below, kept as one constant
    /// so the hanging indent on the rationale and button row can't drift
    /// out of sync with the icon if either changes.
    private enum Layout {
        static let iconWidth: CGFloat = 16
        static let iconTextSpacing: CGFloat = 8
        static let hangingIndent: CGFloat = iconWidth + iconTextSpacing
    }

    let entry: DeferredScopeAttention
    let isActing: Bool
    let onAccept: (String) -> Void
    let onCreateTask: (String) -> Void

    private var presentation: DeferredScopeAttentionPresentation? {
        DeferredScopeAttentionPresentation.forItem(entry.item)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .top, spacing: Layout.iconTextSpacing) {
                Image(systemName: "scissors")
                    .foregroundStyle(.blue)
                    .frame(width: Layout.iconWidth)
                Text(presentation?.summary ?? entry.item.title)
                    .font(.subheadline.weight(.medium))
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let reason = presentation?.reason {
                Text(reason)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, Layout.hangingIndent)
                    .padding(.top, 4)
            }
            HStack(spacing: 8) {
                Button("Create task") {
                    onCreateTask(entry.item.id)
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
                Button("Accept") {
                    onAccept(entry.item.id)
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
            }
            .disabled(isActing)
            .padding(.leading, Layout.hangingIndent)
            .padding(.top, 10)
            .padding(.bottom, 8)
        }
    }
}
