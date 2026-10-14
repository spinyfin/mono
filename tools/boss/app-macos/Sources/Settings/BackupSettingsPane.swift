import AppKit
import SwiftUI

/// Off-machine copies of the engine's state.db snapshots.
struct BackupSettingsPane: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    var body: some View {
        Form {
            Section {
                if let settings = chatModel.displayedOffsiteBackup {
                    Toggle("Copy backups off this machine", isOn: binding(\.enabled, control: .enabled))
                    inlineError(.enabled)
                    HStack {
                        Text(settings.destination ?? "No folder chosen")
                            .foregroundStyle(settings.destination == nil ? .secondary : .primary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .textSelection(.enabled)
                        Spacer()
                        Button("Choose…") { chooseFolder() }
                    }
                    inlineError(.destination)
                    Stepper("Keep \(settings.keepHourly) hourly", value: binding(\.keepHourly, control: .hourly, debounce: true), in: 0...1000)
                    inlineError(.hourly)
                    Stepper("Keep \(settings.keepDaily) daily", value: binding(\.keepDaily, control: .daily, debounce: true), in: 0...1000)
                    inlineError(.daily)
                    if let loadError = chatModel.offsiteBackup?.loadError {
                        Text("The stored backup settings could not be read: \(loadError)")
                            .font(.caption)
                            .foregroundStyle(.red)
                    }
                } else {
                    ProgressView("Loading backup settings…")
                }
            } header: {
                Text("Off-Machine Backup")
            } footer: {
                Text("Each finished state.db snapshot is also copied into <folder>/<this host>/. Pick a folder your sync client (iCloud Drive, Dropbox, Google Drive, …) replicates; Boss confirms the copy landed in the folder but cannot see whether the sync client uploaded it.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .disabled(chatModel.offsiteEdit.requestID != nil)
        }
        .formStyle(.grouped)
        .padding()
    }

    private func binding<Value>(
        _ keyPath: WritableKeyPath<OffsiteBackupSettings, Value>,
        control: OffsiteBackupControl,
        debounce: Bool = false
    ) -> Binding<Value> {
        Binding(
            get: { chatModel.displayedOffsiteBackup![keyPath: keyPath] },
            set: { chatModel.changeOffsiteBackup(keyPath, to: $0, control: control, debounce: debounce) }
        )
    }

    @ViewBuilder
    private func inlineError(_ control: OffsiteBackupControl) -> some View {
        if let error = chatModel.offsiteEdit.errors[control] {
            Text(error)
                .font(.caption)
                .foregroundStyle(.red)
                .fixedSize(horizontal: false, vertical: true)
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
            chatModel.changeOffsiteBackup(\.destination, to: url.path, control: .destination)
        }
    }
}
