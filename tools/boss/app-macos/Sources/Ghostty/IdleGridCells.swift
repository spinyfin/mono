import PaneLayout
import SwiftUI

/// One decorative cell on the Agents grid standing in for an unassigned
/// rectangle. Never selectable and carries no run identity.
struct IdleGridCell: Equatable {
    let localIndex: Int
    let persona: String
    /// Stable flavor-line selector; derived from the same seed as the persona.
    let flavorCycle: Int
}

/// Chooses which off-duty crew member fills each empty cell of a page.
///
/// Pure and deterministic: the result depends only on the inputs, so
/// repeated layout passes over the same agents never re-roll, and a
/// change in the set of agents (the `seedRunIds`) may pick fresh faces.
enum IdleGridCellPlanner {
    static func cells(
        for page: PaneLayoutPage,
        busyPersonas: Set<String>,
        seedRunIds: [String],
        crew: [String] = TrekPortraitGrid.workerNames
    ) -> [IdleGridCell] {
        var available = crew.filter { !busyPersonas.contains($0) }
        let seed = seedRunIds.sorted().joined(separator: "\u{1f}")
        var result: [IdleGridCell] = []
        for cell in page.cells {
            guard case .empty = cell.content else { continue }
            guard !available.isEmpty else { break }
            let hash = fnv1a("\(seed)|\(page.index)|\(cell.localIndex)")
            let pick = Int(hash % UInt64(available.count))
            let persona = available.remove(at: pick)
            result.append(
                IdleGridCell(
                    localIndex: cell.localIndex,
                    persona: persona,
                    flavorCycle: Int((hash >> 20) % 10_000)
                )
            )
        }
        return result
    }

    /// FNV-1a: stable across processes, unlike `Hasher`.
    private static func fnv1a(_ text: String) -> UInt64 {
        var hash: UInt64 = 0xcbf2_9ce4_8422_2325
        for byte in text.utf8 {
            hash ^= UInt64(byte)
            hash = hash &* 0x0000_0100_0000_01b3
        }
        return hash
    }
}

/// Pixel-art portrait, title, and off-duty flavor line. Shared by the
/// whole-pane "No agents running" state and the per-cell idle fillers.
struct IdleCrewView: View {
    let persona: String
    let title: String
    let flavorCycle: Int
    var compact = false

    var body: some View {
        VStack(spacing: compact ? 8 : 14) {
            Spacer()
            if let nsImage = TrekIconAssets.portrait(forPersona: persona) {
                Image(nsImage: nsImage)
                    .resizable()
                    .interpolation(.none)
                    .aspectRatio(contentMode: .fit)
                    .frame(maxWidth: compact ? 120 : 220, maxHeight: compact ? 130 : 240)
                    .opacity(0.85)
            }
            Text(title)
                .font(compact ? .headline : .title3.weight(.semibold))
                .foregroundStyle(Color.white.opacity(0.85))
            Text(TrekIdleFlavor.line(for: persona, cycle: flavorCycle))
                .font(compact ? .caption : .callout)
                .foregroundStyle(Color.white.opacity(0.6))
                .multilineTextAlignment(.center)
                .padding(.horizontal, 24)
                .lineLimit(3)
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .black))
    }
}
