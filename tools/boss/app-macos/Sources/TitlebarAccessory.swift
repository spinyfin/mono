import AppKit
import SwiftUI

/// Hosts a SwiftUI view in the window's titlebar-accessory slot at
/// `layoutAttribute == .bottom` — the AppKit "butterbar" position: directly
/// below the toolbar and directly above the window's content.
///
/// ## Why this exists rather than `VStack { banner; content }`
///
/// `ContentView`'s main chrome is a `NavigationSplitView`. On macOS, SwiftUI
/// only gives an NSV its native window integration — its split-view controller
/// promoted to the window's content view controller, sidebar material bleeding
/// full-height, toolbar divider aligned to the sidebar boundary, safe area
/// derived from the window — when the NSV is the **root** of the window's
/// content. Wrapping it in a `VStack` to make room for a banner demotes it to
/// an ordinary embedded split view confined to a sub-rect. On macOS 26 the
/// `.sidebar`-styled `List` then renders as a floating inset glass panel over
/// the window background, a dark band spans the top of the content area, and
/// the detail column is pushed down and clipped.
///
/// A titlebar accessory reserves real layout space *without* demoting the NSV:
/// AppKit shrinks the window's `contentLayoutRect` by the accessory's height,
/// so the sidebar column and the detail column both begin below the banner.
/// The NSV stays the window root and keeps its native styling, and the banner
/// height still comes from the banner's own content (Dynamic Type, multi-line
/// wrap) — no measured spacer, no hardcoded offset, no change to the sidebar's
/// glass/inset styling.
///
/// The accessory is created lazily: a window that never shows a banner never
/// gets one installed. Once installed it is kept and toggled via
/// `NSTitlebarAccessoryViewController.isHidden`, which collapses it out of the
/// titlebar layout entirely, so a hidden banner costs no height.
///
/// ## Measured
///
/// The three candidate shapes were compared in a standalone AppKit harness
/// (same window style: `.titled + .fullSizeContentView`, unified toolbar, root
/// `NavigationSplitView` with a `.sidebar`-styled `List`), probing where the
/// sidebar's first section header actually lands in window coordinates against
/// a 52pt titlebar:
///
/// | shape | sidebar header y | split view |
/// | --- | --- | --- |
/// | `safeAreaInset` on the root (pre-#2459) | 55 — under a banner spanning 52…83 | promoted, full window height |
/// | `VStack { banner; split view }` (#2459) | 94 — clear of the banner | demoted: sidebar becomes an 8pt-inset floating panel |
/// | titlebar accessory (this) | 91 — clear of the 36pt accessory ending at 88 | promoted, full window height |
///
/// Only the accessory gets both right at once. One deliberate consequence:
/// with a full-height sidebar, AppKit lays the accessory out in the titlebar
/// segment *beside* the sidebar (measured origin x = the sidebar's trailing
/// edge), so the bar spans the content area rather than the whole window —
/// the same placement Xcode and Mail use for their titlebar accessories.
@MainActor
final class TitlebarAccessoryHost {
    private var controller: NSTitlebarAccessoryViewController?
    private var hosting: NSHostingView<AnyView>?
    private weak var attachedWindow: NSWindow?

    /// The accessory controller currently installed on a window, if any.
    /// Exposed for tests.
    var installedController: NSTitlebarAccessoryViewController? { controller }

