import SwiftUI

/// Pure chrome rules for the background-work toolbar button and its
/// read-only popover. Lives outside the view so tests can cover zero /
/// one / many, the 99+ cap, accessibility copy, planner elapsed time,
/// conflict elapsed omission, and last-item dismissal without a
/// SwiftUI host.
enum BackgroundWorkToolbarChrome {
    static let badgeCap = 99

    /// Hidden when the engine snapshot is empty — including when the
    /// mechanical-rung feature flags are off and contribute zero items.
    static func isVisible(count: Int) -> Bool { count > 0 }

    /// Visual badge only. `nil` when the button itself is hidden.
    static func badgeText(count: Int) -> String? {
        guard count > 0 else { return nil }
        if count > badgeCap { return "99+" }
        return "\(count)"
    }

    /// Spoken label uses the true count, not the visual cap.
    static func accessibilityLabel(count: Int) -> String {
        if count == 1 { return "1 background operation running" }
        return "\(count) background operations running"
    }

    /// When the last item completes, dismiss rather than showing an
    /// empty popover. The view applies this on every snapshot replace.
    static func shouldDismissPopover(visibleCount: Int) -> Bool {
        visibleCount == 0
    }

    static func rows(
        items: [BackgroundWorkItem],
        projectName: (String) -> String?,
        workItemName: (String) -> String?,
        now: Date
    ) -> [BackgroundWorkRowPresentation] {
        items.map { item in
            BackgroundWorkRowPresentation.from(
                item: item,
                projectName: item.projectID.flatMap(projectName),
                workItemName: item.workItemID.flatMap(workItemName),
                now: now
            )
        }
    }
}

/// One popover row: engine-authored title and phase, optional project
/// or work-item context, and elapsed time only when the engine supplied
/// a real operation start. No actions.
struct BackgroundWorkRowPresentation: Equatable, Identifiable {
    let id: String
    let title: String
    let context: String?
    let phase: String
    let elapsed: String?

    static func from(
        item: BackgroundWorkItem,
        projectName: String?,
        workItemName: String?,
        now: Date
    ) -> BackgroundWorkRowPresentation {
        let context: String?
        if let workItemID = item.workItemID {
            context = workItemName ?? workItemID
        } else if let projectID = item.projectID {
            context = projectName ?? projectID
        } else {
            context = nil
        }
        return BackgroundWorkRowPresentation(
            id: item.id,
            title: item.title,
            context: context,
            phase: item.phase,
            elapsed: BackgroundWorkElapsed.text(startedAt: item.startedAt, now: now)
        )
    }
}

/// Elapsed-time rendering for a background-work row. Planners carry
/// `started_at` as unix epoch seconds; mechanical conflict rungs omit
/// it, and this helper must not invent a start from attempt age.
enum BackgroundWorkElapsed {
    static func text(startedAt: String?, now: Date) -> String? {
        guard let startedAt, let date = parse(startedAt) else { return nil }
        let seconds = max(0, Int(now.timeIntervalSince(date)))
        return WorkerStaleness.format(seconds: seconds)
    }

    /// Engine planner timestamps are epoch-second strings. ISO-8601 is
    /// accepted so a future source that already stamps that form still
    /// renders; anything else is omitted rather than guessed.
    static func parse(_ raw: String) -> Date? {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty,
           trimmed.allSatisfy({ $0 >= "0" && $0 <= "9" }),
           let secs = TimeInterval(trimmed)
        {
            return Date(timeIntervalSince1970: secs)
        }
        return WorkerStaleness.parse(trimmed)
    }
}

/// Primary-action toolbar button adjacent to the updater. Hidden at
/// count 0; otherwise a compact glyph plus capped numeric badge.
/// Clicking opens the read-only reveal popover.
struct BackgroundWorkToolbarButton: View {
    @ObservedObject var model: ChatViewModel
    @State private var isPopoverPresented = false

    private var count: Int { model.backgroundWorkVisibleCount }

    var body: some View {
        if BackgroundWorkToolbarChrome.isVisible(count: count) {
            Button {
                isPopoverPresented.toggle()
            } label: {
                Image(systemName: "gearshape.2")
                    .overlay(alignment: .topTrailing) {
                        if let badge = BackgroundWorkToolbarChrome.badgeText(count: count) {
                            Text(badge)
                                .font(.system(size: 9, weight: .bold))
                                .foregroundStyle(.white)
                                .padding(.horizontal, 4)
                                .padding(.vertical, 1)
                                .background(Capsule().fill(Color.accentColor))
                                .offset(x: 9, y: -7)
                                .fixedSize()
                        }
                    }
            }
            .help(BackgroundWorkToolbarChrome.accessibilityLabel(count: count))
            .accessibilityLabel(BackgroundWorkToolbarChrome.accessibilityLabel(count: count))
            .accessibilityIdentifier("boss.backgroundWorkToolbar")
            .popover(isPresented: $isPopoverPresented, arrowEdge: .bottom) {
                BackgroundWorkPopover(model: model)
            }
            .onChange(of: count) { _, newCount in
                if BackgroundWorkToolbarChrome.shouldDismissPopover(visibleCount: newCount) {
                    isPopoverPresented = false
                }
            }
        }
    }
}

/// Read-only list of the current snapshot. No buttons, history, empty
/// state, or dashboard chrome. Elapsed time ticks while the popover is
/// open so a long planner run does not freeze at the open instant.
private struct BackgroundWorkPopover: View {
    @ObservedObject var model: ChatViewModel

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            let rows = BackgroundWorkToolbarChrome.rows(
                items: model.backgroundWork,
                projectName: { model.project(withID: $0)?.name },
                workItemName: { model.task(withID: $0)?.name },
                now: context.date
            )
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(rows.enumerated()), id: \.element.id) { index, row in
                        if index > 0 {
                            Divider()
                        }
                        BackgroundWorkRowView(row: row)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(minWidth: 280, maxWidth: 360, maxHeight: 360)
            .padding(4)
        }
    }
}

private struct BackgroundWorkRowView: View {
    let row: BackgroundWorkRowPresentation

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(row.title)
                .font(.headline)
            if let context = row.context {
                Text(context)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(row.phase)
                    .font(.subheadline)
                    .fixedSize(horizontal: false, vertical: true)
                if let elapsed = row.elapsed {
                    Text(elapsed)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(accessibilityLabel)
    }

    private var accessibilityLabel: String {
        var parts = [row.title]
        if let context = row.context { parts.append(context) }
        parts.append(row.phase)
        if let elapsed = row.elapsed { parts.append(elapsed) }
        return parts.joined(separator: ", ")
    }
}
