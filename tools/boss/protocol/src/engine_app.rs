//! Wire types for the engine ↔ app pane RPC layered on the frontend
//! Unix socket. See `tools/boss/docs/designs/engine-app-rpc.md` for
//! the full design (transport choice, trust model, lifecycle).
//!
//! These types appear inside [`FrontendRequest::EngineResponse`] and
//! [`FrontendEvent::EngineRequest`] envelopes. They have no engine or
//! app implementation in this module — separate engine-side dispatch
//! and app-side pane-allocator code consume them.

use serde::{Deserialize, Serialize};

/// One env-var entry to set on the worker process. The shim and
/// `claude` running inside the libghostty pane inherit these. Used to
/// thread `BOSS_EVENTS_SOCKET`, `BOSS_LEASE_ID`, etc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

/// Driver-supplied substrings used to screen-scrape a worker's terminal
/// pane content — e.g. the engine's own tmux-capture-based confirmation
/// that a turn actually ended (see `probe_interrupt::pane_text_shows_turn_ended`
/// in the engine crate). Sourced from `AgentDriver::pane_monitor_spec` in the
/// driver crate (not currently carried on any engine↔app wire message).
///
/// All marker lists are OR-semantics: any hit means the condition is true.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneMonitorSpec {
    /// Substrings whose presence means "the agent is running in this pane".
    pub agent_markers: Vec<String>,
    /// Substrings meaning "a turn is in flight" (Claude: `"esc to interrupt"`).
    pub busy_markers: Vec<String>,
    /// Substrings meaning "starting up, not yet at a prompt".
    pub starting_markers: Vec<String>,
    /// Line prefixes identifying the agent's input prompt (Claude: `"❯"`).
    pub prompt_prefixes: Vec<String>,
    /// Polls of a stable prompt before declaring idle. Claude: 2.
    pub idle_debounce_polls: u8,
}

/// Engine asks the app to attach a Ghostty surface to a worker already
/// running in a Boss-owned tmux session. tmux owns the worker process and
/// its environment; the app does not start a shell or supply an
/// environment, it is only a viewer.
///
/// The engine is the source of truth for which slot a worker lands
/// in: it picks the slot via [`crate::WorkerPool::claim_worker`] and
/// passes the result here as `slot_id`. The app's job is to honor
/// that slot — no fallback / re-allocation. If the slot is already
/// occupied (engine and app disagree), the app returns
/// [`EngineToAppError::SlotBusy`] rather than silently picking a
/// different slot, which would re-introduce the dual-allocator bug
/// the engine-owns-slots refactor exists to fix.
///
/// Naming: `worker-{N}` (engine side) and slot `N` (app side, also
/// 1-indexed) refer to the same physical pane. There is one and
/// only one numbering.
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[builder(on(String, into))]
pub struct AttachWorkerPaneInput {
    pub run_id: String,
    /// 1-indexed slot the engine has claimed for this worker. The
    /// app must host the pane in this exact slot or fail with
    /// [`EngineToAppError::SlotBusy`] / `UnknownSlot`.
    pub slot_id: u8,
    pub session_name: String,
    /// Explicit private tmux socket path used to attach the worker viewer.
    pub tmux_socket_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_title: Option<String>,
}

/// App's reply when it has attached the requested tmux session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachWorkerPaneResult {}

/// Engine asks the app to attach its single Boss-pane surface to the durable
/// coordinator tmux session. The engine owns session creation and restart;
/// the app receives only the verified session identity needed to render it.
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[builder(on(String, into))]
pub struct AttachCoordinatorPaneInput {
    pub session_name: String,
    pub spawn_token: String,
    pub model: String,
    /// Absolute tmux binary selected by the engine preflight.
    pub tmux_program: String,
    /// Explicit private tmux socket path used to attach the coordinator.
    pub tmux_socket_path: String,
    /// The installed `claude` version, present only when it is newer than
    /// the version this coordinator session actually launched with. `None`
    /// covers both "no upgrade available" and "can't tell" (missing launch
    /// record, probe failed, unparseable output) — the app must never
    /// distinguish those and must never render an "up to date" state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator_update_available_version: Option<String>,
}

