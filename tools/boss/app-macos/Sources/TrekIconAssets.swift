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
            keepLargestComponent(buffer, width: width, height: height)
            return context.makeImage()
        }
    }

    /// Clears every opaque pixel not in the slice's largest
    /// 8-connected opaque component. Dukat's and Martok's bounding
    /// boxes overlap, so each crop can contain a sliver of the
    /// neighbouring bust; those slivers are separate components and
    /// are removed here.
    ///
    /// Components are built from horizontal runs of opaque pixels
    /// unioned with touching runs on the row above, rather than
    /// per-pixel fills. Labelling touches hundreds of runs rather than
    /// tens of thousands of pixels; the only per-pixel work is one
    /// alpha read through a raw pointer. The raw-pointer style matters
    /// because the test bundle is built without optimisation, where
    /// per-element Swift array access is slow.
    static func keepLargestComponent(
        _ buffer: UnsafeMutableRawBufferPointer, width: Int, height: Int
    ) {
        guard width > 0, height > 0, buffer.count >= width * height * 4,
              let base = buffer.baseAddress
        else { return }
        let pixels = base.assumingMemoryBound(to: UInt8.self)
        // One entry per run: its row, its first column, one past its last
        // column, and its union-find parent.
        var runRow: [Int] = []
        var runStart: [Int] = []
        var runEnd: [Int] = []
        var parent: [Int] = []
        func find(_ run: Int) -> Int {
            var run = run
            while parent[run] != run {
                parent[run] = parent[parent[run]]
                run = parent[run]
            }
            return run
        }
        var previousRow = 0..<0
        for y in 0..<height {
            let rowBase = y * width * 4
            let firstRunOfRow = runStart.count
            var x = 0
            while x < width {
                if pixels[rowBase + x * 4 + 3] == 0 {
                    x += 1
                    continue
                }
                let start = x
                while x < width, pixels[rowBase + x * 4 + 3] != 0 { x += 1 }
                let run = runStart.count
                runRow.append(y)
                runStart.append(start)
                runEnd.append(x)
                parent.append(run)
                // 8-connectivity: a run on the row above touches this one
                // when the two column spans overlap or sit diagonally
                // adjacent, i.e. the above run starts no later than one
                // past this run's end and ends no earlier than its start.
                for above in previousRow where runStart[above] <= x && runEnd[above] >= start {
                    let aboveRoot = find(above)
                    let root = find(run)
                    if aboveRoot != root { parent[root] = aboveRoot }
                }
            }
            previousRow = firstRunOfRow..<runStart.count
        }
        var sizes = [Int](repeating: 0, count: runStart.count)
        var components = 0
        for run in runStart.indices {
            let root = find(run)
            if sizes[root] == 0 { components += 1 }
            sizes[root] += runEnd[run] - runStart[run]
        }
        guard components > 1,
              let keep = sizes.indices.max(by: { sizes[$0] < sizes[$1] })
        else { return }
        for run in runStart.indices where find(run) != keep {
            let rowBase = runRow[run] * width * 4
            for x in runStart[run]..<runEnd[run] {
                for byte in 0..<4 { pixels[rowBase + x * 4 + byte] = 0 }
            }
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
