import AppKit
import SwiftUI

/// "Backups" pane — off-machine copies of the engine's `state.db` snapshots.
///
/// Edits `[backup.offsite]` in the engine's `settings.toml` through the
/// `set_offsite_backup` RPC, so the folder no longer has to be hand-written
/// into the file. The destination is any directory a sync agent replicates;
/// the engine reads the section at startup, so a saved change applies after
/// an engine restart (offered inline).
struct BackupSettingsPane: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    @State private var draft = OffsiteBackupSettings(
        enabled: false,
        destination: nil,
        keepHourly: OffsiteBackupSettings.defaultKeepHourly,
        keepDaily: OffsiteBackupSettings.defaultKeepDaily
    )
    /// Whether `draft` has been seeded from the engine's answer. Edits made
    /// before the first reply must not be overwritten by it.
    @State private var seeded = false
    @State private var savedNeedsRestart = false

    private var saved: OffsiteBackupSettings? { chatModel.offsiteBackup }

    private var isDirty: Bool {
        guard let saved else { return false }
        return normalized(draft) != normalized(saved)
    }

    var body: some View {
        Form {
            Section {
                Toggle("Copy backups off this machine", isOn: $draft.enabled)
                HStack {
                    Text(draft.destination?.isEmpty == false ? draft.destination! : "No folder chosen")
                        .foregroundStyle(draft.destination?.isEmpty == false ? .primary : .secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .textSelection(.enabled)
                    Spacer()
                    Button("Choose…") { chooseFolder() }
                }
                Stepper("Keep \(draft.keepHourly) hourly", value: $draft.keepHourly, in: 0...1000)
                Stepper("Keep \(draft.keepDaily) daily", value: $draft.keepDaily, in: 0...1000)
                if let problem = draft.validationProblem {
                    Text(problem)
                        .font(.caption)
                        .foregroundStyle(.orange)
                }
                if let loadError = saved?.loadError {
                    Text("The stored [backup.offsite] section could not be read, so defaults are shown and saving replaces it: \(loadError)")
                        .font(.caption)
                        .foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                }
                HStack(spacing: 8) {
                    Button("Save") { save() }
                        .disabled(!isDirty || draft.validationProblem != nil)
                    Button("Revert") { revert() }
                        .disabled(!isDirty)
                    if savedNeedsRestart && !isDirty {
                        Button("Restart engine to apply") {
                            savedNeedsRestart = false
                            chatModel.restartEngine()
                        }
                        .disabled(chatModel.isRestartingEngine)
                    }
                }
            } header: {
                Text("Off-Machine Backup")
            } footer: {
                Text("Each finished state.db snapshot is also copied into <folder>/<this host>/. Pick a folder your sync client (iCloud Drive, Dropbox, Google Drive, …) replicates; Boss confirms the copy landed in the folder but cannot see whether the sync client uploaded it. Changes apply the next time the engine starts.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .formStyle(.grouped)
        .padding()
        .onAppear { seedIfNeeded(chatModel.offsiteBackup) }
        .onChange(of: chatModel.offsiteBackup) { _, newValue in
            seedIfNeeded(newValue)
        }
    }

    private func normalized(_ settings: OffsiteBackupSettings) -> OffsiteBackupSettings {
        var copy = settings
        let trimmed = copy.destination?.trimmingCharacters(in: .whitespacesAndNewlines)
        copy.destination = (trimmed?.isEmpty ?? true) ? nil : trimmed
        copy.loadError = nil
        return copy
    }

    private func seedIfNeeded(_ settings: OffsiteBackupSettings?) {
        guard let settings else { return }
        if !seeded || !isDirty {
            // First reply, or a save echo while the form matches what was sent.
            draft = settings
            draft.loadError = nil
            seeded = true
        }
    }

    private func save() {
        savedNeedsRestart = true
        chatModel.saveOffsiteBackup(normalized(draft))
    }

    private func revert() {
        if let saved {
            draft = saved
            draft.loadError = nil
        }
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = "Choose"
        panel.message = "Choose a folder that your sync client replicates."
        if panel.runModal() == .OK, let url = panel.url {
            draft.destination = url.path
        }
    }
}
