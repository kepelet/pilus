//! A one-slot "events arrived" signal from the connection threads to the GUI.
//!
//! The coalescer signals after it hands a burst to the window's queue; the
//! window's tick loop awaits [`Arrival::wait`] alongside its own timer, so a
//! frame is drained the moment it lands instead of on the next 16ms tick.
//! Signals coalesce: any number before a wait resolve it once, and the window
//! drains every queue each time it wakes.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct State {
    pending: bool,
    waker: Option<Waker>,
}

/// The shared signal. Cheap to clone; every clone is the same slot.
#[derive(Clone, Default)]
pub struct Arrival(Arc<Mutex<State>>);

impl Arrival {
    /// The process-wide signal every connection reports into.
    pub fn global() -> &'static Arrival {
        static GLOBAL: OnceLock<Arrival> = OnceLock::new();
        GLOBAL.get_or_init(Arrival::default)
    }

    /// Mark that events are waiting and wake the awaiting task, if any.
    pub fn signal(&self) {
        let waker = {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            state.pending = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Resolve once a signal has landed since the last resolved wait.
    pub fn wait(&self) -> Wait {
        Wait(self.clone())
    }
}

/// The future returned by [`Arrival::wait`].
pub struct Wait(Arrival);

impl Future for Wait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = (self.0).0.lock().unwrap_or_else(|e| e.into_inner());
        if state.pending {
            state.pending = false;
            Poll::Ready(())
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll_once(wait: &mut Wait, counter: &Arc<Counter>) -> Poll<()> {
        let waker = Waker::from(Arc::clone(counter));
        Pin::new(wait).poll(&mut Context::from_waker(&waker))
    }

    #[test]
    fn a_signal_before_the_wait_resolves_it_once() {
        let arrival = Arrival::default();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        arrival.signal();
        arrival.signal();
        let mut wait = arrival.wait();
        assert_eq!(poll_once(&mut wait, &counter), Poll::Ready(()));
        assert_eq!(poll_once(&mut wait, &counter), Poll::Pending);
    }

    #[test]
    fn a_pending_wait_is_woken_by_the_signal() {
        let arrival = Arrival::default();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let mut wait = arrival.wait();
        assert_eq!(poll_once(&mut wait, &counter), Poll::Pending);
        arrival.signal();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(poll_once(&mut wait, &counter), Poll::Ready(()));
    }
}
