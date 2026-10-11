//! Reserve FIFO turns in the reader loop, before spawned tasks can reorder.
use boss_protocol::FrontendRequest;
use std::{
    collections::HashMap,
    future::{Future, poll_fn},
    pin::Pin,
    sync::Arc,
    task::Poll,
};
use tokio::sync::{Mutex, OwnedMutexGuard};
pub(super) type Turn = Pin<Box<dyn Future<Output = OwnedMutexGuard<()>> + Send>>;
#[derive(Default)]
pub(in crate::app) struct ReadOrdering {
    kinds: HashMap<std::mem::Discriminant<FrontendRequest>, Arc<Mutex<()>>>,
}
impl ReadOrdering {
    pub(in crate::app) async fn reserve(&mut self, request: &FrontendRequest) -> Turn {
        let mutex = self.kinds.entry(std::mem::discriminant(request)).or_default().clone();
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
