import CoreGraphics
import ImageIO
import XCTest
@testable import Boss

/// Guards the crew portrait grid: the single name → cell table, the
/// worker crew derived from it, and the slicing of portraits out of
/// the sheet. Parity with the Rust `ROSTER` is enforced separately by
/// `tools/boss/protocol/tests/worker_roster_parity_test.py`.
final class TrekPortraitGridTests: XCTestCase {
    /// The roster as it stood before the grid extension; persona
    /// assignment order depends on it, so these must never move.
    private let firstFortyWorkerNames = [
        "Riker", "Data", "Worf", "La Forge", "Troi", "Crusher", "Yar", "O'Brien",
        "Kira", "Dax", "Bashir", "Odo", "Quark", "Rom", "Nog", "Garak",
        "Ezri", "Chakotay", "Tuvok", "Paris", "Kim", "Torres", "Neelix", "Kes",
        "Seven", "Doctor", "Guinan", "Pulaski", "Barclay", "Tucker", "Reed", "Sato",
        "T'Pol", "Phlox", "Mayweather", "Vash", "Ro", "Shelby", "Brahms", "Sela",
    ]

    func testGridIsSevenRowsByEightColumnsOfFiftySixUniqueNames() {
        XCTAssertEqual(TrekPortraitGrid.names.count, 7)
        XCTAssertTrue(TrekPortraitGrid.names.allSatisfy { $0.count == 8 })
        let all = TrekPortraitGrid.names.flatMap { $0 }
        XCTAssertEqual(all.count, 56)
        XCTAssertEqual(Set(all).count, 56, "every name appears exactly once")
    }

    func testEveryNameMapsToItsOwnCell() {
        var seen = Set<TrekPortraitGrid.Cell>()
        for (row, rowNames) in TrekPortraitGrid.names.enumerated() {
            for (column, name) in rowNames.enumerated() {
                let cell = TrekPortraitGrid.cell(forPersona: name)
                XCTAssertEqual(cell, TrekPortraitGrid.Cell(row: row, column: column), name)
                if let cell { seen.insert(cell) }
            }
        }
        XCTAssertEqual(seen.count, 56)
    }

    func testRemotePersonaUsesBaseNamePortraitAndFallbacksHaveNone() {
        XCTAssertEqual(
            TrekPortraitGrid.cell(forPersona: "Data (Remote)"),
            TrekPortraitGrid.cell(forPersona: "Data")
        )
        XCTAssertEqual(TrekPortraitGrid.name(forPersona: "Martok (Remote)"), "Martok")
        for persona in ["Ensign 41", "Ensign 41 (Remote)", "Worker 0", "", "riker", nil] {
            XCTAssertNil(TrekPortraitGrid.cell(forPersona: persona), persona ?? "nil")
            XCTAssertNil(TrekIconAssets.portrait(forPersona: persona), persona ?? "nil")
        }
    }

    func testWorkerCrewIsGridOrderWithoutTheCoordinator() {
        let expected = TrekPortraitGrid.names.flatMap { $0 }.filter { $0 != "Picard" }
        XCTAssertEqual(TrekPortraitGrid.workerNames, expected)
        XCTAssertEqual(TrekPortraitGrid.workerNames.count, 55)
        XCTAssertFalse(TrekPortraitGrid.workerNames.contains(TrekPortraitGrid.coordinatorName))
        XCTAssertNotNil(TrekPortraitGrid.cell(forPersona: TrekPortraitGrid.coordinatorName))
        XCTAssertEqual(Array(TrekPortraitGrid.workerNames.prefix(40)), firstFortyWorkerNames)
    }

    func testEveryCrewNameHasAPortraitCell() {
        for name in TrekPortraitGrid.workerNames {
            XCTAssertNotNil(TrekPortraitGrid.cell(forPersona: name), name)
        }
    }