/// App's reply when its Boss pane is attached to the coordinator session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachCoordinatorPaneResult {}

/// Engine asks the app to remove its Ghostty surface from a tmux-hosted
/// worker. This must not signal or otherwise stop the tmux session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DetachWorkerPaneInput {
    pub slot_id: u8,
}

/// App's reply when its surface is detached.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DetachWorkerPaneResult {}

/// Engine asks the app to report every slot it currently hosts a
/// session in, regardless of whether the engine has a live-tracked
/// run for that slot. Powers `bossctl agents list --all`: the engine
/// diffs this report against its own `LiveWorkerStateRegistry`
/// snapshot to surface "husk" panes — slots the app still occupies
/// (and therefore rejects re-dispatch to with `SlotBusy`) that the
/// engine has already forgotten about (crash, terminal-fail path bug,
/// spawn-ack timeout). No input fields — the app reports on whatever
/// it currently has.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListHostedPanesInput {}

/// One slot the app reports as currently hosting a session. Carries
/// only what the app itself knows — it has no opinion on whether the
/// engine still considers the run live.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostedPaneEntry {
    pub slot_id: u8,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_title: Option<String>,
}

/// App's reply to [`EngineToAppRequest::ListHostedPanes`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListHostedPanesResult {
    pub panes: Vec<HostedPaneEntry>,
}

/// Engine asks the app to write text into a worker pane's pty as if
/// it were typed by the user. Used for probe-injection on `Stop`
/// boundaries and for `bossctl agents send`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendToPaneInput {
    pub slot_id: u8,
    pub text: String,
    /// The driver executable this run launched with. The app does not match
    /// it against the observed foreground process (a live agent is often
    /// inside a foreground child, e.g. a `bazel build` a tool call shelled
    /// out to); it refuses when no live process owns the PTY at all, and
    /// echoes this value back in `DriverExited` for diagnostics. An empty
    /// value is treated as a malformed request and refused non-terminally,
    /// without concluding the driver exited.
    pub expected_driver_binary: String,
}

/// App's reply when text injection succeeds. Empty for now.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendToPaneResult {}

/// Engine asks the app to bring a worker pane to the front: select
/// the pane in the Workers grid, focus its surface so keystrokes go
/// to that pty, and raise the app window to the front of the
/// window-server stack. Used by `bossctl agents focus`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FocusWorkerPaneInput {
    pub slot_id: u8,
}

/// App's reply when focus succeeds. Empty for now; reserved for
/// future fields (e.g., whether the window was already key).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FocusWorkerPaneResult {}

/// Engine asks the app to deliver an Esc / interrupt key event to a
/// worker pane's pty — equivalent to the human pressing Esc while
/// the pane has keyboard focus. Used by `bossctl agents interrupt`
/// to cancel a worker's in-flight turn without terminating the run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterruptWorkerPaneInput {
    pub slot_id: u8,
}

/// App's reply when interrupt delivery succeeds. Empty for now.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterruptWorkerPaneResult {}

/// Engine asks the app to scroll the kanban to a specific work item
/// and play a short transient highlight. `work_item_id` is the
/// resolved canonical id (`task_…`/`proj_…`). `product_id` is
/// included so the app can switch to the right product board even
/// when that product is not currently loaded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevealWorkItemInput {
    pub work_item_id: String,
    pub product_id: String,
}

/// App's reply when the reveal animation has been triggered.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevealWorkItemResult {}

/// Engine asks the app to open a markdown document in the same
/// in-app renderer File ▸ Open uses. `path` is an absolute,
/// already-validated (exists, readable, markdown extension) path —
/// the engine owns that validation so the app stays a thin reader.
/// Powers `bossctl open`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenDocumentInput {
    pub path: String,
}

/// App's reply when the document renderer window has been opened
/// (or focused, if a window for the same content was already open).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenDocumentResult {}

