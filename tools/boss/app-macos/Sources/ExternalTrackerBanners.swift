import AppKit
import os.log
import SwiftUI
import UpdateCore

struct ExternalTrackerSyncBanner: View {
    let items: [WorkAttentionItem]
    @State private var isPopoverPresented = false

    private var leadingPresentation: ExternalTrackerAttentionPresentation? {
        items.compactMap { ExternalTrackerAttentionPresentation.forItem($0) }.first
    }

    var body: some View {
        Button {
            isPopoverPresented.toggle()
        } label: {
            HStack(spacing: 6) {
                Image(systemName: leadingPresentation?.iconName ?? "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.orange)
                Text(items.count == 1
                    ? "Sync issue"
                    : "\(items.count) sync issues")
                    .font(.caption)
                    .foregroundStyle(.orange)
                Spacer()
                Image(systemName: "chevron.right")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .background(
                RoundedRectangle(cornerRadius: 6)
                    .fill(Color.orange.opacity(0.12))
                    .overlay(
                        RoundedRectangle(cornerRadius: 6)
                            .stroke(Color.orange.opacity(0.3), lineWidth: 1)
                    )
            )
        }
        .buttonStyle(.plain)
        .popover(isPresented: $isPopoverPresented, arrowEdge: .trailing) {
            ExternalTrackerSyncPopover(items: items)
        }
    }
}

private struct ExternalTrackerSyncPopover: View {
    let items: [WorkAttentionItem]

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("External Tracker Sync Issues")
                .font(.headline)
                .padding(.horizontal, 16)
                .padding(.top, 14)
                .padding(.bottom, 10)

            Divider()

            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(items) { item in
                        if let presentation = ExternalTrackerAttentionPresentation.forItem(item) {
                            ExternalTrackerAttentionRow(presentation: presentation, item: item)
                        } else {
                            ExternalTrackerGenericAttentionRow(item: item)
                        }
                    }
                }
                .padding(16)
            }
            .frame(minWidth: 320, maxWidth: 400, minHeight: 80, maxHeight: 400)
        }
    }
}

private struct ExternalTrackerAttentionRow: View {
    let presentation: ExternalTrackerAttentionPresentation
    let item: WorkAttentionItem

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: presentation.iconName)
                    .foregroundStyle(.orange)
                    .frame(width: 16)
                Text(item.title)
                    .font(.subheadline.weight(.medium))
                    .fixedSize(horizontal: false, vertical: true)
            }
            Text(item.bodyMarkdown)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.leading, 24)
        }
    }
}

private struct ExternalTrackerGenericAttentionRow: View {
    let item: WorkAttentionItem

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
                    .frame(width: 16)
                Text(item.title)
                    .font(.subheadline.weight(.medium))
                    .fixedSize(horizontal: false, vertical: true)
            }
            Text(item.bodyMarkdown)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.leading, 24)
        }
    }
}
