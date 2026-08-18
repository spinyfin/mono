//! The evidence surface: a loopback HTTP server that renders stored
//! attachments for a worker's own verification and local inspection.
//!
//! ## Why HTTP at all
//!
//! Storing evidence that nobody can inspect locally solves nothing. A loopback
//! HTTP URL lets a browser on the capturing machine render stored bytes after
//! the engine restarts, without exposing the store off-host. This is more
//! useful than a `file:` URL because it is stable under the engine state root
//! rather than the ephemeral workspace that produced the image.
//!
//! Inline `![](…)` embedding deliberately is not attempted: GitHub proxies
//! image sources through camo, which fetches from GitHub's servers and cannot
//! reach anyone's loopback. A link is the honest shape.
//!
//! ## Why hand-rolled
//!
//! There is no HTTP server anywhere in this repo to reuse (searched before
//! writing: `http_retry` is a client-side retry helper). What this surface
//! serves is a handful of static byte blobs and one generated page, to a
//! browser on the same machine. Pulling `hyper`/`axum` in would add a
//! vendored dependency tree to the process that owns the engine database, in
//! exchange for routing this file does in a `match`.
//!
//! ## Why it is safe to run
//!
//! It binds `127.0.0.1` only, so nothing off-host can reach it. Two further
//! gates handle the browser-based attacker: the `Host` header must name
//! loopback (a page at `evil.com` whose DNS answers `127.0.0.1` sends
//! `Host: evil.com`, which is refused — the standard DNS-rebinding defence),
//! and no CORS headers are ever emitted, so cross-origin script cannot read a
//! response even if it elicits one. Ids are unguessable (`atc_` + a monotonic
//! id, images additionally content-addressed), the surface is read-only, and
//! `GET`/`HEAD` are the only accepted methods.

use std::sync::Arc;

use boss_protocol::{WorkAttachment, attachment_gallery_path, attachment_image_path};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::store::AttachmentStore;

/// Port the evidence surface listens on by default.
///
/// Fixed rather than ephemeral, and that is load-bearing: a stored local URL
/// an operator follows after the engine restarts must still resolve, so the
/// port cannot be rolled per boot.
pub const DEFAULT_PORT: u16 = 8419;

/// Env var overriding [`DEFAULT_PORT`]. `0` disables the surface entirely.
pub const PORT_ENV: &str = "BOSS_ATTACHMENT_PORT";

/// Largest request head the server will read before giving up. A browser's
/// `GET` with cookies and `Accept-*` is well under this; anything larger is
/// not a request this surface has any business answering.
const MAX_REQUEST_HEAD_BYTES: usize = 8 * 1024;

/// What the engine needs to look up to render a page. Implemented over the
/// work database by `boss-engine-core`; kept as a trait here so this crate
/// never depends on the crate that consumes it.
pub trait EvidenceCatalog: Send + Sync + 'static {
    /// One attachment by id, including reclaimed tombstones.
    fn attachment(&self, attachment_id: &str) -> Option<WorkAttachment>;
    /// Every attachment for a work item, newest first, including tombstones.
    fn attachments_for_work_item(&self, work_item_id: &str) -> Vec<WorkAttachment>;
    /// Human-facing name of the work item, and its PR URL when one is bound.
    /// Both are cosmetic — the gallery renders without them.
    fn work_item_label(&self, work_item_id: &str) -> Option<WorkItemLabel>;
}

/// Cosmetic header material for a gallery page.
#[derive(Debug, Clone, Default)]
pub struct WorkItemLabel {
    pub name: String,
    pub pr_url: Option<String>,
}

/// Resolve the configured port. `None` when the surface is disabled.
pub fn configured_port() -> Option<u16> {
    match std::env::var(PORT_ENV) {
        Ok(raw) => match raw.trim().parse::<u16>() {
            Ok(0) => None,
            Ok(port) => Some(port),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "attachment evidence server: {PORT_ENV} is not a port number; using the default"
                );
                Some(DEFAULT_PORT)
            }
        },
        Err(_) => Some(DEFAULT_PORT),
    }
}

/// The origin links are minted against, e.g. `http://127.0.0.1:8419`.
pub fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// A parsed request head. Only what routing needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub target: String,
    pub host: Option<String>,
}

