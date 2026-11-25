//! A JSON-RPC connection over a line stream that owns its request-id table.
//!
//! The connection frames the incoming bytes, journals every line in both
//! directions, allocates the id of each request it sends, and resolves the
//! matching response. Everything else that arrives (the agent's own requests,
//! notifications, responses nobody is waiting for, lines that are not
//! JSON-RPC) is handed to the caller as an [`Incoming`] value in arrival
//! order.
//!
//! Two properties are deliberate, because a client that outlives one process
//! needs them:
//!
//! - The id of a request is known before the request is on the wire
//!   ([`PendingResponse::id`]), so the caller can record which id carried
//!   what before the response can possibly arrive.
//! - [`Connection::expect_response`] registers an id whose request an
//!   earlier connection published on the same stream. The response is then
//!   delivered here instead of being discarded as unknown, and nothing is
//!   sent a second time.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::framing::{Frame, LineFramer};
use crate::journal::{Direction, Journal};
use crate::schema::{Error as RpcError, RequestId};
use crate::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};

/// Why the connection stopped reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// The agent closed its end of the stream.
    Eof,
    /// Reading the stream failed.
    Read(String),
    /// A line could not be journalled. The connection stops rather than act
    /// on a line the journal does not hold.
    Journal(String),
}

impl std::fmt::Display for CloseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof => write!(f, "the agent closed the stream"),
            Self::Read(error) => write!(f, "reading the stream failed: {error}"),
            Self::Journal(error) => write!(f, "journalling a line failed: {error}"),
        }
    }
}

/// Something that arrived and is not the response to a pending request.
///
/// `seq` is the journal sequence number of the line, or 0 without a journal.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// The agent asks the client for something and waits for
    /// [`Connection::respond`].
    Request {
        id: RequestId,
        method: String,
        params: Value,
        seq: u64,
    },
    Notification {
        method: String,
        params: Value,
        seq: u64,
    },
    /// A response whose id nobody is waiting for: a request sent by an
    /// earlier connection that was not registered with
    /// [`Connection::expect_response`], a duplicate, or an agent error.
    UnmatchedResponse {
        id: RequestId,
        result: Result<Value, RpcError>,
        seq: u64,
    },
    /// A line that is not a JSON-RPC message. The stream stays usable.
    Malformed {
        preview: String,
        reason: String,
        seq: u64,
    },
    /// Nothing more will arrive. Every pending request has been failed.
    Closed(CloseReason),
}

/// Failure to put a message on the wire.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("the connection is closed: {0}")]
    Closed(CloseReason),
    #[error("encoding `{method}`: {reason}")]
    Encode { method: String, reason: String },
    #[error("journalling an outgoing line: {0}")]
    Journal(std::io::Error),
    #[error("writing to the agent: {0}")]
    Write(std::io::Error),
}

/// Failure of one request.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error("`{method}` (id {id}) failed: {} (code {})", .error.message, i32::from(.error.code))]
    Rpc {
        method: String,
        id: RequestId,
        /// Boxed so that the error of every request is not as large as the
        /// agent's largest error payload.
        error: Box<RpcError>,
    },
    #[error("the connection closed before the response to `{method}` (id {id}): {reason}")]
    ClosedBeforeResponse {
        method: String,
        id: RequestId,
        reason: CloseReason,
    },
    #[error("decoding the response to `{method}` (id {id}): {reason}")]
    Decode {
        method: String,
        id: RequestId,
        reason: String,
    },
}

struct PendingEntry {
    reply: oneshot::Sender<Result<Value, RpcError>>,
}

#[derive(Default)]
struct Table {
    pending: HashMap<RequestId, PendingEntry>,
    closed: Option<CloseReason>,
}

struct Shared {
    /// `None` once the writing half was closed by [`Connection::shutdown_writer`].
    writer: tokio::sync::Mutex<Option<Box<dyn AsyncWrite + Send + Unpin>>>,
    table: Mutex<Table>,
    journal: Option<Arc<Journal>>,
    next_id: AtomicI64,
}

