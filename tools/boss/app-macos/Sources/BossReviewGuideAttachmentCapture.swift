import AppKit
import CryptoKit

/// Seeds only the isolated capture store, using a supplied screenshot as sample
/// evidence. The real thumbnail and detail views read it through their normal cache.
@MainActor
enum BossReviewGuideAttachmentCapture {
    static func attachments(taskId: String, captions overrideCaptions: [String]? = nil) throws -> [AttachmentVM] {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return [] }
        let args = CommandLine.arguments
        let count = args.first { $0.hasPrefix("--capture-attachments=") }
            .flatMap { Int($0.dropFirst("--capture-attachments=".count)) } ?? 0
        guard count > 0 else { return [] }
        guard let path = args.first(where: { $0.hasPrefix("--capture-attachment-image=") })
            .map({ String($0.dropFirst("--capture-attachment-image=".count)) }) else {
            throw NSError(domain: "ReviewGuideCapture", code: 1,
                          userInfo: [NSLocalizedDescriptionKey: "Supply --capture-attachment-image=<png>"])
        }
        let bytes = try Data(contentsOf: URL(fileURLWithPath: path))
        guard let bitmap = NSBitmapImageRep(data: bytes) else {
            throw NSError(domain: "ReviewGuideCapture", code: 2,
                          userInfo: [NSLocalizedDescriptionKey: "Fixture image could not be decoded"])
        }
        let digest = SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
        // Deliberately interleaved input exercises before/after adjacency.
        let captions = overrideCaptions ?? ["Wide layout — after", "Narrow layout — before", "Wide layout — before",
                        "Narrow layout — after", "Additional evidence"]
        let attachments = (0..<count).map { index in
            AttachmentVM(
                id: "capture-attachment-\(index)", executionId: "capture-execution", workItemId: taskId,
                caption: captions[index % captions.count], contentDigest: digest,
                createdAt: "1792497600", mediaType: "png", pixelWidth: bitmap.pixelsWide,
                pixelHeight: bitmap.pixelsHigh, sizeBytes: Int64(bytes.count), sourceName: "sample.png", reclaimedAt: nil
            )
        }
        let url = AttachmentBlobPaths.blobURL(for: attachments[0])
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try bytes.write(to: url)
        return attachments
    }
}
