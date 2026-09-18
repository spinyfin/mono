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

        let session = model.slots.first(where: { $0.slotId == 1 })?.session
        XCTAssertEqual(session?.launchSpec.initialInput, "exec tmux -S '/state/boss/tmux.sock' attach-session -t 'boss-1-run-tmux'\n")
        XCTAssertTrue(session?.launchSpec.env.isEmpty ?? false)
    }

    func testDetachUnknownSlotReturnsUnknownSlot() {
        let result = WorkersWorkspaceModel().detachWorkerPane(slotId: 99)
        guard case .failure(.unknownSlot) = result else {
            return XCTFail("expected unknownSlot, got \(result)")
        }
    }

    func testDetachIdleSlotReturnsUnknownSlot() {
        let result = WorkersWorkspaceModel().detachWorkerPane(slotId: 1)
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

        let result = model.detachWorkerPane(slotId: 1)
        guard case .success = result else {
            XCTFail("expected tmux pane detach to succeed, got \(result)")
            return
        }
        XCTAssertNil(model.slots.first(where: { $0.slotId == 1 })?.session)
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

    func testSendToUnknownSlotReturnsUnknownSlot() {
        // Mirrors `focusWorkerPane` / `interruptWorkerPane`: a
        // `SendToPane` for a slot that the workers grid does not host
        // must surface `.unknownSlot` so the engine can decide whether
        // to requeue (probe injection) or surface a `WorkError` (the
        // `agents send` CLI path). Silently no-op'ing here was the
        // shape of the original intervene bug — a missing slot looked
        // like a successful injection, the engine moved on, and the
        // prompt was lost.
        let model = WorkersWorkspaceModel()
        let result = model.sendToPane(slotId: 99, text: "echo hello", expectedDriverBinary: "claude")
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for nonexistent slot, got \(result)")
            return
        }
    }

    func testSendToIdleSlotReturnsUnknownSlot() {
        // An allocated slot with no session attached is the same
        // class of failure as a nonexistent slot — the app has no
        // surface to write to. Matches the equivalent
        // `focusWorkerPane` test so the engine's failure-handling
        // path stays uniform across the three pane verbs.
        let model = WorkersWorkspaceModel()
        let result = model.sendToPane(slotId: 1, text: "echo hello", expectedDriverBinary: "claude")
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for idle slot, got \(result)")
            return
        }
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
        let result = model.focusWorkerPane(slotId: 99)
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
        let result = model.focusWorkerPane(slotId: 1)
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for idle slot, got \(result)")
            return
        }
    }
}

@MainActor
final class WorkersWorkspaceModelPaneInputTests: XCTestCase {
    /// The only signal trustworthy enough to refuse: no live process at all
    /// resolves on the surface's controlling tty (`foregroundPidIsAlive ==
    /// false`, from `pidIsAlive` failing on whatever `foregroundPid`
    /// returned, or the surface never attaching one).
    func testDriverInputRefusesWhenNoForegroundProcessResolves() {
        let error = WorkersWorkspaceModel.driverInputError(
            expectedDriverBinary: "grok",
            foregroundPidIsAlive: false,
            foregroundProcessName: nil
        )
        guard case .driverExited(let expected, let observed) = error else {
            XCTFail("expected no live foreground process to refuse agent input, got \(String(describing: error))")
            return
        }
        XCTAssertEqual(expected, "grok")
        XCTAssertNil(observed)
    }

    func testDriverInputAllowsTheLiveForegroundDriver() {
        XCTAssertNil(
            WorkersWorkspaceModel.driverInputError(
                expectedDriverBinary: "grok",
                foregroundPidIsAlive: true,
                foregroundProcessName: "grok"
            )
        )
    }

    /// A live driver running a foreground child (e.g. a `bazel build` a
    /// tool call shelled out to) is alive, not exited — the pane's
    /// foreground command differing from the driver binary is the normal
    /// shape of that, and must not refuse the write. This is the app-path
    /// analogue of `TmuxWorkerTerminalInspector` carrying
    /// `#{pane_current_command}` as a diagnostic only (not evidence of
    /// death, and not consulted by `classify_semantic_staleness` for health).
    func testDriverInputAllowsALiveForegroundChildOfTheDriver() {
        XCTAssertNil(
            WorkersWorkspaceModel.driverInputError(
                expectedDriverBinary: "claude",
                foregroundPidIsAlive: true,
                foregroundProcessName: "bazel"
            )
        )
    }

