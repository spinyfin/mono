//! The connection through its public API, against a scripted peer, and the
//! official SDK's own connection type on a stream that an earlier process
//! already initialised.
//!
//! The second half exists to answer one design question with running code
//! instead of a reading of the SDK's source: can a client that attaches to a
//! live stream part-way through use the SDK's connection, or does it need its
//! own request-id table? See `tools/boss/docs/investigations/coordinator-acp-spike.md`.

use std::future::Future;
use std::io::Write;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::{Agent, Client, Handled, Lines, v1};
use boss_acp_client::connection::{
    CloseReason, Connection, ConnectionError, ConnectionOptions, Incoming, PreparedConnection, RequestError,
};
use boss_acp_client::journal::{Direction, Journal, WireRecord, read_records, unanswered_requests};
use boss_acp_client::schema::{
    CancelNotification, ContentBlock, ErrorCode, PromptRequest, PromptResponse, RequestId, RequestPermissionOutcome,
    RequestPermissionResponse, StopReason, TextContent,
};
use boss_acp_client::{JsonRpcResponse, UntypedMessage};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc::UnboundedReceiver;

/// Nothing here should take long; a hang is a failure, not a slow test.
async fn soon<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// The agent's side of the stream, driven by the test.
struct Peer {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
}

impl Peer {
    async fn read(&mut self) -> Value {
        let mut line = String::new();
        let read = soon("a line from the client", self.reader.read_line(&mut line))
            .await
            .unwrap();
        assert!(read > 0, "the client closed the stream");
        assert!(line.ends_with('\n'), "every message is newline-terminated: {line:?}");
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("client sent {line:?}: {error}"))
    }

    async fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn send(&mut self, message: Value) {
        self.write(format!("{message}\n").as_bytes()).await;
    }
}

/// A connection that is not reading yet, and the agent's side of its stream.
fn prepare(
    journal: Option<Arc<Journal>>,
    options: ConnectionOptions,
) -> (PreparedConnection<ReadHalf<DuplexStream>>, Peer) {
    let (client_io, peer_io) = tokio::io::duplex(1 << 20);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    let (peer_reader, peer_writer) = tokio::io::split(peer_io);
    let prepared = Connection::prepare(client_reader, client_writer, journal, options);
    let peer = Peer {
        reader: BufReader::new(peer_reader),
        writer: peer_writer,
    };
    (prepared, peer)
}

fn connect_with(
    journal: Option<Arc<Journal>>,
    options: ConnectionOptions,
) -> (Connection, UnboundedReceiver<Incoming>, Peer) {
    let (prepared, peer) = prepare(journal, options);
    let (connection, incoming) = prepared.start();
    (connection, incoming, peer)
}

fn connect() -> (Connection, UnboundedReceiver<Incoming>, Peer) {
    connect_with(None, ConnectionOptions::default())
}

fn prompt(text: &str) -> PromptRequest {
    PromptRequest::new("session-1", vec![ContentBlock::Text(TextContent::new(text))])
}

fn end_turn(id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}})
}

fn text_update(text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": "session-1",
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}},
        },
    })
}

/// The text of a `session/update` that `text_update` made.
fn update_text(incoming: Option<Incoming>) -> String {
    match incoming {
        Some(Incoming::Notification { method, params, .. }) if method == "session/update" => {
            params["update"]["content"]["text"].as_str().unwrap().to_owned()
        }
        other => panic!("expected a session update, got {other:?}"),
    }
}

fn arrived_id(incoming: Option<Incoming>) -> RequestId {
    match incoming {
        Some(Incoming::ResponseArrived { id, .. }) => id,
        other => panic!("expected the arrival of a response, got {other:?}"),
    }
}

#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedBuffer {
    fn records(&self) -> Vec<WireRecord> {
        read_records(self.0.lock().unwrap().as_slice()).unwrap()
    }
}

// ── The connection ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_typed_request_goes_out_with_its_method_and_comes_back_decoded() {
    let (connection, _incoming, mut peer) = connect();
    let pending = connection.send_request(&prompt("hello")).await.unwrap();
    assert_eq!(pending.id(), &RequestId::Number(1));

    let sent = peer.read().await;
    assert_eq!(sent["jsonrpc"], "2.0");
    assert_eq!(sent["id"], 1);
    assert_eq!(
        sent["method"], "session/prompt",
        "the request type fixes the method name"
    );
    assert_eq!(sent["params"]["sessionId"], "session-1");
    assert_eq!(sent["params"]["prompt"][0], json!({"type": "text", "text": "hello"}));

    peer.send(end_turn(&sent["id"])).await;
    let response: PromptResponse = soon("the prompt response", pending.response()).await.unwrap();
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert!(connection.pending_ids().is_empty());
}