/// The response to one request, not yet received.
#[derive(Debug)]
pub struct PendingResponse<T> {
    id: RequestId,
    method: String,
    reply: oneshot::Receiver<Result<Value, RpcError>>,
    shared_closed: Arc<Shared>,
    response: PhantomData<fn() -> T>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl<T: JsonRpcResponse> PendingResponse<T> {
    /// The id the request went out with.
    pub fn id(&self) -> &RequestId {
        &self.id
    }

    pub fn method(&self) -> &str {
        &self.method
    }

    /// Waits for the response and decodes it.
    pub async fn response(self) -> Result<T, RequestError> {
        let Self {
            id,
            method,
            reply,
            shared_closed,
            ..
        } = self;
        match reply.await {
            Ok(Ok(value)) => T::from_json(&method, value).map_err(|error| RequestError::Decode {
                method,
                id,
                reason: error.to_string(),
            }),
            Ok(Err(error)) => Err(RequestError::Rpc {
                method,
                id,
                error: Box::new(error),
            }),
            Err(_) => Err(RequestError::ClosedBeforeResponse {
                method,
                id,
                reason: shared_closed.close_reason().unwrap_or(CloseReason::Eof),
            }),
        }
    }
}

/// Where the connection's own request ids start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionOptions {
    /// The id of the first request this connection sends. A connection that
    /// attaches to a stream an earlier connection used must start above every
    /// id that connection sent, or a late response would be taken for the
    /// answer to a new request.
    pub first_request_id: i64,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self { first_request_id: 1 }
    }
}

/// A cheap, cloneable handle to one connection.
#[derive(Clone, Debug)]
pub struct Connection {
    shared: Arc<Shared>,
}

impl Connection {
    /// Starts reading `reader` on a new task and returns the handle plus the
    /// stream of everything that is not a response to a pending request.
    ///
    /// Nothing is sent: the caller decides whether this stream needs an
    /// `initialize` or already had one.
    pub fn open<R, W>(
        reader: R,
        writer: W,
        journal: Option<Arc<Journal>>,
        options: ConnectionOptions,
    ) -> (Self, mpsc::UnboundedReceiver<Incoming>)
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let shared = Arc::new(Shared {
            writer: tokio::sync::Mutex::new(Some(Box::new(writer))),
            table: Mutex::new(Table::default()),
            journal,
            next_id: AtomicI64::new(options.first_request_id),
        });
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(reader, shared.clone(), incoming_tx));
        (Self { shared }, incoming_rx)
    }

    /// Sends a request. The returned handle already knows the request's id.
    pub async fn send_request<Req: JsonRpcRequest>(
        &self,
        request: &Req,
    ) -> Result<PendingResponse<Req::Response>, ConnectionError> {
        let (method, params) = untyped(request)?;
        let id = RequestId::Number(self.shared.next_id.fetch_add(1, Ordering::Relaxed));
        // Registered before the line is written: the response can arrive
        // before the write call returns.
        let pending = self.register(id.clone(), &method)?;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        if let Err(error) = self.shared.write_line(&line).await {
            self.shared.table().pending.remove(&id);
            return Err(error);
        }
        Ok(pending)
    }

    /// Sends a request and waits for its decoded response.
    pub async fn call<Req: JsonRpcRequest>(&self, request: &Req) -> Result<Req::Response, RequestError> {
        self.send_request(request).await?.response().await
    }

    /// Waits for the response to a request that an earlier connection sent on
    /// this stream. Sends nothing.
    pub fn expect_response<T: JsonRpcResponse>(
        &self,
        id: RequestId,
        method: &str,
    ) -> Result<PendingResponse<T>, ConnectionError> {
        self.register(id, method)
    }

    pub async fn send_notification<N: JsonRpcNotification>(&self, notification: &N) -> Result<(), ConnectionError> {
        let (method, params) = untyped(notification)?;
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        self.shared.write_line(&line).await
    }

    /// Answers one of the agent's requests.
    pub async fn respond<T: Serialize>(
        &self,
        id: RequestId,
        result: Result<T, RpcError>,
    ) -> Result<(), ConnectionError> {
        let mut message = Map::new();
        message.insert("jsonrpc".into(), json!("2.0"));
        message.insert("id".into(), json!(id));
        let encode = |reason: serde_json::Error| ConnectionError::Encode {
            method: format!("response {id}"),
            reason: reason.to_string(),
        };
        match result {
            Ok(result) => message.insert("result".into(), serde_json::to_value(result).map_err(encode)?),
            Err(error) => message.insert("error".into(), serde_json::to_value(error).map_err(encode)?),
        };
        self.shared.write_line(&Value::Object(message).to_string()).await
    }

    /// The ids of the requests still waiting for a response, in no order.
    pub fn pending_ids(&self) -> Vec<RequestId> {
        self.shared.table().pending.keys().cloned().collect()
    }

    /// Why the connection stopped reading, once it has.
    pub fn close_reason(&self) -> Option<CloseReason> {
        self.shared.close_reason()
    }

    /// Closes the writing half, which an agent on stdio reads as end of input.
    /// Nothing can be sent afterwards; reading carries on until the agent
    /// closes its side.
    ///
    /// The writer is dropped as well as shut down: a pipe to a child process
    /// is only closed when its handle is dropped, so shutting it down alone
    /// would leave the agent waiting for input that can never come.
    pub async fn shutdown_writer(&self) -> std::io::Result<()> {
        let Some(mut writer) = self.shared.writer.lock().await.take() else {
            return Ok(());
        };
        writer.shutdown().await
    }

    fn register<T: JsonRpcResponse>(&self, id: RequestId, method: &str) -> Result<PendingResponse<T>, ConnectionError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let mut table = self.shared.table();
        if let Some(reason) = &table.closed {
            return Err(ConnectionError::Closed(reason.clone()));
        }
        table.pending.insert(id.clone(), PendingEntry { reply: reply_tx });
        Ok(PendingResponse {
            id,
            method: method.to_owned(),
            reply: reply_rx,
            shared_closed: self.shared.clone(),
            response: PhantomData,
        })
    }
}

