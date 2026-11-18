import Foundation

enum AttachmentImageSizing {
    /// Fit both viewport dimensions without enlarging the image's native point size.
    static func fit(native: CGSize, available: CGSize) -> CGSize {
        guard native.width > 0, native.height > 0 else { return .zero }
        let scale = max(0, min(1, available.width / native.width, available.height / native.height))
        return CGSize(width: native.width * scale, height: native.height * scale)
    }
}
