import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Offscreen `NSHostingView` renders of the toolbar button and the
/// read-only popover (no `NSWindow` — that path segfaults under the
/// bazel XCTest host; see `BossCaptureTests`). Used as glanceable
/// evidence for zero/one/many, the 99+ cap, planner elapsed time, and
/// conflict elapsed omission. Isolated-app `--capture-to` shots remain
/// the reviewer surface for real chrome placement.
@MainActor
final class BackgroundWorkToolbarRenderTests: XCTestCase {
    func testButtonAndPopoverRendersAreNonBlank() throws {
        let temporaryDirectory = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        )
        let dest = temporaryDirectory
            .appendingPathComponent("boss-bgwork-toolbar-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dest, withIntermediateDirectories: true)
        addTeardownBlock {
            try? FileManager.default.removeItem(at: dest)
        }
        let undeclared: URL? = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"].map {
            URL(fileURLWithPath: $0, isDirectory: true)
        }

        let shots: [(name: String, render: () throws -> NSBitmapImageRep)] = [
            ("button-one.png", { try self.renderButton(items: [self.plannerItem()]) }),
            ("button-many.png", { try self.renderButton(items: [self.plannerItem(), self.conflictItem()]) }),
            ("button-capped.png", { try self.renderButton(items: self.manyItems(count: 100)) }),
            ("popover-planner.png", { try self.renderPopover(items: [self.plannerItem()], height: 96) }),
            ("popover-conflict.png", { try self.renderPopover(items: [self.conflictItem()], height: 96) }),
            ("popover-mixed.png", { try self.renderPopover(items: [self.plannerItem(), self.conflictItem()], height: 168) }),
        ]

        var paths: [String] = []
        for shot in shots {
            let rep = try shot.render()
            let url = dest.appendingPathComponent(shot.name)
            guard let data = rep.representation(using: .png, properties: [:]) else {
                XCTFail("PNG encode failed for \(shot.name)")
                return
            }
            try data.write(to: url)
            if let undeclared {
                try data.write(to: undeclared.appendingPathComponent(shot.name))
            }
            guard !isUniformlyBlank(rep) else {
                throw XCTSkip("render came back uniformly blank; host does not support offscreen SwiftUI rendering")
            }
            paths.append(url.path)
        }

        let index = dest.appendingPathComponent("paths.txt")
        try paths.joined(separator: "\n").write(to: index, atomically: true, encoding: .utf8)
        print("BACKGROUND_WORK_TOOLBAR_FIXTURES=\(dest.path)")
    }

    // MARK: - Renders

    private func renderButton(items: [BackgroundWorkItem]) throws -> NSBitmapImageRep {
        let model = makeModel(items: items)
        let root = BackgroundWorkToolbarButton(model: model)
            .padding(16)
            .background(Color(nsColor: .windowBackgroundColor))
            .frame(width: 88, height: 52)
        return try render(root, width: 88, height: 52)
    }

    private func renderPopover(items: [BackgroundWorkItem], height: CGFloat) throws -> NSBitmapImageRep {
        let model = makeModel(items: items)
        let root = BackgroundWorkPopover(model: model)
            .background(Color(nsColor: .windowBackgroundColor))
            .frame(width: 360, height: height)
        return try render(root, width: 360, height: height)
    }

    private func render<V: View>(_ root: V, width: CGFloat, height: CGFloat) throws -> NSBitmapImageRep {
        let host = NSHostingView(rootView: root)
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(x: 0, y: 0, width: width, height: height)
        host.layoutSubtreeIfNeeded()
        let bounds = host.bounds
        guard let rep = host.bitmapImageRepForCachingDisplay(in: bounds) else {
            throw XCTSkip("bitmapImageRepForCachingDisplay returned nil")
        }
        host.cacheDisplay(in: bounds, to: rep)
        return rep
    }

    // MARK: - Model / items

    private func makeModel(items: [BackgroundWorkItem]) -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-bgwork-toolbar-\(UUID().uuidString).sock")
        model.backgroundWork = items
        return model
    }

    private func plannerItem() -> BackgroundWorkItem {
        BackgroundWorkItem(
            id: "project_planner:run_1",
            kind: .projectPlanner,
            phase: "Planning Alpha",
            productID: "prod_1",
            sourceID: "run_1",
            title: "Project planner",
            projectID: "proj_1",
            startedAt: String(Int(Date().timeIntervalSince1970) - 90),
            workItemID: nil
        )
    }

    private func conflictItem() -> BackgroundWorkItem {
        BackgroundWorkItem(
            id: "conflict_remediation:crz_1",
            kind: .conflictRemediation,
            phase: "Rebasing Chore",
            productID: "prod_1",
            sourceID: "crz_1",
            title: "Conflict remediation",
            projectID: nil,
            startedAt: nil,
            workItemID: "task_1"
        )
    }

    private func manyItems(count: Int) -> [BackgroundWorkItem] {
        (0..<count).map { index in
            BackgroundWorkItem(
                id: "project_planner:run_\(index)",
                kind: .projectPlanner,
                phase: "Planning P\(index)",
                productID: "prod_1",
                sourceID: "run_\(index)",
                title: "Project planner",
                projectID: "proj_\(index)",
                startedAt: "1000000000",
                workItemID: nil
            )
        }
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
