//! Request view identity for admission diagnostics and response freshness.
use boss_protocol::FrontendRequest;
/// Opaque identity of the request's response view, for diagnostics.
pub(in crate::app) fn key_id(request: &FrontendRequest) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key(request).hash(&mut hasher);
    hasher.finish()
}

pub(in crate::app) fn key(request: &FrontendRequest) -> String {
    // Refresh changes how a listing is loaded, not its destination view.
    match request {
        FrontendRequest::GetDriverQuotaUsage { .. } => {
            serde_json::to_string(&FrontendRequest::GetDriverQuotaUsage { refresh: false })
        }
        FrontendRequest::ListProductDesignDocs { product_id, .. } => {
            serde_json::to_string(&FrontendRequest::ListProductDesignDocs {
                product_id: product_id.clone(),
                refresh: false,
            })
        }
        // Sequence ids and snapshot/delta encoding do not change the product view.
        FrontendRequest::GetWorkTree { product_id, .. } => serde_json::to_string(&FrontendRequest::GetWorkTree {
            product_id: product_id.clone(),
            fetch_seq: None,
            delta: false,
        }),
        _ => serde_json::to_string(request),
    }
    .expect("serializable request")
}
