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
/// ## The bar spans the content area, not the whole window
///
/// With a full-height sidebar, AppKit splits the titlebar into a sidebar
/// segment and a content segment, and lays a `.bottom` accessory out in the
/// content segment only — its origin x is the sidebar's trailing edge. So the
/// bar starts where the content starts, the same placement Xcode and Mail use.
/// This is deliberate: reaching the window's left edge would mean giving up
/// full-height sidebar layout, which is exactly the sidebar styling this whole
/// arrangement exists to preserve. Note that the Work `NavigationSplitView`
/// stays mounted (at opacity 0) on the Agents / Designs / Automations tabs, so
/// the inset is present there too even though no sidebar is drawn.
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
    ///
    /// `contentHeight` is the height `content` measured at inside the hosted
    /// SwiftUI tree; pass 0 when it is not known yet.
    func update(
        window: NSWindow?,
        content: AnyView,
        isPresented: Bool,
        contentHeight: CGFloat = 0
    ) {
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
        // declares a minimum height, so report the height the banner actually
        // laid out at — otherwise a health warning vanishes when the window is
        // fullscreened. The measurement comes from inside the hosted SwiftUI
        // tree, so it tracks every height change: Dynamic Type, a live resize
        // that rewraps the text, and changes the banner makes on its own (the
        // health banner grows to list every issue when its chevron is tapped,
        // which never re-runs this view's body).
        if contentHeight > 0, contentHeight != controller.fullScreenMinHeight {
            controller.fullScreenMinHeight = contentHeight
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
    let contentHeight: CGFloat
    let content: AnyView

    func makeCoordinator() -> TitlebarAccessoryHost {
        TitlebarAccessoryHost()
    }

    func makeNSView(context: Context) -> NSView {
        let view = NSView(frame: .zero)
        let host = context.coordinator
        let content = content
        let isPresented = isPresented
        let contentHeight = contentHeight
        DispatchQueue.main.async {
            host.update(
                window: view.window,
                content: content,
                isPresented: isPresented,
                contentHeight: contentHeight
            )
        }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        context.coordinator.update(
            window: nsView.window,
            content: content,
            isPresented: isPresented,
            contentHeight: contentHeight
        )
    }

    static func dismantleNSView(_ nsView: NSView, coordinator: TitlebarAccessoryHost) {
        coordinator.detach()
    }
}

/// Measures the accessory content's height from inside the hosted SwiftUI tree
/// and feeds it back to the host, so `fullScreenMinHeight` tracks height
/// changes the enclosing view's body never sees.
private struct WindowTitlebarAccessoryModifier<Accessory: View>: ViewModifier {
    let isPresented: Bool
    @ViewBuilder let accessory: () -> Accessory

    @State private var contentHeight: CGFloat = 0

    func body(content: Content) -> some View {
        content.background(
            TitlebarAccessoryInstaller(
                isPresented: isPresented,
                contentHeight: contentHeight,
                content: AnyView(measuredAccessory)
            )
            .frame(width: 0, height: 0)
            .allowsHitTesting(false)
        )
    }

    private var measuredAccessory: some View {
        accessory()
            .onGeometryChange(for: CGFloat.self) { proxy in
                proxy.size.height
            } action: { height in
                contentHeight = height
            }
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
        @ViewBuilder content: @escaping () -> Accessory
    ) -> some View {
        modifier(WindowTitlebarAccessoryModifier(isPresented: isPresented, accessory: content))
    }
}
