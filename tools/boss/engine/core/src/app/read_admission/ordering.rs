//! Reserve FIFO turns in the reader loop, before spawned tasks can reorder.
//!
//! A turn is keyed by the full request (kind and parameters): only identical
//! reads can deliver snapshots of the same view that overwrite each other, so
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
        let key = serde_json::to_string(request).unwrap_or_default();
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