impl HttpRequest {
    /// Parse a request head. `None` for anything that is not a plausible
    /// HTTP/1.x request line — the connection is then closed without a reply.
    pub fn parse(head: &str) -> Option<Self> {
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next()?.split(' ');
        let method = request_line.next()?.to_owned();
        let target = request_line.next()?.to_owned();
        if method.is_empty() || !target.starts_with('/') {
            return None;
        }
        let host = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("host").then(|| value.trim().to_owned())
        });
        Some(Self { method, target, host })
    }

    /// Path with any query string stripped.
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }
}

/// A response, ready to serialise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl HttpResponse {
    fn html(status: u16, reason: &'static str, body: String) -> Self {
        Self {
            status,
            reason,
            content_type: "text/html; charset=utf-8",
            body: body.into_bytes(),
        }
    }

    fn ok_html(body: String) -> Self {
        Self::html(200, "OK", body)
    }

    fn image(content_type: &'static str, bytes: Vec<u8>) -> Self {
        Self {
            status: 200,
            reason: "OK",
            content_type,
            body: bytes,
        }
    }

    /// Serialise head + body. `include_body` is false for `HEAD`.
    pub fn to_bytes(&self, include_body: bool) -> Vec<u8> {
        let head = format!(
            "HTTP/1.1 {} {}\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             Cache-Control: no-store\r\n\
             X-Content-Type-Options: nosniff\r\n\
             Referrer-Policy: no-referrer\r\n\
             Content-Security-Policy: default-src 'none'; img-src 'self'; style-src 'unsafe-inline'\r\n\
             Connection: close\r\n\
             \r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len(),
        );
        let mut out = head.into_bytes();
        if include_body {
            out.extend_from_slice(&self.body);
        }
        out
    }
}

/// Whether `host` names loopback. The DNS-rebinding gate: a browser fetching
/// `http://evil.com:8419/…` that resolves to 127.0.0.1 still sends
/// `Host: evil.com:8419`, and is refused here.
fn host_is_loopback(host: Option<&str>) -> bool {
    let Some(host) = host else {
        // HTTP/1.1 requires Host. A request without one is not a browser
        // following a link, so it gets nothing.
        return false;
    };
    let hostname = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
    let hostname = hostname.trim_start_matches('[').trim_end_matches(']');
    matches!(hostname, "127.0.0.1" | "localhost" | "::1")
}

/// Route and render one request. Pure with respect to the network — every
/// routing and rendering decision is testable without a socket.
pub fn respond(request: &HttpRequest, catalog: &dyn EvidenceCatalog, store: &AttachmentStore) -> HttpResponse {
    if !matches!(request.method.as_str(), "GET" | "HEAD") {
        return HttpResponse::html(
            405,
            "Method Not Allowed",
            page("Method not allowed", "<p>This surface is read-only.</p>".to_owned()),
        );
    }
    if !host_is_loopback(request.host.as_deref()) {
        return HttpResponse::html(
            403,
            "Forbidden",
            page(
                "Forbidden",
                "<p>Boss evidence is served to loopback only. Open the link as \
                 <code>127.0.0.1</code>.</p>"
                    .to_owned(),
            ),
        );
    }

    let path = request.path();
    if path == "/" {
        return HttpResponse::ok_html(index_page());
    }
    if let Some(work_item_id) = path.strip_prefix("/w/") {
        return gallery_response(work_item_id, catalog);
    }
    if let Some(attachment_id) = path.strip_prefix("/a/") {
        return image_response(attachment_id, catalog, store);
    }
    not_found()
}

fn not_found() -> HttpResponse {
    HttpResponse::html(
        404,
        "Not Found",
        page("Not found", "<p>No such evidence.</p>".to_owned()),
    )
}

fn gallery_response(work_item_id: &str, catalog: &dyn EvidenceCatalog) -> HttpResponse {
    if !is_plausible_id(work_item_id) {
        return not_found();
    }
    let attachments = catalog.attachments_for_work_item(work_item_id);
    if attachments.is_empty() {
        return HttpResponse::html(
            404,
            "Not Found",
            page(
                "No evidence",
                format!(
                    "<p>No attachments are stored for <code>{}</code>.</p>",
                    escape(work_item_id)
                ),
            ),
        );
    }
    let label = catalog.work_item_label(work_item_id).unwrap_or_default();
    HttpResponse::ok_html(gallery_page(work_item_id, &label, &attachments))
}

