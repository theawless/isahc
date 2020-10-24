//! Helpers for working with tasks and futures.

use crossbeam_utils::sync::{Parker, Unparker};
use futures_util::{pin_mut, task::ArcWake};
use std::{
    future::Future,
    sync::Arc,
    task::{Context, Poll, Waker},
};

/// Extension trait for efficiently blocking on a future.
pub(crate) trait Join: Future {
    fn join(self) -> <Self as Future>::Output;
}

impl<F: Future> Join for F {
    fn join(self) -> <Self as Future>::Output {
        struct ThreadWaker(Unparker);

        impl ArcWake for ThreadWaker {
            fn wake_by_ref(arc_self: &Arc<Self>) {
                arc_self.0.unpark();
            }
        }

        let parker = Parker::new();
        let waker = futures_util::task::waker(Arc::new(ThreadWaker(parker.unparker().clone())));
        let mut context = Context::from_waker(&waker);

        let future = self;
        pin_mut!(future);

        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => parker.park(),
            }
        }
    }
}

/// Create a waker from a closure.
fn waker_fn(f: impl Fn() + Send + Sync + 'static) -> Waker {
    struct Impl<F>(F);

    impl<F: Fn() + Send + Sync + 'static> ArcWake for Impl<F> {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            (&arc_self.0)()
        }
    }

    futures_util::task::waker(Arc::new(Impl(f)))
}

/// Helper methods for working with wakers.
pub(crate) trait WakerExt {
    /// Create a new waker from a closure that accepts this waker as an
    /// argument.
    fn chain(&self, f: impl Fn(&Waker) + Send + Sync + 'static) -> Waker;
}

impl WakerExt for Waker {
    fn chain(&self, f: impl Fn(&Waker) + Send + Sync + 'static) -> Waker {
        let inner = self.clone();
        waker_fn(move || (f)(&inner))
    }
}
