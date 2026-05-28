use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use futures::task::AtomicWaker;

/// Single-core doorbell: senders ring it, the core event loop awaits it.
struct Inner {
    pending: AtomicBool,
    waker: AtomicWaker,
}

/// Held by the core loop: used to clear the flag and await the next ring.
pub struct Doorbell {
    inner: Arc<Inner>,
}

/// Held by senders: cloned into every channel that targets this core.
#[derive(Clone)]
pub struct DoorbellHandle {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for DoorbellHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DoorbellHandle")
    }
}

impl Doorbell {
    #[must_use]
    pub fn new() -> (Self, DoorbellHandle) {
        let inner = Arc::new(Inner {
            pending: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        });
        (
            Self {
                inner: inner.clone(),
            },
            DoorbellHandle { inner },
        )
    }

    /// Clear the pending flag. Call at the START of each drain cycle,
    /// BEFORE draining channels. This way, any `ring()` that lands
    /// during the drain sets the flag, and the next `wait()` returns immediately.
    pub fn clear(&self) {
        self.inner.pending.store(false, Ordering::Release);
    }

    /// Await the next ring. Must call `clear()` before entering the wait.
    pub async fn wait(&self) {
        DoorbellWait {
            inner: self.inner.clone(),
        }
        .await;
    }
}

impl DoorbellHandle {
    /// Ring the doorbell. Called from any thread.
    pub fn ring(&self) {
        self.inner.pending.store(true, Ordering::Release);
        self.inner.waker.wake();
    }
}

struct DoorbellWait {
    inner: Arc<Inner>,
}

impl std::future::Future for DoorbellWait {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // Fast path: already rung.
        if self.inner.pending.load(Ordering::Acquire) {
            return std::task::Poll::Ready(());
        }
        // Register waker BEFORE re-checking (avoid lost wakeup).
        self.inner.waker.register(cx.waker());
        if self.inner.pending.load(Ordering::Acquire) {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }
}