fn image_response(attachment_id: &str, catalog: &dyn EvidenceCatalog, store: &AttachmentStore) -> HttpResponse {
    if !is_plausible_id(attachment_id) {
        return not_found();
    }
    let Some(attachment) = catalog.attachment(attachment_id) else {
        return not_found();
    };
    if let Some(reclaimed_at) = attachment.reclaimed_at.as_deref() {
        return reclaimed_response(&attachment, reclaimed_at);
    }
    match store.read_blob(&attachment.content_digest, attachment.media_type) {
        Ok(Some(bytes)) => HttpResponse::image(attachment.media_type.mime(), bytes),
        // The row says the blob should be here and it is not. That is a
        // retention/store inconsistency, not a caller error, and saying so
        // beats a bare 404 that reads like a bad link.
        Ok(None) => HttpResponse::html(
            410,
            "Gone",
            page(
                "Evidence missing",
                "<p>This attachment's row exists but its bytes are gone from the store. \
                 Run <code>bossctl attachments sweep</code> to reconcile.</p>"
                    .to_owned(),
            ),
        ),
        Err(err) => {
            tracing::warn!(attachment_id, error = %format!("{err:#}"), "evidence server: failed to read blob");
            HttpResponse::html(
                500,
                "Internal Server Error",
                page("Read failed", "<p>The stored image could not be read.</p>".to_owned()),
            )
        }
    }
}

fn reclaimed_response(attachment: &WorkAttachment, reclaimed_at: &str) -> HttpResponse {
    HttpResponse::html(
        410,
        "Gone",
        page(
            "Evidence reclaimed",
            format!(
                "<p>This screenshot was captured by execution <code>{}</code> on {} and reclaimed \
                 by retention on {}.</p><p>Retention is bounded on purpose; see \
                 <code>BOSS_ATTACHMENT_RETENTION_DAYS</code>.</p>",
                escape(&attachment.execution_id),
                escape(&format_epoch(&attachment.created_at)),
                escape(&format_epoch(reclaimed_at)),
            ),
        ),
    )
}

/// Ids are engine-minted (`atc_…`, `task_…`); anything with a path separator
/// or `..` is not one, and rejecting the shape early keeps a traversal
/// attempt from ever reaching a lookup.
fn is_plausible_id(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len() <= 128
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

// ── Rendering ───────────────────────────────────────────────────────────────

fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Epoch-seconds string → a readable UTC stamp. Falls back to the raw value
/// rather than inventing one when the column holds something unexpected.
fn format_epoch(raw: &str) -> String {
    raw.parse::<i64>()
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| raw.to_owned())
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} kB", bytes.div_ceil(1024))
    }
}

const STYLE: &str = "\
:root{color-scheme:light dark}\
body{font:14px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;margin:0 auto;max-width:1100px;padding:2rem 1.25rem}\
h1{font-size:1.3rem;margin:0 0 .25rem}\
.sub{opacity:.7;margin:0 0 2rem}\
h2{font-size:1rem;margin:2rem 0 .25rem;border-top:1px solid rgba(128,128,128,.35);padding-top:1rem}\
figure{margin:1.25rem 0}\
figcaption{margin-top:.5rem}\
img{max-width:100%;height:auto;border:1px solid rgba(128,128,128,.35);border-radius:6px;display:block}\
code{font-size:.9em;opacity:.85}\
.meta{opacity:.65;font-size:.85em}";

fn page(title: &str, body: String) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        escape(title)
    )
}

fn index_page() -> String {
    page(
        "Boss evidence",
        "<h1>Boss evidence</h1>\
         <p class=\"sub\">Screenshots workers attached for their own verification and local inspection, served to loopback only.</p>\
         <p>There is no global index by design. Open a work item's gallery directly \
         (<code>/w/&lt;work-item-id&gt;</code>), or list \
         what is stored with <code>bossctl attachments list</code>.</p>"
            .to_owned(),
    )
}

