use core::{
    pin::Pin,
    task::{Context, Poll, Waker},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    Agents,
    Snoozes,
    Wakes,
    Agent(u128),
    Preview(u128),
}

#[derive(Clone, Default)]
pub struct Notifications(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    next_id: u128,
    listeners: BTreeMap<u128, Listener>,
}

struct Listener {
    topic: Topic,
    ready: bool,
    waker: Option<Waker>,
}

pub struct Notified {
    notifications: Notifications,
    id: u128,
}

impl Notifications {
    pub fn listen(&self, topic: Topic) -> Notified {
        let id = {
            let mut state = self.0.lock().expect("notifications are not poisoned");
            let id = state.next_id;
            state.next_id = id.checked_add(1).expect("listener IDs do not overflow");
            state.listeners.insert(
                id,
                Listener {
                    topic,
                    ready: false,
                    waker: None,
                },
            );
            id
        };
        Notified {
            notifications: self.clone(),
            id,
        }
    }

    pub fn notify(&self, topic: Topic) {
        let wakers = {
            let mut state = self.0.lock().expect("notifications are not poisoned");
            state
                .listeners
                .values_mut()
                .filter(|listener| listener.topic == topic)
                .filter_map(|listener| {
                    listener.ready = true;
                    listener.waker.take()
                })
                .collect::<Vec<_>>()
        };
        for waker in wakers {
            waker.wake();
        }
    }
}

impl Future for Notified {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match self
            .notifications
            .0
            .lock()
            .expect("notifications are not poisoned")
            .listeners
            .get_mut(&self.id)
            .expect("listener exists")
        {
            Listener { ready: true, .. } => Poll::Ready(()),
            listener => {
                listener.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl Drop for Notified {
    fn drop(&mut self) {
        self.notifications
            .0
            .lock()
            .expect("notifications are not poisoned")
            .listeners
            .remove(&self.id);
    }
}
