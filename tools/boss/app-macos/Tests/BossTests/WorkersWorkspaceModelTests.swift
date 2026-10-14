import XCTest
@testable import Boss

@MainActor
final class WorkersWorkspaceModelSendTests: XCTestCase {
    func testAttachUsesTmuxClientCommandWithoutWorkerEnvironment() {
        let model = WorkersWorkspaceModel()
        let result = model.attachWorkerPane(EngineAttachRequest(
            runId: "run-tmux",
            slotId: 1,
            sessionName: "boss-1-run-tmux",
            tmuxSocketPath: "/state/boss/tmux.sock",
            summary: nil,
            taskTitle: nil
        ))
        guard case .success = result else {
            XCTFail("expected tmux pane attach to succeed, got \(result)")
            return
        }

        let session = model.viewersByRunId["run-tmux"]?.session
        XCTAssertEqual(session?.launchSpec.initialInput, "exec tmux -S '/state/boss/tmux.sock' attach-session -t 'boss-1-run-tmux'\n")
        XCTAssertTrue(session?.launchSpec.env.isEmpty ?? false)
        XCTAssertEqual(session?.launchSpec.workingDirectory, FileManager.default.homeDirectoryForCurrentUser.path)

        let launch = WorkerViewerLaunch(attaching: EngineAttachRequest(
            runId: "run-tmux",
            slotId: 1,
            sessionName: "boss-1-run-tmux",
            tmuxSocketPath: "/state/boss/tmux.sock",
            summary: nil,
            taskTitle: nil
        ))
        let extra = SpawnDiagnosticsLog.spawnRequestedExtra(
            slotId: Int(launch.slotId),
            sessionName: launch.sessionName,
            tmuxSocketPath: launch.tmuxSocketPath
        )
        XCTAssertEqual(extra["session_name"] as? String, "boss-1-run-tmux")
        XCTAssertEqual(extra["tmux_socket_path"] as? String, "/state/boss/tmux.sock")
        XCTAssertNil(extra["workspace_path"])
        XCTAssertFalse(
            extra.values.contains { ($0 as? String) == FileManager.default.homeDirectoryForCurrentUser.path },
            "attachWorkerPane must not write the user's home into spawn_requested"
        )
    }

    func testDetachUnknownSlotReturnsUnknownSlot() {
        let result = WorkersWorkspaceModel().detachWorkerPane(runId: "unknown")
        guard case .failure(.unknownSlot) = result else {
            return XCTFail("expected unknownSlot, got \(result)")
        }
    }

    func testDetachIdleSlotReturnsUnknownSlot() {
        let result = WorkersWorkspaceModel().detachWorkerPane(runId: "run-tmux")
        guard case .failure(.unknownSlot) = result else {
            return XCTFail("expected unknownSlot, got \(result)")
        }
    }

    func testDetachRemovesTmuxViewerSurface() {
        let model = WorkersWorkspaceModel()
        _ = model.attachWorkerPane(EngineAttachRequest(
            runId: "run-tmux",
            slotId: 1,
            sessionName: "boss-1-run-tmux",
            tmuxSocketPath: "/state/boss/tmux.sock",
            summary: nil,
            taskTitle: nil
        ))

        let result = model.detachWorkerPane(runId: "run-tmux")
        guard case .success = result else {
            XCTFail("expected tmux pane detach to succeed, got \(result)")
            return
        }
        XCTAssertNil(model.viewersByRunId["run-tmux"]?.session)
    }

    func testAttachRejectsEmptyTmuxSocketPath() {
        let model = WorkersWorkspaceModel()
        let result = model.attachWorkerPane(EngineAttachRequest(
            runId: "run-tmux",
            slotId: 1,
            sessionName: "boss-1-run-tmux",
            tmuxSocketPath: "",
            summary: nil,
            taskTitle: nil
        ))
        guard case .failure(.internalFailure(let message)) = result else {
            XCTFail("expected .internalFailure for empty socket path, got \(result)")
            return
        }
        XCTAssertTrue(message.contains("tmux socket path"), "got \(message)")
    }