#[tokio::test]
async fn responses_are_matched_by_id_not_by_order() {
    let (connection, _incoming, mut peer) = connect();
    let first = connection
        .send_request(&UntypedMessage::new("probe/first", json!({})).unwrap())
        .await
        .unwrap();
    let second = connection
        .send_request(&UntypedMessage::new("probe/second", json!({})).unwrap())
        .await
        .unwrap();
    let (first_id, second_id) = (peer.read().await["id"].clone(), peer.read().await["id"].clone());
    assert_ne!(first_id, second_id);

    peer.send(json!({"jsonrpc": "2.0", "id": second_id, "result": {"answer": "second"}}))
        .await;
    peer.send(json!({"jsonrpc": "2.0", "id": first_id, "result": {"answer": "first"}}))
        .await;
    assert_eq!(
        soon("second", second.response()).await.unwrap(),
        json!({"answer": "second"})
    );
    assert_eq!(
        soon("first", first.response()).await.unwrap(),
        json!({"answer": "first"})
    );
}

#[tokio::test]
async fn an_error_response_fails_only_its_own_request() {
    let (connection, _incoming, mut peer) = connect();
    let failing = connection.send_request(&prompt("one")).await.unwrap();
    let working = connection.send_request(&prompt("two")).await.unwrap();
    let (failing_id, working_id) = (peer.read().await["id"].clone(), peer.read().await["id"].clone());

    peer.send(json!({
        "jsonrpc": "2.0",
        "id": failing_id,
        "error": {"code": -32603, "message": "Internal error", "data": {"detail": "boom"}},
    }))
    .await;
    peer.send(end_turn(&working_id)).await;

    match soon("the error", failing.response()).await {
        Err(RequestError::Rpc { method, id, error }) => {
            assert_eq!(method, "session/prompt");
            assert_eq!(id, RequestId::Number(1));
            assert_eq!(error.code, ErrorCode::InternalError);
            assert_eq!(error.data, Some(json!({"detail": "boom"})));
        }
        other => panic!("expected the agent's error, got {other:?}"),
    }
    assert_eq!(
        soon("the other response", working.response())
            .await
            .unwrap()
            .stop_reason,
        StopReason::EndTurn
    );
}

#[tokio::test]
async fn a_response_that_does_not_decode_is_a_decode_error_naming_the_request() {
    let (connection, _incoming, mut peer) = connect();
    let pending = connection.send_request(&prompt("hello")).await.unwrap();
    let id = peer.read().await["id"].clone();
    peer.send(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": 7}}))
        .await;
    match soon("the response", pending.response()).await {
        Err(RequestError::Decode { method, id, .. }) => {
            assert_eq!((method.as_str(), id), ("session/prompt", RequestId::Number(1)));
        }
        other => panic!("expected a decode error, got {other:?}"),
    }
}

#[tokio::test]
async fn fragmented_and_coalesced_lines_are_each_one_message() {
    let (_connection, mut incoming, mut peer) = connect();
    // One notification in three writes, the middle one splitting a UTF-8 character.
    let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"text":"snow ☃"}}"#.as_bytes();
    let snowman = line.windows(3).position(|window| window == "☃".as_bytes()).unwrap();
    peer.write(&line[..10]).await;
    peer.write(&line[10..snowman + 1]).await;
    peer.write(&line[snowman + 1..]).await;
    // Then its terminator and two more messages in a single write.
    peer.write(
        concat!(
            "\n",
            r#"{"jsonrpc":"2.0","method":"probe/a"}"#,
            "\r\n\n",
            r#"{"jsonrpc":"2.0","method":"probe/b","params":[1,2]}"#,
            "\n"
        )
        .as_bytes(),
    )
    .await;

    let mut seen = Vec::new();
    for _ in 0..3 {
        match soon("a notification", incoming.recv()).await {
            Some(Incoming::Notification { method, params, .. }) => seen.push((method, params)),
            other => panic!("expected a notification, got {other:?}"),
        }
    }
    assert_eq!(
        seen,
        [
            ("session/update".to_owned(), json!({"text": "snow ☃"})),
            ("probe/a".to_owned(), Value::Null),
            ("probe/b".to_owned(), json!([1, 2])),
        ]
    );
}

