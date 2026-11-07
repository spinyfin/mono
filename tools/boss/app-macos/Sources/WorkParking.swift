import SwiftUI

// ===========================================================================
// Parking: the owner's explicit "not now" for a task, chore or project.
//
// The engine owns the semantics (see `tools/boss/engine/work-db/src/parking.rs`
// and section 1 of `docs/designs/task-persistence-parking-and-autonomous-
// progression.md`): a parked row keeps its lifecycle status and is skipped by
// automatic dispatch and upkeep. This file is the app side only: the request
// the context menus raise, the confirmation sheet, the board filter's
// "is this row parked" answer, and the grey chip a parked card carries.
// ===========================================================================

/// What the park sheet is confirming. Projects park every open task in them.
struct ParkRequest: Identifiable, Equatable {
    let id: String
    let name: String
    let isProject: Bool
}

/// The "Parked · <relative time> · <reason>" chip, with the strings fixed at
/// snapshot-build time so the card snapshot stays pure and `Equatable`.
struct ParkedChipPresentation: Equatable {
    /// Chip text, e.g. `Parked · 3 hr. ago · waiting on the design review`.
    let text: String
    /// Full, untruncated explanation for the tooltip.
    let tooltip: String
    /// The row still has a live execution that parking let run to its end.
    let finishingCurrentRun: Bool

    /// `parkedAt` is the engine's epoch-seconds decimal string. An
    /// unparseable stamp drops the time rather than the chip: the row is
    /// still parked.
    static func make(
        parkedAt: String?,
        reason: String?,
        viaProject: Bool,
        finishingCurrentRun: Bool,
        now: Date
    ) -> ParkedChipPresentation {
        var parts = ["Parked"]
        var tooltipParts = [viaProject ? "Parked with its project" : "Parked"]
        if let parkedAt, let seconds = TimeInterval(parkedAt) {
            let date = Date(timeIntervalSince1970: seconds)
            let formatter = RelativeDateTimeFormatter()
            formatter.unitsStyle = .short
            // Clamp so clock skew between engine and app never reads "in 3 sec.".
            let relative = date >= now ? "just now" : formatter.localizedString(for: date, relativeTo: now)
            parts.append(relative)
            tooltipParts.append(relative)
        }
        let trimmed = reason?.trimmingCharacters(in: .whitespacesAndNewlines)
        if let trimmed, !trimmed.isEmpty {
            parts.append(trimmed)
            tooltipParts.append(trimmed)
        }
        var tooltip = tooltipParts.joined(separator: " · ")
            + ". Hidden from the default board; no automatic dispatch, CI fixes, conflict resolution or review runs for it until it is un-parked."
        if finishingCurrentRun {
            tooltip += " Its current run is allowed to finish."
        }
        return ParkedChipPresentation(
            text: parts.joined(separator: " · "),
            tooltip: tooltip,
            finishingCurrentRun: finishingCurrentRun
        )
    }
}

/// Grey capsule in the style of `FutureScopeBadge`.
struct ParkedChip: View {
    let presentation: ParkedChipPresentation

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "pause.circle.fill")
                .font(.caption2)
            Text(presentation.text)
                .font(.caption.weight(.semibold))
                .lineLimit(1)
                .truncationMode(.tail)
        }
        .foregroundStyle(Color.secondary)
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .background(Color.secondary.opacity(0.12))
        .clipShape(Capsule())
        .help(presentation.tooltip)
        .accessibilityLabel(presentation.text)
        .accessibilityValue(presentation.tooltip)
    }
}