    func testAttachRejectsRelativeTmuxSocketPath() {
        let model = WorkersWorkspaceModel()
        let result = model.attachWorkerPane(EngineAttachRequest(
            runId: "run-tmux",
            slotId: 1,
            sessionName: "boss-1-run-tmux",
            tmuxSocketPath: "tmux.sock",
            summary: nil,
            taskTitle: nil
        ))
        guard case .failure(.internalFailure(let message)) = result else {
            XCTFail("expected .internalFailure for relative socket path, got \(result)")
            return
        }
        XCTAssertTrue(message.contains("tmux socket path"), "got \(message)")
    }

}

@MainActor
final class GhosttyTerminalHostSubmissionPlanTests: XCTestCase {
    func testPreservesBodyAndAlwaysSubmitsWhenNoTrailingNewline() {
        // The bug we are fixing: the prompt landed in the worker's
        // input buffer but was never submitted. The writer must
        // always follow the paste with a Return keystroke, regardless
        // of whether the caller bothered to terminate the text.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "echo hello")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "echo hello", sendReturn: true))
    }

    func testStripsSingleTrailingNewlineBeforeSubmitting() {
        // Earlier revisions of `bossctl agents send` appended `\n`
        // to the payload in the belief that libghostty's paste path
        // would treat it as Enter. It does not — the `\n` lands as a
        // literal newline character in the input field, leaving the
        // prompt with a trailing blank line when the writer adds its
        // own Return. Strip the trailing newline so the submitted
        // prompt matches what the human meant to type.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "echo hello\n")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "echo hello", sendReturn: true))
    }

    func testStripsTrailingCRLFAndRepeatedNewlines() {
        // Heredoc-quoted prompts coming through shells can carry
        // `\r\n` line endings or a couple of trailing newlines.
        // Strip them all — they would otherwise pollute the input
        // field with stray blank lines before the Return keystroke
        // submits.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "first\nsecond\r\n\n")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "first\nsecond", sendReturn: true))
    }

    func testInternalNewlinesArePreserved() {
        // Multi-line prompts (e.g. a Stop-boundary probe asking the
        // worker to "explain what you're blocked on" across two
        // sentences) must keep their internal newlines so the paste
        // delivers the full body. Only the *trailing* newline gets
        // stripped before the Return submits.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "line one\nline two")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "line one\nline two", sendReturn: true))
    }

    func testEmptyPayloadStillSubmits() {
        // A degenerate "press enter" intervene (empty body) is rare
        // but well-defined: submit whatever the human had already
        // typed into the input field. The writer should still
        // synthesize Return — the body just has nothing to paste.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "", sendReturn: true))
    }

    func testWhitespaceOnlyPayloadKeepsLeadingSpaces() {
        // Trailing newlines come off; other whitespace stays. A
        // human who explicitly typed a leading space (e.g. quoting
        // shell input) should see that space preserved in the
        // submitted prompt.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "  spaced\n")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "  spaced", sendReturn: true))
    }
}

@MainActor
final class GhosttyTerminalHostSurfaceFailureDiagnosticTests: XCTestCase {
    func testDiagnosticReportsEveryControlledInput() {
        // When `ghostty_surface_new` returns NULL the host view no
        // longer `fatalError`s (issue #800 — a no-active-display
        // condition crashed the whole app). The NULL path is now a
        // logged, recoverable event, so the diagnostic block is the
        // only signal that survives into the dev log / os_log. Pin its
        // contract: every input we control must be reported, so a
        // future libghostty-rejection is still debuggable from the log
        // alone.
        let diagnostic = GhosttyTerminalHostView.surfaceFailureDiagnostic(
            appNonNil: true,
            workingDirectory: "/tmp/workdir",
            cwdExists: false,
            isDirectory: false,
            fontSize: 13,
            scaleFactor: 2.0,
            envVarCount: 3,
            envSummary: "PATH=/usr/bin, TERM=xterm",
            initialInputCount: 42
        )

        // Match label and value independently so the test pins the
        // contract (every field is reported) without being brittle to
        // the column-alignment whitespace.
        XCTAssertTrue(diagnostic.contains("ghostty_surface_new returned NULL"))
        XCTAssertTrue(diagnostic.contains("runtime.app != nil:"))
        XCTAssertTrue(diagnostic.contains("workingDirectory:"))
        XCTAssertTrue(diagnostic.contains("/tmp/workdir"))
        XCTAssertTrue(diagnostic.contains("env_var_count:"))
        XCTAssertTrue(diagnostic.contains("env (first 8):"))
        XCTAssertTrue(diagnostic.contains("PATH=/usr/bin, TERM=xterm"))
        XCTAssertTrue(diagnostic.contains("initialInput (chars):"))
        XCTAssertTrue(diagnostic.contains("42"))
    }

