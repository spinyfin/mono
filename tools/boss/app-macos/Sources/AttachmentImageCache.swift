import AppKit
import ImageIO

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
// ===========================================================================

enum AttachmentImageCache {
    private struct ThumbnailKey: Hashable {
        let digest: String
        let maxPixelSize: Int
    }

    private static let lock = NSLock()
    nonisolated(unsafe) private static var thumbnailCache: [ThumbnailKey: NSImage] = [:]
    nonisolated(unsafe) private static var thumbnailNegativeCache: Set<ThumbnailKey> = []
    nonisolated(unsafe) private static var fullImageCache: [String: NSImage] = [:]
    nonisolated(unsafe) private static var fullImageNegativeCache: Set<String> = []

    /// A downsampled thumbnail for `attachment`, decoded at up to
    /// `maxPixelSize` on its longest side. Cached by content digest, so the
    /// key is stable and collision-free regardless of which row references
    /// the blob.
    static func thumbnail(for attachment: AttachmentVM, maxPixelSize: Int = 80) -> NSImage? {
        let key = ThumbnailKey(digest: attachment.contentDigest, maxPixelSize: maxPixelSize)
        lock.lock()
        if let cached = thumbnailCache[key] {
            lock.unlock()
            return cached
        }
        if thumbnailNegativeCache.contains(key) {
            lock.unlock()
            return nil
        }
        lock.unlock()

        let loaded = loadThumbnail(for: attachment, maxPixelSize: maxPixelSize)

        lock.lock()
        if let loaded {
            thumbnailCache[key] = loaded
        } else {
            thumbnailNegativeCache.insert(key)
        }
        lock.unlock()
        return loaded
    }

    /// The full-resolution image for `attachment`, cached by content
    /// digest. Used by the detail pane, where a large image is legitimately
    /// wanted at full size — but only decoded once, not on every re-render.
    static func fullImage(for attachment: AttachmentVM) -> NSImage? {
        let digest = attachment.contentDigest
        lock.lock()
        if let cached = fullImageCache[digest] {
            lock.unlock()
            return cached
        }
        if fullImageNegativeCache.contains(digest) {
            lock.unlock()
            return nil
        }
        lock.unlock()

        let loaded = NSImage(contentsOf: AttachmentBlobPaths.blobURL(for: attachment))

        lock.lock()
        if let loaded {
            fullImageCache[digest] = loaded
        } else {
            fullImageNegativeCache.insert(digest)
        }
        lock.unlock()
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
}
