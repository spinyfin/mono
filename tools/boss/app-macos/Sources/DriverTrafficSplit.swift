import Foundation

/// The three drivers global traffic allocation distributes eligible dispatch
/// between, in the order they occupy the engine's `[0, 100)` bucket line
/// (`codex | claude | grok`, low to high — see the engine's
/// `work::driver_allocation`). The Settings bar and stepper list render in
/// this order so the on-screen bar reads left to right as the same line the
/// engine compares its hash against.
enum DriverSlug: String, CaseIterable, Sendable {
    case codex
    case claude
    case grok

    var displayName: String {
        switch self {
        case .codex: return "Codex"
        case .claude: return "Claude"
        case .grok: return "Grok"
        }
    }
}

/// Mirror of the engine's `DriverTrafficSplit`: three whole-percentage shares
/// that must sum to exactly 100.
///
/// The engine *rejects* a split that does not sum to 100 rather than repairing
/// it, so the UI's job is to make an invalid split unreachable in the first
/// place. That is what `adjusting(_:to:)` is for: the operator edits one
/// driver's share and the others absorb the difference, so the sum invariant
/// holds by construction at every intermediate step. There is no "apply"
/// button, no rejected intermediate, and every valid state — including any
/// one or any two drivers at zero — is reachable by repeated single-share
/// edits.
struct DriverTrafficSplit: Equatable, Sendable {
    var grok: Int
    var claude: Int
    var codex: Int

    /// The engine's default and the behaviour-preserving state: everything on
    /// `claude` (the engine's default driver), nothing on `grok` or `codex`.
    static let engineDefault = DriverTrafficSplit(grok: 0, claude: 100, codex: 0)

    var total: Int { grok + claude + codex }

    var isValid: Bool { total == 100 }

    func share(for driver: DriverSlug) -> Int {
        switch driver {
        case .codex: return codex
        case .claude: return claude
        case .grok: return grok
        }
    }

    private func settingShare(_ value: Int, for driver: DriverSlug) -> DriverTrafficSplit {
        var next = self
        switch driver {
        case .codex: next.codex = value
        case .claude: next.claude = value
        case .grok: next.grok = value
        }
        return next
    }

    /// Which drivers give way when `driver`'s share moves, in priority order.
    ///
    /// `claude` is first for the other two because it is the engine's default
    /// driver — the safest place for traffic nobody has deliberately claimed,
    /// and therefore both the natural donor when a share is raised and the
    /// natural recipient when one is lowered. Lowering `claude` itself has to
    /// go somewhere, and it goes to `codex` before `grok`: `grok` only ever
    /// receives traffic the operator explicitly gave it.
    private static func donors(for driver: DriverSlug) -> [DriverSlug] {
        [.claude, .codex, .grok].filter { $0 != driver }
    }

    /// This split with `driver` set to `value`, the difference taken from (or
    /// given back to) the other two in `donors(for:)` order. The result always
    /// sums to 100, so it can always be sent to the engine as-is.
    ///
    /// Raising a share drains the donors in order, each only as far as zero.
    /// Because the shares already sum to 100, the donors between them always
    /// hold exactly enough to cover any target up to 100 — a raise never
    /// falls short and never needs clamping. Lowering hands the whole surplus
    /// to the first donor.
    func adjusting(_ driver: DriverSlug, to value: Int) -> DriverTrafficSplit {
        let target = min(max(value, 0), 100)
        var delta = target - share(for: driver)
        guard delta != 0 else { return self }
        var next = settingShare(target, for: driver)
        let donors = Self.donors(for: driver)
        if delta > 0 {
            for donor in donors where delta > 0 {
                let taken = min(delta, next.share(for: donor))
                next = next.settingShare(next.share(for: donor) - taken, for: donor)
                delta -= taken
            }
        } else if let recipient = donors.first {
            next = next.settingShare(next.share(for: recipient) - delta, for: recipient)
        }
        return next
    }
}
