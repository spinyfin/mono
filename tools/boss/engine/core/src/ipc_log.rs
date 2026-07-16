//! Append-only JSONL log of every IPC exchange between the engine and
//! the macOS app on the Unix socket. Rotates daily; retains the last
//! N days (default 7). Writes are dispatched to a background task so
//! the hot path (send_to_app / deliver_app_response) is never blocked
//! on disk I/O.
//!
//! Log lives at: `<boss-state-root>/ipc/ipc-YYYY-MM-DD.jsonl`
//!
//! Each line is a JSON object:
//!   `ts_epoch_ms`  – milliseconds since Unix epoch
//!   `direction`    – `"engine→app"` or `"app→engine"`
//!   `request_id`   – opaque id that pairs a request with its response
//!   `kind`         – snake_case discriminant (e.g. `"release_worker_pane"`)
//!   `body`         – the full serialised request or response payload
//!
//! Built on the generic day-rotated writer in [`boss_engine_day_rotated_log`].

use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::protocol::{EngineToAppRequest, EngineToAppResponse};
use boss_engine_day_rotated_log::{DayRotatedLogger, TimestampedRecord};

const FILE_PREFIX: &str = "ipc-";

#[derive(Debug, Serialize)]
struct IpcLogEntry {
    ts_epoch_ms: u128,
    direction: &'static str,
    request_id: String,
    kind: &'static str,
    body: Value,
}

impl TimestampedRecord for IpcLogEntry {
    fn ts_epoch_ms(&self) -> u128 {
        self.ts_epoch_ms
    }
}

/// Async-safe, append-only IPC log writer.
///
/// Calls to [`log_request`] and [`log_response`] are non-blocking:
/// entries are sent over an in-process channel to a background task
/// that owns the file handles and performs all I/O.
pub struct IpcLogger {
    inner: DayRotatedLogger<IpcLogEntry>,
}

impl IpcLogger {
    /// Create a new logger that writes under `<root>/ipc/`.
    /// Spawns a Tokio background task when a runtime is available.
    /// When called outside a Tokio runtime (e.g. synchronous unit tests),
    /// the channel is created but the writer task is not spawned — log
    /// entries queue up and are silently dropped when the sender is dropped.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            inner: DayRotatedLogger::new(root.into().join("ipc"), FILE_PREFIX),
        }
    }

    /// Log an outbound request (engine → app).
    pub fn log_request(&self, request_id: &str, request: &EngineToAppRequest) {
        self.send(IpcLogEntry {
            ts_epoch_ms: boss_engine_day_rotated_log::now_ms(),
            direction: "engine→app",
            request_id: request_id.to_owned(),
            kind: request_kind(request),
            body: serde_json::to_value(request).unwrap_or(Value::Null),
        });
    }

    /// Log an inbound response (app → engine).
    pub fn log_response(&self, request_id: &str, response: &EngineToAppResponse) {
        self.send(IpcLogEntry {
            ts_epoch_ms: boss_engine_day_rotated_log::now_ms(),
            direction: "app→engine",
            request_id: request_id.to_owned(),
            kind: response_kind(response),
            body: serde_json::to_value(response).unwrap_or(Value::Null),
        });
    }

    fn send(&self, entry: IpcLogEntry) {
        self.inner.emit(entry);
    }
}

fn request_kind(req: &EngineToAppRequest) -> &'static str {
    match req {
        EngineToAppRequest::SpawnWorkerPane(_) => "spawn_worker_pane",
        EngineToAppRequest::ReleaseWorkerPane(_) => "release_worker_pane",
        EngineToAppRequest::SendToPane(_) => "send_to_pane",
        EngineToAppRequest::FocusWorkerPane(_) => "focus_worker_pane",
        EngineToAppRequest::InterruptWorkerPane(_) => "interrupt_worker_pane",
        EngineToAppRequest::RevealWorkItem(_) => "reveal_work_item",
        EngineToAppRequest::ListHostedPanes(_) => "list_hosted_panes",
    }
}

