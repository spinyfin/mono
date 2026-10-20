//! Ownership of engine background tasks, including tasks spawned by other tasks.
//!
//! A scope cancels and drains its tasks while Tokio's drivers are still alive.
//! Outside a scope, `spawn` has Tokio's usual behavior (useful for standalone
//! subsystem tests). Blocking work remains subject to the engine's bounded
//! runtime shutdown; it must not start asynchronous engine work of its own.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::sync::Notify;
use tokio::task::{AbortHandle, JoinHandle, JoinSet};

tokio::task_local! {
    static CURRENT: Arc<Shared>;
}

#[derive(Default)]
struct State {
    closed: bool,
    next_id: u64,
    tasks: HashMap<u64, Option<AbortHandle>>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Notify,
}

impl Shared {
    fn cancel(&self) {
        let tasks: Vec<_> = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.tasks.values().filter_map(Clone::clone).collect()
        };
        // Tokio can drop a cancelled task synchronously during runtime teardown.
        // Never hold the registry lock while running task destructors.
        for task in tasks {
            task.abort();
        }
        self.changed.notify_waiters();
    }

    async fn cancelled(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.state.lock().unwrap().closed {
                return;
            }
            changed.await;
        }
    }

    async fn shutdown(&self) {
        self.cancel();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.state.lock().unwrap().tasks.is_empty() {
                return;
            }
            changed.await;
        }
    }
}

/// Cancels tasks even if the caller drops the owning server future early.
/// Normal exits additionally await all task destructors through `run`.
struct Owner(Arc<Shared>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Run one engine lifetime, draining tasks on both successful and error returns.
pub async fn run<F: Future>(future: F) -> F::Output {
    let owner = Owner(Arc::default());
    let output = CURRENT.scope(owner.0.clone(), future).await;
    owner.0.shutdown().await;
    output
}

/// Close the current scope before shutdown bookkeeping begins. New work is
/// cancelled too, including work spawned by a cancelled task's destructor.
/// Call from the scope's owning future, never from one of its spawned tasks.
pub async fn shutdown() {
    if let Ok(shared) = CURRENT.try_with(Arc::clone) {
        shared.shutdown().await;
    }
}

/// Tokio-compatible spawn that inherits the current engine's task ownership.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let Ok(shared) = CURRENT.try_with(Arc::clone) else {
        return tokio::spawn(future);
    };
    register(
        shared.clone(),
        future,
        true,
        |future| tokio::spawn(CURRENT.scope(shared, future)),
        JoinHandle::abort_handle,
    )
}

/// Spawn into a caller-owned join set while retaining engine shutdown ownership.
/// Dropping the set aborts its children; the scope also waits for their cleanup.
pub fn spawn_into<F>(set: &mut JoinSet<F::Output>, future: F) -> AbortHandle
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let Ok(shared) = CURRENT.try_with(Arc::clone) else {
        return set.spawn(future);
    };
    register(
        shared.clone(),
        future,
        true,
        |future| set.spawn(CURRENT.scope(shared, future)),
        Clone::clone,
    )
}

/// Drive a read-handler future on the blocking pool without losing its engine
/// scope. Unlike pure blocking work, these handlers can poll timers and spawn
/// children, so they must cancel and drain before the runtime's drivers stop.
/// Synchronous portions finish their current poll before cancellation is seen.
pub fn spawn_blocking_future<F>(future: F) -> JoinHandle<Option<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let runtime = tokio::runtime::Handle::current();
    let Ok(shared) = CURRENT.try_with(Arc::clone) else {
        return tokio::task::spawn_blocking(move || Some(runtime.block_on(future)));
    };
    let cancellation = shared.clone();
    let future = async move {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            output = future => Some(output),
        }
    };
    register(
        shared.clone(),
        future,
        false,
        |future| tokio::task::spawn_blocking(move || runtime.block_on(CURRENT.scope(shared, future))),
        JoinHandle::abort_handle,
    )
}

fn register<F, H>(
    shared: Arc<Shared>,
    future: F,
    suspend_when_closed: bool,
    spawn: impl FnOnce(Tracked<F>) -> H,
    abort_handle: impl FnOnce(&H) -> AbortHandle,
) -> H {
    // Reserve before spawning so shutdown also waits for registrations in
    // flight. Spawning into a closing runtime can synchronously drop the future,
    // so both spawn and abort must happen outside the registry lock.
    let id = {
        let mut state = shared.state.lock().unwrap();
        let id = state.next_id;
        state.next_id += 1;
        state.tasks.insert(id, None);
        id
    };
    let task = spawn(Tracked {
        future: Some(Box::pin(future)),
        shared: shared.clone(),
        id,
        suspend_when_closed,
    });
    let abort_handle = abort_handle(&task);
    let closed = {
        let mut state = shared.state.lock().unwrap();
        // A fast task may already have completed and removed its reservation.
        if let Some(entry) = state.tasks.get_mut(&id) {
            *entry = Some(abort_handle.clone());
        }
        state.closed
    };
    if closed {
        abort_handle.abort();
    }
    task
}

struct Tracked<F> {
    future: Option<Pin<Box<F>>>,
    shared: Arc<Shared>,
    id: u64,
    suspend_when_closed: bool,
}

impl<F: Future> Future for Tracked<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // A spawn racing cancellation must never start new engine work.
        if self.suspend_when_closed && self.shared.state.lock().unwrap().closed {
            return Poll::Pending;
        }
        self.future.as_mut().unwrap().as_mut().poll(cx)
    }
}

impl<F> Drop for Tracked<F> {
    fn drop(&mut self) {
        struct Complete<'a>(&'a Shared, u64);
        impl Drop for Complete<'_> {
            fn drop(&mut self) {
                self.0.state.lock().unwrap().tasks.remove(&self.1);
                self.0.changed.notify_waiters();
            }
        }
        let _complete = Complete(&self.shared, self.id);
        // Finish user destructors before advertising that this task drained.
        // Those destructors may themselves spawn tasks into the closed scope.
        // Completion must also be recorded if one of them panics.
        CURRENT.sync_scope(self.shared.clone(), || drop(self.future.take()));
    }
}

#[cfg(test)]
mod tests;
