//! Shared Anthropic Messages API loopback for Claude live tests.
//!
//! One HTTP/1.1 server: `count_tokens` returns JSON, everything else is a
//! short SSE assistant turn. Tests that need to inspect the model request
//! can read [`AnthropicSseLoopback::captured`]; [`Drop`] posts `/__shutdown`
//! so the listener thread does not leak.
//!
//! Each accepted connection is read/written with a timeout and then shut
//! down. A client that never closes its write half cannot pin the listener
//! (and therefore [`Drop`]) for the rest of the test process.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(10);

const SSE_OK: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fixture\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"fixture ready\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

/// Loopback Anthropic Messages + token-count fixture.
pub struct AnthropicSseLoopback {
    addr: SocketAddr,
    captured: Arc<Mutex<Vec<(String, String)>>>,
    join: Option<JoinHandle<()>>,
}

impl AnthropicSseLoopback {
    /// Bind `127.0.0.1:0` and serve until shutdown.
    pub fn spawn() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let join = thread::spawn(move || serve(listener, captured_clone));
        Ok(Self {
            addr,
            captured,
            join: Some(join),
        })
    }

    /// Address to put in `ANTHROPIC_BASE_URL`.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Captured `(request_line, body)` pairs, excluding the shutdown probe.
    pub fn captured(&self) -> Vec<(String, String)> {
        self.captured.lock().expect("captured lock").clone()
    }

    /// Bodies of `/messages` requests that are not `count_tokens`, in order.
    pub fn messages_bodies(&self) -> Vec<String> {
        self.captured()
            .into_iter()
            .filter_map(|(req, body)| (req.contains("messages") && !req.contains("count_tokens")).then_some(body))
            .collect()
    }

    /// Body of the first `/messages` request that is not `count_tokens`.
    pub fn first_messages_body(&self) -> Option<String> {
        self.messages_bodies().into_iter().next()
    }

    fn shutdown(&mut self) {
        if let Ok(mut conn) = TcpStream::connect(self.addr) {
            let _ = conn.set_nodelay(true);
            let _ = conn.set_write_timeout(Some(IO_TIMEOUT));
            let _ = write!(
                conn,
                "POST /__shutdown HTTP/1.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = conn.flush();
            let _ = conn.shutdown(Shutdown::Write);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for AnthropicSseLoopback {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn configure_stream(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
}

fn serve(listener: TcpListener, captured: Arc<Mutex<Vec<(String, String)>>>) {
    while let Ok((stream, _)) = listener.accept() {
        configure_stream(&stream);
        let mut reader = BufReader::new(stream);
        let mut req_line = String::new();
        if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
            continue;
        }
        let mut content_length = 0usize;
        let mut header = String::new();
        loop {
            header.clear();
            if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                break;
            }
            if let Some(val) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = val.trim().parse::<usize>().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            let _ = reader.read_exact(&mut body);
        }
        let is_shutdown = req_line.contains("/__shutdown");
        if !is_shutdown {
            captured
                .lock()
                .expect("captured lock")
                .push((req_line.clone(), String::from_utf8_lossy(&body).into_owned()));
        }
        let (content_type, resp_body) = if req_line.contains("count_tokens") {
            ("application/json", r#"{"input_tokens":1}"#.to_owned())
        } else if is_shutdown {
            ("text/plain", "bye".to_owned())
        } else {
            ("text/event-stream", SSE_OK.to_owned())
        };
        let mut stream = reader.into_inner();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
            resp_body.len()
        );
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Both);
        if is_shutdown {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AnthropicSseLoopback, IO_TIMEOUT};
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpStream};

    fn post(addr: std::net::SocketAddr, path: &str, body: &str) -> String {
        let mut conn = TcpStream::connect(addr).expect("connect");
        conn.set_nodelay(true).expect("nodelay");
        conn.set_read_timeout(Some(IO_TIMEOUT)).expect("read timeout");
        conn.set_write_timeout(Some(IO_TIMEOUT)).expect("write timeout");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        conn.write_all(request.as_bytes()).unwrap();
        conn.flush().unwrap();
        let _ = conn.shutdown(Shutdown::Write);
        let mut resp = String::new();
        conn.read_to_string(&mut resp).unwrap();
        resp
    }

    #[test]
    fn loopback_serves_count_tokens_json_and_messages_sse_and_captures() {
        let fixture = AnthropicSseLoopback::spawn().expect("bind");
        let addr = fixture.addr();

        let tokens_resp = post(addr, "/v1/messages/count_tokens", "{}");
        assert!(
            tokens_resp.contains("application/json") && tokens_resp.contains("input_tokens"),
            "{tokens_resp}"
        );

        let messages_resp = post(addr, "/v1/messages", r#"{"model":"x"}"#);
        assert!(
            messages_resp.contains("text/event-stream") && messages_resp.contains("message_stop"),
            "{messages_resp}"
        );

        let bodies = fixture.captured();
        assert_eq!(bodies.len(), 2, "{bodies:?}");
        assert!(bodies[0].0.contains("count_tokens"));
        assert!(fixture.first_messages_body().unwrap().contains("model"));
    }
}
