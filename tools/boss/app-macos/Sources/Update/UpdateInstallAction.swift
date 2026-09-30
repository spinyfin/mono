import SwiftUI
import UpdateCore

/// Shared Install & Relaunch / failure mapping used by the update sheet and
/// the chrome popover. Marks installed state before requesting quit so a
/// vetoed terminate still shows Quit to Finish.
@MainActor
enum UpdateInstallAction {
    static let notInstalledReason =
        "The app bundle could not be updated. Make sure Boss is installed in /Applications and try again."

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
                Button("Download") {
                    updateModel.downloadAvailableUpdate()
                }
                .keyboardShortcut(.defaultAction)
            }
        }
    }
}