/// What the engine is asking the app to do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineToAppRequest {
    AttachWorkerPane(AttachWorkerPaneInput),
    AttachCoordinatorPane(AttachCoordinatorPaneInput),
    DetachWorkerPane(DetachWorkerPaneInput),
    SendToPane(SendToPaneInput),
    FocusWorkerPane(FocusWorkerPaneInput),
    InterruptWorkerPane(InterruptWorkerPaneInput),
    RevealWorkItem(RevealWorkItemInput),
    OpenDocument(OpenDocumentInput),
    ListHostedPanes(ListHostedPanesInput),
}

/// App's reply, paired with the `request_id` from the originating
/// [`crate::FrontendEvent::EngineRequest`].
///
/// The result is `Ok(...)` on success and `Err(EngineToAppError)` on
/// any failure the app can surface. Engine-side timeouts and
/// app-disconnect failures are synthesised by the engine itself; they
/// don't travel on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineToAppResponse {
    AttachWorkerPane {
        result: Result<AttachWorkerPaneResult, EngineToAppError>,
    },
    AttachCoordinatorPane {
        result: Result<AttachCoordinatorPaneResult, EngineToAppError>,
    },
    DetachWorkerPane {
        result: Result<DetachWorkerPaneResult, EngineToAppError>,
    },
    SendToPane {
        result: Result<SendToPaneResult, EngineToAppError>,
    },
    FocusWorkerPane {
        result: Result<FocusWorkerPaneResult, EngineToAppError>,
    },
    InterruptWorkerPane {
        result: Result<InterruptWorkerPaneResult, EngineToAppError>,
    },
    RevealWorkItem {
        result: Result<RevealWorkItemResult, EngineToAppError>,
    },
    OpenDocument {
        result: Result<OpenDocumentResult, EngineToAppError>,
    },
    ListHostedPanes {
        result: Result<ListHostedPanesResult, EngineToAppError>,
    },
}

