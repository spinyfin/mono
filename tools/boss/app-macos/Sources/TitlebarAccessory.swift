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
/// ## Height is two-sided: windowed frame + fullscreen min height
///
/// AppKit clips a bottom accessory with an internal clip view. Two signals
/// must track the measured content height:
///
/// - **Windowed**: `NSTitlebarAccessoryViewController` observes the view's
///   frame. `automaticallyAdjustsSize` defaults to `true` and replaces that
///   frame with a system default, so expanded multi-issue banners get clipped
///   unless we turn it off and push the measured height onto the view frame.
/// - **Fullscreen**: a non-zero `fullScreenMinHeight` keeps the accessory
///   visible when the menubar auto-hides; `0` lets it hide with the menubar.
///   Height is not sticky — a drop back to 0 (banner gone / unmeasured) must
///   clear both signals, or a window that once showed a banner keeps a black
///   reserved band forever.
///
/// ## Full-width over a full-height sidebar
///
/// With a full-height sidebar, AppKit splits the titlebar into a sidebar
/// segment and a content segment and lays a `.bottom` accessory out in the
/// content segment only — origin-x at the sidebar's trailing edge — while
/// still shrinking `contentLayoutRect` for the whole window. That leaves an
/// empty band above the nav panel while the bar paints only above the detail
/// column. The accessory's root view is a container that, on every layout
/// pass, places the hosted banner at the window's leading edge with the full
/// window width (and disables clipping on the intervening AppKit views) so the
/// butterbar spans both columns and the two column tops stay flush. The Work
/// `NavigationSplitView` stays mounted (at opacity 0) on the Agents / Designs
/// / Automations tabs, so the inset is present there too even though no
/// sidebar is drawn.
@MainActor
final class TitlebarAccessoryHost {
    private var controller: NSTitlebarAccessoryViewController?
    private var container: TitlebarAccessoryContainer?
    private weak var attachedWindow: NSWindow?

    /// The accessory controller currently installed on a window, if any.
    /// Exposed for tests.
    var installedController: NSTitlebarAccessoryViewController? { controller }

    /// The container view currently installed as the accessory's view, if any.
    /// Exposed for tests so windowed-mode height can be asserted via frame.
    var installedContainerView: TitlebarAccessoryContainer? { container }

    /// Installs (on first presentation), refreshes, and shows/hides the
    /// accessory. Safe to call on every SwiftUI layout pass.
    ///
    /// `contentHeight` is the height `content` measured at inside the hosted
    /// SwiftUI tree; pass 0 when it is not known yet. Zero is meaningful: it
    /// clears both the windowed frame height and `fullScreenMinHeight` so a
    /// dismissed banner does not leave a reserved band.
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

        let container = container ?? makeContainer(content)
        container.hosting.rootView = content

        let controller = controller ?? install(container, in: window)
        controller.isHidden = !isPresented