    /// Installs (on first presentation), refreshes, and shows/hides the
    /// accessory. Safe to call on every SwiftUI layout pass.
    func update(window: NSWindow?, content: AnyView, isPresented: Bool) {
        guard let window else { return }
        // AppKit can reparent a hosting view into a different window (tab
        // detachment); move the accessory rather than leaving it behind.
        if attachedWindow !== window {
            detach()
        }
        // Nothing to show and nothing installed: leave the titlebar untouched.
        guard isPresented || controller != nil else { return }

        let hosting = hosting ?? makeHostingView(content)
        hosting.rootView = content

        let controller = controller ?? install(hosting, in: window)
        controller.isHidden = !isPresented

        // A titlebar accessory hides with the titlebar in fullscreen unless it
        // declares a minimum height. Report the height the banner actually laid
        // out at so a health warning does not vanish when the operator
        // fullscreens the window. Deferred a runloop tick because the accessory
        // has no resolved width — and so no wrapped-text height — until AppKit
        // has laid the titlebar out once.
        DispatchQueue.main.async { [weak controller, weak hosting] in
            guard let controller, let hosting, !controller.isHidden else { return }
            let height = hosting.frame.height
            if height > 0, height != controller.fullScreenMinHeight {
                controller.fullScreenMinHeight = height
            }
        }
    }

    /// Removes the accessory from its window and drops the hosting view.
    func detach() {
        if let attachedWindow, let controller,
           let index = attachedWindow.titlebarAccessoryViewControllers.firstIndex(where: { $0 === controller }) {
            attachedWindow.removeTitlebarAccessoryViewController(at: index)
        }
        controller = nil
        hosting = nil
        attachedWindow = nil
    }

    private func makeHostingView(_ content: AnyView) -> NSHostingView<AnyView> {
        let hosting = NSHostingView(rootView: content)
        hosting.translatesAutoresizingMaskIntoConstraints = false
        // The accessory's width is pinned to the titlebar by AppKit; its height
        // must come from the SwiftUI content so the bar grows with Dynamic Type
        // and multi-line wrapping instead of a fixed constant.
        hosting.sizingOptions = [.intrinsicContentSize]
        self.hosting = hosting
        return hosting
    }

    private func install(
        _ hosting: NSHostingView<AnyView>,
        in window: NSWindow
    ) -> NSTitlebarAccessoryViewController {
        let controller = NSTitlebarAccessoryViewController()
        controller.layoutAttribute = .bottom
        controller.view = hosting
        window.addTitlebarAccessoryViewController(controller)
        self.controller = controller
        attachedWindow = window
        return controller
    }
}

/// Zero-size `NSView` that captures its hosting `NSWindow` and keeps a
/// `TitlebarAccessoryHost` in sync with SwiftUI state. Mirrors the
/// deferred-capture pattern used by `WindowMenuRegistrar` in `DesignsView.swift`
/// and `CommentLayerWindowBinder` in `Comments/CommentLayer.swift`: the view is
/// not attached to a window yet when `makeNSView` runs, so the first update is
/// deferred one runloop tick.
private struct TitlebarAccessoryInstaller: NSViewRepresentable {
    let isPresented: Bool
    let content: AnyView

    func makeCoordinator() -> TitlebarAccessoryHost {
        TitlebarAccessoryHost()
    }

    func makeNSView(context: Context) -> NSView {
        let view = NSView(frame: .zero)
        let host = context.coordinator
        let content = content
        let isPresented = isPresented
        DispatchQueue.main.async {
            host.update(window: view.window, content: content, isPresented: isPresented)
        }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        context.coordinator.update(window: nsView.window, content: content, isPresented: isPresented)
    }

    static func dismantleNSView(_ nsView: NSView, coordinator: TitlebarAccessoryHost) {
        coordinator.detach()
    }
}

extension View {
    /// Renders `content` in the hosting window's titlebar-accessory slot,
    /// directly below the toolbar and above this view, reserving layout space
    /// for it at the AppKit level.
    ///
    /// Use this instead of stacking chrome above the content in a `VStack` when
    /// the content is (or contains) a root `NavigationSplitView` — see
    /// `TitlebarAccessoryHost` for why that demotion breaks sidebar rendering.
    func windowTitlebarAccessory<Accessory: View>(
        isPresented: Bool,
        @ViewBuilder content: () -> Accessory
    ) -> some View {
        background(
            TitlebarAccessoryInstaller(isPresented: isPresented, content: AnyView(content()))
                .frame(width: 0, height: 0)
                .allowsHitTesting(false)
        )
    }
}
