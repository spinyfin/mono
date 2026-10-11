import SwiftUI
import UpdateCore

/// Shared Install & Relaunch / failure mapping used by the update sheet and
/// the chrome popover. Marks installed state before requesting quit so a
/// vetoed terminate still shows Quit to Finish.
@MainActor
enum UpdateInstallAction {
    static let notInstalledReason =
        "The app bundle could not be updated. Make sure Boss is installed in /Applications and try again."

    /// Status line for a version the relaunch watchdog rolled back: explains why the
    /// primary action reads "Retry Update" instead of "Download". `nil` when `update`
    /// is not blocklisted.
    static func blockedStatusNote(for update: AvailableUpdate, updateModel: UpdateModel) -> String? {
        guard updateModel.isBlocked(update.version) else { return nil }
        guard let record = updateModel.rollbackRecord(for: update.version) else {
            return "Boss \(update.version) failed to launch after a previous install and was rolled back. Retry Update downloads and installs it again."
        }
        var when = ""
        if let date = ISO8601DateFormatter.rollbackParser().date(from: record.recordedAt) {
            when = " on \(date.formatted(date: .abbreviated, time: .shortened))"
        }
        return "Boss \(update.version) failed to launch\(when) (\(record.reason.userDescription)) and was rolled back. Retry Update downloads and installs it again."
    }

    static func installAndRequestQuit(
        version: VersionTuple,
        updateModel: UpdateModel,
        requestQuit: () -> Void,
        install: () -> InstallOutcome = { UpdateLifecycle.installStagedAndRelaunch() }
    ) {
        switch install() {
        case .relaunchPending:
            // Persist installed state before dismissal. A cancelled quit can
            // reopen the sheet and retry without swapping again.
            updateModel.markInstalledPendingRelaunch(version: version, willRelaunch: true)
            updateModel.clearQuitReturnedWithoutTerminating()
            requestQuit()
        case .installedNoRelaunch:
            updateModel.markInstalledPendingRelaunch(version: version, willRelaunch: false)
        case .notInstalled:
            updateModel.markInstallFailed(version: version, reason: notInstalledReason)
        }
    }
}

/// Trailing call-to-action shared by ``UpdateResultSheet`` and the toolbar
/// badge popover. Dev builds keep the manual browser download; release builds
/// stage in-app and then offer Install & Relaunch / Quit to Finish.
struct UpdatePrimaryActionButton: View {
    let update: AvailableUpdate
    @ObservedObject var updateModel: UpdateModel
    let requestQuit: () -> Void
    let onDevDownload: () -> Void

    var body: some View {
        if updateModel.isDevBuild {
            Button("Download", action: onDevDownload)
                .keyboardShortcut(.defaultAction)
        } else {
            switch updateModel.downloadState {
            case .downloading(let v, _) where v == update.version:
                Button {
                } label: {
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.small)
                        Text("Downloading…")
                    }
                }
                .disabled(true)

            case .installFailed(let v, _) where v == update.version:
                Button("Install Failed") {}
                    .disabled(true)

            case .readyToInstall(let v) where v == update.version:
                Button("Install & Relaunch") {
                    UpdateInstallAction.installAndRequestQuit(
                        version: v,
                        updateModel: updateModel,
                        requestQuit: requestQuit
                    )
                }
                .keyboardShortcut(.defaultAction)

            case .installedPendingRelaunch(let v, _) where v == update.version:
                Button("Quit to Finish") {
                    updateModel.clearQuitReturnedWithoutTerminating()
                    requestQuit()
                }
                .keyboardShortcut(.defaultAction)

            case .failed(let v, _) where v == update.version:
                Button("Retry Download") {
                    updateModel.downloadAvailableUpdate()
                }
                .keyboardShortcut(.defaultAction)

            default:
                if updateModel.isBlocked(update.version) {
                    // The watchdog rolled this version back. Background polling never
                    // re-stages it; only this deliberate retry clears the block.
                    Button("Retry Update") {
                        updateModel.retryBlockedUpdate(update.version)
                    }
                    .keyboardShortcut(.defaultAction)
                } else {
                    Button("Download") {
                        updateModel.downloadAvailableUpdate()
                    }
                    .keyboardShortcut(.defaultAction)
                }
            }
        }
    }
}

extension ISO8601DateFormatter {
    /// Parses `RollbackRecord.recordedAt` (UTC with fractional seconds). Built per call:
    /// the formatter is not `Sendable`, so a shared static would fail strict concurrency.
    static func rollbackParser() -> ISO8601DateFormatter {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }
}