        // Presented accessories track the measured height; hidden / unmeasured
        // ones report 0 so fullscreen does not keep a black menubar band and
        // the windowed clip view does not keep a stale collapsed slot.
        let resolvedHeight = isPresented ? contentHeight : 0
        applyContentHeight(resolvedHeight, to: controller, container: container)
    }

    /// Removes the accessory from its window and drops the hosting view.
    func detach() {
        if let attachedWindow, let controller,
           let index = attachedWindow.titlebarAccessoryViewControllers.firstIndex(where: { $0 === controller }) {
            attachedWindow.removeTitlebarAccessoryViewController(at: index)
        }
        controller = nil
        container = nil
        attachedWindow = nil
    }

    /// Pushes `height` into both the windowed frame and `fullScreenMinHeight`.
    ///
    /// A titlebar accessory hides with the titlebar in fullscreen unless it
    /// declares a minimum height, so a non-zero measurement keeps the banner
    /// visible when the menubar auto-hides. The same measurement must also
    /// land on the view frame: AppKit sizes the windowed clip view from that
    /// frame (with `automaticallyAdjustsSize == false`), which is how expand
    /// / collapse of the health banner stays unclipped. Zero clears both.
    func applyContentHeight(
        _ height: CGFloat,
        to controller: NSTitlebarAccessoryViewController,
        container: TitlebarAccessoryContainer
    ) {
        if controller.fullScreenMinHeight != height {
            controller.fullScreenMinHeight = height
        }

        // AppKit observes the accessory view's frame for bottom layout
        // attributes and fills width automatically; only height is ours to
        // drive. Width stays whatever AppKit assigned for the content segment
        // — the container then draws the banner full-window-wide on layout.
        let current = container.frame
        let width = current.width > 0
            ? current.width
            : (attachedWindow?.contentLayoutRect.width ?? 1)
        if abs(current.height - height) > 0.5 || (height > 0 && current.width <= 0) {
            container.setFrameSize(NSSize(width: max(width, 1), height: height))
            container.needsLayout = true
        }
    }

    private func makeContainer(_ content: AnyView) -> TitlebarAccessoryContainer {
        let hosting = NSHostingView(rootView: content)
        // Frame-based placement inside the container; intrinsic size still
        // seeds the first layout pass before an explicit measurement arrives.
        hosting.translatesAutoresizingMaskIntoConstraints = true
        hosting.sizingOptions = [.intrinsicContentSize]
        let container = TitlebarAccessoryContainer(hosting: hosting)
        self.container = container
        return container
    }

    private func install(
        _ container: TitlebarAccessoryContainer,
        in window: NSWindow
    ) -> NSTitlebarAccessoryViewController {
        let controller = NSTitlebarAccessoryViewController()
        controller.layoutAttribute = .bottom
        // Default is true and replaces the view frame with a system default
        // height, which clips any banner taller than that default (expanded
        // multi-issue health list) and prevents measurement from driving
        // windowed layout. We size from the measured content height instead.
        controller.automaticallyAdjustsSize = false
        controller.view = container
        window.addTitlebarAccessoryViewController(controller)
        self.controller = controller
        attachedWindow = window
        // Clip-view hierarchy exists only after the accessory is added;
        // unclip so the container can paint into the sidebar segment.
        DispatchQueue.main.async { [weak container] in
            container?.unclipAncestorsForFullWidth()
        }
        return controller
    }
}

/// Accessory root view. AppKit sizes this to the content-segment width; on
/// every layout pass we place the hosted SwiftUI banner at the window's
/// leading edge with the full window width so the butterbar also covers the
/// band above a full-height sidebar.
final class TitlebarAccessoryContainer: NSView {
    let hosting: NSHostingView<AnyView>

    init(hosting: NSHostingView<AnyView>) {
        self.hosting = hosting
        super.init(frame: .zero)
        // Allow the hosted banner to draw past our content-segment bounds
        // into the sidebar segment of the titlebar.
        clipsToBounds = false
        addSubview(hosting)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override func layout() {
        super.layout()
        layoutHostingFullWindowWidth()
    }

    /// Positions `hosting` so its leading edge is the window's leading edge
    /// and its width is the window's width, while keeping our own bounds (the
    /// ones AppKit uses for contentLayoutRect) at the content-segment size.
    func layoutHostingFullWindowWidth() {
        let height = bounds.height
        guard height > 0 else {
            hosting.frame = .zero
            return
        }
        guard window != nil else {
            hosting.frame = bounds
            return
        }

        // Our origin in window base coordinates. With a full-height sidebar
        // AppKit places us at the content segment, so origin.x is the sidebar
        // width and -origin.x is the local x that maps back to window leading.
        let originInWindow = convert(NSPoint.zero, to: nil)
        let fullWidth = window?.frame.width ?? bounds.width
        hosting.frame = NSRect(
            x: -originInWindow.x,
            y: 0,
            width: fullWidth,
            height: height
        )
    }

    /// Walks the AppKit titlebar clip-view chain and turns off clipping so
    /// the banner's leading overhang into the sidebar segment is visible.
    func unclipAncestorsForFullWidth() {
        var view: NSView? = self
        // Titlebar accessory depth is small (view → clip view → titlebar
        // view → …); a short walk is enough and avoids touching the content
        // view hierarchy.
        for _ in 0..<6 {
            guard let current = view else { break }
            current.clipsToBounds = false
            if let clip = current as? NSClipView {
                clip.clipsToBounds = false
            }
            view = current.superview
        }
        needsLayout = true
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
/// and feeds it back to the host, so both the windowed frame height and
/// `fullScreenMinHeight` track height changes the enclosing view's body never
/// sees (banner expand/collapse, Dynamic Type, rewrap on live resize).
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
            .frame(maxWidth: .infinity)
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
