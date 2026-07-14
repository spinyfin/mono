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
/// `deferred_scope` attention items — the operator directive's "prominent
/// in the kanban UI" affordance. A single icon+count chip (not a full
/// label) to respect existing card row space (cf. the merge-queue badge
/// truncation chore T2531 before adding more chrome). Clicking opens a
/// popover listing every item with per-item actions.
struct DeferredScopeCardBadge: View {
    let items: [DeferredScopeAttention]
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
            DeferredScopePopover(items: items, onAccept: onAccept, onCreateTask: onCreateTask)
        }
    }
}

struct DeferredScopePopover: View {
    let items: [DeferredScopeAttention]
    let onAccept: (String) -> Void
    let onCreateTask: (String) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Deferred Scope")
                .font(.headline)
                .padding(.horizontal, 16)
                .padding(.top, 14)
                .padding(.bottom, 10)

            Divider()

            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(items) { entry in
                        DeferredScopeAttentionRow(entry: entry, onAccept: onAccept, onCreateTask: onCreateTask)
                        if entry.id != items.last?.id {
                            Divider()
                        }
                    }
                }
                .padding(16)
            }
            .frame(minWidth: 340, maxWidth: 420, minHeight: 80, maxHeight: 440)
        }
    }
}

struct DeferredScopeAttentionRow: View {
    let entry: DeferredScopeAttention
    let onAccept: (String) -> Void
    let onCreateTask: (String) -> Void

    @State private var isActing = false

    private var presentation: DeferredScopeAttentionPresentation? {
        DeferredScopeAttentionPresentation.forItem(entry.item)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: "scissors")
                    .foregroundStyle(.blue)
                    .frame(width: 16)
                Text(presentation?.summary ?? entry.item.title)
                    .font(.subheadline.weight(.medium))
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let reason = presentation?.reason {
                Text(reason)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, 24)
            }
            HStack(spacing: 8) {
                Button("Create task") {
                    isActing = true
                    onCreateTask(entry.item.id)
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
                Button("Accept") {
                    isActing = true
                    onAccept(entry.item.id)
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
            }
            .disabled(isActing)
            .padding(.leading, 24)
        }
    }
}
