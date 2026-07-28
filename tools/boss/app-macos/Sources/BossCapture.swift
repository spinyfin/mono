import AppKit
import Foundation
import os.log

private let captureLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "capture")

/// CLI flags for the agent-capture path.
///
/// ```
/// BOSS_SOCKET_PATH=/tmp/boss-shot-$ID.sock BOSS_ENGINE_AUTOSTART=0 \
///   bazel run //tools/boss/app-macos:Boss -- --capture-to /tmp/shot.png
/// ```
///
/// Optional `--capture-after <seconds>` delays the bitmap grab so SwiftUI
/// can finish its first layout pass (measured: captures are byte-identical
/// from 0.05s onward; the default is a safety margin, not a necessity).
struct BossCaptureArgs: Equatable, Sendable {
    /// Destination PNG path. `nil` means normal interactive launch.
    var captureTo: String?
    /// Delay before capture, in seconds.
    var captureAfter: TimeInterval

    static let defaultCaptureAfter: TimeInterval = 0.6

    static let shared: BossCaptureArgs = parse(CommandLine.arguments)

    static func parse(_ arguments: [String]) -> BossCaptureArgs {
        var captureTo: String?
        var captureAfter = defaultCaptureAfter
        var i = 1 // skip argv[0]
        while i < arguments.count {
            let arg = arguments[i]
            if arg == "--capture-to", i + 1 < arguments.count {
                captureTo = arguments[i + 1]
                i += 2
                continue
            }
            if arg.hasPrefix("--capture-to=") {
                captureTo = String(arg.dropFirst("--capture-to=".count))
                i += 1
                continue
            }
            if arg == "--capture-after", i + 1 < arguments.count {
                if let value = TimeInterval(arguments[i + 1]) {
                    captureAfter = max(0, value)
                }
                i += 2
                continue
            }
            if arg.hasPrefix("--capture-after=") {
                if let value = TimeInterval(String(arg.dropFirst("--capture-after=".count))) {
                    captureAfter = max(0, value)
                }
                i += 1
                continue
            }
            i += 1
        }
        return BossCaptureArgs(captureTo: captureTo, captureAfter: captureAfter)
    }

    var isCaptureMode: Bool { captureTo != nil }
}

/// In-process window capture via `cacheDisplay` — no ScreenCaptureKit,
/// no `screencapture`, no `CGWindowListCreateImage`, no TCC grant.
///
/// Captures `contentView.superview` (not `contentView`) so the unified
/// titlebar / toolbar are included. Under SwiftUI `WindowGroup` the two
/// views share identical bounds, so size cannot distinguish them.
enum BossWindowCapture {
    enum CaptureError: Error, CustomStringConvertible {
        case noWindow
        case noFrameView
        case emptyBounds
        case bitmapFailed
        case blankImage(width: Int, height: Int, nonBlankFraction: Double)
        case writeFailed(path: String, underlying: String)
        case notIsolated

        var description: String {
            switch self {
            case .noWindow:
                return "capture failed: NSApp.windows is empty (no WindowGroup window created)"
            case .noFrameView:
                return "capture failed: window.contentView?.superview is nil"
            case .emptyBounds:
                return "capture failed: frame view bounds are empty"
            case .bitmapFailed:
                return "capture failed: bitmapImageRepForCachingDisplay returned nil"
            case .blankImage(let w, let h, let frac):
                return "capture failed: image \(w)x\(h) is effectively blank (non-blank fraction \(String(format: "%.4f", frac))) — a green empty shot is worse than no shot"
            case .writeFailed(let path, let underlying):
                return "capture failed: could not write PNG to \(path): \(underlying)"
            case .notIsolated:
                return "capture failed: --capture-to requires BOSS_SOCKET_PATH set to a non-production socket (not \(BossEnginePaths.defaultProductionSocket))"
            }
        }
    }

    /// Capture the primary window's chrome + content to `path` as PNG.
    ///
    /// The window need not be ordered front or key — measured: a never-ordered
    /// window (`isVisible == false`) captures correctly and byte-identically
    /// to an on-screen one. Do **not** call `NSApp.activate` or
    /// `makeKeyAndOrderFront` from the capture path.
    @MainActor
    static func capturePrimaryWindow(to path: String) throws {
        guard BossEnginePaths.isIsolatedInstance else {
            throw CaptureError.notIsolated
        }
        guard let window = pickWindow() else {
            throw CaptureError.noWindow
        }
        // contentView.superview includes the titlebar / unified toolbar.
        // Capturing contentView alone leaves a black band where chrome should be.
        guard let frameView = window.contentView?.superview else {
            throw CaptureError.noFrameView
        }
        frameView.layoutSubtreeIfNeeded()
        let bounds = frameView.bounds
        guard bounds.width > 1, bounds.height > 1 else {
            throw CaptureError.emptyBounds
        }
        guard let rep = frameView.bitmapImageRepForCachingDisplay(in: bounds) else {
            throw CaptureError.bitmapFailed
        }
        frameView.cacheDisplay(in: bounds, to: rep)

        let (nonBlank, total) = sampleNonBlankPixels(rep)
        let fraction = total == 0 ? 0.0 : Double(nonBlank) / Double(total)
        // A real Boss chrome+detail render has substantial non-blank content
        // even with a flat white sidebar (NavigationSplitView + .listStyle(.sidebar)
        // loses row *text* under cacheDisplay, but the detail pane and toolbar
        // still paint). Below ~1% is "all one colour" territory.
        if fraction < 0.01 {
            throw CaptureError.blankImage(
                width: rep.pixelsWide,
                height: rep.pixelsHigh,
                nonBlankFraction: fraction
            )
        }

        guard let data = rep.representation(using: .png, properties: [:]) else {
            throw CaptureError.bitmapFailed
        }
        do {
            try data.write(to: URL(fileURLWithPath: path), options: .atomic)
        } catch {
            throw CaptureError.writeFailed(path: path, underlying: error.localizedDescription)
        }
        captureLog.notice(
            "wrote capture \(path, privacy: .public) \(rep.pixelsWide)x\(rep.pixelsHigh) nonBlank=\(fraction, format: .fixed(precision: 3)) windowVisible=\(window.isVisible)"
        )
    }

