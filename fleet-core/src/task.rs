use alloc::sync::Arc;
use core::{
    future,
    sync::atomic::{AtomicU8, Ordering},
    task::Poll,
};
use futures_util::task::AtomicWaker;

/// Creates a new task channel, returning the sender and receiver halves.
pub fn channel() -> (Sender, Receiver) {
    let state = Arc::new(ChannelState {
        waker: AtomicWaker::new(),
        pending: AtomicU8::new(0),
    });

    let sender = Sender {
        state: state.clone(),
    };
    let receiver = Receiver { state };

    (sender, receiver)
}

/// The sending half of a task [channel].
#[derive(Clone)]
pub struct Sender {
    state: Arc<ChannelState>,
}

/// The receiving half of a task [channel].
pub struct Receiver {
    state: Arc<ChannelState>,
}

struct ChannelState {
    waker: AtomicWaker,
    // 0 = none, 1 = Check, 2 = Recheck, 4 = Stop.
    pending: AtomicU8,
}

/// A notification for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Notification {
    /// The task should check for new work after any ongoing work finishes.
    Check,
    /// The task should cancel ongoing work and check for new work.
    Recheck,
    /// The task should cancel ongoing work and shut down.
    Stop,
}

/// A priority notification for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PriorityNotification {
    /// The task should cancel ongoing work and check for new work.
    Recheck,
    /// The task should cancel ongoing work and shut down.
    Stop,
}

impl Sender {
    /// Sends a notification.
    pub fn send(&self, notification: Notification) {
        self.state
            .pending
            .update(
                Ordering::AcqRel,
                Ordering::Acquire,
                move |pending| match notification {
                    Notification::Check if pending == 2 => 2,
                    Notification::Check if pending == 4 => 2,
                    Notification::Check => 1,
                    Notification::Recheck => 2,
                    Notification::Stop => 4,
                },
            );

        self.state.waker.wake();
    }
}

#[expect(clippy::needless_pass_by_ref_mut, reason = "prevents concurrent usage")]
impl Receiver {
    /// Waits for the next notification.
    pub fn notification(&mut self) -> impl Future<Output = Notification> {
        future::poll_fn(|cx| {
            self.state.waker.register(cx.waker());
            self.take_notification().map_or(Poll::Pending, Poll::Ready)
        })
    }

    /// Waits for the next priority notification.
    pub fn priority_notification(&mut self) -> impl Future<Output = PriorityNotification> {
        future::poll_fn(|cx| {
            self.state.waker.register(cx.waker());
            self.take_priority_notification()
                .map_or(Poll::Pending, Poll::Ready)
        })
    }

    /// Consumes any existing notification without waiting.
    pub fn take_notification(&mut self) -> Option<Notification> {
        match self.state.pending.swap(0, Ordering::AcqRel) {
            0 => None,
            1 => Some(Notification::Check),
            2 => Some(Notification::Recheck),
            4 => Some(Notification::Stop),
            _ => unreachable!("pending is never set to any other value"),
        }
    }

    /// Consumes any existing priority notification without waiting.
    fn take_priority_notification(&mut self) -> Option<PriorityNotification> {
        match self.state.pending.fetch_and(1, Ordering::AcqRel) {
            0 | 1 => None,
            2 => Some(PriorityNotification::Recheck),
            4 => Some(PriorityNotification::Stop),
            _ => unreachable!("pending is never set to any other value"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        sync::atomic::AtomicBool,
        task::{Context, Poll},
    };
    use futures_util::{FutureExt as _, task::ArcWake};

    #[derive(Default)]
    struct WakeFlag(AtomicBool);

    impl WakeFlag {
        #[track_caller]
        fn assert_woken(&self) {
            assert!(self.0.load(Ordering::Relaxed));
        }

        #[track_caller]
        fn assert_not_woken(&self) {
            assert!(!self.0.load(Ordering::Relaxed));
        }
    }

    impl ArcWake for WakeFlag {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn take_notification_from_empty_channel() {
        let (_, mut receiver) = channel();
        assert_eq!(receiver.take_notification(), None);
    }

    #[test]
    fn take_priority_notification_from_empty_channel() {
        let (_, mut receiver) = channel();
        assert_eq!(receiver.take_priority_notification(), None);
    }

    #[test]
    fn take_priority_notification_from_channel_preserves_check() {
        let (sender, mut receiver) = channel();
        sender.send(Notification::Check);
        assert_eq!(receiver.take_priority_notification(), None);
        assert_eq!(receiver.take_notification(), Some(Notification::Check));
    }

    #[test]
    fn channel_notification_delivery() {
        let notifications = [
            Notification::Check,
            Notification::Recheck,
            Notification::Stop,
        ];

        for notification in notifications {
            let (sender, mut receiver) = channel();
            sender.send(notification);
            assert_eq!(receiver.take_notification(), Some(notification));
            assert_eq!(receiver.take_notification(), None);
        }
    }

    #[test]
    fn channel_priority_notification_delivery() {
        let notifications = [
            (Notification::Check, None),
            (Notification::Recheck, Some(PriorityNotification::Recheck)),
            (Notification::Stop, Some(PriorityNotification::Stop)),
        ];

        for (notification, expected) in notifications {
            let (sender, mut receiver) = channel();

            sender.send(notification);

            assert_eq!(
                receiver.take_priority_notification(),
                expected,
                "notification: {notification:?}",
            );
            assert_eq!(
                receiver.take_priority_notification(),
                None,
                "notification: {notification:?}",
            );
        }
    }

    #[test]
    fn channel_state_transitions() {
        use Notification::{Check, Recheck, Stop};

        let notifications = [Check, Recheck, Stop];

        let cases = [
            (None, [Check, Recheck, Stop]),
            (Some(Check), [Check, Recheck, Stop]),
            (Some(Recheck), [Recheck, Recheck, Stop]),
            (Some(Stop), [Recheck, Recheck, Stop]),
        ];

        for (seed, expected) in cases {
            for (notification, expected) in notifications.into_iter().zip(expected) {
                let (sender, mut receiver) = channel();

                if let Some(seed) = seed {
                    sender.send(seed);
                }

                sender.send(notification);

                assert_eq!(
                    receiver.take_notification(),
                    Some(expected),
                    "seed: {seed:?}, notification: {notification:?}",
                );
            }
        }
    }

    #[test]
    fn channel_notification_wake() {
        let flag = Arc::new(WakeFlag::default());
        let waker = futures_util::task::waker_ref(&flag);
        let mut cx = Context::from_waker(&waker);

        let (sender, mut receiver) = channel();

        let mut future = receiver.notification();

        assert_eq!(future.poll_unpin(&mut cx), Poll::Pending);

        flag.assert_not_woken();
        sender.send(Notification::Stop);
        flag.assert_woken();

        assert_eq!(future.poll_unpin(&mut cx), Poll::Ready(Notification::Stop));
    }

    #[test]
    fn channel_priority_notification_wake() {
        let flag = Arc::new(WakeFlag::default());
        let waker = futures_util::task::waker_ref(&flag);
        let mut cx = Context::from_waker(&waker);

        let (sender, mut receiver) = channel();

        let mut future = receiver.priority_notification();

        assert_eq!(future.poll_unpin(&mut cx), Poll::Pending);

        flag.assert_not_woken();
        sender.send(Notification::Stop);
        flag.assert_woken();

        assert_eq!(
            future.poll_unpin(&mut cx),
            Poll::Ready(PriorityNotification::Stop),
        );
    }
}