fn untyped<M: crate::JsonRpcMessage>(message: &M) -> Result<(String, Value), ConnectionError> {
    message
        .to_untyped_message()
        .map(|untyped| untyped.into_parts())
        .map_err(|error| ConnectionError::Encode {
            method: message.method().to_owned(),
            reason: error.to_string(),
        })
}

impl Shared {
    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn close_reason(&self) -> Option<CloseReason> {
        self.table().closed.clone()
    }

    /// Journals and writes one line. The writer lock is held across both, so
    /// outgoing lines appear in the journal in the order they hit the wire.
    async fn write_line(&self, line: &str) -> Result<(), ConnectionError> {
        if let Some(reason) = self.close_reason() {
            return Err(ConnectionError::Closed(reason));
        }
        let mut writer = self.writer.lock().await;
        let Some(writer) = writer.as_mut() else {
            return Err(ConnectionError::Write(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the writing half was closed",
            )));
        };
        if let Some(journal) = &self.journal {
            journal
                .append(Direction::ToAgent, line)
                .map_err(ConnectionError::Journal)?;
        }
        let mut framed = Vec::with_capacity(line.len() + 1);
        framed.extend_from_slice(line.as_bytes());
        framed.push(b'\n');
        writer.write_all(&framed).await.map_err(ConnectionError::Write)?;
        writer.flush().await.map_err(ConnectionError::Write)
    }

    /// Fails every pending request and refuses new ones.
    fn close(&self, reason: CloseReason) {
        let mut table = self.table();
        if table.closed.is_none() {
            table.closed = Some(reason);
        }
        // Dropping the senders resolves every waiter with the close reason.
        table.pending.clear();
    }
}

