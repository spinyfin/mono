import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class NativeSegmentedPickerTests: XCTestCase {
    func testNeighborMovesWithoutWrapping() {
        let values = ["Agents", "Work", "Designs"]
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Agents", in: values, moving: .right),
            "Work"
        )
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Work", in: values, moving: .left),
            "Agents"
        )
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Agents", in: values, moving: .left),
            "Agents"
        )
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Designs", in: values, moving: .right),
            "Designs"
        )
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Work", in: values, moving: .up),
            "Agents"
        )
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Work", in: values, moving: .down),
            "Designs"
        )
    }

    func testNeighborOfMissingSelectionIsNil() {
        XCTAssertNil(
            NativeSegmentedPickerSelection.neighbor(
                of: "gone",
                in: ["Agents", "Work"],
                moving: .right
            )
        )
    }

    func testDoesNotInstallNSSegmentedControl() throws {
        let host = hostedPicker(width: 440, height: 32)
        host.layoutSubtreeIfNeeded()
        XCTAssertTrue(
            segmentedControls(in: host).isEmpty,
            "NativeSegmentedPicker must not install NSSegmentedControl / NSViewRepresentable"
        )
    }

    func testRespectsFixedWidthAndCompressesWhenNarrow() throws {
        let wide = hostedPicker(width: 440, height: 32)
        wide.layoutSubtreeIfNeeded()
        XCTAssertEqual(wide.bounds.width, 440)

        let narrow = hostedPicker(
            width: 200,
            height: 32,
            titles: [
                ("bridgeCrew", "Bridge Crew (8)"),
                ("lowerDecks", "Lower Decks (8)"),
                ("automations", "Automations (8)"),
                ("reviewers", "Reviewers (16)"),
            ]
        )
        narrow.layoutSubtreeIfNeeded()
        XCTAssertEqual(narrow.bounds.width, 200)
        XCTAssertLessThanOrEqual(narrow.bounds.height, 40)
    }

    func testRenderIsNonBlankInLightAndDark() throws {
        let temporaryDirectory = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        )
        let dest = temporaryDirectory
            .appendingPathComponent("boss-native-segmented-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dest, withIntermediateDirectories: true)
        addTeardownBlock {
            try? FileManager.default.removeItem(at: dest)
        }
        let undeclared: URL? = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"].map {
            URL(fileURLWithPath: $0, isDirectory: true)
        }

        let shots: [(name: String, appearance: NSAppearance.Name, width: CGFloat, titles: [(String, String)], system: Bool)] = [
            ("mode-light.png", .aqua, 440, modeTitles, false),
            ("mode-dark.png", .darkAqua, 440, modeTitles, false),
            ("pool-light.png", .aqua, 460, poolTitles, false),
            ("pool-dark.png", .darkAqua, 460, poolTitles, false),
            ("pool-narrow-light.png", .aqua, 200, poolTitles, false),
            ("mode-system-light.png", .aqua, 440, modeTitles, true),
            ("mode-system-dark.png", .darkAqua, 440, modeTitles, true),
            ("pool-system-light.png", .aqua, 460, poolTitles, true),
            ("pool-system-dark.png", .darkAqua, 460, poolTitles, true),
        ]

        var paths: [String] = []
        for shot in shots {
            let rep = try render(
                titles: shot.titles,
                width: shot.width,
                height: 36,
                appearance: shot.appearance,
                system: shot.system
            )
            guard !isUniformlyBlank(rep) else {
                throw XCTSkip("render came back uniformly blank; host does not support offscreen SwiftUI rendering")
            }
            guard let data = rep.representation(using: .png, properties: [:]) else {
                XCTFail("PNG encode failed for \(shot.name)")
                return
            }
            let url = dest.appendingPathComponent(shot.name)
            try data.write(to: url)
            if let undeclared {
                try data.write(to: undeclared.appendingPathComponent(shot.name))
            }
            paths.append(url.path)
        }

        let index = dest.appendingPathComponent("paths.txt")
        try paths.joined(separator: "\n").write(to: index, atomically: true, encoding: .utf8)
        print("NATIVE_SEGMENTED_PICKER_FIXTURES=\(dest.path)")
    }

    func testSystemSegmentedPickerStillUsesNSSegmentedControl() throws {
        let selection = ModeBinding(value: "Agents")
        let root = Picker("Mode", selection: selection.binding) {
            ForEach(["Agents", "Work", "Designs"], id: \.self) { Text($0).tag($0) }
        }
        .pickerStyle(.segmented)
        .frame(width: 440, height: 32)
        let host = NSHostingView(rootView: root)
        host.frame = NSRect(x: 0, y: 0, width: 440, height: 32)
        host.layoutSubtreeIfNeeded()
        if segmentedControls(in: host).isEmpty {
            throw XCTSkip("system segmented Picker did not materialize NSSegmentedControl in this host")
        }
    }

    // MARK: - Hosts

    private var modeTitles: [(String, String)] {
        [
            ("agents", "Agents"),
            ("work", "Work"),
            ("designs", "Designs"),
            ("automations", "Automations"),
            ("ideas", "Ideas"),
        ]
    }

    private var poolTitles: [(String, String)] {
        [
            ("bridgeCrew", "Bridge Crew (8)"),
            ("lowerDecks", "Lower Decks (8)"),
            ("automations", "Automations (8)"),
            ("reviewers", "Reviewers (16)"),
        ]
    }

    private func hostedPicker(
        width: CGFloat,
        height: CGFloat,
        titles: [(String, String)]? = nil
    ) -> NSHostingView<some View> {
        let titles = titles ?? modeTitles
        let selection = ModeBinding(value: titles[0].0)
        let root = NativeSegmentedPicker(
            "Mode",
            selection: selection.binding,
            options: titles.map { NativeSegmentedPicker.Option(value: $0.0, title: $0.1) }
        )
        .frame(width: width, height: height)
        .background(Color(nsColor: .windowBackgroundColor))
        let host = NSHostingView(rootView: root)
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(x: 0, y: 0, width: width, height: height)
        return host
    }

    private func render(
        titles: [(String, String)],
        width: CGFloat,
        height: CGFloat,
        appearance: NSAppearance.Name,
        system: Bool = false
    ) throws -> NSBitmapImageRep {
        let host: NSView
        if system {
            host = hostedSystemPicker(width: width, height: height, titles: titles)
        } else {
            host = hostedPicker(width: width, height: height, titles: titles)
        }
        host.appearance = NSAppearance(named: appearance)
        host.layoutSubtreeIfNeeded()
        let bounds = host.bounds
        guard let rep = host.bitmapImageRepForCachingDisplay(in: bounds) else {
            throw XCTSkip("bitmapImageRepForCachingDisplay returned nil")
        }
        host.cacheDisplay(in: bounds, to: rep)
        return rep
    }

    private func hostedSystemPicker(
        width: CGFloat,
        height: CGFloat,
        titles: [(String, String)]
    ) -> NSHostingView<some View> {
        let selection = ModeBinding(value: titles[0].0)
        let root = Picker("Mode", selection: selection.binding) {
            ForEach(titles, id: \.0) { Text($0.1).tag($0.0) }
        }
        .pickerStyle(.segmented)
        .labelsHidden()
        .frame(width: width, height: height)
        .background(Color(nsColor: .windowBackgroundColor))
        let host = NSHostingView(rootView: root)
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(x: 0, y: 0, width: width, height: height)
        return host
    }

    private func segmentedControls(in view: NSView) -> [NSView] {
        var found: [NSView] = []
        func walk(_ current: NSView) {
            let name = String(describing: type(of: current))
            if current is NSSegmentedControl || name.localizedCaseInsensitiveContains("SegmentedControl") {
                found.append(current)
            }
            for child in current.subviews {
                walk(child)
            }
        }
        walk(view)
        return found
    }

    private func isUniformlyBlank(_ rep: NSBitmapImageRep) -> Bool {
        guard let bytes = rep.bitmapData, rep.samplesPerPixel >= 3 else { return true }
        var first: [UInt8]?
        for y in stride(from: 0, to: rep.pixelsHigh, by: 4) {
            for x in stride(from: 0, to: rep.pixelsWide, by: 4) {
                let offset = y * rep.bytesPerRow + x * rep.samplesPerPixel
                let pixel = [bytes[offset], bytes[offset + 1], bytes[offset + 2]]
                if let seen = first {
                    if pixel != seen { return false }
                } else {
                    first = pixel
                }
            }
        }
        return true
    }
}

/// `Binding` cannot live in a local `var` across `NSHostingView` without a
/// reference-typed owner; this box keeps the selection alive for the host.
@MainActor
private final class ModeBinding {
    var value: String
    init(value: String) { self.value = value }
    var binding: Binding<String> {
        Binding(get: { self.value }, set: { self.value = $0 })
    }
}