    func testSurfaceFailureReasonNamesTheDisplayStateActuallyObserved() {
        // The reason string is the human-facing explanation the app
        // stores in its durable viewer attachment diagnostics. A reason that names display
        // availability whatever the real display state is makes the
        // recoverable #800 condition and a genuine non-transient rejection
        // (env pollution, bad cwd, version mismatch) indistinguishable in
        // the record, so each branch must name what it actually observed.
        // Measured via HostDisplaySnapshot (CG active count), not NSScreen.main.
        // Realistic lock-screen shape: active=0, online=1, nsScreenMainNonNil=true.
        let noDisplay = GhosttyTerminalHostView.surfaceFailureReason(
            host: .make(
                activeDisplayCount: 0,
                onlineDisplayCount: 1,
                mainDisplayAsleep: true,
                sessionLocked: true,
                screenCount: 1,
                nsScreenMainNonNil: true
            )
        )
        XCTAssertTrue(
            noDisplay.contains("no active CG displays"),
            "the no-display case must name it as the cause; got: \(noDisplay)"
        )

        let withDisplay = GhosttyTerminalHostView.surfaceFailureReason(
            host: .make(
                activeDisplayCount: 1,
                onlineDisplayCount: 1,
                screenCount: 1,
                nsScreenMainNonNil: true
            )
        )
        XCTAssertTrue(
            withDisplay.contains("active CG displays present"),
            "a failure with a display present must not blame display availability; got: \(withDisplay)"
        )
        XCTAssertTrue(
            withDisplay.contains("bossctl logs spawn"),
            "active-display branch must point at retrievable spawn logs; got: \(withDisplay)"
        )
        XCTAssertFalse(
            withDisplay.lowercased().contains("stderr"),
            "must not point at unreadable stderr; got: \(withDisplay)"
        )
        XCTAssertNotEqual(
            noDisplay,
            withDisplay,
            "the two causes must be distinguishable from the reason string alone"
        )
    }
}

@MainActor
final class WorkersWorkspaceModelFocusTests: XCTestCase {
    func testFocusUnknownSlotReturnsUnknownSlot() {
        let model = WorkersWorkspaceModel()
        // Interactive grid is 1...16 (Bridge Crew + Lower Decks); 99 has no slot at all.
        let result = model.focusWorkerPane(runId: "unknown")
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for nonexistent slot, got \(result)")
            return
        }
    }

    func testFocusIdleSlotReturnsUnknownSlot() {
        let model = WorkersWorkspaceModel()
        // All slots start without a session attached. Focusing an
        // idle slot should fail the same way as an unknown one — the
        // app has nothing to raise. Mirrors the
        // `detachWorkerPane` semantics for idle slots so the engine
        // can treat both cases the same way.
        let result = model.focusWorkerPane(runId: "run-tmux")
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for idle slot, got \(result)")
            return
        }
    }
}

@MainActor
final class WorkersWorkspaceModelAttachSlotRoutingTests: XCTestCase {
    func testSameRunAttachPreservesSurfaceAndOriginalSlotMetadata() {
        let model = WorkersWorkspaceModel()
        _ = model.attachWorkerPane(makeAttachRequest(slot: 3))
        let session = model.viewersByRunId["run-test"]?.session
        for slot in [3, 5] {
            guard case .success = model.attachWorkerPane(makeAttachRequest(slot: slot)) else {
                return XCTFail("same-run attach must succeed")
            }
            XCTAssertTrue(model.viewersByRunId["run-test"]?.session === session)
            XCTAssertEqual(model.listHostedPanes().map(\.slotId), [3])
            XCTAssertEqual(model.listHostedPanes().map(\.runId), ["run-test"])
        }
    }

