import AppKit
import SwiftUI

struct WorkSwitcherToolbar: View {
    @ObservedObject var model: ChatViewModel
    @State private var isPresented = false

    static func isVisible(model: ChatViewModel) -> Bool {
        model.navigationMode == .work && model.isWorkSidebarHidden
    }

    var body: some View {
        Button { isPresented.toggle() } label: {
            HStack(spacing: 5) {
                Text(model.workSwitcherLabel).lineLimit(1).truncationMode(.middle)
                Image(systemName: "chevron.down").font(.caption2)
            }
            .frame(maxWidth: 240)
        }
        .keyboardShortcut("p", modifiers: [.command, .shift])
        .help("Switch product or project (⌘⇧P)")
        .accessibilityLabel("Switch product or project: \(model.workSwitcherLabel)")
        .accessibilityIdentifier("work.recentSwitcher")
        .popover(isPresented: $isPresented, arrowEdge: .bottom) {
            WorkSwitcherMenu(model: model) { isPresented = false }
        }
    }
}

/// Both mouse and keyboard activation call the sidebar's existing commands.
enum WorkSwitcherSelection: Hashable {
    case product(String)
    case allProjects
    case project(String)

    @MainActor func apply(to model: ChatViewModel) {
        switch self {
        case .product(let id): model.selectWorkProduct(id)
        case .allProjects: model.clearProjectFilters()
        case .project(let id): model.toggleProjectFilter(id)
        }
    }
}

struct WorkSwitcherMenu: View {
    @ObservedObject var model: ChatViewModel
    var dismiss: () -> Void
    @State private var highlighted: WorkSwitcherSelection?

    private var choices: [WorkSwitcherSelection] {
        model.recentWorkProducts.map { .product($0.id) }
            + (model.selectedProduct == nil ? [] : [.allProjects] + model.recentWorkProjects.map { .project($0.id) })
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            heading("Recent products")
            ForEach(model.recentWorkProducts) { product in
                row(product.name, choice: .product(product.id), selected: model.selectedWorkProductID == product.id)
            }
            if model.recentWorkProducts.isEmpty {
                Text("No active products").foregroundStyle(.secondary).padding(8)
            }
            if model.selectedProduct != nil {
                Divider().padding(.vertical, 5)
                heading("Recent projects")
                row("All projects", choice: .allProjects, selected: !model.hasProjectFilters)
                ForEach(model.recentWorkProjects) { project in
                    row(project.name, choice: .project(project.id), selected: model.selectedProjectFilterIDs.contains(project.id))
                }
            }
        }
        .padding(8)
        .frame(width: 300)
        .onAppear { highlighted = choices.first }
        .background(WorkSwitcherKeyboard { code in
            switch code {
            case 125: move(1)
            case 126: move(-1)
            case 36, 76:
                if let highlighted, choices.contains(highlighted) { activate(highlighted) }
            case 53: dismiss()
            default: return false
            }
            return true
        }.frame(width: 0, height: 0))
        .onChange(of: choices) { _, updated in
            if let highlighted, !updated.contains(highlighted) { self.highlighted = updated.first }
        }
    }

    private func heading(_ title: String) -> some View {
        Text(title).font(.caption.weight(.semibold)).foregroundStyle(.secondary)
            .padding(.horizontal, 8).padding(.vertical, 4)
    }

    private func row(_ title: String, choice: WorkSwitcherSelection, selected: Bool) -> some View {
        Button { activate(choice) } label: {
            HStack(spacing: 8) {
                Image(systemName: "checkmark").opacity(selected ? 1 : 0).frame(width: 12)
                Text(title).lineLimit(2).multilineTextAlignment(.leading)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 8).padding(.vertical, 6)
            .contentShape(Rectangle())
            .background(highlighted == choice ? Color.accentColor.opacity(0.18) : Color.clear,
                        in: RoundedRectangle(cornerRadius: 5))
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
        .onHover { if $0 { highlighted = choice } }
    }

    private func move(_ step: Int) {
        guard !choices.isEmpty else { return }
        let index = highlighted.flatMap { choices.firstIndex(of: $0) } ?? (step > 0 ? -1 : 0)
        highlighted = choices[(index + step + choices.count) % choices.count]
    }

    private func activate(_ choice: WorkSwitcherSelection) {
        choice.apply(to: model)
        dismiss()
    }
}

/// Give the popup a real first responder, independent of the macOS setting
/// that makes buttons keyboard-focusable. Only this popup receives the keys.
private struct WorkSwitcherKeyboard: NSViewRepresentable {
    var handle: (UInt16) -> Bool

    func makeNSView(context: Context) -> KeyView { KeyView() }
    func updateNSView(_ view: KeyView, context: Context) { view.handle = handle }

    final class KeyView: NSView {
        var handle: (UInt16) -> Bool = { _ in false }
        override var acceptsFirstResponder: Bool { true }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                self.window?.makeFirstResponder(self)
            }
        }

        override func keyDown(with event: NSEvent) {
            if !handle(event.keyCode) { super.keyDown(with: event) }
        }
    }
}