#[tokio::test]
async fn the_agents_requests_keep_their_own_ids_and_can_be_answered_either_way() {
    let (connection, mut incoming, mut peer) = connect();
    // The agent's ids are its own: a number here, a string below, and the
    // number collides with the id of a request the client sent.
    let ours = connection.send_request(&prompt("hello")).await.unwrap();
    assert_eq!(peer.read().await["id"], 1);
    peer.send(json!({"jsonrpc": "2.0", "id": 1, "method": "session/request_permission", "params": {"n": 1}}))
        .await;
    peer.send(json!({"jsonrpc": "2.0", "id": "perm-b", "method": "fs/read_text_file", "params": {}}))
        .await;

    let Some(Incoming::Request { id, method, params, .. }) = soon("the first request", incoming.recv()).await else {
        panic!("expected a request");
    };
    assert_eq!(
        (&id, method.as_str(), &params),
        (&RequestId::Number(1), "session/request_permission", &json!({"n": 1}))
    );
    assert_eq!(
        connection.pending_ids(),
        [RequestId::Number(1)],
        "an incoming request with our id is not our response"
    );
    connection
        .respond(
            id,
            Ok(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)),
        )
        .await
        .unwrap();
    assert_eq!(
        peer.read().await,
        json!({"jsonrpc": "2.0", "id": 1, "result": {"outcome": {"outcome": "cancelled"}}})
    );

    let Some(Incoming::Request { id, .. }) = soon("the second request", incoming.recv()).await else {
        panic!("expected a request");
    };
    assert_eq!(id, RequestId::Str("perm-b".into()));
    connection
        .respond::<Value>(id, Err(boss_acp_client::schema::Error::method_not_found()))
        .await
        .unwrap();
    let refusal = peer.read().await;
    assert_eq!(refusal["id"], "perm-b");
    assert_eq!(refusal["error"]["code"], -32601);
    assert!(refusal.get("result").is_none());

    peer.send(end_turn(&json!(1))).await;
    soon("our own response", ours.response()).await.unwrap();
}

#[tokio::test]
async fn a_notification_is_sent_without_an_id() {
    let (connection, _incoming, mut peer) = connect();
    connection
        .send_notification(&CancelNotification::new("session-1"))
        .await
        .unwrap();
    assert_eq!(
        peer.read().await,
        json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "session-1"}})
    );
}

#[tokio::test]
async fn lines_that_are_not_messages_are_reported_and_the_stream_stays_usable() {
    let (connection, mut incoming, mut peer) = connect();
    let pending = connection.send_request(&prompt("hello")).await.unwrap();
    let id = peer.read().await["id"].clone();

    for junk in [
        "not json at all\n".as_bytes(),
        b"[{\"jsonrpc\":\"2.0\",\"method\":\"probe/batched\"}]\n",
        b"42\n",
        b"{\"jsonrpc\":\"2.0\"}\n",
        b"{\"jsonrpc\":\"2.0\",\"id\":9}\n",
        b"{\"jsonrpc\":\"2.0\",\"id\":{\"bad\":true},\"method\":\"probe/x\"}\n",
        b"{\"jsonrpc\":\"2.0\",\"method\":5}\n",
        b"\xff\xfe\xfd\n",
    ] {
        peer.write(junk).await;
        match soon("a malformed report", incoming.recv()).await {
            Some(Incoming::Malformed { reason, .. }) => assert!(!reason.is_empty()),
            other => panic!("{:?} should be malformed, got {other:?}", String::from_utf8_lossy(junk)),
        }
    }

    peer.send(end_turn(&id)).await;
    soon("the response after the junk", pending.response()).await.unwrap();
    assert_eq!(connection.close_reason(), None);
}

#[tokio::test]
async fn a_response_nobody_waits_for_is_surfaced_not_swallowed() {
    let (_connection, mut incoming, mut peer) = connect();
    peer.send(end_turn(&json!(77))).await;
    peer.send(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}))
        .await;
    match soon("the unmatched response", incoming.recv()).await {
        Some(Incoming::UnmatchedResponse { id, result, .. }) => {
            assert_eq!(id, RequestId::Number(77));
            assert_eq!(result.unwrap(), json!({"stopReason": "end_turn"}));
        }
        other => panic!("expected an unmatched response, got {other:?}"),
    }
    match soon("the id-less error", incoming.recv()).await {
        Some(Incoming::UnmatchedResponse { id, result, .. }) => {
            assert_eq!(id, RequestId::Null);
            assert_eq!(result.unwrap_err().code, ErrorCode::ParseError);
        }
        other => panic!("expected an unmatched error, got {other:?}"),
    }
}

