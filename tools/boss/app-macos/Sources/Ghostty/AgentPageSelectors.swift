import PaneLayout
import SwiftUI

/// Value inputs expose selection and membership changes to SwiftUI diffing,
/// just as they do for AgentSurfaceCollection. Capturing the model reference
/// inside ForEach would hide changes when selector identities stay the same.
struct AgentPageSelectors<Chip: View>: View {
    let layout: PaneLayoutModel
    let onSelect: (Int) -> Void
    @ViewBuilder var chip: (PageSelector, Bool, @escaping () -> Void) -> Chip

    var body: some View {
        HStack(spacing: 6) {
            Text("Page:")
                .font(.caption)
                .foregroundStyle(.secondary)
            ForEach(layout.pageSelectors, id: \.index) { selector in
                chip(selector, selector.index == layout.selectedPage) {
                    onSelect(selector.index)
                }
            }
        }
    }

}

extension AgentPageSelectors where Chip == AgentPageSelectorChip {
    init(layout: PaneLayoutModel, onSelect: @escaping (Int) -> Void) {
        self.init(layout: layout, onSelect: onSelect) { selector, selected, action in
            AgentPageSelectorChip(selector: selector, isSelected: selected, onSelect: action)
        }
    }
}

struct AgentPageSelectorChip: View {
    let selector: PageSelector
    let isSelected: Bool
    let onSelect: () -> Void

    var body: some View {
        Button(action: onSelect) {
            HStack(spacing: 4) {
                Text(pageLabel)
                    .font(.caption.weight(.semibold))
                Text("·")
                    .foregroundStyle(.secondary)
                Text(occupiedLabel)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if selector.hasWaiting {
                    Circle()
                        .fill(Color.orange)
                        .frame(width: 6, height: 6)
                        .accessibilityLabel("Waiting workers")
                }
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(
                isSelected
                    ? Color.accentColor.opacity(0.18)
                    : Color.secondary.opacity(0.08)
            )
            .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
        }
        .buttonStyle(.plain)
        .accessibilityValue(isSelected ? "Selected" : "Not selected")
        .accessibilityLabel("Page \(selector.index + 1), \(selector.occupiedCount) agents")
    }

    private var pageLabel: String { String(selector.index + 1) }
    private var occupiedLabel: String {
        "\(selector.occupiedCount) \(selector.occupiedCount == 1 ? "Agent" : "Agents")"
    }
}