    func testDelayedDetachAndFocusCannotTargetReplacementInReusedSlot() {
        let model = WorkersWorkspaceModel()
        _ = model.attachWorkerPane(makeAttachRequest(slot: 3, runId: "old-run"))
        guard case .success = model.detachWorkerPane(runId: "old-run") else {
            return XCTFail("first detach must succeed")
        }
        _ = model.attachWorkerPane(makeAttachRequest(slot: 3, runId: "new-run"))
        let replacement = model.viewersByRunId["new-run"]?.session
        guard case .failure(.unknownSlot) = model.detachWorkerPane(runId: "old-run") else {
            return XCTFail("delayed detach must report the old viewer absent")
        }
        guard case .failure(.unknownSlot) = model.focusWorkerPane(runId: "old-run") else {
            return XCTFail("delayed focus must report the old viewer absent")
        }
        guard case .failure(.internalFailure("pane has no live surface")) = model.focusWorkerPane(runId: "new-run") else {
            return XCTFail("focus must resolve the new viewer before checking its surface")
        }
        XCTAssertTrue(model.viewersByRunId["new-run"]?.session === replacement)
        XCTAssertEqual(model.listHostedPanes().map(\.runId), ["new-run"])
        XCTAssertEqual(model.listHostedPanes().map(\.slotId), [3])
    }

    func testViewerReportSurvivesGridCapacityChangesUntilDetach() {
        let model = WorkersWorkspaceModel()
        _ = model.attachWorkerPane(makeAttachRequest(slot: 40, runId: "review-run"))
        let session = model.viewersByRunId["review-run"]?.session
        model.configureSlots(workerCount: 16, automationCount: 8, reviewCount: 8)
        XCTAssertEqual(model.reviewSlotCount, 8)
        XCTAssertEqual(model.listHostedPanes().map(\.slotId), [40])
        XCTAssertEqual(model.listHostedPanes().map(\.runId), ["review-run"])
        XCTAssertTrue(model.viewersByRunId["review-run"]?.session === session)
        guard case .success = model.detachWorkerPane(runId: "review-run") else {
            return XCTFail("viewer outside the grid must remain detachable by run")
        }
        XCTAssertTrue(model.listHostedPanes().isEmpty)
    }

    private func makeAttachRequest(slot: Int, runId: String = "run-test") -> EngineAttachRequest {
        EngineAttachRequest(
            runId: runId,
            slotId: slot,
            sessionName: "boss-\(slot)-\(runId)",
            tmuxSocketPath: "/state/boss/tmux.sock",
            summary: nil,
            taskTitle: nil
        )
    }

    func testAttachHonorsEngineClaimedSlot() {
        // Engine asked for slot 5. The app must host the pane in
        // slot 5 — not the lowest free slot, not a random one. This
        // is the contract that replaces the old firstIndex(where:)
        // heuristic.
        let model = WorkersWorkspaceModel()
        let result = model.attachWorkerPane(makeAttachRequest(slot: 5))
        guard case .success = result else {
            XCTFail("expected .success, got \(result)")
            return
        }
        XCTAssertNotNil(
            model.viewersByRunId["run-test"]?.session,
            "slot 5 should now host a session"
        )
        XCTAssertEqual(model.viewersByRunId["run-test"]?.slotId, 5)
        XCTAssertEqual(model.viewersByRunId.count, 1)
    }

    func testAttachIntoOccupiedSlotReturnsSlotBusy() {
        // Engine and app disagree about whether slot 3 is free. The
        // app must surface .slotBusy rather than silently picking a
        // different slot — that would re-introduce the dual
        // allocator the engine-owns-slots refactor exists to remove.
        let model = WorkersWorkspaceModel()
        _ = model.attachWorkerPane(makeAttachRequest(slot: 3, runId: "run-first"))
        let result = model.attachWorkerPane(makeAttachRequest(slot: 3, runId: "run-second"))
        guard case .failure(.slotBusy(let occupyingRunId)) = result else {
            XCTFail("expected .slotBusy when engine asks for an occupied slot, got \(result)")
            return
        }
        XCTAssertEqual(
            occupyingRunId,
            "run-first",
            "slotBusy should report the run already hosted in the slot"
        )
    }

