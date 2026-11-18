import CoreGraphics
import Foundation

/// Layout of the bundled crew portrait sheet,
/// `Sources/Resources/TrekIcons/trek-crew-grid.png`: 56 pixel-art
/// busts in 7 rows × 8 columns. This file is the single name → cell
/// table; `TrekIconAssets` slices portraits out of the sheet at
/// runtime and nothing else knows where a character sits.
///
/// The engine's persona roster is this grid in reading order, minus the
/// coordinator. Order is load-bearing for stable persona leases, so new
/// characters are appended (new row, or the end of the last row), never
/// inserted. `tools/boss/protocol/tests/worker_roster_parity_test.py`
/// keeps this table and the Rust `ROSTER` in step. The app never derives
/// a worker's name from this table; names come from the engine.
enum TrekPortraitGrid {
    /// Pixel size of `trek-crew-grid.png`. `TrekIconAssets` refuses to
    /// slice a sheet of any other size, so a swapped-in image can't
    /// silently show the wrong crop.
    static let imageWidth = 1340
    static let imageHeight = 1174

    /// The coordinator ("Boss") pane's name. It owns a grid portrait
    /// but is reserved: it is never handed out as a worker name.
    static let coordinatorName = "Picard"

    /// Suffix the engine appends to a persona running on a remote host.
    private static let remoteSuffix = " (Remote)"

    /// Character names by row, left to right.
    static let names: [[String]] = [
        ["Riker", "Data", "Worf", "La Forge", "Troi", "Crusher", "Yar", "O'Brien"],
        ["Kira", "Dax", "Bashir", "Odo", "Quark", "Rom", "Nog", "Garak"],
        ["Ezri", "Chakotay", "Tuvok", "Paris", "Kim", "Torres", "Neelix", "Kes"],
        ["Seven", "Doctor", "Guinan", "Pulaski", "Barclay", "Tucker", "Reed", "Sato"],
        ["T'Pol", "Phlox", "Mayweather", "Vash", "Ro", "Shelby", "Brahms", "Sela"],
        ["Picard", "Sisko", "Janeway", "Archer", "Kirk", "Spock", "McCoy", "Scotty"],
        ["Uhura", "Sulu", "Chekov", "Q", "Dukat", "Martok", "Chapel", "Rand"],
    ]

    /// A position in `names` / `cropRects`.
    struct Cell: Hashable {
        let row: Int
        let column: Int
    }

    /// Reading-order names with the coordinator removed: the crew the
    /// idle empty state draws its flavour portraits from.
    static let workerNames: [String] = names.flatMap { $0 }.filter { $0 != coordinatorName }

    private static let cellsByName: [String: Cell] = {
        var cells: [String: Cell] = [:]
        for (row, rowNames) in names.enumerated() {
            for (column, name) in rowNames.enumerated() {
                cells[name] = Cell(row: row, column: column)
            }
        }
        return cells
    }()

    /// Grid name for an engine persona, ignoring only the remote-host
    /// qualifier, so a remote worker is treated as its base name.
    /// `nil` for personas with no portrait ("Ensign 41", "Worker 0", …).
    static func name(forPersona persona: String?) -> String? {
        guard var name = persona else { return nil }
        if name.hasSuffix(remoteSuffix) {
            name = String(name.dropLast(remoteSuffix.count))
        }
        return cellsByName[name] == nil ? nil : name
    }

    static func cell(forPersona persona: String?) -> Cell? {
        name(forPersona: persona).flatMap { cellsByName[$0] }
    }

    /// Pixel rect (top-left origin) of one portrait on the sheet,
    /// measured from the art itself.
    ///
    /// The sheet is not a regular grid: 1340 / 8 and 1174 / 7 are not
    /// whole numbers, the busts sit in uneven columns (shoulders nearly
    /// touch), and each is cut flat along a row baseline that drifts
    /// down the sheet. An even `width / 8` division therefore clips
    /// shoulders and pulls in a neighbour's pixels. Each rect here is
    /// the tight bounding box of one portrait's opaque (alpha ≥ 128)
    /// pixels, found by flood-filling the sheet into its 56 separate
    /// busts; the soft red-tinted fringe around them is
    /// low-alpha and falls outside it. The one place two boxes overlap
    /// (Dukat's right shoulder against Martok's pauldron) is split at
    /// the midpoint of the overlap; `TrekIconAssets.slice` then drops
    /// any opaque pixels outside the slice's own bust (the largest
    /// connected component), so neither crop keeps the neighbour's
    /// sliver.
    static func cropRect(for cell: Cell) -> CGRect {
        let rect = cropRects[cell.row][cell.column]
        return CGRect(x: rect.x, y: rect.y, width: rect.width, height: rect.height)
    }

    struct PixelRect: Equatable {
        let x: Int
        let y: Int
        let width: Int
        let height: Int
    }

    private static func rect(_ x: Int, _ y: Int, _ width: Int, _ height: Int) -> PixelRect {
        PixelRect(x: x, y: y, width: width, height: height)
    }

    /// Same shape as `names`: `cropRects[row][column]`.
    static let cropRects: [[PixelRect]] = [
        [
            rect(11, 7, 159, 173), rect(173, 11, 153, 169), rect(327, 10, 173, 170), rect(504, 14, 159, 166),
            rect(663, 12, 171, 168), rect(836, 9, 167, 171), rect(1006, 14, 154, 166), rect(1162, 12, 167, 168),
        ],
        [
            rect(10, 185, 156, 164), rect(170, 185, 157, 164), rect(332, 186, 159, 163), rect(496, 184, 162, 165),
            rect(662, 193, 168, 156), rect(834, 185, 164, 164), rect(1002, 194, 155, 156), rect(1160, 185, 168, 164),
        ],
        [
            rect(12, 355, 154, 163), rect(169, 357, 161, 161), rect(334, 357, 155, 161), rect(493, 354, 165, 164),
            rect(662, 357, 164, 161), rect(831, 359, 162, 159), rect(996, 353, 171, 165), rect(1171, 360, 153, 158),
        ],
        [
            rect(12, 526, 149, 158), rect(165, 530, 164, 154), rect(331, 524, 169, 160), rect(504, 524, 161, 160),
            rect(669, 530, 160, 154), rect(833, 530, 166, 154), rect(1003, 530, 166, 154), rect(1173, 530, 154, 154),
        ],
        [
            rect(10, 691, 146, 150), rect(160, 690, 170, 151), rect(334, 691, 164, 150), rect(502, 689, 163, 152),
            rect(669, 692, 159, 149), rect(833, 691, 161, 150), rect(998, 692, 166, 149), rect(1166, 691, 165, 150),
        ],
        [
            rect(7, 846, 156, 155), rect(167, 845, 163, 156), rect(333, 844, 160, 157), rect(497, 844, 166, 157),
            rect(667, 844, 162, 157), rect(833, 847, 165, 154), rect(1001, 846, 166, 155), rect(1171, 849, 163, 152),
        ],
        [
            rect(12, 1002, 154, 158), rect(170, 1005, 160, 155), rect(333, 1005, 165, 155), rect(502, 1003, 166, 157),
            rect(671, 1004, 156, 157), rect(827, 1003, 186, 158), rect(1013, 1003, 157, 158), rect(1179, 1003, 148, 158),
        ],
    ]
}
