import AppKit
import SwiftUI

struct AttachmentZoomView: View {
    let image: NSImage
    @State private var command = AttachmentZoomCommand()

    var body: some View {
        VStack(spacing: 4) {
            HStack(spacing: 8) {
                Button("Fit") { command = .init(action: .fit) }
                    .keyboardShortcut("0", modifiers: .command)
                Button("Fit Width") { command = .init(action: .width) }
                Button("Actual Size") { command = .init(action: .actual) }
                    .keyboardShortcut("1", modifiers: .command)
                Spacer()
                Button { command = .init(action: .out) } label: {
                    Image(systemName: "minus.magnifyingglass")
                }
                .keyboardShortcut("-", modifiers: .command)
                .help("Zoom out (⌘−)")
                Button { command = .init(action: .in) } label: {
                    Image(systemName: "plus.magnifyingglass")
                }
                .keyboardShortcut("=", modifiers: .command)
                .help("Zoom in (⌘=)")
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
            .padding(8)
            AttachmentZoomBridge(image: image, command: command)
        }
    }
}

struct AttachmentZoomCommand: Equatable {
    enum Action { case fit, width, actual, `in`, out }
    var action: Action = .fit
    private let id = UUID()
}

private struct AttachmentZoomBridge: NSViewRepresentable {
    let image: NSImage
    let command: AttachmentZoomCommand

    func makeNSView(context: Context) -> AttachmentZoomScrollView {
        AttachmentZoomScrollView(image: image)
    }

    func updateNSView(_ view: AttachmentZoomScrollView, context: Context) {
        guard view.lastCommand != command else { return }
        view.lastCommand = command
        view.apply(command.action)
    }
}

/// AppKit owns pinch magnification, cursor anchoring, and two-axis scrolling.
final class AttachmentZoomScrollView: NSScrollView {
    enum Mode { case fit, width, manual }
    private(set) var mode: Mode = .fit
    var lastCommand: AttachmentZoomCommand?
    private var previousSize = NSSize.zero
    private var applyingMode = false
    let picture: AttachmentZoomImageView

    init(image: NSImage) {
        picture = AttachmentZoomImageView(frame: NSRect(origin: .zero, size: image.size))
        super.init(frame: .zero)
        contentView = AttachmentCenteringClipView()
        drawsBackground = false
        hasVerticalScroller = true
        hasHorizontalScroller = true
        autohidesScrollers = true
        scrollerStyle = .overlay
        allowsMagnification = true
        minMagnification = 0.001
        maxMagnification = 32
        picture.image = image
        picture.imageScaling = .scaleAxesIndependently
        documentView = picture
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func layout() {
        super.layout()
        guard !applyingMode, contentSize != previousSize else { return }
        previousSize = contentSize
        if mode != .manual { apply(mode == .fit ? .fit : .width) }
    }

    func apply(_ action: AttachmentZoomCommand.Action, anchor: NSPoint? = nil) {
        guard picture.frame.width > 0, picture.frame.height > 0,
              contentSize.width > 0, contentSize.height > 0 else { return }
        applyingMode = true
        defer { applyingMode = false }
        let fitted = AttachmentImageSizing.fit(native: picture.frame.size, available: contentSize)
        let fit = fitted.width / picture.frame.width
        let scale: CGFloat
        switch action {
        case .fit: mode = .fit; scale = fit
        case .width: mode = .width; scale = contentSize.width / picture.frame.width
        case .actual: mode = .manual; scale = 1
        case .in: mode = .manual; scale = magnification * 1.25
        case .out: mode = .manual; scale = magnification / 1.25
        }
        let visible = picture.visibleRect
        let cursor = window.map { picture.convert($0.mouseLocationOutsideOfEventStream, from: nil) }
        let cursorAnchor = cursor.flatMap { visible.contains($0) ? $0 : nil }
        let center = anchor ?? cursorAnchor ?? NSPoint(x: visible.midX, y: visible.midY)
        setMagnification(min(max(scale, minMagnification), maxMagnification), centeredAt: center)
        if action == .fit || action == .width {
            // Legacy scrollers consume viewport space as they appear or disappear.
            // Settle both axes before using the final viewport for a fitted mode.
            for _ in 0..<3 {
                tile()
                let viewport = contentView.frame.size
                let fitted = AttachmentImageSizing.fit(native: picture.frame.size, available: viewport)
                let desired = action == .width ? viewport.width / picture.frame.width : fitted.width / picture.frame.width
                let bounded = min(max(desired, minMagnification), maxMagnification)
                guard abs(bounded - magnification) > 0.000001 else { break }
                setMagnification(bounded, centeredAt: center)
            }
        }
        if action == .width {
            contentView.scroll(to: NSPoint(x: 0, y: picture.frame.height - contentView.bounds.height))
            reflectScrolledClipView(contentView)
        }
    }

    override func magnify(with event: NSEvent) {
        mode = .manual
        super.magnify(with: event)
    }

    func toggleFit(at point: NSPoint) {
        apply(mode == .fit ? .actual : .fit, anchor: point)
    }
}

/// Center images smaller than the viewport without changing their native dimensions.
private final class AttachmentCenteringClipView: NSClipView {
    override func constrainBoundsRect(_ proposedBounds: NSRect) -> NSRect {
        var result = super.constrainBoundsRect(proposedBounds)
        guard let documentView else { return result }
        if documentView.frame.width < result.width {
            result.origin.x = (documentView.frame.width - result.width) / 2
        }
        if documentView.frame.height < result.height {
            result.origin.y = (documentView.frame.height - result.height) / 2
        }
        return result
    }
}

final class AttachmentZoomImageView: NSImageView {
    private var dragStart: NSPoint?
    private var scrollStart = NSPoint.zero

    override func mouseDown(with event: NSEvent) {
        guard let scroll = enclosingScrollView as? AttachmentZoomScrollView else { return }
        if event.clickCount == 2 {
            scroll.toggleFit(at: convert(event.locationInWindow, from: nil))
            dragStart = nil
        } else {
            dragStart = event.locationInWindow
            scrollStart = scroll.contentView.bounds.origin
        }
    }

    override func mouseDragged(with event: NSEvent) {
        guard let start = dragStart, let scroll = enclosingScrollView else { return }
        let delta = NSPoint(x: (event.locationInWindow.x - start.x) / scroll.magnification,
                            y: (event.locationInWindow.y - start.y) / scroll.magnification)
        scroll.contentView.scroll(to: NSPoint(x: scrollStart.x - delta.x, y: scrollStart.y - delta.y))
        scroll.reflectScrolledClipView(scroll.contentView)
    }

    override func mouseUp(with event: NSEvent) { dragStart = nil }
}