fn gallery_page(work_item_id: &str, label: &WorkItemLabel, attachments: &[WorkAttachment]) -> String {
    let mut body = String::new();
    body.push_str(&format!(
        "<h1>{}</h1><p class=\"sub\">Evidence for <code>{}</code>",
        escape(if label.name.is_empty() {
            "Stored evidence"
        } else {
            &label.name
        }),
        escape(work_item_id),
    ));
    if let Some(pr_url) = label.pr_url.as_deref() {
        body.push_str(&format!(" · <a href=\"{}\">PR</a>", escape(pr_url)));
    }
    body.push_str("</p>");

    // Grouped by execution, newest run first — someone inspecting a revision
    // chain locally needs to know which run produced which image, and wants the latest at
    // the top. `attachments` arrives newest-first, so first-seen order is
    // already the order to render.
    let mut execution_order: Vec<&str> = Vec::new();
    for attachment in attachments {
        if !execution_order.contains(&attachment.execution_id.as_str()) {
            execution_order.push(&attachment.execution_id);
        }
    }

    for (index, execution_id) in execution_order.iter().enumerate() {
        let latest = if index == 0 { " · latest run" } else { "" };
        body.push_str(&format!("<h2>Run <code>{}</code>{latest}</h2>", escape(execution_id)));
        for attachment in attachments.iter().filter(|a| a.execution_id == **execution_id) {
            body.push_str(&figure(attachment));
        }
    }
    page("Stored evidence", body)
}

fn figure(attachment: &WorkAttachment) -> String {
    let caption = if attachment.caption.trim().is_empty() {
        attachment.source_name.clone()
    } else {
        attachment.caption.clone()
    };
    let meta = format!(
        "{}x{} · {} · {}",
        attachment.pixel_width,
        attachment.pixel_height,
        human_bytes(attachment.size_bytes),
        format_epoch(&attachment.created_at),
    );
    if attachment.is_reclaimed() {
        return format!(
            "<figure><figcaption>{}<br><span class=\"meta\">reclaimed by retention on {} — bytes \
             are gone, this row is the record that it existed</span></figcaption></figure>",
            escape(&caption),
            escape(&format_epoch(attachment.reclaimed_at.as_deref().unwrap_or(""))),
        );
    }
    format!(
        "<figure><a href=\"{href}\"><img src=\"{href}\" alt=\"{alt}\" width=\"{w}\" height=\"{h}\"></a>\
         <figcaption>{caption}<br><span class=\"meta\">{meta}</span></figcaption></figure>",
        href = escape(&attachment_image_path(&attachment.id)),
        alt = escape(&caption),
        w = attachment.pixel_width,
        h = attachment.pixel_height,
        caption = escape(&caption),
        meta = escape(&meta),
    )
}

/// The loopback gallery URL for one work item.
pub fn gallery_url_for(port: u16, work_item_id: &str) -> String {
    format!("{}{}", base_url(port), attachment_gallery_path(work_item_id))
}

// ── Serving ─────────────────────────────────────────────────────────────────

/// Bind the evidence surface to loopback. `Ok(None)` when it is disabled by
/// configuration; `Err` when the port is taken (the caller logs and carries
/// on — a missing evidence surface must never stop the engine booting).
pub async fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port)).await
}