#[tokio::test]
async fn end_of_stream_fails_every_outstanding_request_and_refuses_new_ones() {
    let (connection, mut incoming, mut peer) = connect();
    let first = connection.send_request(&prompt("one")).await.unwrap();
    let second = connection.send_request(&prompt("two")).await.unwrap();
    peer.read().await;
    peer.read().await;
    // The agent dies part-way through writing a line.
    peer.write(br#"{"jsonrpc":"2.0","id":1,"resu"#).await;
    drop(peer);

    match soon("the cut-off line", incoming.recv()).await {
        Some(Incoming::Malformed { reason, .. }) => assert!(reason.contains("middle of a line"), "{reason}"),
        other => panic!("expected the cut-off line to be reported, got {other:?}"),
    }
    assert_eq!(
        soon("the close", incoming.recv()).await,
        Some(Incoming::Closed(CloseReason::Eof))
    );
    for pending in [first, second] {
        match soon("the failed request", pending.response()).await {
            Err(RequestError::ClosedBeforeResponse { reason, .. }) => assert_eq!(reason, CloseReason::Eof),
            other => panic!("expected the request to fail with the close, got {other:?}"),
        }
    }
    assert!(connection.pending_ids().is_empty());
    assert!(matches!(
        connection.send_request(&prompt("late")).await,
        Err(ConnectionError::Closed(CloseReason::Eof))
    ));
    assert_eq!(connection.close_reason(), Some(CloseReason::Eof));
}

#[tokio::test]
async fn nothing_can_be_sent_after_the_writing_half_is_closed_but_reading_continues() {
    let (connection, mut incoming, mut peer) = connect();
    connection.shutdown_writer().await.unwrap();

    let mut line = String::new();
    assert_eq!(
        soon("end of input at the agent", peer.reader.read_line(&mut line))
            .await
            .unwrap(),
        0
    );
    assert!(matches!(
        connection
            .send_notification(&CancelNotification::new("session-1"))
            .await,
        Err(ConnectionError::Write(_))
    ));

    // The agent can still finish what it was saying.
    peer.send(json!({"jsonrpc": "2.0", "method": "probe/late"})).await;
    assert!(matches!(
        soon("a late notification", incoming.recv()).await,
        Some(Incoming::Notification { .. })
    ));
    connection.shutdown_writer().await.unwrap();
}

/// A child process sees end of input only when the pipe to it is closed, and
/// a pipe is closed by dropping its handle: shutting it down is not enough.
/// `cat` stands in for an agent that exits when its input ends.
#[tokio::test]
async fn closing_the_writing_half_ends_the_input_of_a_child_process() {
    let mut child = tokio::process::Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (connection, mut incoming) = Connection::open(
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
        None,
        ConnectionOptions::default(),
    );

    connection
        .send_notification(&CancelNotification::new("session-1"))
        .await
        .unwrap();
    // `cat` writes the line back, so it arrives as a notification.
    match soon("the echoed line", incoming.recv()).await {
        Some(Incoming::Notification { method, .. }) => assert_eq!(method, "session/cancel"),
        other => panic!("expected the echoed notification, got {other:?}"),
    }

    connection.shutdown_writer().await.unwrap();
    assert_eq!(
        soon("the end of the stream", incoming.recv()).await,
        Some(Incoming::Closed(CloseReason::Eof))
    );
    assert!(soon("the child's exit", child.wait()).await.unwrap().success());
}

#[tokio::test]
async fn the_journal_holds_every_line_in_both_directions_in_wire_order() {
    let buffer = SharedBuffer::default();
    let journal = Arc::new(Journal::to_writer(buffer.clone()));
    let (connection, mut incoming, mut peer) = connect_with(Some(journal), ConnectionOptions::default());

    let pending = connection.send_request(&prompt("hello")).await.unwrap();
    let request = peer.read().await;
    // Unusual spacing and key order: the journal must keep the line as sent.
    let update = r#"{"method":"session/update",  "jsonrpc":"2.0","params":{"b":1,"a":2}}"#;
    peer.write(format!("{update}\n").as_bytes()).await;
    let Some(Incoming::Notification { seq, .. }) = soon("the update", incoming.recv()).await else {
        panic!("expected the update");
    };
    peer.write(b"garbage\n").await;
    let Some(Incoming::Malformed { seq: garbage_seq, .. }) = soon("the garbage", incoming.recv()).await else {
        panic!("expected the garbage to be reported");
    };
    peer.send(end_turn(&request["id"])).await;
    soon("the response", pending.response()).await.unwrap();

    let records = buffer.records();
    assert_eq!(
        records.iter().map(|record| record.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(
        records.iter().map(|record| record.dir).collect::<Vec<_>>(),
        [
            Direction::ToAgent,
            Direction::FromAgent,
            Direction::FromAgent,
            Direction::FromAgent
        ]
    );
    assert_eq!(serde_json::from_str::<Value>(&records[0].line).unwrap(), request);
    assert_eq!(records[1].line, update, "the line is journalled verbatim");
    assert_eq!(
        (seq, garbage_seq),
        (2, 3),
        "an event names the journal record it came from"
    );
    assert_eq!(records[2].line, "garbage");
    assert!(unanswered_requests(&records).is_empty());
}

// ── Attaching to a stream an earlier connection used ─────────────────────────

/// An earlier process sent `session/prompt` with id 7 on this stream, then
/// went away. Its journal says so. A new connection must take the response
/// when it comes, must not resend the prompt, and must not reuse the id.
#[tokio::test]
async fn a_new_connection_takes_the_response_to_a_request_an_earlier_one_sent() {
    let earlier = [
        WireRecord {
            seq: 1,
            ts_ms: 0,
            dir: Direction::ToAgent,
            line: r#"{"jsonrpc":"2.0","id":6,"method":"session/new","params":{}}"#.into(),
        },
        WireRecord {
            seq: 2,
            ts_ms: 0,
            dir: Direction::FromAgent,
            line: r#"{"jsonrpc":"2.0","id":6,"result":{"sessionId":"session-1"}}"#.into(),
        },
        WireRecord {
            seq: 3,
            ts_ms: 0,
            dir: Direction::ToAgent,
            line: r#"{"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{}}"#.into(),
        },
        WireRecord {
            seq: 4,
            ts_ms: 0,
            dir: Direction::FromAgent,
            line: r#"{"jsonrpc":"2.0","id":"p-1","method":"session/request_permission","params":{}}"#.into(),
        },
    ];
    let open = unanswered_requests(&earlier);
    let ours: Vec<_> = open
        .iter()
        .filter(|request| request.dir == Direction::ToAgent)
        .collect();
    assert_eq!(ours.len(), 1);
    assert_eq!(
        (&ours[0].id, ours[0].method.as_str()),
        (&RequestId::Number(7), "session/prompt")
    );

    let (prepared, mut peer) = prepare(None, ConnectionOptions { first_request_id: 8 });
    let old_prompt = prepared
        .expect_response::<PromptResponse>(ours[0].id.clone(), &ours[0].method)
        .unwrap();
    let (connection, mut incoming) = prepared.start();
    assert_eq!(connection.pending_ids(), [RequestId::Number(7)]);

    // The permission request the earlier process never answered is answered
    // by the new one, by the agent's id for it.
    connection
        .respond(
            RequestId::Str("p-1".into()),
            Ok(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)),
        )
        .await
        .unwrap();
    assert_eq!(peer.read().await["id"], "p-1");

    // A new request does not collide with the old id.
    let new_prompt = connection.send_request(&prompt("next")).await.unwrap();
    let sent = peer.read().await;
    assert_eq!(sent["id"], 8);
    assert_eq!(sent["method"], "session/prompt");

    peer.send(json!({"jsonrpc": "2.0", "id": 7, "result": {"stopReason": "cancelled"}}))
        .await;
    peer.send(end_turn(&json!(8))).await;
    assert_eq!(
        soon("the old prompt's response", old_prompt.response())
            .await
            .unwrap()
            .stop_reason,
        StopReason::Cancelled
    );
    assert_eq!(
        soon("the new prompt's response", new_prompt.response())
            .await
            .unwrap()
            .stop_reason,
        StopReason::EndTurn
    );
    // Both responses were consumed by their waiters; each is still reported
    // once as an arrival, and neither was reported as an unmatched response.
    assert_eq!(
        arrived_id(soon("an arrival", incoming.recv()).await),
        RequestId::Number(7)
    );
    assert_eq!(
        arrived_id(soon("an arrival", incoming.recv()).await),
        RequestId::Number(8)
    );
    assert!(incoming.try_recv().is_err(), "nothing was left over");
}

/// The response to the earlier process's prompt is already in the stream when
/// the new connection is built. Registration happens before the first read,
/// so the waiter gets it however the runtime schedules the reader.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_response_already_buffered_when_the_connection_starts_still_reaches_its_waiter() {
    let (prepared, mut peer) = prepare(None, ConnectionOptions { first_request_id: 8 });
    peer.send(json!({"jsonrpc": "2.0", "id": 7, "result": {"stopReason": "cancelled"}}))
        .await;
    let old_prompt = prepared
        .expect_response::<PromptResponse>(RequestId::Number(7), "session/prompt")
        .unwrap();
    let (connection, mut incoming) = prepared.start();

    assert_eq!(
        soon("the buffered response", old_prompt.response())
            .await
            .unwrap()
            .stop_reason,
        StopReason::Cancelled
    );
    assert_eq!(
        arrived_id(soon("its arrival", incoming.recv()).await),
        RequestId::Number(7)
    );
    assert!(connection.pending_ids().is_empty());
    assert!(incoming.try_recv().is_err(), "it was not reported as unmatched");
}

#[tokio::test]
async fn an_id_that_is_already_waiting_is_refused_and_the_first_waiter_is_untouched() {
    let (prepared, mut peer) = prepare(None, ConnectionOptions::default());
    let first = prepared
        .expect_response::<PromptResponse>(RequestId::Number(1), "session/prompt")
        .unwrap();
    let duplicate = prepared.expect_response::<PromptResponse>(RequestId::Number(1), "session/prompt");
    assert!(
        matches!(
            duplicate.err(),
            Some(ConnectionError::DuplicateRequestId(RequestId::Number(1)))
        ),
        "the second registration of an id is an error, not a replacement"
    );
    let (connection, _incoming) = prepared.start();
    assert_eq!(connection.pending_ids(), [RequestId::Number(1)]);

    peer.send(end_turn(&json!(1))).await;
    assert_eq!(
        soon("the first waiter", first.response()).await.unwrap().stop_reason,
        StopReason::EndTurn,
        "the response reaches the waiter that registered first, decoded as its type"
    );
}

#[tokio::test]
async fn a_new_request_never_takes_an_id_that_is_waiting_for_a_response() {
    // The connection would allocate 1 next, and 1 is a recovered request.
    let (prepared, mut peer) = prepare(None, ConnectionOptions::default());
    let recovered = prepared
        .expect_response::<PromptResponse>(RequestId::Number(1), "session/prompt")
        .unwrap();
    let (connection, _incoming) = prepared.start();

    let fresh = connection.send_request(&prompt("next")).await.unwrap();
    assert_eq!(fresh.id(), &RequestId::Number(2), "id 1 is skipped");
    assert_eq!(peer.read().await["id"], 2);

    peer.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "cancelled"}}))
        .await;
    peer.send(end_turn(&json!(2))).await;
    assert_eq!(
        soon("the recovered request", recovered.response())
            .await
            .unwrap()
            .stop_reason,
        StopReason::Cancelled
    );
    assert_eq!(
        soon("the new request", fresh.response()).await.unwrap().stop_reason,
        StopReason::EndTurn
    );
}