    /// `proc_name` reports the kernel accounting name of whatever was
    /// exec'd, which can differ from `DriverDescriptor.binary` for a
    /// wrapped CLI (an interpreter or shim). That must not read as a
    /// driver exit either — the decision no longer compares names at all.
    func testDriverInputAllowsAProcessNameThatDiffersFromTheDriverInvocationName() {
        XCTAssertNil(
            WorkersWorkspaceModel.driverInputError(
                expectedDriverBinary: "grok-cli",
                foregroundPidIsAlive: true,
                foregroundProcessName: "python3"
            )
        )
    }

    /// A live pid whose name `proc_name` cannot report (a transient
    /// EPERM/ESRCH race, or a name the kernel won't report) must not be
    /// conflated with "no process" — that would reap a healthy worker.
    func testDriverInputAllowsALiveProcessWhoseNameIsUnavailable() {
        XCTAssertNil(
            WorkersWorkspaceModel.driverInputError(
                expectedDriverBinary: "claude",
                foregroundPidIsAlive: true,
                foregroundProcessName: nil
            )
        )
    }

    /// An empty `expectedDriverBinary` is a field-level engine/app protocol
    /// skew (or a malformed frame), not death evidence. It must refuse the
    /// write without concluding the driver exited — a `.driverExited`
    /// outcome here would reap every live worker the app writes to on a
    /// single bad frame.
    func testDriverInputRefusesEmptyExpectedBinaryNonTerminally() {
        let error = WorkersWorkspaceModel.driverInputError(
            expectedDriverBinary: "",
            foregroundPidIsAlive: true,
            foregroundProcessName: "claude"
        )
        guard case .internalFailure = error else {
            XCTFail("expected an empty expected_driver_binary to refuse non-terminally, got \(String(describing: error))")
            return
        }
    }
}

@MainActor
final class WorkersWorkspaceModelAttachSlotRoutingTests: XCTestCase {
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
            model.slots.first(where: { $0.slotId == 5 })?.session,
            "slot 5 should now host a session"
        )
        XCTAssertNil(
            model.slots.first(where: { $0.slotId == 1 })?.session,
            "no other slot should be touched when the engine asked for slot 5"
        )
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

    func testInteractivePoolIsSixteenSlotsSplitIntoTwoPages() {
        // The interactive pool is now two pages of 8: Bridge Crew (slots
        // 1...8) and Lower Decks (slots 9...16). Both are drawn from the flat
        // `slots` array; the pages must be disjoint and cover it exactly.
        let model = WorkersWorkspaceModel()
        XCTAssertEqual(model.slots.count, 16, "main pool must span both pages")
        XCTAssertEqual(model.bridgeCrewSlots.map(\.slotId), Array(1...8))
        XCTAssertEqual(model.lowerDecksSlots.map(\.slotId), Array(9...16))
        // Namespace agreement with the engine: automation floats immediately
        // above the interactive pool (worker 16 → automation base 17).
        XCTAssertEqual(WorkersWorkspaceModel.automationSlotBase, 17)
    }

    func testAttachIntoLowerDecksSlotSucceedsAndRoutesToMainPool() {
        // Slot 9 is Lower Decks slot 1 — the first spillover slot. Before the
        // second page existed it was the automation pool and would not host a
        // main worker; now it must attach into the main `slots` array and show
        // up under `lowerDecksSlots`, indistinguishable from a Bridge Crew pane.
        let model = WorkersWorkspaceModel()
        let result = model.attachWorkerPane(makeAttachRequest(slot: 9, runId: "run-ld1"))
        guard case .success = result else {
            XCTFail("expected .success attaching Lower Decks slot 9, got \(result)")
            return
        }
        XCTAssertNotNil(model.slots.first(where: { $0.slotId == 9 })?.session)
        XCTAssertNotNil(
            model.lowerDecksSlots.first(where: { $0.slotId == 9 })?.session,
            "the attached pane must appear on the Lower Decks page"
        )
        XCTAssertTrue(
            model.bridgeCrewSlots.allSatisfy { $0.session == nil },
            "attaching Lower Decks must not touch any Bridge Crew slot"
        )
    }

    func testAttachIntoTopLowerDecksSlotSucceeds() {
        // Slot 16 is the last interactive slot. It must be a valid attach
        // target (it was out of range when the pool capped at 8).
        let model = WorkersWorkspaceModel()
        let result = model.attachWorkerPane(makeAttachRequest(slot: 16, runId: "run-ld8"))
        guard case .success = result else {
            XCTFail("expected .success attaching Lower Decks slot 16, got \(result)")
            return
        }
        XCTAssertNotNil(model.slots.first(where: { $0.slotId == 16 })?.session)
    }
}