async fn read_loop<R>(mut reader: R, shared: Arc<Shared>, incoming: mpsc::UnboundedSender<Incoming>)
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let mut framer = LineFramer::new();
    let mut chunk = vec![0_u8; 64 * 1024];
    let reason = 'read: loop {
        let frames = match reader.read(&mut chunk).await {
            Ok(0) => {
                let last = framer.finish();
                if let Some(frame) = last
                    && let Err(error) = handle_frame(&shared, &incoming, frame)
                {
                    break CloseReason::Journal(error.to_string());
                }
                break CloseReason::Eof;
            }
            Ok(read) => framer.push(&chunk[..read]),
            Err(error) => break CloseReason::Read(error.to_string()),
        };
        for frame in frames {
            if let Err(error) = handle_frame(&shared, &incoming, frame) {
                break 'read CloseReason::Journal(error.to_string());
            }
        }
    };
    shared.close(reason.clone());
    let _ = incoming.send(Incoming::Closed(reason));
}

fn handle_frame(shared: &Shared, incoming: &mpsc::UnboundedSender<Incoming>, frame: Frame) -> std::io::Result<()> {
    let journal = |line: &str| match &shared.journal {
        Some(journal) => journal.append(Direction::FromAgent, line),
        None => Ok(0),
    };
    let event = match frame {
        Frame::Invalid { preview, error } => {
            let seq = journal(&preview)?;
            Incoming::Malformed {
                preview,
                reason: error.to_string(),
                seq,
            }
        }
        Frame::Line(line) => {
            let seq = journal(&line)?;
            match classify(&line, seq) {
                Classified::Event(event) => event,
                Classified::Response { id, result } => {
                    let entry = shared.table().pending.remove(&id);
                    match entry {
                        // The waiter may have gone away; the response is
                        // still consumed, exactly once.
                        Some(entry) => {
                            let _ = entry.reply.send(result);
                            return Ok(());
                        }
                        None => Incoming::UnmatchedResponse { id, result, seq },
                    }
                }
            }
        }
    };
    let _ = incoming.send(event);
    Ok(())
}

enum Classified {
    Event(Incoming),
    Response {
        id: RequestId,
        result: Result<Value, RpcError>,
    },
}

/// Sorts one line into request, notification, response or malformed.
fn classify(line: &str, seq: u64) -> Classified {
    let malformed = |reason: String| {
        Classified::Event(Incoming::Malformed {
            preview: line.chars().take(256).collect(),
            reason,
            seq,
        })
    };
    let mut message = match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(message)) => message,
        Ok(Value::Array(_)) => return malformed("JSON-RPC batches are not part of ACP".into()),
        Ok(_) => return malformed("not a JSON object".into()),
        Err(error) => return malformed(format!("not JSON: {error}")),
    };
    let id = match message.remove("id") {
        None => None,
        Some(id) => match serde_json::from_value::<RequestId>(id) {
            Ok(id) => Some(id),
            Err(_) => return malformed("`id` is not a string, an integer or null".into()),
        },
    };
    if let Some(method) = message.remove("method") {
        let Value::String(method) = method else {
            return malformed("`method` is not a string".into());
        };
        let params = message.remove("params").unwrap_or(Value::Null);
        return Classified::Event(match id {
            // JSON-RPC treats a null id as "no usable id": it cannot be answered.
            Some(RequestId::Null) | None => Incoming::Notification { method, params, seq },
            Some(id) => Incoming::Request {
                id,
                method,
                params,
                seq,
            },
        });
    }
    let Some(id) = id else {
        return malformed("neither a method nor an id".into());
    };
    if let Some(error) = message.remove("error") {
        return match serde_json::from_value::<RpcError>(error) {
            Ok(error) => Classified::Response { id, result: Err(error) },
            Err(error) => malformed(format!("`error` is not a JSON-RPC error object: {error}")),
        };
    }
    match message.remove("result") {
        Some(result) => Classified::Response { id, result: Ok(result) },
        None => malformed("a response with neither `result` nor `error`".into()),
    }
}