/// The chip, plus a "finishing current run" sub-label while the parked row
/// still has a live execution.
struct WorkBoardCardParkedRow: View, @MainActor Equatable {
    let presentation: ParkedChipPresentation

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.presentation == rhs.presentation
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            ParkedChip(presentation: presentation)
            if presentation.finishingCurrentRun {
                Text("Finishing current run")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .padding(.leading, 6)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Confirmation sheet for "Park…": an optional reason, then Park or Cancel.
/// A confirmation of an action the user just chose, which `AGENTS.md` allows.
struct ParkWorkItemSheet: View {
    let request: ParkRequest
    let onCancel: () -> Void
    let onPark: (String?) -> Void

    @State private var reason = ""

    private var explanation: String {
        let scope = request.isProject
            ? "every open task in this project"
            : "this item"
        return "Parking hides \(scope) from the default board and stops automatic dispatch, CI fixes, conflict resolution and review for it. A run that is already in progress is allowed to finish. Un-park it any time; it is brought up to date before work resumes."
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Park \u{201C}\(request.name)\u{201D}?")
                .font(.title3.weight(.semibold))
                .lineLimit(2)
            Text(explanation)
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            TextField("Reason (optional)", text: $reason, axis: .vertical)
                .textFieldStyle(.roundedBorder)
                .lineLimit(1...4)
                .onSubmit(park)
            HStack {
                Spacer()
                Button("Cancel", role: .cancel, action: onCancel)
                    .keyboardShortcut(.cancelAction)
                Button("Park", action: park)
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 420)
    }

    private func park() {
        let trimmed = reason.trimmingCharacters(in: .whitespacesAndNewlines)
        onPark(trimmed.isEmpty ? nil : trimmed)
    }
}

// MARK: - Model

extension ChatViewModel {
    /// Execution states in which a worker is still running for the row.
    /// Parking never kills a live execution (design section 1.3), so a parked
    /// card in one of these is "finishing its current run".
    static let liveExecutionStatuses: Set<String> = ["claimed", "running", "waiting_human"]

    /// The park stamp that applies to `task`: its own, else its project's
    /// (the engine's predicate treats a row in a parked project as parked even
    /// before the cascade stamp lands). `nil` when the row is not parked.
    func parkStamp(for task: WorkTask) -> (parkedAt: String?, reason: String?, viaProject: Bool)? {
        if task.hasOwnParkStamp {
            return (task.parkedAt, task.parkedReason, task.isParkedViaProject)
        }
        guard task.status != "done", task.status != "archived",
              let projectID = task.projectID,
              let project = projectsByProductID[task.productID]?.first(where: { $0.id == projectID }),
              project.isParked
        else { return nil }
        return (project.parkedAt, project.parkedReason, true)
    }

    /// Whether the default board leaves `task` off. Finished work is never
    /// parked, matching the engine.
    func isParked(_ task: WorkTask) -> Bool {
        parkStamp(for: task) != nil
    }

    /// Parked open tasks and chores in `projectID`, for the sidebar count.
    /// Revision rows follow their parent and are not counted separately.
    func parkedTaskCount(forProjectID projectID: String) -> Int {
        (tasksByProjectID[projectID] ?? []).filter { $0.kind != "revision" && isParked($0) }.count
    }

    func parkedChipPresentation(for task: WorkTask, now: Date = Date()) -> ParkedChipPresentation? {
        guard let stamp = parkStamp(for: task) else { return nil }
        let status = taskRuntime(for: task.id)?.executionStatus ?? ""
        return ParkedChipPresentation.make(
            parkedAt: stamp.parkedAt,
            reason: stamp.reason,
            viaProject: stamp.viaProject,
            finishingCurrentRun: Self.liveExecutionStatuses.contains(status),
            now: now
        )
    }

    /// Whether the card menu offers Park / Unpark for `task` at all. A
    /// revision follows its parent PR and finished work has nothing to park.
    func canPark(_ task: WorkTask) -> Bool {
        task.kind != "revision" && task.status != "done" && task.status != "archived"
    }

    func requestPark(task: WorkTask) {
        pendingParkRequest = ParkRequest(id: task.id, name: task.name, isProject: false)
    }

    func requestPark(project: WorkProject) {
        pendingParkRequest = ParkRequest(id: project.id, name: project.name, isProject: true)
    }

    func dismissParkRequest() {
        pendingParkRequest = nil
    }

    func submitPark(_ request: ParkRequest, reason: String?) {
        pendingParkRequest = nil
        engine.sendParkWorkItem(id: request.id, parked: true, reason: reason)
    }

    /// Un-park a task, chore or project. The engine's reply (and the
    /// invalidation push for a project's cascaded children) updates the board.
    func unpark(id: String) {
        engine.sendParkWorkItem(id: id, parked: false)
    }
}
