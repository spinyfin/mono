import SwiftUI

/// The "Driver Quota" section of the Engine settings pane: what each
/// driver's *provider* says is left of the maintainer's subscription window,
/// all three side by side.
///
/// # A thin renderer, on purpose
///
/// Everything hard — invoking each CLI, parsing three different output
/// formats, caching, rate-limiting — happens in the engine
/// (`boss-engine-driver-quota`). This view only decides how to phrase what it
/// was handed. It never shells out and never computes a percentage.
///
/// # Failure is a first-class rendering
///
/// Every driver in the roster gets a row, always. A driver the engine could
/// not read shows its typed failure and a one-line reason where the
/// percentage would be — not a blank, not a dash, not a zero. This is the
/// whole point of the pane: a quota display that renders a broken fetch as
/// nothing reads as "plenty of headroom" precisely when the maintainer most
/// needs the truth, so a healthy 0% and a failed probe are given visibly
/// different treatments (a filled bar track and a percentage versus a warning
/// glyph and a label).
///
/// Staleness gets the same treatment: no figure is ever shown without the
/// "Checked …" line, and the engine's throttle decision is surfaced rather
/// than leaving a reader who pressed Refresh wondering why nothing moved.
struct DriverQuotaSection: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    /// Re-rendered once a minute so "Checked 4 min ago" does not sit there
    /// claiming to be fresh while the pane stays open.
    @State private var now: Date = Date()

    private let tick = Timer.publish(every: 60, on: .main, in: .common).autoconnect()

    var body: some View {
        Section {
            ForEach(DriverQuotaSection.roster(from: chatModel.driverQuota), id: \.driver) { entry in
                DriverQuotaRow(entry: entry, now: now)
            }
            HStack(spacing: 8) {
                Text(chatModel.driverQuota.checkedText(now: now))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if chatModel.driverQuota.refreshThrottled {
                    Text("· refresh declined, checked very recently")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("Refresh") {
                    chatModel.refreshDriverQuota(force: true)
                }
                .disabled(chatModel.isRefreshingDriverQuota)
            }
            .padding(.top, 2)
        } header: {
            Text("Driver Quota")
        } footer: {
            Text("Each figure comes from that driver's own provider, read out of band — Claude Code's /usage in print mode, Codex's app-server rate-limit method, and the endpoint Grok's own /usage calls. These are provider numbers, not Boss's internal token accounting, which measures only work Boss dispatched and would understate the total. Read-only: nothing here throttles dispatch or changes the traffic split. Refreshed at most every 15 minutes on its own, or on demand with Refresh (declined if a check ran in the last minute). A driver Boss cannot read says so explicitly rather than showing nothing.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .onReceive(tick) { now = $0 }
    }

    /// Every driver Boss implements, in a stable order, with the engine's
    /// entry where there is one.
    ///
    /// A driver the engine omitted entirely still gets a row, marked as
    /// having produced no result. Silently dropping it would be exactly the
    /// "quietly skip a driver that failed" behaviour this pane exists to
    /// prevent — the roster is the app's own guarantee that the reader sees
    /// three drivers or is told why they do not.
    static func roster(from snapshot: DriverQuotaSnapshot) -> [DriverQuotaEntry] {
        let known = Dictionary(uniqueKeysWithValues: snapshot.entries.map { ($0.driver, $0) })
        return DriverSlug.allCases
            .map(\.rawValue)
            .sorted()
            .map { slug in
                known[slug]
                    ?? DriverQuotaEntry(
                        driver: slug,
                        observedAtEpochS: snapshot.generatedAtEpochS ?? 0,
                        outcome: .unavailable(
                            kind: .probeFailed,
                            reason: snapshot.neverChecked
                                ? "not checked yet"
                                : "the engine returned no result for this driver"
                        )
                    )
            }
    }
}

/// One driver's row: name, figure or failure, and the "as of" that makes the
/// figure meaningful.
private struct DriverQuotaRow: View {
    let entry: DriverQuotaEntry
    let now: Date

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Circle()
                    .fill(DriverSlug(rawValue: entry.driver)?.tint ?? .secondary)
                    .frame(width: 8, height: 8)
                Text(entry.displayName)
                    .font(.body.weight(.medium))
                    .frame(minWidth: 64, alignment: .leading)
                Spacer()
                switch entry.outcome {
                case .reading(let reading):
                    Text("\(reading.usedPercentText) used")
                        .font(.body.monospacedDigit())
                    Text(reading.window.label)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                case .unavailable(let kind, _):
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)
                    Text(kind.shortLabel)
                        .font(.body)
                        .foregroundStyle(.orange)
                }
            }
            switch entry.outcome {
            case .reading(let reading):
                DriverQuotaBar(usedPercent: reading.usedPercent)
                Text(detailLine(for: reading))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            case .unavailable(_, let reason):
                Text("\(reason) · tried \(observedText)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(entry.displayName) quota")
        .accessibilityValue(accessibilityValue)
    }

    /// Reset clause (when the provider gave one), the "as of", and the
    /// mechanism that produced the figure — joined into one caption so a
    /// number never stands alone.
    private func detailLine(for reading: DriverQuotaReading) -> String {
        var parts: [String] = []
        if let resets = reading.resetsText(now: now) { parts.append(resets) }
        parts.append("read \(observedText)")
        parts.append("via \(reading.source)")
        return parts.joined(separator: " · ")
    }

    private var observedText: String {
        guard entry.observedAtEpochS > 0 else { return "never" }
        let age = Int(now.timeIntervalSince1970) - entry.observedAtEpochS
        return DriverQuotaSnapshot.ageText(seconds: age)
    }

    private var accessibilityValue: String {
        switch entry.outcome {
        case .reading(let reading):
            return "\(reading.usedPercentText) used \(reading.window.label), read \(observedText)"
        case .unavailable(let kind, let reason):
            return "unavailable: \(kind.shortLabel), \(reason)"
        }
    }
}

/// Usage rendered as a proportional bar.
///
/// Only ever drawn for a real reading, so an empty track unambiguously means
/// "0% used" and never "we could not tell" — the failure case never reaches
/// this view.
private struct DriverQuotaBar: View {
    let usedPercent: Double

    var body: some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                RoundedRectangle(cornerRadius: 3)
                    .fill(Color.secondary.opacity(0.2))
                RoundedRectangle(cornerRadius: 3)
                    .fill(Color.accentColor)
                    .frame(width: geo.size.width * CGFloat(min(max(usedPercent, 0), 100)) / 100)
            }
        }
        .frame(height: 6)
        .accessibilityHidden(true)
    }
}
