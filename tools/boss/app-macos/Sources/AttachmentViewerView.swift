import AppKit
import SwiftUI

// MARK: - AttachmentRow

/// Renders one row of the attachment viewer's screenshot list: a small
/// thumbnail plus the caption (falling back to the source filename) and
/// timestamp.
struct AttachmentRow: View {
    let attachment: AttachmentVM

    var body: some View {
        HStack(spacing: 8) {
            AttachmentThumbnail(attachment: attachment)
                .frame(width: 40, height: 40)
            VStack(alignment: .leading, spacing: 2) {
                Text(attachment.displayTitle)
                    .font(.callout)
                    .lineLimit(1)
                Text(AttachmentDateFormatting.format(attachment.createdAt))
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
        }
        .padding(.vertical, 2)
    }
}

/// Small square thumbnail shared by the list row and the detail pane's
/// missing-blob states. Reads the image straight off disk from the
/// engine's content-addressed store — never through the loopback gallery.
struct AttachmentThumbnail: View {
    let attachment: AttachmentVM

    var body: some View {
        Group {
            if attachment.isReclaimed {
                Image(systemName: "trash")
                    .foregroundStyle(.secondary)
            } else if let image = loadedImage {
                Image(nsImage: image)
                    .resizable()
                    .aspectRatio(contentMode: .fill)
            } else {
                Image(systemName: "photo.badge.exclamationmark")
                    .foregroundStyle(.secondary)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.secondary.opacity(0.1))
        .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
    }

    private var loadedImage: NSImage? {
        NSImage(contentsOf: AttachmentBlobPaths.blobURL(for: attachment))
    }
}

/// Epoch-seconds-as-a-string (the repo's `created_at` convention) rendered
/// as a local date/time, falling back to the raw string if it fails to
/// parse.
enum AttachmentDateFormatting {
    static func format(_ epochSecondsString: String, style: DateFormatter.Style = .short) -> String {
        guard let epoch = TimeInterval(epochSecondsString) else { return epochSecondsString }
        let display = DateFormatter()
        display.dateStyle = style
        display.timeStyle = style
        return display.string(from: Date(timeIntervalSince1970: epoch))
    }
}

// MARK: - AttachmentDetailView

/// Right-pane detail: the full-size image (or an explanatory placeholder
/// when the bytes are gone) plus caption and metadata.
struct AttachmentDetailView: View {
    let attachment: AttachmentVM

    private var loadedImage: NSImage? {
        NSImage(contentsOf: AttachmentBlobPaths.blobURL(for: attachment))
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                imageArea
                if !attachment.caption.isEmpty {
                    Text(attachment.caption)
                        .font(.body)
                }
                metadataRow
            }
            .padding()
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .navigationTitle(attachment.displayTitle)
    }

    @ViewBuilder
    private var imageArea: some View {
        if attachment.isReclaimed {
            ContentUnavailableView {
                Label("Screenshot Reclaimed", systemImage: "trash")
            } description: {
                Text(reclaimedDescription)
            }
            .frame(maxWidth: .infinity, minHeight: 240)
        } else if let image = loadedImage {
            Image(nsImage: image)
                .resizable()
                .aspectRatio(contentMode: .fit)
                .frame(maxWidth: .infinity)
                .background(Color.black.opacity(0.05))
                .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        } else {
            ContentUnavailableView {
                Label("Screenshot Unavailable", systemImage: "photo.badge.exclamationmark")
            } description: {
                Text("The image bytes could not be found on disk.")
            }
            .frame(maxWidth: .infinity, minHeight: 240)
        }
    }

    private var reclaimedDescription: String {
        guard let reclaimedAt = attachment.reclaimedAt else {
            return "This screenshot's bytes were reclaimed by retention. The record is kept for provenance."
        }
        return "This screenshot's bytes were reclaimed by retention on \(AttachmentDateFormatting.format(reclaimedAt, style: .medium)). The record is kept for provenance."
    }