// ── Order ────────────────────────────────────────────────────────────────────

/// One read carries prompt A's last update, A's response, prompt B's first
/// update and B's response. The arrival of each response is on the incoming
/// stream between the updates it separates, so a caller that reports from that
/// stream cannot see B's update before A's end.
#[tokio::test]
async fn a_response_is_reported_in_line_order_with_the_updates_around_it() {
    let (connection, mut incoming, mut peer) = connect();
    let first = connection.send_request(&prompt("A")).await.unwrap();
    let second = connection.send_request(&prompt("B")).await.unwrap();
    let (first_id, second_id) = (peer.read().await["id"].clone(), peer.read().await["id"].clone());

    let lines = [
        text_update("A-last"),
        end_turn(&first_id),
        text_update("B-first"),
        end_turn(&second_id),
    ]
    .map(|line| format!("{line}\n"))
    .concat();
    peer.write(lines.as_bytes()).await;

    assert_eq!(update_text(soon("A's update", incoming.recv()).await), "A-last");
    assert_eq!(
        arrived_id(soon("A's arrival", incoming.recv()).await),
        first.id().clone(),
        "A ends before B's update is reported"
    );
    assert_eq!(update_text(soon("B's update", incoming.recv()).await), "B-first");
    assert_eq!(
        arrived_id(soon("B's arrival", incoming.recv()).await),
        second.id().clone()
    );
    // The result was in the waiter's hands before the arrival was reported.
    assert_eq!(first.response().await.unwrap().stop_reason, StopReason::EndTurn);
    assert_eq!(second.response().await.unwrap().stop_reason, StopReason::EndTurn);
}