/// Accept and answer forever. One request per connection (`Connection: close`)
/// — this surface serves a browser clicking a link, not a keep-alive workload.
pub async fn serve(listener: TcpListener, catalog: Arc<dyn EvidenceCatalog>, store: AttachmentStore) {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                tracing::warn!(error = %err, "evidence server: accept failed");
                continue;
            }
        };
        let catalog = Arc::clone(&catalog);
        let store = store.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, catalog.as_ref(), &store).await {
                tracing::debug!(error = %err, "evidence server: connection ended early");
            }
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    catalog: &dyn EvidenceCatalog,
    store: &AttachmentStore,
) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if head.len() > MAX_REQUEST_HEAD_BYTES {
            return Ok(());
        }
    }
    let Some(request) = std::str::from_utf8(&head).ok().and_then(HttpRequest::parse) else {
        return Ok(());
    };
    let include_body = request.method != "HEAD";
    let response = respond(&request, catalog, store);
    stream.write_all(&response.to_bytes(include_body)).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::png_bytes;
    use boss_protocol::AttachmentMediaType;

    struct FakeCatalog {
        attachments: Vec<WorkAttachment>,
    }

    impl EvidenceCatalog for FakeCatalog {
        fn attachment(&self, attachment_id: &str) -> Option<WorkAttachment> {
            self.attachments.iter().find(|a| a.id == attachment_id).cloned()
        }
        fn attachments_for_work_item(&self, work_item_id: &str) -> Vec<WorkAttachment> {
            self.attachments
                .iter()
                .filter(|a| a.work_item_id == work_item_id)
                .cloned()
                .collect()
        }
        fn work_item_label(&self, _work_item_id: &str) -> Option<WorkItemLabel> {
            Some(WorkItemLabel {
                name: "Wide table layout".to_owned(),
                pr_url: Some("https://github.com/o/r/pull/1".to_owned()),
            })
        }
    }

    fn attachment(id: &str, execution_id: &str, digest: &str) -> WorkAttachment {
        WorkAttachment::builder()
            .id(id)
            .execution_id(execution_id)
            .work_item_id("task_1")
            .caption("after the fix")
            .content_digest(digest)
            .created_at("1700000000")
            .media_type(AttachmentMediaType::Png)
            .pixel_width(800)
            .pixel_height(600)
            .size_bytes(4096)
            .source_name("after.png")
            .build()
    }

    fn get(target: &str) -> HttpRequest {
        HttpRequest {
            method: "GET".to_owned(),
            target: target.to_owned(),
            host: Some("127.0.0.1:8419".to_owned()),
        }
    }

    fn fixture() -> (tempfile::TempDir, AttachmentStore, FakeCatalog, String) {
        let tmp = tempfile::tempdir().unwrap();
        let store = AttachmentStore::at(tmp.path().join("attachments"));
        let bytes = png_bytes(800, 600);
        let digest = crate::store::digest_hex(&bytes);
        store.write_blob(&digest, AttachmentMediaType::Png, &bytes).unwrap();
        let catalog = FakeCatalog {
            attachments: vec![
                attachment("atc_new", "exec_2", &digest),
                attachment("atc_old", "exec_1", &digest),
            ],
        };
        (tmp, store, catalog, digest)
    }

    #[test]
    fn serves_the_image_bytes() {
        let (_tmp, store, catalog, _digest) = fixture();
        let response = respond(&get("/a/atc_new"), &catalog, &store);
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "image/png");
        assert_eq!(response.body, png_bytes(800, 600));
    }

    #[test]
    fn gallery_groups_by_execution_newest_first() {
        let (_tmp, store, catalog, _digest) = fixture();
        let response = respond(&get("/w/task_1"), &catalog, &store);
        assert_eq!(response.status, 200);
        let html = String::from_utf8(response.body).unwrap();

        let newest = html.find("exec_2").expect("newest run must be rendered");
        let oldest = html.find("exec_1").expect("older run must be rendered");
        assert!(newest < oldest, "newest run heads the page");
        assert!(html.contains("latest run"), "the newest run is labelled");
        assert!(html.contains("src=\"/a/atc_new\""), "images are linked by id");
        assert!(html.contains("Wide table layout"), "the work item name titles the page");
    }

    #[test]
    fn reclaimed_attachment_explains_itself_instead_of_404ing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = AttachmentStore::at(tmp.path().join("attachments"));
        let mut gone = attachment("atc_gone", "exec_1", "deadbeef");
        gone.reclaimed_at = Some("1700086400".to_owned());
        let catalog = FakeCatalog {
            attachments: vec![gone],
        };

        let response = respond(&get("/a/atc_gone"), &catalog, &store);
        assert_eq!(response.status, 410, "Gone, not Not Found");
        let html = String::from_utf8(response.body).unwrap();
        assert!(html.contains("reclaimed by retention"), "{html}");
        assert!(html.contains("exec_1"), "says which run captured it: {html}");
    }

    #[test]
    fn rejects_a_non_loopback_host_header() {
        // DNS rebinding: evil.com resolves to 127.0.0.1, but the browser
        // still sends the attacker's hostname.
        let (_tmp, store, catalog, _digest) = fixture();
        let request = HttpRequest {
            host: Some("evil.example.com:8419".to_owned()),
            ..get("/a/atc_new")
        };
        assert_eq!(respond(&request, &catalog, &store).status, 403);

        let hostless = HttpRequest {
            host: None,
            ..get("/a/atc_new")
        };
        assert_eq!(respond(&hostless, &catalog, &store).status, 403);
    }

    #[test]
    fn rejects_writes_and_traversal() {
        let (_tmp, store, catalog, _digest) = fixture();
        let post = HttpRequest {
            method: "POST".to_owned(),
            ..get("/a/atc_new")
        };
        assert_eq!(respond(&post, &catalog, &store).status, 405);
        assert_eq!(respond(&get("/a/../../etc/passwd"), &catalog, &store).status, 404);
        assert_eq!(respond(&get("/a/%2e%2e%2fsecret"), &catalog, &store).status, 404);
        assert_eq!(respond(&get("/nope"), &catalog, &store).status, 404);
    }

    #[test]
    fn unknown_work_item_says_so() {
        let (_tmp, store, catalog, _digest) = fixture();
        let response = respond(&get("/w/task_absent"), &catalog, &store);
        assert_eq!(response.status, 404);
        assert!(String::from_utf8(response.body).unwrap().contains("task_absent"));
    }

    #[test]
    fn query_strings_do_not_defeat_routing() {
        let (_tmp, store, catalog, _digest) = fixture();
        assert_eq!(respond(&get("/a/atc_new?v=2"), &catalog, &store).status, 200);
    }

    #[test]
    fn captions_and_names_are_escaped() {
        let tmp = tempfile::tempdir().unwrap();
        let store = AttachmentStore::at(tmp.path().join("attachments"));
        let mut hostile = attachment("atc_x", "exec_1", "aa");
        hostile.caption = "<script>alert(1)</script>".to_owned();
        let catalog = FakeCatalog {
            attachments: vec![hostile],
        };
        let html = String::from_utf8(respond(&get("/w/task_1"), &catalog, &store).body).unwrap();
        assert!(!html.contains("<script>"), "worker-supplied text must be escaped");
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn parses_a_request_head() {
        let request = HttpRequest::parse("GET /w/task_1 HTTP/1.1\r\nHost: 127.0.0.1:8419\r\nAccept: */*\r\n\r\n")
            .expect("a normal browser GET must parse");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/w/task_1");
        assert_eq!(request.host.as_deref(), Some("127.0.0.1:8419"));
        assert!(HttpRequest::parse("garbage").is_none());
    }

    #[test]
    fn head_response_omits_the_body_but_keeps_the_length() {
        let (_tmp, store, catalog, _digest) = fixture();
        let response = respond(&get("/a/atc_new"), &catalog, &store);
        let wire = response.to_bytes(false);
        let text = String::from_utf8_lossy(&wire);
        assert!(text.contains(&format!("Content-Length: {}", response.body.len())));
        assert!(!wire.ends_with(&response.body));
    }

    #[test]
    fn responses_never_carry_cors_headers() {
        let (_tmp, store, catalog, _digest) = fixture();
        let wire = respond(&get("/a/atc_new"), &catalog, &store).to_bytes(true);
        let text = String::from_utf8_lossy(&wire);
        assert!(
            !text.to_ascii_lowercase().contains("access-control-allow"),
            "cross-origin script must never be able to read evidence bytes"
        );
        assert!(text.contains("X-Content-Type-Options: nosniff"));
    }

    #[test]
    fn port_config_reads_the_env_override() {
        assert_eq!(base_url(8419), "http://127.0.0.1:8419");
        assert_eq!(gallery_url_for(8419, "task_1"), "http://127.0.0.1:8419/w/task_1");
    }

    #[tokio::test]
    async fn end_to_end_over_a_real_socket() {
        let (_tmp, store, catalog, _digest) = fixture();
        let listener = bind(0).await.expect("loopback bind");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve(listener, Arc::new(catalog), store));

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(format!("GET /a/atc_new HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();

        let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&response[..split]);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("Content-Type: image/png"), "{head}");
        assert_eq!(
            &response[split + 4..],
            png_bytes(800, 600).as_slice(),
            "the bytes a browser receives are the bytes that were stored"
        );
    }
}