    func testAttachRejectsOutOfRangeSlot() {
        let model = WorkersWorkspaceModel()
        let zeroResult = model.attachWorkerPane(makeAttachRequest(slot: 0))
        guard case .failure(.internalFailure) = zeroResult else {
            XCTFail("expected .internalFailure for slot 0, got \(zeroResult)")
            return
        }
        let highResult = model.attachWorkerPane(makeAttachRequest(slot: 99))
        guard case .failure(.internalFailure) = highResult else {
            XCTFail("expected .internalFailure for slot 99, got \(highResult)")
            return
        }
    }
}

@MainActor
final class GhosttyRuntimeCloseDetectionTests: XCTestCase {
    func testCloseCallbackRequiresActualCurrentChildExit() {
        XCTAssertFalse(
            GhosttyRuntime.shouldReportChildExit(
                needsConfirmation: false,
                isCurrentAttachedSurface: true,
                isReleased: false,
                processExited: false
            ),
            "a no-confirm close request does not mean the child exited"
        )
        XCTAssertFalse(
            GhosttyRuntime.shouldReportChildExit(
                needsConfirmation: false,
                isCurrentAttachedSurface: false,
                isReleased: false,
                processExited: true
            ),
            "a stale or not-yet-attached surface cannot report this session dead"
        )
        XCTAssertFalse(
            GhosttyRuntime.shouldReportChildExit(
                needsConfirmation: false,
                isCurrentAttachedSurface: true,
                isReleased: true,
                processExited: true
            ),
            "engine-driven release must not echo a new pane-death report"
        )
        XCTAssertTrue(
            GhosttyRuntime.shouldReportChildExit(
                needsConfirmation: false,
                isCurrentAttachedSurface: true,
                isReleased: false,
                processExited: true
            )
        )
    }
}

@MainActor
final class WorkersWorkspaceModelPageTests: XCTestCase {
    private func makeAttachRequest(slot: Int, runId: String = "run-page") -> EngineAttachRequest {
        EngineAttachRequest(
            runId: runId,
            slotId: slot,
            sessionName: "boss-\(slot)-\(runId)",
            tmuxSocketPath: "/state/boss/tmux.sock",
            summary: nil,
            taskTitle: nil
        )
    }

    func testInteractivePoolCapacityAndAutomationBaseStayPinned() {
        let model = WorkersWorkspaceModel()
        XCTAssertEqual(WorkersWorkspaceModel.workerSlotCount, 16)
        XCTAssertEqual(WorkersWorkspaceModel.automationSlotBase, 17)
        XCTAssertEqual(model.interactivePoolSize, 16)
        XCTAssertEqual(model.automationPoolSize, 8)
        XCTAssertEqual(model.reviewSlotCount, 16)
    }

    func testAttachIntoInteractiveSpilloverSlotSucceeds() {
        let model = WorkersWorkspaceModel()
        let result = model.attachWorkerPane(makeAttachRequest(slot: 9, runId: "run-ld1"))
        guard case .success = result else {
            XCTFail("expected .success attaching slot 9, got \(result)")
            return
        }
        XCTAssertEqual(model.viewersByRunId["run-ld1"]?.slotId, 9)
        XCTAssertNotNil(model.viewersByRunId["run-ld1"]?.session)
    }

    func testAttachIntoLastInteractiveSlotSucceeds() {
        let model = WorkersWorkspaceModel()
        let result = model.attachWorkerPane(makeAttachRequest(slot: 16, runId: "run-ld8"))
        guard case .success = result else {
            XCTFail("expected .success attaching slot 16, got \(result)")
            return
        }
        XCTAssertEqual(model.viewersByRunId["run-ld8"]?.slotId, 16)
    }

    func testConfigureSlotsPublishesEnginePoolSizes() {
        let model = WorkersWorkspaceModel()
        model.configureSlots(workerCount: 16, automationCount: 8, reviewCount: 12)
        XCTAssertEqual(model.interactivePoolSize, 16)
        XCTAssertEqual(model.automationPoolSize, 8)
        XCTAssertEqual(model.reviewSlotCount, 12)
    }
}