/// A response that is the last thing the agent says is reported, then the
/// close; the close does not turn the answered request into a failed one.
#[tokio::test]
async fn a_response_followed_by_end_of_stream_is_reported_before_the_close() {
    let (connection, mut incoming, mut peer) = connect();
    let pending = connection.send_request(&prompt("A")).await.unwrap();
    let id = peer.read().await["id"].clone();
    peer.write(format!("{}\n", end_turn(&id)).as_bytes()).await;
    drop(peer);

    assert_eq!(
        arrived_id(soon("the arrival", incoming.recv()).await),
        pending.id().clone()
    );
    assert_eq!(
        soon("the close", incoming.recv()).await,
        Some(Incoming::Closed(CloseReason::Eof))
    );
    assert_eq!(
        soon("the answered request", pending.response())
            .await
            .unwrap()
            .stop_reason,
        StopReason::EndTurn
    );
}

/// Without the registration, the same late response is not lost either: it
/// is handed to the caller, who can still read it.
#[tokio::test]
async fn an_unregistered_old_response_is_still_delivered_to_the_caller() {
    let (_connection, mut incoming, mut peer) = connect_with(None, ConnectionOptions { first_request_id: 8 });
    peer.send(json!({"jsonrpc": "2.0", "id": 7, "result": {"stopReason": "end_turn"}}))
        .await;
    let Some(Incoming::UnmatchedResponse { id, result, .. }) = soon("the old response", incoming.recv()).await else {
        panic!("expected the old response to be surfaced");
    };
    assert_eq!(id, RequestId::Number(7));
    let response = PromptResponse::from_json("session/prompt", result.unwrap()).unwrap();
    assert_eq!(response.stop_reason, StopReason::EndTurn);
}

