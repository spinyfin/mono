import Foundation

enum OffsiteBackupControl: Hashable {
    case enabled, destination, hourly, daily
}

struct OffsiteBackupEdit {
    var draft: OffsiteBackupSettings?
    var controls: Set<OffsiteBackupControl> = []
    var errors: [OffsiteBackupControl: String] = [:]
    var requestID: String?
    var refreshID: String?
    var needsReconciliation = false
    var debounce: Task<Void, Never>?
}

extension ChatViewModel {
    var displayedOffsiteBackup: OffsiteBackupSettings? {
        offsiteEdit.draft ?? offsiteBackup
    }

    /// Controls share one pending draft so a toggle or folder choice also
    /// commits any retention clicks whose debounce has not fired yet.
    func changeOffsiteBackup<Value>(
        _ keyPath: WritableKeyPath<OffsiteBackupSettings, Value>,
        to value: Value,
        control: OffsiteBackupControl,
        debounce: Bool = false
    ) {
        guard !offsiteEdit.needsReconciliation, offsiteEdit.requestID == nil, var next = displayedOffsiteBackup else { return }
        offsiteEdit.debounce?.cancel()
        next[keyPath: keyPath] = value
        next.loadError = nil
        offsiteEdit.draft = next
        offsiteEdit.controls.insert(control)
        offsiteEdit.errors[control] = nil
        if debounce {
            offsiteEdit.debounce = Task { @MainActor [weak self] in
                do { try await Task.sleep(for: .milliseconds(300)) } catch { return }
                self?.applyOffsiteBackupEdit()
            }
        } else {
            applyOffsiteBackupEdit()
        }
    }

    private func applyOffsiteBackupEdit() {
        offsiteEdit.debounce = nil
        guard let draft = offsiteEdit.draft else { return }
        guard let id = engine.sendSetOffsiteBackup(draft) else {
            rejectOffsiteBackupEdit("Not connected to the engine. Try again after reconnecting.")
            return
        }
        offsiteEdit.requestID = id
    }

    func receiveOffsiteBackup(_ settings: OffsiteBackupSettings, requestID: String?) {
        if offsiteEdit.needsReconciliation {
            guard let refresh = offsiteEdit.refreshID, requestID == refresh else { return }
            offsiteBackup = settings
            offsiteEdit = OffsiteBackupEdit()
            return
        }
        offsiteBackup = settings
        if let pending = offsiteEdit.requestID, requestID == pending {
            offsiteEdit = OffsiteBackupEdit()
        }
    }

    func disconnectOffsiteBackupEdit() {
        offsiteEdit.debounce?.cancel()
        offsiteEdit.debounce = nil
        offsiteEdit.requestID = nil
        offsiteEdit.refreshID = nil
        offsiteEdit.needsReconciliation = true
        for control in offsiteEdit.controls {
            offsiteEdit.errors[control] = "Connection lost. Waiting to confirm stored settings."
        }
    }

    func settleOffsiteBackupError(_ message: String, requestID: String?) -> Bool {
        guard let pending = offsiteEdit.requestID, requestID == pending else { return false }
        rejectOffsiteBackupEdit(message)
        return true
    }

    func rejectOffsiteBackupEdit(_ message: String) {
        offsiteEdit.debounce?.cancel()
        for control in offsiteEdit.controls {
            offsiteEdit.errors[control] = message
        }
        offsiteEdit.controls = []
        offsiteEdit.draft = nil
        offsiteEdit.requestID = nil
        offsiteEdit.debounce = nil
    }
}
