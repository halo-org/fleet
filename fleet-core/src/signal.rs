use alloc::sync::Arc;
use core::{
    future,
    sync::atomic::{AtomicU8, Ordering},
    task::Poll,
};
use futures_util::task::AtomicWaker;

/// Creates a signal channel, returning the sender and receiver.
pub fn signal_channel() -> (SignalSender, SignalReceiver) {
    let state = Arc::new(ChannelState {
        waker: AtomicWaker::new(),
        pending: AtomicU8::new(0),
    });

    let sender = SignalSender {
        state: state.clone(),
    };
    let receiver = SignalReceiver { state };

    (sender, receiver)
}

/// The sending half of a [signal channel](signal_channel).
#[derive(Clone)]
pub struct SignalSender {
    state: Arc<ChannelState>,
}

/// The receiving half of a [signal channel](signal_channel).
pub struct SignalReceiver {
    state: Arc<ChannelState>,
}

struct ChannelState {
    waker: AtomicWaker,
    // 0 = none, 1 = Check, 2 = Recheck, 4 = Stop.
    pending: AtomicU8,
}

/// A signal for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    /// The task should check for new work after any ongoing work finishes.
    Check,
    /// The task should cancel ongoing work and check for new work.
    Recheck,
    /// The task should cancel ongoing work and shut down.
    Stop,
}

/// A priority signal for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrioritySignal {
    /// The task should cancel ongoing work and check for new work.
    Recheck,
    /// The task should cancel ongoing work and shut down.
    Stop,
}

impl SignalSender {
    /// Sends a signal.
    pub fn send(&self, signal: Signal) {
        self.state.pending.update(
            Ordering::AcqRel,
            Ordering::Acquire,
            move |pending| match signal {
                Signal::Check if pending == 2 => 2,
                Signal::Check if pending == 4 => 2,
                Signal::Check => 1,
                Signal::Recheck => 2,
                Signal::Stop => 4,
            },
        );

        self.state.waker.wake();
    }
}

#[expect(clippy::needless_pass_by_ref_mut, reason = "prevents concurrent usage")]
impl SignalReceiver {
    /// Waits for the next signal.
    pub fn signal(&mut self) -> impl Future<Output = Signal> {
        future::poll_fn(|cx| {
            self.state.waker.register(cx.waker());
            self.take_signal().map_or(Poll::Pending, Poll::Ready)
        })
    }

    /// Waits for the next priority signal.
    pub fn priority_signal(&mut self) -> impl Future<Output = PrioritySignal> {
        future::poll_fn(|cx| {
            self.state.waker.register(cx.waker());
            self.take_priority_signal()
                .map_or(Poll::Pending, Poll::Ready)
        })
    }

    /// Consumes any existing signal without waiting.
    pub fn take_signal(&mut self) -> Option<Signal> {
        match self.state.pending.swap(0, Ordering::AcqRel) {
            0 => None,
            1 => Some(Signal::Check),
            2 => Some(Signal::Recheck),
            4 => Some(Signal::Stop),
            _ => unreachable!("pending is never set to any other value"),
        }
    }