// ── The official SDK's connection on the same kind of stream ─────────────────

type AgentLines = futures::channel::mpsc::UnboundedReceiver<String>;
type ClientLines = futures::channel::mpsc::UnboundedSender<std::io::Result<String>>;

/// A line transport for the SDK plus the agent's ends of it.
fn sdk_lines() -> (
    Lines<
        impl futures::Sink<String, Error = std::io::Error> + Send + 'static,
        impl futures::Stream<Item = std::io::Result<String>> + Send + 'static,
    >,
    AgentLines,
    ClientLines,
) {
    let (to_agent, agent_reads) = futures::channel::mpsc::unbounded::<String>();
    let (agent_writes, from_agent) = futures::channel::mpsc::unbounded::<std::io::Result<String>>();
    let transport = Lines::new(to_agent.sink_map_err(std::io::Error::other), from_agent);
    (transport, agent_reads, agent_writes)
}

async fn next_line(agent_reads: &mut AgentLines) -> Value {
    let line = soon("a line from the SDK client", agent_reads.next())
        .await
        .expect("the SDK client closed the stream");
    serde_json::from_str(&line).unwrap()
}

/// The SDK's v1 client connection does not insist on sending `initialize`
/// itself, and it tells the caller the id of each request: the first line a
/// fresh connection puts on an already-initialised stream is the prompt, and
/// its wire id is the one `SentRequest::id` reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sdk_connection_runs_on_an_initialised_stream_and_exposes_its_request_ids() {
    let (transport, mut agent_reads, agent_writes) = sdk_lines();
    let agent = tokio::spawn(async move {
        let first = next_line(&mut agent_reads).await;
        agent_writes
            .unbounded_send(Ok(end_turn(&first["id"]).to_string()))
            .unwrap();
        first
    });

    let (reported_id, stop_reason) = soon(
        "the SDK conversation",
        Client
            .v1()
            .name("attached-client")
            .connect_with(transport, async |connection: v1::ConnectionTo<Agent>| {
                let sent = connection.send_request(prompt("again"));
                let id = sent.id().clone();
                let response = sent.block_task().await?;
                Ok((id, response.stop_reason))
            }),
    )
    .await
    .unwrap();

    let first = agent.await.unwrap();
    assert_eq!(first["method"], "session/prompt", "no `initialize` was sent first");
    assert_eq!(
        first["id"],
        json!(reported_id),
        "the id on the wire is the one the SDK reported"
    );
    assert_eq!(stop_reason, StopReason::EndTurn);
}