fn response_kind(resp: &EngineToAppResponse) -> &'static str {
    match resp {
        EngineToAppResponse::SpawnWorkerPane { .. } => "spawn_worker_pane",
        EngineToAppResponse::ReleaseWorkerPane { .. } => "release_worker_pane",
        EngineToAppResponse::SendToPane { .. } => "send_to_pane",
        EngineToAppResponse::FocusWorkerPane { .. } => "focus_worker_pane",
        EngineToAppResponse::InterruptWorkerPane { .. } => "interrupt_worker_pane",
        EngineToAppResponse::RevealWorkItem { .. } => "reveal_work_item",
        EngineToAppResponse::ListHostedPanes { .. } => "list_hosted_panes",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        EngineToAppResponse, FocusWorkerPaneInput, FocusWorkerPaneResult, InterruptWorkerPaneInput,
        InterruptWorkerPaneResult, ListHostedPanesInput, ListHostedPanesResult, ReleaseWorkerPaneInput,
        ReleaseWorkerPaneResult, RevealWorkItemInput, RevealWorkItemResult, SendToPaneInput, SendToPaneResult,
        SpawnWorkerPaneInput, SpawnWorkerPaneResult,
    };

    /// Every `EngineToAppRequest` variant must emit its documented
    /// snake_case `kind` discriminant. This is a wire contract downstream
    /// log consumers depend on, so a silent rename of any arm should fail
    /// here.
    #[test]
    fn request_kind_covers_every_variant() {
        let cases: Vec<(EngineToAppRequest, &str)> = vec![
            (
                EngineToAppRequest::SpawnWorkerPane(SpawnWorkerPaneInput {
                    run_id: "run-1".into(),
                    workspace_path: "/tmp/ws".into(),
                    slot_id: 1,
                    initial_input: "claude\n".into(),
                    env: vec![],
                    summary: None,
                    task_title: None,
                }),
                "spawn_worker_pane",
            ),
            (
                EngineToAppRequest::ReleaseWorkerPane(ReleaseWorkerPaneInput {
                    slot_id: 1,
                    kill_grace_seconds: 0,
                }),
                "release_worker_pane",
            ),
            (
                EngineToAppRequest::SendToPane(SendToPaneInput {
                    slot_id: 1,
                    text: "hi".into(),
                }),
                "send_to_pane",
            ),
            (
                EngineToAppRequest::FocusWorkerPane(FocusWorkerPaneInput { slot_id: 1 }),
                "focus_worker_pane",
            ),
            (
                EngineToAppRequest::InterruptWorkerPane(InterruptWorkerPaneInput { slot_id: 1 }),
                "interrupt_worker_pane",
            ),
            (
                EngineToAppRequest::RevealWorkItem(RevealWorkItemInput {
                    work_item_id: "task_1".into(),
                    product_id: "prod_1".into(),
                }),
                "reveal_work_item",
            ),
            (
                EngineToAppRequest::ListHostedPanes(ListHostedPanesInput {}),
                "list_hosted_panes",
            ),
        ];

        for (req, expected_kind) in cases {
            assert_eq!(request_kind(&req), expected_kind, "kind for {req:?}");
        }
    }

    /// Every `EngineToAppResponse` variant must emit its documented
    /// snake_case `kind` discriminant, mirroring the request side.
    #[test]
    fn response_kind_covers_every_variant() {
        let cases: Vec<(EngineToAppResponse, &str)> = vec![
            (
                EngineToAppResponse::SpawnWorkerPane {
                    result: Ok(SpawnWorkerPaneResult {
                        slot_id: 1,
                        shell_pid: 42,
                    }),
                },
                "spawn_worker_pane",
            ),
            (
                EngineToAppResponse::ReleaseWorkerPane {
                    result: Ok(ReleaseWorkerPaneResult {}),
                },
                "release_worker_pane",
            ),
            (
                EngineToAppResponse::SendToPane {
                    result: Ok(SendToPaneResult {}),
                },
                "send_to_pane",
            ),
            (
                EngineToAppResponse::FocusWorkerPane {
                    result: Ok(FocusWorkerPaneResult {}),
                },
                "focus_worker_pane",
            ),
            (
                EngineToAppResponse::InterruptWorkerPane {
                    result: Ok(InterruptWorkerPaneResult {}),
                },
                "interrupt_worker_pane",
            ),
            (
                EngineToAppResponse::RevealWorkItem {
                    result: Ok(RevealWorkItemResult {}),
                },
                "reveal_work_item",
            ),
            (
                EngineToAppResponse::ListHostedPanes {
                    result: Ok(ListHostedPanesResult { panes: vec![] }),
                },
                "list_hosted_panes",
            ),
        ];

        for (resp, expected_kind) in cases {
            assert_eq!(response_kind(&resp), expected_kind, "kind for {resp:?}");
        }
    }

    #[tokio::test]
    async fn ipc_logger_writes_and_rotates() {
        let dir = tempfile::TempDir::new().unwrap();
        let logger = IpcLogger::new(dir.path());

        let req = EngineToAppRequest::ReleaseWorkerPane(ReleaseWorkerPaneInput {
            slot_id: 3,
            kill_grace_seconds: 5,
        });
        logger.log_request("eng-req-42", &req);

        let resp = EngineToAppResponse::ReleaseWorkerPane {
            result: Ok(ReleaseWorkerPaneResult {}),
        };
        logger.log_response("eng-req-42", &resp);

        // Let the background task flush.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let ipc_dir = dir.path().join("ipc");
        let mut files: Vec<_> = std::fs::read_dir(&ipc_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        files.sort();
        assert_eq!(files.len(), 1, "one daily log file");

        let content = std::fs::read_to_string(&files[0]).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        let req_entry: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(req_entry["direction"], "engine→app");
        assert_eq!(req_entry["kind"], "release_worker_pane");
        assert_eq!(req_entry["request_id"], "eng-req-42");
        assert!(req_entry["ts_epoch_ms"].is_number());

        let resp_entry: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(resp_entry["direction"], "app→engine");
        assert_eq!(resp_entry["kind"], "release_worker_pane");
        assert_eq!(resp_entry["request_id"], "eng-req-42");
    }
}
