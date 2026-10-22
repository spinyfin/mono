import SwiftUI

struct AgentsToolbar: View {
    @ObservedObject var grid: AgentsGridModel
    let feedUnavailable: Bool

    var body: some View {
        ViewThatFits(in: .horizontal) {
            controls
            ScrollView(.horizontal) {
                controls.fixedSize(horizontal: true, vertical: false)
            }
            .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(Color(nsColor: .windowBackgroundColor))
    }

    // Preserve every control's readable width. When their combined ideal
    // width no longer fits, the toolbar scrolls instead of squeezing labels.
    private var controls: some View {
        HStack(spacing: 10) {
            if feedUnavailable {
                Label("Showing last known agents", systemImage: "wifi.slash")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .help("The engine feed is disconnected. Membership is the last accepted snapshot.")
            }
            filterMenus
            if grid.layout.selectorsVisible {
                pageSelectors
            }
            if let banner = grid.filterClearedBanner {
                Text(banner)
                    .font(.caption.weight(.medium))
                    .foregroundStyle(Color.accentColor)
                    .transition(.opacity)
            }
            Spacer(minLength: 8)
            if grid.layout.filter.isActive {
                hiddenSummary
            }
            if grid.layout.waitingCount > 0 {
                Text("Needs input \(grid.layout.waitingCount)")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(.orange)
            }
            Button("Tidy") { grid.tidy() }
                .font(.caption)
                .disabled(!grid.layout.isTidyHighlighted)
                .opacity(grid.layout.isTidyHighlighted ? 1 : 0.45)
                .help("Pack agents into the current sort order and drop empty cells.")
            Text(grid.poolStrip)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
        .lineLimit(1)
        .fixedSize(horizontal: true, vertical: false)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var filterMenus: some View {
        HStack(spacing: 6) {
            Menu {
                ForEach(grid.projectOptions, id: \.self) { project in
                    Toggle(grid.projectLabel(project), isOn: Binding(
                        get: { grid.layout.filter.projects?.contains(project) ?? false },
                        set: { _ in grid.toggleProject(project) }
                    ))
                }
            } label: {
                Text(filterTitle("Project", count: grid.layout.filter.projects?.count))
            }
            Menu {
                ForEach(grid.typeOptions, id: \.self) { type in
                    Toggle(type.badgeLabel, isOn: Binding(
                        get: { grid.layout.filter.types?.contains(type) ?? false },
                        set: { _ in grid.toggleType(type) }
                    ))
                }
            } label: {
                Text(filterTitle("Type", count: grid.layout.filter.types?.count))
            }
            if grid.layout.filter.isActive {
                Button("Clear") { grid.clearFilter() }
                    .help("Show all agents again.")
            }
        }
        .font(.caption)
        .menuStyle(.borderlessButton)
        .fixedSize()
    }

    private func filterTitle(_ name: String, count: Int?) -> String {
        guard let count else { return "\(name): All" }
        return "\(name): \(count)"
    }

    /// Hidden agents keep running and streaming; a hidden agent that needs
    /// input is called out so a filter cannot bury it.
    private var hiddenSummary: some View {
        HStack(spacing: 6) {
            Text("\(grid.layout.hiddenCount) hidden")
                .font(.caption)
                .foregroundStyle(.secondary)
            if grid.layout.hiddenWaitingCount > 0 {
                Text("\(grid.layout.hiddenWaitingCount) need input")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(.orange)
            }
        }
        .help("Filtered-out agents are still running; filtering only changes what is shown.")
    }

    private var pageSelectors: some View {
        AgentPageSelectors(layout: grid.layout, onSelect: grid.selectPage)
    }

}