/// `prepare_request` gives the id before anything is on the wire, so a caller
/// can record which id will carry a prompt before it is published.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sdk_reports_a_request_id_before_the_request_is_published() {
    let (transport, mut agent_reads, agent_writes) = sdk_lines();
    let agent = tokio::spawn(async move {
        let first = next_line(&mut agent_reads).await;
        let second = next_line(&mut agent_reads).await;
        agent_writes
            .unbounded_send(Ok(end_turn(&second["id"]).to_string()))
            .unwrap();
        (first, second)
    });

    let prepared_id = soon(
        "the SDK conversation",
        Client
            .v1()
            .name("attached-client")
            .connect_with(transport, async |connection: v1::ConnectionTo<Agent>| {
                let prepared = connection.prepare_request(prompt("later"));
                let id = prepared.id().clone();
                // Sent after the request was prepared, and still first on the wire.
                connection.send_notification(CancelNotification::new("marker"))?;
                prepared.block_task().await?;
                Ok(id)
            }),
    )
    .await
    .unwrap();

    let (first, second) = agent.await.unwrap();
    assert_eq!(
        first["method"], "session/cancel",
        "preparing a request publishes nothing"
    );
    assert_eq!(second["method"], "session/prompt");
    assert_eq!(second["id"], json!(prepared_id));
}

/// The gap. A fresh SDK connection has an empty pending table and offers no
/// way to add to it, and the ids it generates are its own. So the response
/// to a prompt that an earlier process sent arrives, matches nothing, and is
/// dropped: no handler sees it and no future resolves. The connection itself
/// carries on. The handler below sees every other message, including the
/// response to the connection's own prompt, which is what makes the old
/// response's absence meaningful.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sdk_connection_drops_the_response_to_a_request_an_earlier_process_sent() {
    let (transport, mut agent_reads, agent_writes) = sdk_lines();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorder = seen.clone();

    let agent = tokio::spawn(async move {
        // The answer to the earlier process's prompt, id 7, comes first.
        agent_writes
            .unbounded_send(Ok(
                json!({"jsonrpc": "2.0", "id": 7, "result": {"stopReason": "end_turn"}}).to_string(),
            ))
            .unwrap();
        // Then an ordinary notification, to show the handler is live.
        agent_writes
            .unbounded_send(Ok(
                json!({"jsonrpc": "2.0", "method": "probe/after", "params": {}}).to_string()
            ))
            .unwrap();
        let first = next_line(&mut agent_reads).await;
        agent_writes
            .unbounded_send(Ok(end_turn(&first["id"]).to_string()))
            .unwrap();
        first
    });

    let new_id = soon(
        "the SDK conversation",
        Client
            .v1()
            .name("attached-client")
            .on_receive_dispatch(
                async move |dispatch: v1::Dispatch, _connection: v1::ConnectionTo<Agent>| {
                    let description = match &dispatch {
                        v1::Dispatch::Request(request, _) => format!("request {}", request.method()),
                        v1::Dispatch::Notification(notification) => {
                            format!("notification {}", notification.method())
                        }
                        v1::Dispatch::Response(_, router) => format!("response to {}", router.method()),
                    };
                    recorder.lock().unwrap().push(description);
                    // Seen, not claimed: a response still has to reach the
                    // future that is waiting for it.
                    Ok(Handled::No {
                        message: dispatch,
                        retry: false,
                    })
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async |connection: v1::ConnectionTo<Agent>| {
                let sent = connection.send_request(prompt("next"));
                let id = sent.id().clone();
                sent.block_task().await?;
                Ok(id)
            }),
    )
    .await
    .unwrap();

    let first = agent.await.unwrap();
    assert_ne!(first["id"], json!(7), "the SDK chose its own id for the new request");
    assert_eq!(first["id"], json!(new_id));
    assert!(
        matches!(&first["id"], Value::String(id) if id.len() == 36),
        "a generated UUID, which a caller cannot choose: {}",
        first["id"]
    );
    assert_eq!(
        *seen.lock().unwrap(),
        ["notification probe/after", "response to session/prompt"],
        "every message passes the handler, responses included, yet the old response never did"
    );
}
