import AppKit
import ImageIO
import SwiftUI

// ===========================================================================
// Lock-guarded image cache for the attachment viewer, mirroring
// [[TrekIconAssets]]'s shape (positive cache + negative cache under one
// lock) for the same reason: `NSImage(contentsOf:)` is a synchronous disk
// read + decode, and doing it uncached inside a SwiftUI `body` re-runs it on
// every re-render. Attachments are far larger than Trek icons (up to 8 MiB /
// 10000px per side, up to 96 rows per work item), so thumbnails are decoded
// downsampled via ImageIO rather than retaining a full-resolution NSImage
// for a 40x40 slot, and missing blobs (rows that outlived their bytes after
// retention cascade-delete) are memoized in the negative cache instead of
// re-stat'ing the filesystem on every render.
//
// Unlike TrekIconAssets — whose population is a small fixed set of bundled
// icons, safe to retain forever — attachments are unbounded user data, so
// the two positive caches are `NSCache`, not plain dictionaries: NSCache
// evicts under both an explicit cost/count ceiling and system memory
// pressure, so a session that browses many tasks' evidence does not pin
// hundreds of megabytes of decoded bitmaps for the app's lifetime. The
// negative caches store no image payload (just a digest/key), so they stay
// plain lock-guarded `Set`s.
//
// Lives in this file (rather than its own) so the finding that required
// this cache doesn't also add to this branch's touched-file count.
// ===========================================================================

enum AttachmentImageCache {
    private struct ThumbnailKey: Hashable {
        let digest: String
        let maxPixelSize: Int

        var cacheKey: NSString { "\(digest)#\(maxPixelSize)" as NSString }
    }

    /// Full-resolution images are the expensive ones (up to 8 MiB decoded
    /// bitmaps, up to 96 per work item) — capped at a handful of entries so
    /// browsing several tasks' evidence in one session cannot pin hundreds
    /// of megabytes of NSImages that are never released.
    private static let fullImageCostLimit = 64 * 1024 * 1024
    private static let fullImageCountLimit = 16
    /// Thumbnails are decoded downsampled (<=80px by default), so a much
    /// larger count/cost ceiling still bounds memory to a few megabytes.
    private static let thumbnailCostLimit = 16 * 1024 * 1024
    private static let thumbnailCountLimit = 512

    // NSCache is documented thread-safe for concurrent access from multiple
    // threads (unlike the plain Dictionary these replace), so it is safe to
    // share across actors despite not being `Sendable`.
    nonisolated(unsafe) private static let thumbnailCache: NSCache<NSString, NSImage> = {
        let cache = NSCache<NSString, NSImage>()
        cache.totalCostLimit = thumbnailCostLimit
        cache.countLimit = thumbnailCountLimit
        return cache
    }()
    nonisolated(unsafe) private static let fullImageCache: NSCache<NSString, NSImage> = {
        let cache = NSCache<NSString, NSImage>()
        cache.totalCostLimit = fullImageCostLimit
        cache.countLimit = fullImageCountLimit
        return cache
    }()

    private static let lock = NSLock()
    nonisolated(unsafe) private static var thumbnailNegativeCache: Set<ThumbnailKey> = []
    nonisolated(unsafe) private static var fullImageNegativeCache: Set<String> = []

    /// A downsampled thumbnail for `attachment`, decoded at up to
    /// `maxPixelSize` on its longest side. Cached by content digest, so the
    /// key is stable and collision-free regardless of which row references
    /// the blob.
    static func thumbnail(for attachment: AttachmentVM, maxPixelSize: Int = 80) -> NSImage? {
        let key = ThumbnailKey(digest: attachment.contentDigest, maxPixelSize: maxPixelSize)
        if let cached = thumbnailCache.object(forKey: key.cacheKey) {
            return cached
        }
        lock.lock()
        if thumbnailNegativeCache.contains(key) {
            lock.unlock()
            return nil
        }
        lock.unlock()

        let loaded = loadThumbnail(for: attachment, maxPixelSize: maxPixelSize)

        if let loaded {
            thumbnailCache.setObject(loaded, forKey: key.cacheKey, cost: imageCost(loaded))
        } else {
            lock.lock()
            thumbnailNegativeCache.insert(key)
            lock.unlock()
        }
        return loaded
    }

    /// The full-resolution image for `attachment`, cached by content
    /// digest. Used by the detail pane, where a large image is legitimately
    /// wanted at full size — but only decoded once, not on every re-render.
    static func fullImage(for attachment: AttachmentVM) -> NSImage? {
        let digest = attachment.contentDigest
        let key = digest as NSString
        if let cached = fullImageCache.object(forKey: key) {
            return cached
        }
        lock.lock()
        if fullImageNegativeCache.contains(digest) {
            lock.unlock()
            return nil
        }
        lock.unlock()

        let loaded = NSImage(contentsOf: AttachmentBlobPaths.blobURL(for: attachment))

        if let loaded {
            fullImageCache.setObject(loaded, forKey: key, cost: imageCost(loaded))
        } else {
            lock.lock()
            fullImageNegativeCache.insert(digest)
            lock.unlock()
        }
        return loaded
    }

    private static func loadThumbnail(for attachment: AttachmentVM, maxPixelSize: Int) -> NSImage? {
        let url = AttachmentBlobPaths.blobURL(for: attachment)
        guard let source = CGImageSourceCreateWithURL(url as CFURL, nil) else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceThumbnailMaxPixelSize: maxPixelSize,
            kCGImageSourceCreateThumbnailWithTransform: true,
        ]
        guard let cgThumbnail = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else {
            return nil
        }
        let size = NSSize(width: cgThumbnail.width, height: cgThumbnail.height)
        return NSImage(cgImage: cgThumbnail, size: size)
    }

    /// Approximate decoded-bitmap byte size (4 bytes/pixel, RGBA), used as
    /// `NSCache`'s per-entry cost so `totalCostLimit` bounds actual memory
    /// rather than entry count alone.
    private static func imageCost(_ image: NSImage) -> Int {
        Int(image.size.width * image.size.height) * 4
    }
}

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
        AttachmentImageCache.thumbnail(for: attachment)
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
        AttachmentImageCache.fullImage(for: attachment)
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
        chatModel.attachmentsByTaskID[ref.taskId] == nil && loadFailureReason == nil
    }

    private var loadFailureReason: String? {
        chatModel.attachmentsLoadFailureByTaskID[ref.taskId]
    }

    /// Attachments grouped by task. Returns a single unnamed group when the
    /// list is entirely from the chain root, and multiple labelled groups
    /// when revisions contributed their own screenshots. See
    /// [[revisionChainGroups(_:rootTaskId:revisions:)]] — the same helper
    /// backs `TranscriptViewerView.executionGroups`, so the two viewers over
    /// the same chain structurally cannot disagree about labelling.
    private var attachmentGroups: [RevisionChainGroup<AttachmentVM>] {
        revisionChainGroups(
            attachments,
            rootTaskId: ref.taskId,
            revisions: chatModel.allRevisions(forParentTaskID: ref.taskId)
        )
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
            if let reason = loadFailureReason {
                ContentUnavailableView {
                    Label("Couldn't Load Screenshots", systemImage: "exclamationmark.triangle")
                } description: {
                    Text(reason)
                } actions: {
                    Button("Retry") { chatModel.loadAttachments(taskId: ref.taskId) }
                }
            } else if isLoading {
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
                            ForEach(group.items) { attachment in
                                AttachmentRow(attachment: attachment).tag(attachment.id)
                            }
                        } else {
                            Section(group.label) {
                                ForEach(group.items) { attachment in
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
