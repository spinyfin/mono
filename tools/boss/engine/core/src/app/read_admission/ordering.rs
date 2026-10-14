//! Reserve FIFO turns in the reader loop, before spawned tasks can reorder.
//!
//! A turn is keyed by the response view, excluding execution options, so
//! a slow read never delays a read of a different key.
use boss_protocol::FrontendRequest;
use std::{
    collections::HashMap,
    future::{Future, poll_fn},
    pin::Pin,
    sync::{Arc, Weak},
    task::Poll,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

pub(super) type Turn = Pin<Box<dyn Future<Output = OwnedMutexGuard<()>> + Send>>;

#[derive(Default)]
pub(in crate::app) struct ReadOrdering {
    // Weak so a key with no outstanding turn is dropped, not retained forever.
    keys: HashMap<String, Weak<Mutex<()>>>,
}

impl ReadOrdering {
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.keys.len()
    }

    pub(in crate::app) async fn reserve(&mut self, request: &FrontendRequest) -> Turn {
        self.keys.retain(|_, mutex| mutex.strong_count() > 0);
        // Refresh changes how a listing is loaded, not its destination view.
        let key = match request {
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
        .expect("serializable request");
        let mutex = match self.keys.get(&key).and_then(Weak::upgrade) {
            Some(mutex) => mutex,
            None => {
                let mutex = Arc::new(Mutex::new(()));
                self.keys.insert(key, Arc::downgrade(&mutex));
                mutex
            }
        };
        let mut turn = Box::pin(mutex.lock_owned());
        // Tokio locks queue on first poll, not when the future is created.
        let ready = poll_fn(|cx| Poll::Ready(turn.as_mut().poll(cx))).await;
        Box::pin(async move {
            match ready {
                Poll::Ready(guard) => guard,
                Poll::Pending => turn.await,
            }
        })
    }
}
