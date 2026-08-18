//! Engine wiring for the evidence HTTP surface: the database-backed
//! [`EvidenceCatalog`] and the bind/spawn the server loop hangs off.
//!
//! The surface itself (routing, rendering, the loopback and `Host`-header
//! gates) lives in `boss_engine_attachments::http`, which knows nothing about
//! `WorkDb`. This module is the one-directional edge between them.
//!
//! ## Bind failure is not a boot failure
//!
//! If the port is taken — most often by a second engine, e.g. an operator's
//! production engine while a `--socket-path` fixture starts — the surface
//! stays down and the engine boots normally. Everything else about
//! attachments keeps working: ingest still validates and stores, listings
//! still work, retention still sweeps. What a worker gets in that case is
//! `evidence_base_url: None` and an explicit "no gallery link", which is the
//! honest answer. The alternative — minting a URL against a port this engine
//! does not own — would hand a local browser someone else's gallery.

use std::sync::{Arc, OnceLock};

use boss_engine_attachments::http::{self, EvidenceCatalog, WorkItemLabel};
use boss_engine_attachments::store::AttachmentStore;
use boss_protocol::WorkAttachment;

use crate::work::WorkDb;

/// [`EvidenceCatalog`] over the work database.
pub struct WorkDbEvidenceCatalog {
    work_db: Arc<WorkDb>,
}

impl WorkDbEvidenceCatalog {
    pub fn new(work_db: Arc<WorkDb>) -> Self {
        Self { work_db }
    }
}

impl EvidenceCatalog for WorkDbEvidenceCatalog {
    fn attachment(&self, attachment_id: &str) -> Option<WorkAttachment> {
        match self.work_db.get_work_attachment(attachment_id) {
            Ok(found) => found,
            Err(err) => {
                tracing::warn!(attachment_id, ?err, "evidence server: attachment lookup failed");
                None
            }
        }
    }

    fn attachments_for_work_item(&self, work_item_id: &str) -> Vec<WorkAttachment> {
        match self.work_db.list_work_attachments_for_work_item(work_item_id) {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(work_item_id, ?err, "evidence server: gallery lookup failed");
                Vec::new()
            }
        }
    }

    fn work_item_label(&self, work_item_id: &str) -> Option<WorkItemLabel> {
        // Attachments hang off executions, whose work items are always
        // tasks/chores/revisions — but the header is cosmetic, so a
        // project/product id resolves to a name with no PR rather than
        // failing the page.
        Some(match self.work_db.get_work_item(work_item_id).ok()? {
            boss_protocol::WorkItem::Task(task) | boss_protocol::WorkItem::Chore(task) => WorkItemLabel {
                name: task.name,
                pr_url: task.pr_url,
            },
            boss_protocol::WorkItem::Project(project) => WorkItemLabel {
                name: project.name,
                pr_url: None,
            },
            boss_protocol::WorkItem::Product(product) => WorkItemLabel {
                name: product.name,
                pr_url: None,
            },
        })
    }
}