/// Errors the app can return on an engine→app pane RPC, plus a few
/// engine-synthesised variants that never travel on the wire.
///
/// Capacity / concurrency limits are **not** expressed here. The engine
/// decides whether a worker may claim a slot before it ever sends
/// `AttachWorkerPane`. Do not read [`Self::SlotBusy`] (or the legacy
/// [`Self::NoAvailableSlot`]) as "the system is at capacity" — that
/// misread is the defect this surface exists to prevent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineToAppError {
    /// Legacy "app could not free a slot" signal from before the engine
    /// owned slot allocation. Retained so older wire payloads still
    /// deserialise; unreachable from modern pane RPCs because the
    /// engine only requests a slot it has already claimed. **Not**
    /// the operator-facing capacity signal — concurrency caps live on
    /// the engine claim path, not this RPC.
    #[error("no free worker slot (legacy app signal; engine-side claim already enforces concurrency)")]
    NoAvailableSlot,
    /// `DetachWorkerPane` / `SendToPane` / `FocusWorkerPane` /
    /// `InterruptWorkerPane` referred to a slot the app does not
    /// recognise — already released, never allocated, or stale after
    /// an app restart.
    #[error("unknown worker slot")]
    UnknownSlot,
    /// Engine↔app **slot occupancy desync** — not capacity exhaustion.
    ///
    /// `AttachWorkerPane` asked for a slot the app already hosts a
    /// session in. The engine believed the slot free (it claimed it);
    /// the app disagrees. Reconcile husk panes / leaked claims rather
    /// than treating this as "no capacity" or retrying the same slot
    /// blindly.
    ///
    /// `occupying_run_id` is the run id the app has stamped on the slot
    /// (`None` only for apps predating this field). The engine already
    /// knows the requested `slot_id` (it sent it on
    /// [`AttachWorkerPaneInput`]) and logs both into `dispatch.jsonl`
    /// under `details.slot_busy`, so an echoed `slot_id` on this error
    /// is unnecessary — the squatting pane is identifiable without it.
    #[error(
        "requested slot already hosts a pane (engine/app slot desync, not capacity); \
         occupying_run_id={occupying_run_id:?}"
    )]
    SlotBusy {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        occupying_run_id: Option<String>,
    },
    /// App lost its connection to the engine before responding. The
    /// engine synthesises this on the caller's side; the app never
    /// sends it on the wire.
    #[error("app disconnected")]
    AppDisconnected,
    /// Engine-side timeout. Synthesised by the engine.
    #[error("engine→app request timed out")]
    Timeout,
    /// App-side failure with detail.
    #[error("app internal error: {message}")]
    Internal { message: String },
    /// The app inspected the PTY immediately before input and found that the
    /// run's driver was no longer foreground. This is terminal evidence for
    /// the worker run, not a retryable pane-write error.
    #[error(
        "worker driver exited before pane input (expected {expected_driver_binary:?}, observed {observed_process:?})"
    )]
    DriverExited {
        expected_driver_binary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_process: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_busy_error_round_trips() {
        let original = EngineToAppResponse::AttachWorkerPane {
            result: Err(EngineToAppError::SlotBusy { occupying_run_id: None }),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("slot_busy"));
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn slot_busy_error_carries_occupying_run_id() {
        let original = EngineToAppResponse::AttachWorkerPane {
            result: Err(EngineToAppError::SlotBusy {
                occupying_run_id: Some("run-husk".into()),
            }),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("run-husk"));
        // Wire shape stays `slot_busy` + optional `occupying_run_id` only —
        // no `slot_id` echo and no pool/capacity fields. The engine already
        // knows the requested slot from AttachWorkerPaneInput.
        assert!(!json.contains("slot_id"));
        assert!(!json.contains("pool"));
        assert!(!json.contains("capacity"));
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn slot_busy_display_teaches_desync_not_capacity() {
        // Operators and coordinator memory historically misread SlotBusy
        // as "pool full / no capacity". Display must make the real meaning
        // self-evident so that misread cannot re-form from the error text.
        let err = EngineToAppError::SlotBusy {
            occupying_run_id: Some("run-husk".into()),
        };
        let text = err.to_string();
        assert!(
            text.contains("desync"),
            "SlotBusy Display must name desync; got {text:?}"
        );
        assert!(
            text.contains("not capacity"),
            "SlotBusy Display must reject the capacity misread; got {text:?}"
        );
        assert!(
            text.contains("run-husk"),
            "SlotBusy Display must surface occupying_run_id; got {text:?}"
        );
        assert!(
            !text.to_lowercase().contains("pool"),
            "SlotBusy Display must not reintroduce pool vocabulary; got {text:?}"
        );

        let legacy = EngineToAppError::NoAvailableSlot.to_string();
        assert!(
            !legacy.to_lowercase().contains("pool"),
            "NoAvailableSlot Display must not say pool; got {legacy:?}"
        );
    }

    #[test]
    fn list_hosted_panes_round_trips() {
        let original = EngineToAppRequest::ListHostedPanes(ListHostedPanesInput {});
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("list_hosted_panes"));
        let parsed: EngineToAppRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn list_hosted_panes_response_round_trips() {
        let original = EngineToAppResponse::ListHostedPanes {
            result: Ok(ListHostedPanesResult {
                panes: vec![HostedPaneEntry {
                    slot_id: 4,
                    run_id: "run-husk".into(),
                    summary: Some("fixing the fencer scraper".into()),
                    task_title: None,
                }],
            }),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn attach_and_detach_round_trip() {
        let attach = EngineToAppRequest::AttachWorkerPane(AttachWorkerPaneInput {
            run_id: "run-tmux".into(),
            slot_id: 3,
            session_name: "boss-3-run-tmux".into(),
            tmux_socket_path: "/state/boss/tmux.sock".into(),
            summary: Some("implementing attach mode".into()),
            task_title: Some("attach panes".into()),
        });
        let attach_json = serde_json::to_string(&attach).unwrap();
        assert!(attach_json.contains("attach_worker_pane"));
        assert_eq!(
            serde_json::from_str::<EngineToAppRequest>(&attach_json).unwrap(),
            attach
        );

        let detach = EngineToAppResponse::DetachWorkerPane {
            result: Ok(DetachWorkerPaneResult {}),
        };
        let detach_json = serde_json::to_string(&detach).unwrap();
        assert!(detach_json.contains("detach_worker_pane"));
        assert_eq!(
            serde_json::from_str::<EngineToAppResponse>(&detach_json).unwrap(),
            detach
        );
    }

    #[test]
    fn coordinator_attach_round_trips_with_its_verified_identity() {
        let request = EngineToAppRequest::AttachCoordinatorPane(AttachCoordinatorPaneInput {
            session_name: "boss-coordinator".into(),
            spawn_token: "opaque-token".into(),
            model: "opus".into(),
            tmux_program: "/opt/homebrew/bin/tmux".into(),
            tmux_socket_path: "/state/boss/tmux.sock".into(),
            coordinator_update_available_version: None,
        });
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("attach_coordinator_pane"));
        assert!(
            !json.contains("coordinator_update_available_version"),
            "None must be omitted, not serialized as null: {json}"
        );
        assert_eq!(serde_json::from_str::<EngineToAppRequest>(&json).unwrap(), request);

        let response = EngineToAppResponse::AttachCoordinatorPane {
            result: Ok(AttachCoordinatorPaneResult {}),
        };
        assert_eq!(
            serde_json::from_str::<EngineToAppResponse>(&serde_json::to_string(&response).unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn coordinator_attach_round_trips_a_present_update_available_version() {
        let request = EngineToAppRequest::AttachCoordinatorPane(AttachCoordinatorPaneInput {
            session_name: "boss-coordinator".into(),
            spawn_token: "opaque-token".into(),
            model: "opus".into(),
            tmux_program: "/opt/homebrew/bin/tmux".into(),
            tmux_socket_path: "/state/boss/tmux.sock".into(),
            coordinator_update_available_version: Some("2.2.0".into()),
        });
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"coordinator_update_available_version\":\"2.2.0\""));
        assert_eq!(serde_json::from_str::<EngineToAppRequest>(&json).unwrap(), request);
    }

    #[test]
    fn attach_response_legacy_no_available_slot_error_round_trips() {
        // Legacy wire compat: an older app may still send this variant.
        let original = EngineToAppResponse::AttachWorkerPane {
            result: Err(EngineToAppError::NoAvailableSlot),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn internal_error_carries_message() {
        let err = EngineToAppError::Internal {
            message: "surface init failed".into(),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("surface init failed"));
        let parsed: EngineToAppError = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, err);
    }

    #[test]
    fn focus_request_round_trips() {
        let original = EngineToAppRequest::FocusWorkerPane(FocusWorkerPaneInput { slot_id: 4 });
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("focus_worker_pane"));
        let parsed: EngineToAppRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn focus_response_ok_round_trips() {
        let original = EngineToAppResponse::FocusWorkerPane {
            result: Ok(FocusWorkerPaneResult {}),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn focus_response_err_round_trips() {
        let original = EngineToAppResponse::FocusWorkerPane {
            result: Err(EngineToAppError::UnknownSlot),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn interrupt_request_round_trips() {
        let original = EngineToAppRequest::InterruptWorkerPane(InterruptWorkerPaneInput { slot_id: 7 });
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("interrupt_worker_pane"));
        let parsed: EngineToAppRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn interrupt_response_ok_round_trips() {
        let original = EngineToAppResponse::InterruptWorkerPane {
            result: Ok(InterruptWorkerPaneResult {}),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn interrupt_response_err_round_trips() {
        let original = EngineToAppResponse::InterruptWorkerPane {
            result: Err(EngineToAppError::UnknownSlot),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn open_document_request_round_trips() {
        let original = EngineToAppRequest::OpenDocument(OpenDocumentInput {
            path: "/Users/dev/design.md".into(),
        });
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("open_document"));
        let parsed: EngineToAppRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn open_document_response_ok_round_trips() {
        let original = EngineToAppResponse::OpenDocument {
            result: Ok(OpenDocumentResult {}),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn open_document_response_err_round_trips() {
        let original = EngineToAppResponse::OpenDocument {
            result: Err(EngineToAppError::Internal {
                message: "renderer window failed to open".into(),
            }),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }
}
