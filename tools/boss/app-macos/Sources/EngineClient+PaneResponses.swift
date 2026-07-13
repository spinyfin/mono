import Foundation

// Split out of EngineClient.swift for file-size hygiene; behavior is
// unchanged from when these lived inline. Encodes each
// `EngineToAppResponse` variant's success/failure payload and writes
// it back to the engine — the reply half of the engine→app pane RPC
// EngineClient.swift's `engine_request` decode case handles on the
// way in.
extension EngineClient {
    func sendSpawnWorkerPaneResponse(requestId: String, result: EngineSpawnResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success(let slotId, let shellPid):
            resultPayload = [
                "Ok": [
                    "slot_id": slotId,
                    "shell_pid": Int(shellPid),
                ]
            ]
        case .failure(let error):
            resultPayload = ["Err": engineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "spawn_worker_pane",
                "result": resultPayload,
            ],
        ])
    }

    func sendReleaseWorkerPaneResponse(requestId: String, result: EngineReleaseResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success:
            resultPayload = ["Ok": [String: Any]()]
        case .failure(let error):
            resultPayload = ["Err": releaseEngineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "release_worker_pane",
                "result": resultPayload,
            ],
        ])
    }

    func sendSendToPaneResponse(requestId: String, result: EngineSendResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success:
            resultPayload = ["Ok": [String: Any]()]
        case .failure(let error):
            resultPayload = ["Err": sendEngineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "send_to_pane",
                "result": resultPayload,
            ],
        ])
    }

    func sendFocusWorkerPaneResponse(requestId: String, result: EngineFocusResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success:
            resultPayload = ["Ok": [String: Any]()]
        case .failure(let error):
            resultPayload = ["Err": focusEngineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "focus_worker_pane",
                "result": resultPayload,
            ],
        ])
    }

    func sendInterruptWorkerPaneResponse(requestId: String, result: EngineInterruptResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success:
            resultPayload = ["Ok": [String: Any]()]
        case .failure(let error):
            resultPayload = ["Err": interruptEngineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "interrupt_worker_pane",
                "result": resultPayload,
            ],
        ])
    }

    func sendRevealWorkItemResponse(requestId: String, result: EngineRevealResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success:
            resultPayload = ["Ok": [String: Any]()]
        case .failure(let error):
            resultPayload = ["Err": revealEngineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "reveal_work_item",
                "result": resultPayload,
            ],
        ])
    }

    /// Reply to `EngineRequestKind.listHostedPanes`. Always `Ok` — this
    /// is a read-only enumeration of whatever the app currently has;
    /// there is no app-side failure mode analogous to `.unknownSlot` /
    /// `.slotBusy` for a query with no target slot.
    func sendListHostedPanesResponse(requestId: String, panes: [EngineHostedPaneEntry]) {
        let paneDicts: [[String: Any]] = panes.map { pane in
            var dict: [String: Any] = [
                "slot_id": pane.slotId,
                "run_id": pane.runId,
            ]
            if let summary = pane.summary {
                dict["summary"] = summary
            }
            if let taskTitle = pane.taskTitle {
                dict["task_title"] = taskTitle
            }
            return dict
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "list_hosted_panes",
                "result": ["Ok": ["panes": paneDicts]],
            ],
        ])
    }

    private func engineToAppErrorPayload(_ error: EngineSpawnError) -> [String: Any] {
        switch error {
        case .noAvailableSlot:
            return ["kind": "no_available_slot"]
        case .slotBusy(let occupyingRunId):
            var payload: [String: Any] = ["kind": "slot_busy"]
            if let occupyingRunId {
                payload["occupying_run_id"] = occupyingRunId
            }
            return payload
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }

    private func releaseEngineToAppErrorPayload(_ error: EngineReleaseError) -> [String: Any] {
        switch error {
        case .unknownSlot:
            return ["kind": "unknown_slot"]
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }

    private func sendEngineToAppErrorPayload(_ error: EngineSendError) -> [String: Any] {
        switch error {
        case .unknownSlot:
            return ["kind": "unknown_slot"]
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }

    private func focusEngineToAppErrorPayload(_ error: EngineFocusError) -> [String: Any] {
        switch error {
        case .unknownSlot:
            return ["kind": "unknown_slot"]
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }

    private func interruptEngineToAppErrorPayload(_ error: EngineInterruptError) -> [String: Any] {
        switch error {
        case .unknownSlot:
            return ["kind": "unknown_slot"]
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }

    private func revealEngineToAppErrorPayload(_ error: EngineRevealError) -> [String: Any] {
        switch error {
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }
}