/// Bind the evidence surface and spawn its accept loop.
///
/// On success `evidence_port` is set to the port actually bound, which is
/// what unblocks URL minting (see `ServerState::evidence_base_url`). Returns
/// `None` — after logging why — when the surface is disabled or the bind
/// fails; the caller carries on regardless.
pub async fn spawn(
    work_db: Arc<WorkDb>,
    store: AttachmentStore,
    evidence_port: Arc<OnceLock<u16>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let Some(port) = http::configured_port() else {
        tracing::info!(
            "attachment evidence server: disabled ({}=0); attachments will still be stored, but \
             no local gallery URL will be minted",
            http::PORT_ENV
        );
        return None;
    };

    let listener = match http::bind(port).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!(
                port,
                error = %err,
                "attachment evidence server: bind failed; evidence is still stored and \
                 inspectable via `bossctl attachments list`, but no gallery URL will be minted \
                 (another engine may already hold the port — set {} to move it)",
                http::PORT_ENV,
            );
            return None;
        }
    };

    let bound = listener.local_addr().ok().map(|addr| addr.port()).unwrap_or(port);
    // `set` can only fail if something already bound, which cannot happen —
    // this runs once per engine — but a lost race must not silently publish
    // the wrong port, so it is reported rather than ignored.
    if evidence_port.set(bound).is_err() {
        tracing::warn!(bound, "attachment evidence server: port was already published");
    }
    tracing::info!(
        url = %http::base_url(bound),
        "attachment evidence server: listening on loopback",
    );

    let catalog: Arc<dyn EvidenceCatalog> = Arc::new(WorkDbEvidenceCatalog::new(work_db));
    Some(tokio::spawn(http::serve(listener, catalog, store)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::work::SubmitAttachmentInput;
    use boss_engine_attachments::IngestedImage;
    use boss_protocol::AttachmentMediaType;

    #[test]
    fn catalog_reads_rows_and_labels_from_the_database() {
        let (_dir, db) = open_db();
        let product = create_test_product_with_repo(&db, "p", Some("https://github.com/test/repo"));
        let chore = create_test_chore(&db, &product.id, "evidence-chore");
        let execution = create_ready_chore_execution(&db, &chore.id);
        let stored = db
            .submit_work_attachment(SubmitAttachmentInput {
                execution_id: &execution.id,
                work_item_id: &chore.id,
                image: &IngestedImage {
                    content_digest: "aa".to_owned(),
                    media_type: AttachmentMediaType::Png,
                    pixel_width: 100,
                    pixel_height: 80,
                    size_bytes: 512,
                    source_name: "shot.png".to_owned(),
                },
                caption: "the fix",
            })
            .unwrap()
            .unwrap();

        let catalog = WorkDbEvidenceCatalog::new(Arc::new(db));
        assert_eq!(
            catalog.attachment(&stored.attachment.id).map(|a| a.caption),
            Some("the fix".to_owned())
        );
        assert_eq!(catalog.attachments_for_work_item(&chore.id).len(), 1);
        assert_eq!(
            catalog.work_item_label(&chore.id).map(|l| l.name),
            Some("evidence-chore".to_owned())
        );
        assert!(catalog.attachment("atc_absent").is_none());
        assert!(catalog.attachments_for_work_item("task_absent").is_empty());
    }

    /// The reviewer's path, end to end and over a real socket: a PNG on disk
    /// is ingested exactly as `SubmitAttachment` ingests it, recorded in the
    /// database, and then fetched back through the HTTP surface — first the
    /// gallery page a PR-body link points at, then the image it embeds.
    ///
    /// This is the claim the whole feature rests on ("a mechanism that stores
    /// images but leaves the reviewer unable to see them has not solved
    /// anything"), so it is asserted against the bytes a browser would
    /// actually receive rather than against any intermediate layer.
    #[tokio::test]
    async fn a_reviewer_can_fetch_a_stored_screenshot_over_http() {
        use boss_engine_attachments::AllowedRoots;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = AttachmentStore::at(tmp.path().join("attachments"));

        // A PNG the "worker" rendered into its workspace.
        let mut png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        let rendered = workspace.join("after.png");
        std::fs::write(&rendered, &png).unwrap();

        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "Wide table layout");
        let execution = create_ready_chore_execution(&db, &chore.id);

        let image = store
            .ingest(&rendered, &AllowedRoots::from_paths([workspace.clone()]))
            .expect("a workspace render must ingest");
        let stored = db
            .submit_work_attachment(SubmitAttachmentInput {
                execution_id: &execution.id,
                work_item_id: &chore.id,
                image: &image,
                caption: "wide table, after the fix",
            })
            .unwrap()
            .unwrap();

        // The workspace is released and recycled after a run; the evidence
        // must not depend on it.
        std::fs::remove_dir_all(&workspace).unwrap();

        let db = Arc::new(db);
        let listener = http::bind(0).await.expect("loopback bind");
        let port = listener.local_addr().unwrap().port();
        let catalog: Arc<dyn EvidenceCatalog> = Arc::new(WorkDbEvidenceCatalog::new(db));
        tokio::spawn(http::serve(listener, catalog, store));

        async fn fetch(port: u16, path: &str) -> (String, Vec<u8>) {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream
                .write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).await.unwrap();
            let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            (
                String::from_utf8_lossy(&raw[..split]).into_owned(),
                raw[split + 4..].to_vec(),
            )
        }

        // 1. The loopback gallery URL for a work item (local verification).
        let (head, body) = fetch(port, &boss_protocol::attachment_gallery_path(&chore.id)).await;
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        let html = String::from_utf8(body).unwrap();
        assert!(
            html.contains("Wide table layout"),
            "the work item titles the page: {html}"
        );
        assert!(
            html.contains("wide table, after the fix"),
            "the caption renders: {html}"
        );
        assert!(html.contains(&execution.id), "the producing run is named: {html}");
        let image_path = boss_protocol::attachment_image_path(&stored.attachment.id);
        assert!(html.contains(&format!("src=\"{image_path}\"")), "{html}");

        // 2. The image the page embeds — byte-for-byte what was rendered.
        let (head, bytes) = fetch(port, &image_path).await;
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("Content-Type: image/png"), "{head}");
        assert_eq!(bytes, png, "the reviewer sees exactly the bytes the worker rendered");
    }
}
