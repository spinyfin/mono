import AppKit
import SwiftUI
import UpdateCore

/// Sheet shown by "Check for Updates…" and (in a later task) by the chrome badge.
/// Driven by `UpdateModel`; all state transitions happen there.
struct UpdateResultSheet: View {
    let requestQuit: () -> Void

    @EnvironmentObject private var updateModel: UpdateModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        content
            .padding(24)
            .frame(minWidth: 480, maxWidth: 560)
    }

    // MARK: - State dispatch

    @ViewBuilder
    private var content: some View {
        if updateModel.isChecking {
            checkingView
        } else {
            switch updateModel.lastCheckResult {
            case nil:
                checkingView
            case .upToDate:
                upToDateView
            case .available(let update):
                availableView(update: update)
            case .rateLimited(let retryAfter):
                rateLimitedView(retryAfter: retryAfter)
            case .networkError(let message):
                errorView(message: message)
            }
        }
    }

    // MARK: - Checking state

    private var checkingView: some View {
        VStack(alignment: .leading, spacing: 20) {
            HStack(spacing: 12) {
                ProgressView()
                    .controlSize(.regular)
                Text("Checking for updates…")
                    .font(.title2.weight(.semibold))
            }
            Divider()
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
            }
        }
    }

    // MARK: - Up to date

    private var upToDateView: some View {
        let version = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "?"
        return VStack(alignment: .leading, spacing: 20) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "checkmark.circle.fill")
                    .font(.largeTitle)
                    .foregroundStyle(.green)
                VStack(alignment: .leading, spacing: 4) {
                    Text("Boss is up to date")
                        .font(.title2.weight(.semibold))
                    Text("Boss \(version) is the latest version.")
                        .foregroundStyle(.secondary)
                }
            }
            Divider()
            HStack {
                Spacer()
                Button("OK") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
        }
    }

    // MARK: - Update available

    @ViewBuilder
    private func availableView(update: AvailableUpdate) -> some View {
        let isDevBuild = updateModel.isDevBuild
        let fullVersion = Bundle.main.infoDictionary?["BossFullVersion"] as? String

        VStack(alignment: .leading, spacing: 20) {
            // Header
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "arrow.down.circle.fill")
                    .font(.largeTitle)
                    .foregroundStyle(Color.accentColor)
                VStack(alignment: .leading, spacing: 4) {
                    Text("Boss \(update.version.description) is available")
                        .font(.title2.weight(.semibold))
                    if isDevBuild, let fv = fullVersion {
                        Label("Running a development build (\(fv))", systemImage: "hammer")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    }
                }
            }

            // Release notes
            if !update.changelog.isEmpty || !update.releaseNotes.isEmpty {
                changelogView(changelog: update.changelog, fallbackNotes: update.releaseNotes)
            }

            // In-app download/stage status (release builds only).
            if !isDevBuild {
                if let note = downloadStatusNote(for: update) {
                    Text(note)
                        .font(.caption)
                        .foregroundStyle(downloadFailed(for: update) ? .orange : .secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                downloadProgressBar(for: update)
            }

            Divider()

            // Buttons
            HStack(spacing: 8) {
                if !isDevBuild {
                    Button("Skip This Version") {
                        updateModel.skipCurrentVersion()
                        dismiss()
                    }
                    .foregroundStyle(.secondary)
                    .buttonStyle(.plain)
                }

                Spacer()

                Button("Later") { dismiss() }
                    .keyboardShortcut(.cancelAction)

                UpdatePrimaryActionButton(
                    update: update,
                    updateModel: updateModel,
                    requestQuit: requestQuit,
                    onDevDownload: {
                        NSWorkspace.shared.open(update.assetURL)
                        dismiss()
                    }
                )
            }
        }
    }

    /// The download progress bar, shown only while `update` is actively
    /// downloading. Determinate when the server reported a content length,
    /// indeterminate otherwise. `EmptyView` for every other state.
    @ViewBuilder
    private func downloadProgressBar(for update: AvailableUpdate) -> some View {
        if case .downloading(let v, let progress) = updateModel.downloadState, v == update.version {
            switch progress {
            case .determinate(let fraction):
                ProgressView(value: fraction)
            case .indeterminate:
                ProgressView()
            }
        }
    }

    /// One-line status under the release notes describing the current download/stage
    /// for `update`. `nil` when idle (the button text carries the affordance).
    private func downloadStatusNote(for update: AvailableUpdate) -> String? {
        if let cancelled = updateModel.quitCancelledStatusNote(for: update.version) {
            return cancelled
        }
        switch updateModel.downloadState {
        case .downloading(let v, let progress) where v == update.version:
            switch progress {
            case .determinate(let fraction):
                let pct = Int((fraction * 100).rounded())
                return pct > 0 ? "Downloading Boss \(update.version)… \(pct)%" : "Downloading Boss \(update.version)…"
            case .indeterminate:
                return "Downloading Boss \(update.version)…"
            }
        case .readyToInstall(let v) where v == update.version:
            return "Boss \(update.version) downloaded and verified. Install & Relaunch to apply it now."
        case .installedPendingRelaunch(let v, let willRelaunch) where v == update.version:
            return willRelaunch
                ? "Boss \(update.version) is installed. Quit Boss to finish — it will relaunch on the new version."
                : "Boss \(update.version) is installed. Quit and reopen Boss to finish updating."
        case .failed(let v, let reason) where v == update.version:
            return "Download failed: \(reason)"
        case .installFailed(let v, let reason) where v == update.version:
            return "Install failed: \(reason)"
        default:
            return nil
        }
    }

    private func downloadFailed(for update: AvailableUpdate) -> Bool {
        switch updateModel.downloadState {
        case .failed(let v, _) where v == update.version: return true
        case .installFailed(let v, _) where v == update.version: return true
        default: return false
        }
    }

    // MARK: - Changelog helpers

    @ViewBuilder
    private func changelogView(changelog: [ReleaseNote], fallbackNotes: String) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Release Notes")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .textCase(.uppercase)

            ScrollView {
                ReleaseNotesContent(changelog: changelog, fallbackNotes: fallbackNotes)
                    .padding(12)
            }
            .frame(maxHeight: 300)
            .background(Color(nsColor: .controlBackgroundColor))
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .overlay(
                RoundedRectangle(cornerRadius: 8)
                    .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
            )
        }
    }

    // MARK: - Rate limited

    private func rateLimitedView(retryAfter: Date) -> some View {
        let formatted = retryAfter.formatted(.dateTime.hour().minute())
        return VStack(alignment: .leading, spacing: 20) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "clock.fill")
                    .font(.largeTitle)
                    .foregroundStyle(.orange)
                VStack(alignment: .leading, spacing: 4) {
                    Text("Rate limit reached")
                        .font(.title2.weight(.semibold))
                    Text("Too many requests to GitHub. Try again after \(formatted).")
                        .foregroundStyle(.secondary)
                }
            }
            Divider()
            HStack {
                Spacer()
                Button("OK") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
        }
    }

    // MARK: - Network error

    private func errorView(message: String) -> some View {
        VStack(alignment: .leading, spacing: 20) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.largeTitle)
                    .foregroundStyle(.orange)
                VStack(alignment: .leading, spacing: 4) {
                    Text("Update check failed")
                        .font(.title2.weight(.semibold))
                    Text(message)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Divider()
            HStack {
                Spacer()
                Button("OK") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
        }
    }
}
