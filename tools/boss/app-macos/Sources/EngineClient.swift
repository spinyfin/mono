import Foundation
import Network

enum EngineEvent {
    case connected
    case disconnected
    case chunk(String)
    case done(String)
    case toolCall(name: String, status: String)
    case permissionRequest(id: String, title: String)
    case error(String)
}

final class EngineClient: @unchecked Sendable {
    var onEvent: (@MainActor @Sendable (EngineEvent) -> Void)?

    private let socketPath: String
    private let queue = DispatchQueue(label: "BossMacApp.EngineClient")
    private var connection: NWConnection?
    private var buffer = Data()
    private var shouldReconnect = false

    init(socketPath: String) {
        self.socketPath = socketPath
    }

    func start() {
        shouldReconnect = true
        connect()
    }

    func stop() {
        shouldReconnect = false
        connection?.cancel()
        connection = nil
        buffer.removeAll(keepingCapacity: false)
    }

    private func connect() {
        guard connection == nil else {
            return
        }

        let parameters = NWParameters(tls: nil, tcp: NWProtocolTCP.Options())
        let endpoint = NWEndpoint.unix(path: socketPath)
        let connection = NWConnection(to: endpoint, using: parameters)
        self.connection = connection

        connection.stateUpdateHandler = { [weak self] (state: NWConnection.State) in
            guard let self else { return }
            switch state {
            case .ready:
                self.emit(.connected)
                self.receiveNext()
            case .waiting(let error):
                self.emit(.error("socket waiting: \(error.localizedDescription)"))
                self.connection = nil
                connection.cancel()
                self.emit(.disconnected)
                self.scheduleReconnect()
            case .failed(let error):
                self.emit(.error("socket failed: \(error.localizedDescription)"))
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
            case .cancelled:
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
            default:
                break
            }
        }

        connection.start(queue: queue)
    }

    func sendPrompt(_ text: String) {
        sendLine([
            "type": "prompt",
            "text": text,
        ])
    }

    func sendPermissionResponse(id: String, granted: Bool) {
        sendLine([
            "type": "permission_response",
            "id": id,
            "granted": granted,
        ])
    }

    private func sendLine(_ payload: [String: Any]) {
        guard let connection else {
            emit(.error("engine connection is not established"))
            return
        }

        do {
            var data = try JSONSerialization.data(withJSONObject: payload, options: [])
            data.append(0x0A)

            connection.send(content: data, completion: .contentProcessed { [weak self] error in
                guard let self else { return }
                if let error {
                    self.emit(.error("socket send failed: \(error.localizedDescription)"))
                }
            })
        } catch {
            emit(.error("failed to encode payload: \(error.localizedDescription)"))
        }
    }

    private func receiveNext() {
        connection?.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, isComplete, error in
            guard let self else { return }

            if let error {
                self.emit(.error("socket receive failed: \(error.localizedDescription)"))
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
                return
            }

            if let data, !data.isEmpty {
                self.buffer.append(data)
                self.consumeLines()
            }

            if isComplete {
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
                return
            }

            self.receiveNext()
        }
    }

    private func consumeLines() {
        while let newline = buffer.firstIndex(of: 0x0A) {
            let lineData = buffer[..<newline]
            buffer.removeSubrange(...newline)

            guard !lineData.isEmpty else {
                continue
            }

            guard let payload = try? JSONSerialization.jsonObject(with: Data(lineData)) as? [String: Any],
                let type = payload["type"] as? String
            else {
                emit(.error("received invalid JSON message from engine"))
                continue
            }

            switch type {
            case "chunk":
                if let text = payload["text"] as? String {
                    emit(.chunk(text))
                }
            case "done":
                let stopReason = payload["stop_reason"] as? String ?? "unknown"
                emit(.done(stopReason))
            case "tool_call":
                let name = payload["name"] as? String ?? "tool"
                let status = payload["status"] as? String ?? "update"
                emit(.toolCall(name: name, status: status))
            case "permission_request":
                let id = payload["id"] as? String ?? ""
                let title = payload["title"] as? String ?? "Permission"
                emit(.permissionRequest(id: id, title: title))
            case "error":
                let message = payload["message"] as? String ?? "unknown engine error"
                emit(.error(message))
            default:
                break
            }
        }
    }

    private func emit(_ event: EngineEvent) {
        Task { @MainActor in
            onEvent?(event)
        }
    }

    private func scheduleReconnect() {
        guard shouldReconnect else {
            return
        }

        queue.asyncAfter(deadline: .now() + 1.0) { [weak self] in
            guard let self, self.shouldReconnect, self.connection == nil else {
                return
            }
            self.connect()
        }
    }
}
