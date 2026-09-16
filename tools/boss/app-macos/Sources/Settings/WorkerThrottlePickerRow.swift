import SwiftUI

/// False is Off, true is Darwin's background tier. Uses the engine's
/// persisted Settings snapshot so there is no separate app preference.
struct WorkerThrottlePickerRow: View {
    let setting: EngineSetting
    let onChange: (Bool) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Picker("Worker throttle level", selection: Binding(
                get: { setting.enabled },
                set: { onChange($0) }
            )) {
                Text("Off (default)").tag(false)
                Text("Background").tag(true)
            }
            .pickerStyle(.segmented)
            Text(setting.description)
                .font(.caption)
                .foregroundStyle(.secondary)
            Text("Applies to newly spawned panes immediately. Existing panes keep their priority. Build servers started by a background pane keep that priority until they exit.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(.vertical, 2)
    }
}