    /// Consumes any existing priority signal without waiting.
    fn take_priority_signal(&mut self) -> Option<PrioritySignal> {
        match self.state.pending.fetch_and(1, Ordering::AcqRel) {
            0 | 1 => None,
            2 => Some(PrioritySignal::Recheck),
            4 => Some(PrioritySignal::Stop),
            _ => unreachable!("pending is never set to any other value"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        sync::atomic::AtomicU32,
        task::{Context, Poll},
    };
    use futures_util::{FutureExt as _, task::ArcWake};

    struct CountingWake {
        count: AtomicU32,
    }

    impl CountingWake {
        fn new() -> Self {
            Self {
                count: AtomicU32::new(0),
            }
        }

        #[track_caller]
        fn assert_wake_count(&self, expected: u32) {
            assert_eq!(self.count.load(Ordering::Relaxed), expected);
        }
    }

    impl ArcWake for CountingWake {
        fn wake_by_ref(this: &Arc<Self>) {
            this.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn take_signal_returns_none_when_empty() {
        let (_, mut receiver) = signal_channel();
        assert_eq!(receiver.take_signal(), None);
    }

    #[test]
    fn take_priority_signal_returns_none_when_empty() {
        let (_, mut receiver) = signal_channel();
        assert_eq!(receiver.take_priority_signal(), None);
    }

    #[test]
    fn take_priority_signal_preserves_check() {
        let (sender, mut receiver) = signal_channel();

        sender.send(Signal::Check);
        assert_eq!(receiver.take_priority_signal(), None);
        assert_eq!(receiver.take_signal(), Some(Signal::Check));
    }

    #[test]
    fn take_signal_returns_and_clears_pending_signal() {
        let signals = [Signal::Check, Signal::Recheck, Signal::Stop];

        for signal in signals {
            let (sender, mut receiver) = signal_channel();

            sender.send(signal);
            assert_eq!(receiver.take_signal(), Some(signal));
            assert_eq!(receiver.take_signal(), None);
        }
    }

    #[test]
    fn take_priority_signal_returns_and_clears_pending_signal() {
        let signals = [
            (Signal::Check, None),
            (Signal::Recheck, Some(PrioritySignal::Recheck)),
            (Signal::Stop, Some(PrioritySignal::Stop)),
        ];

        for (signal, expected) in signals {
            let (sender, mut receiver) = signal_channel();

            sender.send(signal);
            assert_eq!(
                receiver.take_priority_signal(),
                expected,
                "signal: {signal:?}",
            );
            assert_eq!(receiver.take_priority_signal(), None, "signal: {signal:?}");
        }
    }

    #[test]
    fn send_coalesces_pending_signals() {
        use Signal::{Check, Recheck, Stop};

        let signals = [Check, Recheck, Stop];

        let cases = [
            (None, [Check, Recheck, Stop]),
            (Some(Check), [Check, Recheck, Stop]),
            (Some(Recheck), [Recheck, Recheck, Stop]),
            (Some(Stop), [Recheck, Recheck, Stop]),
        ];

        for (seed, expected) in cases {
            for (signal, expected) in signals.into_iter().zip(expected) {
                let (sender, mut receiver) = signal_channel();

                if let Some(seed) = seed {
                    sender.send(seed);
                }

                sender.send(signal);
                assert_eq!(
                    receiver.take_signal(),
                    Some(expected),
                    "seed: {seed:?}, signal: {signal:?}",
                );
            }
        }
    }

    #[test]
    fn send_wakes_signal_waiter() {
        let wake = Arc::new(CountingWake::new());
        let waker = futures_util::task::waker_ref(&wake);
        let mut cx = Context::from_waker(&waker);

        let (sender, mut receiver) = signal_channel();
        let mut future = receiver.signal();

        assert_eq!(future.poll_unpin(&mut cx), Poll::Pending);
        wake.assert_wake_count(0);

        sender.send(Signal::Stop);
        wake.assert_wake_count(1);
        assert_eq!(future.poll_unpin(&mut cx), Poll::Ready(Signal::Stop));
    }

    #[test]
    fn send_wakes_priority_signal_waiter() {
        let wake = Arc::new(CountingWake::new());
        let waker = futures_util::task::waker_ref(&wake);
        let mut cx = Context::from_waker(&waker);

        let (sender, mut receiver) = signal_channel();
        let mut future = receiver.priority_signal();

        assert_eq!(future.poll_unpin(&mut cx), Poll::Pending);
        wake.assert_wake_count(0);

        sender.send(Signal::Stop);
        wake.assert_wake_count(1);
        assert_eq!(
            future.poll_unpin(&mut cx),
            Poll::Ready(PrioritySignal::Stop),
        );
    }
}
