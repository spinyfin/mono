//! Awaiting a large future without spending the caller's stack on it.
//!
//! An `async fn` compiles to one `poll` function, and in an unoptimised build
//! that function's stack frame has a separate slot for every temporary in its
//! body. `child().await` has two such temporaries the size of `child`'s
//! future: the value the call returns, and the copy handed to `IntoFuture`
//! before it is moved into the parent's own state. A body with sixty awaits
//! therefore reserves roughly twice the sum of sixty futures on every poll,
//! whichever single branch it takes.
//!
//! `Box::pin(child()).await` removes one of the two: the parent's state now
//! holds a pointer, but `child()` is still returned into the caller's frame
//! before `Box::pin` moves it to the heap. Large callees can therefore consume
//! most of a thread's stack even when the parent stores only boxed futures.
//!
//! [`on_heap`] takes a closure that makes the future, so the future is built
//! in a frame that has returned before anything is polled.

use std::future::Future;
use std::pin::Pin;

/// Build the future `make` returns directly into a heap allocation and return
/// it pinned, ready to await.
///
/// Write `on_heap(|| self.step(&input)).await` where `self.step(&input).await`
/// or `Box::pin(self.step(&input)).await` would put a large future in the
/// caller's stack frame. The closure only borrows what the call needs, and
/// the future it returns may keep those borrows, so arguments built in place
/// (`&format!(..)`) have to be bound to a `let` first.
///
/// Never inlined: the point is that the future's only stack copy lives in
/// this function's frame, which is gone by the time the caller polls.
#[inline(never)]
pub fn on_heap<F, Fut>(make: F) -> Pin<Box<Fut>>
where
    F: FnOnce() -> Fut,
    Fut: Future,
{
    Box::pin(make())
}

/// Address of a local in this helper's own frame, as a stand-in for the
/// stack pointer in tests and diagnostics.
///
/// Two readings on one thread differ by the stack in use between the two
/// calls (the stack grows down, so the deeper reading is the smaller one).
/// That is the only meaningful use: a guard test takes one reading where a
/// call chain starts and another inside a collaborator the chain reaches, and
/// asserts on the difference.
#[inline(never)]
pub fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&raw const marker) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    const PAYLOAD: usize = 32 * 1024;
    const AWAITS: usize = 10;

    /// Records how deep the stack is when it is polled.
    struct RecordDepth<'a>(&'a AtomicUsize);

    impl Future for RecordDepth<'_> {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            self.0.store(stack_position(), Ordering::Relaxed);
            Poll::Ready(())
        }
    }

    /// A future that is `PAYLOAD` bytes because it holds a buffer across its
    /// one await.
    async fn large_child(deepest: &AtomicUsize, fill: u8) -> u8 {
        let payload = [fill; PAYLOAD];
        RecordDepth(deepest).await;
        std::hint::black_box(&payload)[PAYLOAD - 1]
    }

    async fn parent_awaiting_on_heap(deepest: &AtomicUsize) -> usize {
        let mut total = 0usize;
        total += usize::from(on_heap(|| large_child(deepest, 1)).await);
        total += usize::from(on_heap(|| large_child(deepest, 2)).await);
        total += usize::from(on_heap(|| large_child(deepest, 3)).await);
        total += usize::from(on_heap(|| large_child(deepest, 4)).await);
        total += usize::from(on_heap(|| large_child(deepest, 5)).await);
        total += usize::from(on_heap(|| large_child(deepest, 6)).await);
        total += usize::from(on_heap(|| large_child(deepest, 7)).await);
        total += usize::from(on_heap(|| large_child(deepest, 8)).await);
        total += usize::from(on_heap(|| large_child(deepest, 9)).await);
        total += usize::from(on_heap(|| large_child(deepest, 10)).await);
        total
    }

    #[test]
    fn a_future_awaited_on_the_heap_is_not_in_the_parents_state() {
        let deepest = AtomicUsize::new(0);
        assert!(std::mem::size_of_val(&large_child(&deepest, 0)) >= PAYLOAD);
        let parent = parent_awaiting_on_heap(&deepest);
        assert!(
            std::mem::size_of_val(&parent) < 256,
            "the parent holds a pointer per await, not the child: {} bytes",
            std::mem::size_of_val(&parent),
        );
    }

    /// The property the helper exists for, and the one `Box::pin(child())`
    /// at the await does not have: the parent's poll frame does not grow with
    /// the size or number of the futures it awaits.
    #[test]
    fn awaiting_on_the_heap_keeps_the_child_out_of_the_parents_stack_frame() {
        let deepest = AtomicUsize::new(0);
        let mut parent = Box::pin(parent_awaiting_on_heap(&deepest));
        let start = stack_position();
        let mut cx = Context::from_waker(Waker::noop());
        let Poll::Ready(total) = parent.as_mut().poll(&mut cx) else {
            panic!("every child is ready on its first poll");
        };
        assert_eq!(total, (1..=AWAITS).sum::<usize>());
        let depth = start - deepest.load(Ordering::Relaxed);
        assert!(
            depth < PAYLOAD,
            "{AWAITS} awaits of a {PAYLOAD}-byte future put the last child's poll {depth} bytes \
             below the caller; one copy of one child in the parent's frame would already be {PAYLOAD}",
        );
    }
}