    private var metadataRow: some View {
        HStack(spacing: 12) {
            Text("\(attachment.pixelWidth)×\(attachment.pixelHeight)")
            Text(ByteCountFormatter.string(fromByteCount: attachment.sizeBytes, countStyle: .file))
            Text(AttachmentDateFormatting.format(attachment.createdAt, style: .medium))
        }
        .font(.caption)
        .foregroundStyle(.secondary)
    }
}

// MARK: - AttachmentViewerView

/// Master/detail screenshot viewer: the revision chain's screenshots on the
/// left, grouped and labelled exactly like [[TranscriptViewerView]] — the
/// two viewers over the same chain must never disagree about labelling —
/// and the selected screenshot on the right.
struct AttachmentViewerView: View {
    let ref: AttachmentViewerRef

    @EnvironmentObject private var chatModel: ChatViewModel
    @State private var selectedAttachmentId: String?

    private var attachments: [AttachmentVM] {
        chatModel.attachmentsByTaskID[ref.taskId] ?? []
    }

    private var isLoading: Bool {
        chatModel.attachmentsByTaskID[ref.taskId] == nil
    }

    /// Attachments grouped by task. Returns a single unnamed group when the
    /// list is entirely from the chain root, and multiple labelled groups
    /// when revisions contributed their own screenshots. Mirrors
    /// `TranscriptViewerView.executionGroups` exactly.
    private var attachmentGroups: [AttachmentGroup] {
        let all = attachments
        guard !all.isEmpty else { return [] }

        var byTask: [String: [AttachmentVM]] = [:]
        for attachment in all {
            byTask[attachment.workItemId, default: []].append(attachment)
        }

        let revisions = chatModel.allRevisions(forParentTaskID: ref.taskId)
        var orderedTaskIds: [String] = [ref.taskId]
        orderedTaskIds.append(contentsOf: revisions.map { $0.id })
        for taskId in byTask.keys where !orderedTaskIds.contains(taskId) {
            orderedTaskIds.append(taskId)
        }

        let hasRevisionAttachments = orderedTaskIds.dropFirst().contains {
            byTask[$0] != nil
        }

        return orderedTaskIds.compactMap { taskId -> AttachmentGroup? in
            guard let items = byTask[taskId], !items.isEmpty else { return nil }
            let label: String
            if taskId == ref.taskId {
                label = hasRevisionAttachments ? "Original" : ""
            } else {
                let seq = revisions.first(where: { $0.id == taskId })?.revisionSeq
                label = seq.map { "R\($0)" } ?? "Revision"
            }
            return AttachmentGroup(taskId: taskId, label: label, attachments: items)
        }
    }

    var body: some View {
        NavigationSplitView {
            attachmentList
                .navigationSplitViewColumnWidth(min: 240, ideal: 280, max: 340)
        } detail: {
            detail
        }
        .onAppear {
            chatModel.loadAttachments(taskId: ref.taskId)
        }
        .navigationTitle("Screenshots")
    }

    // MARK: Left pane — screenshot list

    @ViewBuilder
    private var attachmentList: some View {
        Group {
            if isLoading {
                VStack {
                    Spacer()
                    ProgressView()
                    Spacer()
                }
            } else if attachments.isEmpty {
                ContentUnavailableView(
                    "No Screenshots",
                    systemImage: "photo.on.rectangle.angled",
                    description: Text("No evidence has been attached to this task yet.")
                )
            } else {
                List(selection: $selectedAttachmentId) {
                    ForEach(attachmentGroups) { group in
                        if group.label.isEmpty {
                            ForEach(group.attachments) { attachment in
                                AttachmentRow(attachment: attachment).tag(attachment.id)
                            }
                        } else {
                            Section(group.label) {
                                ForEach(group.attachments) { attachment in
                                    AttachmentRow(attachment: attachment).tag(attachment.id)
                                }
                            }
                        }
                    }
                }
            }
        }
        .navigationTitle("Screenshots")
    }

    // MARK: Right pane — selected screenshot

    @ViewBuilder
    private var detail: some View {
        if let id = selectedAttachmentId, let attachment = attachments.first(where: { $0.id == id }) {
            AttachmentDetailView(attachment: attachment)
        } else {
            ContentUnavailableView(
                "No Screenshot Selected",
                systemImage: "photo",
                description: Text("Select a screenshot from the list to view it.")
            )
        }
    }
}
