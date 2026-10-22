import SwiftUI

/// App-only evidence beside the guide. Never adds local paths or gallery URLs
/// to the markdown that can be copied, exported, or posted to GitHub.
struct ReviewGuideAttachments: View {
    @ObservedObject var chatModel: ChatViewModel
    let rootTaskId: String
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text("Registered screenshots").font(.caption.weight(.semibold))
                Spacer()
                Button("Refresh") { chatModel.loadAttachments(taskId: rootTaskId) }
                    .controlSize(.mini)
                    .disabled(chatModel.attachmentsByTaskID[rootTaskId] == nil
                        && chatModel.attachmentsLoadFailureByTaskID[rootTaskId] == nil)
            }
            if let failure = chatModel.attachmentsLoadFailureByTaskID[rootTaskId] {
                HStack {
                    Text(failure).foregroundStyle(.secondary)
                    Button("Retry") { chatModel.loadAttachments(taskId: rootTaskId) }
                }
                .font(.caption)
            } else if let attachments = chatModel.attachmentsByTaskID[rootTaskId] {
                if attachments.isEmpty {
                    Text("No screenshots registered for this review chain.")
                        .font(.caption).foregroundStyle(.secondary)
                } else {
                    ScrollView(.horizontal) {
                        HStack(alignment: .top, spacing: 10) {
                            ForEach(ReviewGuideAttachmentOrdering.arrange(attachments)) { attachment in
                                Button {
                                    chatModel.selectedAttachmentIDByTaskID[rootTaskId] = attachment.id
                                    openWindow(id: "attachment-viewer", value: AttachmentViewerRef(taskId: rootTaskId))
                                } label: {
                                    VStack(alignment: .leading, spacing: 4) {
                                        AttachmentThumbnail(attachment: attachment, maxPixelSize: 320, contentMode: .fit)
                                            .frame(width: 160, height: 90)
                                        Text(attachment.displayTitle)
                                            .font(.caption).lineLimit(2)
                                            .frame(width: 160, alignment: .leading)
                                    }
                                    .contentShape(Rectangle())
                                }
                                .buttonStyle(.plain)
                                .help(attachment.displayTitle)
                                .accessibilityLabel("Open screenshot: \(attachment.displayTitle)")
                            }
                        }
                        .padding(.bottom, 4)
                    }
                    .fixedSize(horizontal: false, vertical: true)
                }
            } else {
                HStack {
                    ProgressView().controlSize(.mini)
                    Text("Loading screenshots…").font(.caption).foregroundStyle(.secondary)
                }
            }
        }
        .accessibilityIdentifier("review-guide-screenshots")
    }
}