    func testCropRectsAreInBoundsAndNeverShareAPixel() {
        let rects = TrekPortraitGrid.cropRects
        XCTAssertEqual(rects.count, 7)
        XCTAssertTrue(rects.allSatisfy { $0.count == 8 })
        let bounds = CGRect(
            x: 0, y: 0, width: TrekPortraitGrid.imageWidth, height: TrekPortraitGrid.imageHeight
        )
        var all: [(TrekPortraitGrid.Cell, CGRect)] = []
        for row in 0..<7 {
            for column in 0..<8 {
                let cell = TrekPortraitGrid.Cell(row: row, column: column)
                let rect = TrekPortraitGrid.cropRect(for: cell)
                XCTAssertTrue(bounds.contains(rect), "r\(row) c\(column) leaves the sheet")
                XCTAssertGreaterThan(rect.width, 100)
                XCTAssertGreaterThan(rect.height, 100)
                all.append((cell, rect))
            }
        }
        for (i, first) in all.enumerated() {
            for second in all[(i + 1)...] {
                XCTAssertTrue(
                    first.1.intersection(second.1).isNull
                        || first.1.intersection(second.1).isEmpty,
                    "\(first.0) overlaps \(second.0)"
                )
            }
        }
    }

    // MARK: - Slicing

    /// A synthetic sheet: every crop rect filled opaque red over a
    /// low-alpha red-tinted background standing in for the fringe.
    private func makeSheet(width: Int, height: Int) throws -> CGImage {
        let space = try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB))
        let context = try XCTUnwrap(CGContext(
            data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
            space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ))
        context.setFillColor(CGColor(red: 0.3, green: 0, blue: 0, alpha: 0.3))
        context.fill(CGRect(x: 0, y: 0, width: width, height: height))
        context.setFillColor(CGColor(red: 1, green: 0, blue: 0, alpha: 1))
        for row in TrekPortraitGrid.cropRects {
            for rect in row {
                // CGContext is bottom-left origin; the sheet's rects are top-left.
                context.fill(CGRect(
                    x: rect.x, y: height - rect.y - rect.height,
                    width: rect.width, height: rect.height
                ))
            }
        }
        return try XCTUnwrap(context.makeImage())
    }

    func testSliceIsExactlyTheCropRectAndKeepsEveryPortraitPixel() throws {
        let sheet = try makeSheet(
            width: TrekPortraitGrid.imageWidth, height: TrekPortraitGrid.imageHeight
        )
        let cell = TrekPortraitGrid.Cell(row: 6, column: 5) // Martok: the widest box
        let rect = TrekPortraitGrid.cropRect(for: cell)
        let slice = try XCTUnwrap(TrekIconAssets.slice(sheet, cell: cell))
        XCTAssertEqual(slice.width, Int(rect.width))
        XCTAssertEqual(slice.height, Int(rect.height))
        XCTAssertEqual(try alphaRange(of: slice).min, 255, "every pixel inside the crop is kept")
    }

    func testSliceClearsLowAlphaFringeInsideTheCrop() throws {
        // A sheet that is *only* fringe: every pixel below the cutoff.
        let space = try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB))
        let context = try XCTUnwrap(CGContext(
            data: nil, width: TrekPortraitGrid.imageWidth, height: TrekPortraitGrid.imageHeight,
            bitsPerComponent: 8, bytesPerRow: 0, space: space,
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ))
        context.setFillColor(CGColor(red: 0.4, green: 0, blue: 0, alpha: 0.2))
        context.fill(CGRect(
            x: 0, y: 0, width: TrekPortraitGrid.imageWidth, height: TrekPortraitGrid.imageHeight
        ))
        let sheet = try XCTUnwrap(context.makeImage())
        let slice = try XCTUnwrap(
            TrekIconAssets.slice(sheet, cell: TrekPortraitGrid.Cell(row: 0, column: 0))
        )
        XCTAssertEqual(try alphaRange(of: slice).max, 0)
    }

    func testSliceDropsANeighbouringBustsOpaquePixels() throws {
        let width = TrekPortraitGrid.imageWidth, height = TrekPortraitGrid.imageHeight
        let space = try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB))
        let context = try XCTUnwrap(CGContext(
            data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
            space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ))
        context.setFillColor(CGColor(red: 1, green: 0, blue: 0, alpha: 1))
        let cell = TrekPortraitGrid.Cell(row: 0, column: 0)
        let rect = TrekPortraitGrid.cropRect(for: cell)
        // Own bust: most of the crop. Neighbour sliver: a detached 5px strip at its right edge.
        context.fill(CGRect(
            x: rect.minX, y: CGFloat(height) - rect.maxY, width: rect.width - 20, height: rect.height
        ))
        context.fill(CGRect(
            x: rect.maxX - 5, y: CGFloat(height) - rect.maxY, width: 5, height: rect.height
        ))
        let slice = try XCTUnwrap(
            TrekIconAssets.slice(try XCTUnwrap(context.makeImage()), cell: cell)
        )
        let alphas = try alphaColumns(of: slice)
        XCTAssertEqual(alphas.last, 0, "detached sliver is cleared")
        XCTAssertEqual(alphas.first, 255, "own bust is kept")
    }

    /// Against the real bundled sheet: every sliced portrait is exactly
    /// one connected bust, so no slice (Dukat / Martok especially)
    /// keeps a neighbour's pixels.
    func testEveryRealPortraitIsASingleConnectedBust() throws {
        let sheet = try realSheet()
        for row in 0..<TrekPortraitGrid.names.count {
            for column in 0..<TrekPortraitGrid.names[row].count {
                let cell = TrekPortraitGrid.Cell(row: row, column: column)
                let image = try XCTUnwrap(
                    TrekIconAssets.slice(sheet, cell: cell),
                    "no portrait for row \(row) column \(column)"
                )
                XCTAssertEqual(
                    try opaqueComponentCount(of: image), 1,
                    "row \(row) column \(column) has stray opaque pixels"
                )
            }
        }
    }

    /// The bundled sheet: from Bazel runfiles under `bazel test`, else
    /// relative to this source file (SwiftPM / Xcode).
    private func realSheet() throws -> CGImage {
        let relative = "Sources/Resources/TrekIcons/trek-crew-grid.png"
        let env = ProcessInfo.processInfo.environment
        var candidates = [
            URL(fileURLWithPath: #filePath)
                .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
                .appendingPathComponent(relative),
        ]
        if let srcdir = env["TEST_SRCDIR"], let workspace = env["TEST_WORKSPACE"] {
            candidates.insert(
                URL(fileURLWithPath: srcdir).appendingPathComponent(workspace)
                    .appendingPathComponent("tools/boss/app-macos").appendingPathComponent(relative),
                at: 0
            )
        }
        let url = try XCTUnwrap(
            candidates.first { FileManager.default.fileExists(atPath: $0.path) },
            "trek-crew-grid.png not found"
        )
        let source = try XCTUnwrap(CGImageSourceCreateWithURL(url as CFURL, nil))
        return try XCTUnwrap(CGImageSourceCreateImageAtIndex(source, 0, nil))
    }

    /// Counts 8-connected opaque components with a pixel flood fill —
    /// deliberately a different algorithm from the run-based labelling in
    /// `TrekIconAssets.keepLargestComponent`, so the two cross-check each
    /// other. Bookkeeping lives in raw buffers and plain `while` loops:
    /// the test bundle is compiled without optimisation, where each Swift
    /// array access costs on the order of a microsecond, and the earlier
    /// array-based fill took over 30 s across the 56 real portraits.
    private func opaqueComponentCount(of image: CGImage) throws -> Int {
        let width = image.width, height = image.height
        let pixelCount = width * height
        guard pixelCount > 0 else { return 0 }
        var pixels = [UInt8](repeating: 0, count: pixelCount * 4)
        let space = try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB))
        try pixels.withUnsafeMutableBytes { buffer in
            let context = try XCTUnwrap(CGContext(
                data: buffer.baseAddress, width: width, height: height, bitsPerComponent: 8,
                bytesPerRow: width * 4, space: space,
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
            ))
            context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        }
        let seen = UnsafeMutablePointer<Bool>.allocate(capacity: pixelCount)
        seen.initialize(repeating: false, count: pixelCount)
        defer { seen.deallocate() }
        // Every pixel is pushed at most once (it is marked seen as it is
        // pushed), so the stack never needs more than one slot per pixel.
        let stack = UnsafeMutablePointer<Int>.allocate(capacity: pixelCount)
        defer { stack.deallocate() }
        return pixels.withUnsafeBufferPointer { buffer -> Int in
            let alpha = buffer.baseAddress!
            var components = 0
            var start = 0
            while start < pixelCount {
                defer { start += 1 }
                if seen[start] || alpha[start * 4 + 3] == 0 { continue }
                components += 1
                seen[start] = true
                var top = 1
                stack[0] = start
                while top > 0 {
                    top -= 1
                    let index = stack[top]
                    let x = index % width, y = index / width
                    var ny = max(y - 1, 0)
                    while ny <= min(y + 1, height - 1) {
                        var nx = max(x - 1, 0)
                        while nx <= min(x + 1, width - 1) {
                            let n = ny * width + nx
                            if !seen[n], alpha[n * 4 + 3] != 0 {
                                seen[n] = true
                                stack[top] = n
                                top += 1
                            }
                            nx += 1
                        }
                        ny += 1
                    }
                }
            }
            return components
        }
    }

    private func alphaColumns(of image: CGImage) throws -> [UInt8] {
        let width = image.width, height = image.height
        var pixels = [UInt8](repeating: 0, count: width * height * 4)
        let space = try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB))
        try pixels.withUnsafeMutableBytes { buffer in
            let context = try XCTUnwrap(CGContext(
                data: buffer.baseAddress, width: width, height: height, bitsPerComponent: 8,
                bytesPerRow: width * 4, space: space,
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
            ))
            context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        }
        // Alpha of the middle-row pixel in each column.
        return (0..<width).map { pixels[((height / 2) * width + $0) * 4 + 3] }
    }

    func testSliceRejectsASheetOfTheWrongSize() throws {
        let sheet = try makeSheet(width: 1340 / 2, height: 1174 / 2)
        XCTAssertNil(TrekIconAssets.slice(sheet, cell: TrekPortraitGrid.Cell(row: 0, column: 0)))
    }

    private func alphaRange(of image: CGImage) throws -> (min: UInt8, max: UInt8) {
        let width = image.width, height = image.height
        var pixels = [UInt8](repeating: 0, count: width * height * 4)
        let space = try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB))
        try pixels.withUnsafeMutableBytes { buffer in
            let context = try XCTUnwrap(CGContext(
                data: buffer.baseAddress, width: width, height: height, bitsPerComponent: 8,
                bytesPerRow: width * 4, space: space,
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
            ))
            context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        }
        let alphas = stride(from: 3, to: pixels.count, by: 4).map { pixels[$0] }
        return (alphas.min() ?? 0, alphas.max() ?? 0)
    }

    // MARK: - Idle flavor

    func testIdleFlavorCoversEveryGridName() {
        for name in TrekPortraitGrid.names.flatMap({ $0 }) {
            XCTAssertFalse(TrekIdleFlavor.line(for: name, cycle: 0).isEmpty, name)
        }
        XCTAssertTrue(TrekIdleFlavor.line(for: "Kirk", cycle: 3).contains("Kirk"))
        XCTAssertEqual(
            TrekIdleFlavor.line(for: "Riker", cycle: 0),
            TrekIdleFlavor.lines(for: "Riker")[0]
        )
    }
}
