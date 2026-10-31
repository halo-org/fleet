use core::{pin::pin, time::Duration};
use futures_util::future::{self, Either};
use tokio::sync::watch;

/// Tracks mutations for idle shutdown.
#[derive(Clone)]
pub struct IdleHandler {
    state: watch::Sender<State>,
    idle_timeout: Option<Duration>,
}

/// Keeps a mutation active until dropped.
pub struct MutationGuard {
    idle_handler: IdleHandler,
}

#[derive(Default)]
struct State {
    mutations: usize,
    last_activity: u64,
    shutting_down: bool,
}

impl IdleHandler {
    /// Creates an idle handler.
    pub fn new(idle_timeout: Option<Duration>) -> Self {
        let (state, _) = watch::channel(State::default());

        Self {
            state,
            idle_timeout,
        }
    }

    /// Returns a marker for the latest mutation activity.
    pub fn last_activity(&self) -> u64 {
        self.state.borrow().last_activity
    }

    /// Tracks a mutation unless shutdown has begun.
    pub fn track_mutation(&self) -> Option<MutationGuard> {
        let is_tracked = self.state.send_if_modified(|state| {
            if state.shutting_down {
                return false;
            }

            state.mutations = state
                .mutations
                .checked_add(1)
                .expect("mutation count never overflows");
            state.last_activity = state.last_activity.wrapping_add(1);

            true
        });

        is_tracked.then(|| MutationGuard {
            idle_handler: self.clone(),
        })
    }

    /// Begins shutdown.
    pub fn shutdown(&self) {
        self.state.send_modify(|state| state.shutting_down = true);
    }

    /// Begins shutdown if idle since `last_activity`.
    pub fn try_shutdown(&self, last_activity: u64) -> bool {
        self.state.send_if_modified(|state| {
            if state.mutations != 0 || state.last_activity != last_activity {
                return false;
            }

            state.shutting_down = true;

            true
        })
    }

    /// Waits for shutdown to begin.
    pub async fn wait_for_shutdown(&self) {
        self.state
            .subscribe()
            .wait_for(|state| state.shutting_down)
            .await
            .expect("sender outlives receiver");
    }

    /// Waits for pending mutations to finish.
    pub async fn wait_for_mutations(&self) {
        self.state
            .subscribe()
            .wait_for(|state| state.mutations == 0)
            .await
            .expect("sender outlives receiver");
    }
}

impl fleet_core::IdleHandler for IdleHandler {
    async fn on_idle(&self) {
        let Some(timeout) = self.idle_timeout else {
            return core::future::pending().await;
        };

        let mut receiver = self.state.subscribe();

        loop {
            let mutations = receiver.borrow_and_update().mutations;

            let result = if mutations != 0 {
                receiver.changed().await
            } else {
                let timeout = pin!(tokio::time::sleep(timeout));
                let changed = pin!(receiver.changed());

                match future::select(timeout, changed).await {
                    Either::Left(((), _)) => return,
                    Either::Right((result, _)) => result,
                }
            };

            result.expect("sender outlives receiver");
        }
    }
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        self.idle_handler.state.send_modify(|state| {
            state.mutations = state.mutations.checked_sub(1).expect("mutation is active");
            state.last_activity = state.last_activity.wrapping_add(1);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::IdleHandler as _;
    use futures_util::FutureExt as _;

    #[test]
    fn try_shutdown_requires_idle_and_unchanged_activity() {
        let idle_handler = IdleHandler::new(None);
        let initial_activity = idle_handler.last_activity();

        let guard = idle_handler.track_mutation().unwrap();
        let last_activity = idle_handler.last_activity();
        assert!(!idle_handler.try_shutdown(last_activity));

        drop(guard);
        assert!(!idle_handler.try_shutdown(initial_activity));
        assert!(!idle_handler.try_shutdown(last_activity));

        let last_activity = idle_handler.last_activity();
        assert!(idle_handler.try_shutdown(last_activity));
        assert!(idle_handler.track_mutation().is_none());

        let output = idle_handler.wait_for_shutdown().now_or_never();
        assert_eq!(output, Some(()));
    }

    #[test]
    fn on_idle_waits_for_timeout_after_mutations() {
        let timeout = Duration::from_secs(10);
        let idle_handler = IdleHandler::new(Some(timeout));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();

        runtime.block_on(async {
            let mut future = pin!(idle_handler.on_idle());

            let output = future.as_mut().now_or_never();
            assert_eq!(output, None);

            tokio::time::advance(Duration::from_secs(5)).await;
            let guard = idle_handler.track_mutation().unwrap();
            let output = future.as_mut().now_or_never();
            assert_eq!(output, None);

            tokio::time::advance(timeout).await;
            let output = future.as_mut().now_or_never();
            assert_eq!(output, None);

            drop(guard);
            let output = future.as_mut().now_or_never();
            assert_eq!(output, None);

            tokio::time::advance(Duration::from_secs(9)).await;
            let output = future.as_mut().now_or_never();
            assert_eq!(output, None);

            tokio::time::advance(Duration::from_secs(1)).await;
            let output = future.as_mut().now_or_never();
            assert_eq!(output, Some(()));
        });
    }

    #[test]
    fn on_idle_stays_pending_without_timeout() {
        let idle_handler = IdleHandler::new(None);
        let output = idle_handler.on_idle().now_or_never();
        assert_eq!(output, None);
    }
}