    /// Prefer a titled content window; fall back to any window with a contentView.
    @MainActor
    private static func pickWindow() -> NSWindow? {
        let windows = NSApp.windows
        if let titled = windows.first(where: {
            $0.contentView != nil && $0.styleMask.contains(.titled)
        }) {
            return titled
        }
        return windows.first(where: { $0.contentView != nil })
    }

    /// Count non-near-white / non-near-black samples across a coarse grid.
    /// Near-uniform fills (solid white sidebar, solid black void) contribute
    /// little; real UI chrome and detail content contribute a lot.
    ///
    /// Reads raw sRGB bytes when available so colour-space conversion in
    /// `colorAt` cannot collapse a mid-tone fill to "near black".
    static func sampleNonBlankPixels(_ rep: NSBitmapImageRep) -> (nonBlank: Int, total: Int) {
        let w = rep.pixelsWide
        let h = rep.pixelsHigh
        guard w > 0, h > 0 else { return (0, 0) }
        let step = max(1, min(w, h) / 48)
        var nonBlank = 0
        var total = 0
        let spp = rep.samplesPerPixel
        let hasAlpha = rep.hasAlpha
        let bytes = rep.bitmapData
        let bpr = rep.bytesPerRow
        for y in stride(from: 0, to: h, by: step) {
            for x in stride(from: 0, to: w, by: step) {
                total += 1
                let r: CGFloat
                let g: CGFloat
                let b: CGFloat
                if let bytes, spp >= 3, bpr > 0 {
                    let offset = y * bpr + x * spp
                    r = CGFloat(bytes[offset]) / 255.0
                    g = CGFloat(bytes[offset + 1]) / 255.0
                    b = CGFloat(bytes[offset + 2]) / 255.0
                    _ = hasAlpha
                } else if let color = rep.colorAt(x: x, y: y)?.usingColorSpace(.deviceRGB) {
                    r = color.redComponent
                    g = color.greenComponent
                    b = color.blueComponent
                } else {
                    continue
                }
                // Not near-white AND not near-black → content.
                let nearWhite = r > 0.92 && g > 0.92 && b > 0.92
                let nearBlack = r < 0.08 && g < 0.08 && b < 0.08
                if !nearWhite && !nearBlack {
                    nonBlank += 1
                }
            }
        }
        return (nonBlank, total)
    }

    /// Schedule capture after `delay`, with a short retry loop for the
    /// macOS 26 command-line launch regression (FB21087054) where
    /// `NSApp.windows` can be empty intermittently.
    @MainActor
    static func scheduleCapture(to path: String, after delay: TimeInterval) {
        let run = {
            Task { @MainActor in
                await runCaptureWithRetry(to: path)
            }
        }
        if delay <= 0 {
            _ = run()
        } else {
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
                _ = run()
            }
        }
    }

    @MainActor
    private static func runCaptureWithRetry(to path: String) async {
        let maxAttempts = 8
        var lastError: Error = CaptureError.noWindow
        for attempt in 1...maxAttempts {
            if NSApp.windows.contains(where: { $0.contentView != nil }) {
                do {
                    try capturePrimaryWindow(to: path)
                    NSApp.terminate(nil)
                    return
                } catch let error as CaptureError {
                    lastError = error
                    // Only no-window is retry-worthy (FB21087054). Everything
                    // else fails loudly — a green blank shot is worse than no shot.
                    if case .noWindow = error {
                        // fall through to retry
                    } else {
                        failAndExit(error)
                    }
                } catch {
                    lastError = error
                }
            } else {
                lastError = CaptureError.noWindow
            }
            if attempt < maxAttempts {
                try? await Task.sleep(nanoseconds: 150_000_000)
            }
        }
        failAndExit(lastError)
    }

    private static func failAndExit(_ error: Error) -> Never {
        let message = "Boss capture: \(error)\n"
        fputs(message, stderr)
        captureLog.error("\(message, privacy: .public)")
        // Exit non-zero so a worker's shell sees the failure.
        exit(1)
    }
}
