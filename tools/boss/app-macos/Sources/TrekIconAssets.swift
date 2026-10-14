import AppKit
import CoreGraphics
import Foundation
import ImageIO

/// Crew portraits, sliced at runtime from the single bundled sheet
/// `Sources/Resources/TrekIcons/trek-crew-grid.png` (layout and crop
/// rects: `TrekPortraitGrid`). SwiftPM builds access it via
/// `Bundle.module`; Bazel builds find it in `Bundle.main` because the
/// Bazel `macos_application` places it at
/// `Contents/Resources/TrekIcons/` via `additional_contents`.
///
/// Every lookup takes the persona name the engine shows (`"Data"`,
/// `"Data (Remote)"`, the coordinator's `"Picard"`). A name with no
/// grid cell, or a sheet that can't be loaded, yields `nil` and the
/// caller keeps its generic icon — nothing here can crash or hand
/// back a blank image.
///
/// Draw the result with `.interpolation(.none)` so the pixel art stays
/// crisp when SwiftUI scales it.
enum TrekIconAssets {
    /// Pixels below this alpha are cleared from a slice. The sheet is
    /// ringed with a soft, low-alpha, red-tinted fringe that reads as
    /// speckle on a dark pane background; the portrait itself is
    /// well above this.
    static let fringeAlphaCutoff: UInt8 = 128

    // Decoded NSImages are immutable for our consumers (they only
    // render via SwiftUI `Image(nsImage:)`), so handing the same
    // instance to many views is safe. There are at most 56 cells —
    // no eviction needed.
    private static let lock = NSLock()
    nonisolated(unsafe) private static var cache: [TrekPortraitGrid.Cell: NSImage] = [:]
    nonisolated(unsafe) private static var negativeCache: Set<TrekPortraitGrid.Cell> = []
    nonisolated(unsafe) private static var cachedSheet: CGImage?
    nonisolated(unsafe) private static var sheetLoaded = false

    /// Portrait for an engine persona name, or `nil` when the name has
    /// no grid cell (e.g. the "Worker N" / "Ensign N" fallbacks).
    static func portrait(forPersona persona: String?) -> NSImage? {
        guard let cell = TrekPortraitGrid.cell(forPersona: persona) else { return nil }
        return portrait(for: cell)
    }

    /// The coordinator ("Boss") pane's portrait.
    static func coordinatorPortrait() -> NSImage? {
        portrait(forPersona: TrekPortraitGrid.coordinatorName)
    }

    static func portrait(for cell: TrekPortraitGrid.Cell) -> NSImage? {
        lock.lock()
        if let cached = cache[cell] {
            lock.unlock()
            return cached
        }
        if negativeCache.contains(cell) {
            lock.unlock()
            return nil
        }
        lock.unlock()

        var loaded: NSImage?
        if let sheet = loadSheet(), let slice = slice(sheet, cell: cell) {
            loaded = NSImage(cgImage: slice, size: NSSize(width: slice.width, height: slice.height))
        }

        lock.lock()
        if let loaded {
            cache[cell] = loaded
        } else {
            negativeCache.insert(cell)
        }
        lock.unlock()
        return loaded
    }

    /// Crops one portrait out of the sheet and clears the fringe.
    /// `nil` unless `sheet` is the expected size, so a replaced or
    /// resampled image degrades to the generic icon instead of
    /// showing misaligned crops.
    static func slice(_ sheet: CGImage, cell: TrekPortraitGrid.Cell) -> CGImage? {
        guard sheet.width == TrekPortraitGrid.imageWidth,
              sheet.height == TrekPortraitGrid.imageHeight,
              let cropped = sheet.cropping(to: TrekPortraitGrid.cropRect(for: cell))
        else { return nil }
        return clearingFringe(of: cropped)
    }

    private static func clearingFringe(of image: CGImage) -> CGImage? {
        let width = image.width
        let height = image.height
        let bytesPerRow = width * 4
        var pixels = [UInt8](repeating: 0, count: bytesPerRow * height)
        return pixels.withUnsafeMutableBytes { buffer -> CGImage? in
            guard let colorSpace = CGColorSpace(name: CGColorSpace.sRGB),
                  let context = CGContext(
                      data: buffer.baseAddress, width: width, height: height,
                      bitsPerComponent: 8, bytesPerRow: bytesPerRow, space: colorSpace,
                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
                  )
            else { return nil }
            context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
            // Premultiplied RGBA: a fully transparent pixel is four zero bytes.
            for offset in stride(from: 0, to: buffer.count, by: 4)
            where buffer[offset + 3] < fringeAlphaCutoff {
                buffer[offset] = 0
                buffer[offset + 1] = 0
                buffer[offset + 2] = 0
                buffer[offset + 3] = 0
            }
            return context.makeImage()
        }
    }

    /// Decodes the bundled sheet once; a missing or undecodable file is
    /// remembered as `nil` so every later lookup falls back cheaply.
    private static func loadSheet() -> CGImage? {
        lock.lock()
        defer { lock.unlock() }
        if sheetLoaded { return cachedSheet }
        sheetLoaded = true
        #if SWIFT_PACKAGE
        let bundle = Bundle.module
        #else
        let bundle = Bundle.main
        #endif
        if let url = bundle.url(forResource: "TrekIcons/trek-crew-grid", withExtension: "png"),
           let source = CGImageSourceCreateWithURL(url as CFURL, nil) {
            cachedSheet = CGImageSourceCreateImageAtIndex(source, 0, nil)
        }
        return cachedSheet
    }
}
