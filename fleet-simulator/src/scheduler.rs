use crate::Probability;
use core::{
    future, mem,
    ops::RangeBounds,
    pin::{Pin, pin},
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
};
use fastrand::Rng;
use futures_util::{Stream, StreamExt as _, stream};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    task::Wake,
};

const MAX_RUN_STEPS: usize = 100_000;

/// Controls simulated scheduling and faults.
///
/// Use [`Self::run`] to drive work or [`Self::release`] to resume suspended operations manually.
pub struct Scheduler {
    state: Mutex<State>,
}

/// Determines which pending suspension is released next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SchedulerPolicy {
    /// Chooses uniformly among pending suspensions.
    Uniform,
    /// Releases suspensions in the order they were first polled.
    Fifo,
    /// Releases suspensions in reverse order of their first poll.
    Lifo,
    /// Releases the highest priority, randomly assigned once on each suspension's first poll.
    RandomPriority,
}

struct State {
    rng: Rng,
    policy: SchedulerPolicy,
    suspensions_enabled: bool,
    faults_enabled: bool,
    suspensions: BTreeMap<u64, PendingSuspension>,
    next_suspension_id: u64,
}

struct PendingSuspension {
    priority: u64,
    waker: Waker,
}

struct RunWaker {
    notified: AtomicBool,
}

/// The suspension and error probabilities for [`Scheduler::try_suspend`].
pub struct TrySuspendConfig {
    /// The probability of suspending.
    pub suspend: Probability,
    /// The probability of returning an injected error.
    pub error: Probability,
}

/// The suspension and error probabilities for [`Scheduler::wrap`].
pub struct WrapConfig {
    /// The probability of suspending before the future runs.
    pub suspend_before: Probability,
    /// The probability of suspending after the future completes.
    pub suspend_after: Probability,
    /// The probability of returning an injected error.
    pub error: Probability,
    /// The probability that an injected error occurs after the future completes.
    pub error_after: Probability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RandomErrorPosition {
    Before,
    After,
}

#[must_use = "futures do nothing unless you `.await` or poll them"]
struct Suspension<'scheduler> {
    id: Option<u64>,
    scheduler: &'scheduler Scheduler,
}

impl Scheduler {
    /// Creates a scheduler from the given seed.
    ///
    /// Suspensions and faults start enabled. The seed selects the release policy.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let mut rng = Rng::with_seed(seed);

        let policy = match rng.u8(0..4) {
            0 => SchedulerPolicy::Uniform,
            1 => SchedulerPolicy::Fifo,
            2 => SchedulerPolicy::Lifo,
            _ => SchedulerPolicy::RandomPriority,
        };

        Self {
            state: Mutex::new(State::new(rng, policy)),
        }
    }

    /// Runs a future and releases its simulated suspensions until it completes.
    ///
    /// # Panics
    ///
    /// Panics if the simulation stalls or exceeds 100,000 steps.
    pub fn run<F: Future>(&self, future: F) -> F::Output {
        let mut future = pin!(future);
        let wake = Arc::new(RunWaker {
            notified: AtomicBool::new(true),
        });
        let waker = Waker::from(wake.clone());
        let mut steps = 0;

        futures_executor::block_on(future::poll_fn(|cx| {
            assert!(steps < MAX_RUN_STEPS, "simulation exceeded its step limit");
            steps = steps.saturating_add(1);

            if wake.notified.swap(false, Ordering::AcqRel)
                && let Poll::Ready(output) = future.as_mut().poll(&mut Context::from_waker(&waker))
            {
                return Poll::Ready(output);
            }

            if !wake.notified.load(Ordering::Acquire) {
                assert!(
                    self.release(),
                    "simulation stalled without a wake or suspension"
                );
            }

            cx.waker().wake_by_ref();

            Poll::Pending
        }))
    }

    /// Disables simulated suspensions and wakes suspended operations.
    pub fn disable_suspensions(&self) {
        let suspensions = {
            let mut state = self.state.lock().expect("never poisoned");
            state.suspensions_enabled = false;
            mem::take(&mut state.suspensions)
        };

        for suspension in suspensions.into_values() {
            suspension.waker.wake();
        }
    }

    /// Stops sampling new faults.
    ///
    /// Suspensions and generated output remain enabled. Previously sampled faults may still occur.
    pub fn disable_faults(&self) {
        self.state.lock().expect("never poisoned").faults_enabled = false;
    }

    /// Stops sampling new faults and switches to FIFO scheduling.
    ///
    /// Pending and future suspensions are released in the order first polled. Previously sampled
    /// faults may still occur.
    pub fn begin_recovery(&self) {
        let mut state = self.state.lock().expect("never poisoned");
        state.faults_enabled = false;
        state.policy = SchedulerPolicy::Fifo;
    }

    /// Releases one pending suspension and wakes its task.
    ///
    /// Returns whether a suspension was released. If none were pending, work may still be waiting
    /// elsewhere.
    pub fn release(&self) -> bool {
        self.take_waker().is_some_and(|waker| {
            waker.wake();
            true
        })
    }

    const fn suspend(&self) -> Suspension<'_> {
        Suspension {
            id: None,
            scheduler: self,
        }
    }

    /// Returns whether to inject a fault.
    ///
    /// Always returns `false` when faults are disabled.
    pub(crate) fn sample_fault(&self, probability: Probability) -> bool {
        self.state
            .lock()
            .expect("never poisoned")
            .sample_fault(probability)
    }

    /// Samples a fault and its position within the given range.
    ///
    /// Returns `None` if no fault is sampled or faults are disabled.
    pub(crate) fn sample_fault_position(
        &self,
        probability: Probability,
        positions: impl RangeBounds<usize>,
    ) -> Option<usize> {
        let mut state = self.state.lock().expect("never poisoned");

        state
            .sample_fault(probability)
            .then(|| state.rng.usize(positions))
    }

    /// Suspends with the given probability.
    pub(crate) async fn maybe_suspend(&self, probability: Probability) {
        let suspend = probability.sample(&mut self.state.lock().expect("never poisoned").rng);

        if suspend {
            self.suspend().await;
        }
    }

    /// Simulates a suspension point.
    ///
    /// # Errors
    ///
    /// Returns `random_error()` when an error is injected.
    pub(crate) async fn try_suspend<E>(
        &self,
        config: &TrySuspendConfig,
        random_error: impl FnOnce() -> E,
    ) -> Result<(), E> {
        let (suspend, error) = {
            let mut state = self.state.lock().expect("never poisoned");

            (
                config.suspend.sample(&mut state.rng),
                state.sample_fault(config.error),
            )
        };

        if suspend {
            self.suspend().await;
        }

        if error { Err(random_error()) } else { Ok(()) }
    }

    /// Runs a future with simulated suspensions and errors.
    ///
    /// # Errors
    ///
    /// Returns `random_error()` when an error is injected. Otherwise returns the future's error.
    /// Injected errors before execution skip the future. Injected errors after execution replace
    /// its result.
    pub(crate) async fn wrap<T, E>(
        &self,
        config: &WrapConfig,
        random_error: impl FnOnce() -> E,
        future: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let (suspend_before, suspend_after, error_position) = {
            let mut state = self.state.lock().expect("never poisoned");

            (
                config.suspend_before.sample(&mut state.rng),
                config.suspend_after.sample(&mut state.rng),
                state.sample_fault(config.error).then(|| {
                    if config.error_after.sample(&mut state.rng) {
                        RandomErrorPosition::After
                    } else {
                        RandomErrorPosition::Before
                    }
                }),
            )
        };

        if suspend_before {
            self.suspend().await;
        }

        if error_position == Some(RandomErrorPosition::Before) {
            return Err(random_error());
        }

        let result = future.await;

        if suspend_after {
            self.suspend().await;
        }

        if error_position == Some(RandomErrorPosition::After) {
            return Err(random_error());
        }

        result
    }

    /// Streams the items in order and may suspend before each item.
    pub(crate) fn suspend_stream<T>(
        &self,
        suspend_probability: Probability,
        items: Vec<T>,
    ) -> impl Stream<Item = T> {
        let plan_future = async move {
            let rng = &mut self.state.lock().expect("never poisoned").rng;

            items
                .into_iter()
                .map(|item| (suspend_probability.sample(rng), item))
                .collect::<Vec<_>>()
        };

        stream::once(plan_future).flat_map(move |plan| {
            stream::iter(plan).then(async move |(suspend, item)| {
                if suspend {
                    self.suspend().await;
                }

                item
            })
        })
    }

    #[expect(clippy::significant_drop_tightening, reason = "false positive")]
    fn take_waker(&self) -> Option<Waker> {
        let mut state = self.state.lock().expect("never poisoned");
        let State {
            rng,
            policy,
            suspensions,
            ..
        } = &mut *state;

        if suspensions.is_empty() {
            return None;
        }

        let id = match policy {
            SchedulerPolicy::Uniform => rng.choice(suspensions.keys()),
            SchedulerPolicy::Fifo => suspensions.first_key_value().map(|(id, _)| id),
            SchedulerPolicy::Lifo => suspensions.last_key_value().map(|(id, _)| id),
            SchedulerPolicy::RandomPriority => suspensions
                .iter()
                .max_by_key(|(_, suspension)| suspension.priority)
                .map(|(id, _)| id),
        };

        let id = id.copied().expect("suspensions is not empty");

        let suspension = suspensions.remove(&id).expect("selected suspension exists");

        Some(suspension.waker)
    }
}

impl Wake for RunWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.notified.store(true, Ordering::Release);
    }
}

impl State {
    const fn new(rng: Rng, policy: SchedulerPolicy) -> Self {
        Self {
            rng,
            policy,
            suspensions_enabled: true,
            faults_enabled: true,
            suspensions: BTreeMap::new(),
            next_suspension_id: 0,
        }
    }

    fn sample_priority(&mut self) -> u64 {
        if self.policy == SchedulerPolicy::RandomPriority {
            self.rng.u64(..)
        } else {
            0
        }
    }

    fn sample_fault(&mut self, probability: Probability) -> bool {
        self.faults_enabled && probability.sample(&mut self.rng)
    }
}

impl TrySuspendConfig {
    /// Generates suspension and error probabilities.
    pub fn generate(rng: &mut Rng) -> Self {
        Self {
            suspend: Probability::generate(rng),
            error: Probability::generate_for_faults(rng),
        }
    }
}

impl WrapConfig {
    /// Generates suspension and error probabilities.
    pub fn generate(rng: &mut Rng) -> Self {
        Self {
            suspend_before: Probability::generate(rng),
            suspend_after: Probability::generate(rng),
            error: Probability::generate_for_faults(rng),
            error_after: Probability::generate(rng),
        }
    }
}

impl Future for Suspension<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut state = this.scheduler.state.lock().expect("never poisoned");

        if !state.suspensions_enabled {
            return Poll::Ready(());
        }

        let Some(id) = this.id else {
            let id = state.next_suspension_id;
            state.next_suspension_id = id.checked_add(1).expect("IDs never overflow");
            let priority = state.sample_priority();

            state.suspensions.insert(
                id,
                PendingSuspension {
                    priority,
                    waker: cx.waker().clone(),
                },
            );

            this.id = Some(id);

            return Poll::Pending;
        };

        state
            .suspensions
            .get_mut(&id)
            .map_or(Poll::Ready(()), |suspension| {
                suspension.waker.clone_from(cx.waker());
                Poll::Pending
            })
    }
}

impl Drop for Suspension<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.scheduler
                .state
                .lock()
                .expect("never poisoned")
                .suspensions
                .remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        cell::Cell,
        pin::pin,
        task::{Context, Poll, Waker},
    };
    use futures_util::FutureExt as _;

    impl Scheduler {
        const fn with_policy(seed: u64, policy: SchedulerPolicy) -> Self {
            Self {
                state: Mutex::new(State::new(Rng::with_seed(seed), policy)),
            }
        }

        fn faults_enabled(&self) -> bool {
            self.state.lock().unwrap().faults_enabled
        }
    }

    fn probability(value: bool) -> Probability {
        if value {
            Probability::ALWAYS
        } else {
            Probability::NEVER
        }
    }

    #[test]
    fn run_returns_output_after_suspensions() {
        let scheduler = Scheduler::new(0);
        let first = async {
            scheduler.suspend().await;
            42
        };
        let second = async {
            scheduler.suspend().await;
            43
        };

        let result = scheduler.run(futures_util::future::join(first, second));
        assert_eq!(result, (42, 43));
        assert!(!scheduler.release());
    }

    #[test]
    fn run_polls_self_woken_futures() {
        let scheduler = Scheduler::new(0);
        let mut polled = false;
        let future = future::poll_fn(|cx| {
            if polled {
                Poll::Ready(42)
            } else {
                polled = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        });

        let result = scheduler.run(future);
        assert_eq!(result, 42);
    }

    #[test]
    #[should_panic(expected = "simulation stalled without a wake or suspension")]
    fn run_panics_when_future_stalls() {
        let scheduler = Scheduler::new(0);

        scheduler.run(future::pending::<()>());
    }

    #[test]
    #[should_panic(expected = "simulation exceeded its step limit")]
    fn run_panics_when_step_limit_is_exceeded() {
        let scheduler = Scheduler::new(0);
        let future = future::poll_fn(|cx| {
            cx.waker().wake_by_ref();
            Poll::<()>::Pending
        });

        scheduler.run(future);
    }

    #[test]
    fn release_skips_cancelled_suspensions() {
        for policy in [
            SchedulerPolicy::Uniform,
            SchedulerPolicy::Fifo,
            SchedulerPolicy::Lifo,
            SchedulerPolicy::RandomPriority,
        ] {
            let scheduler = Scheduler::with_policy(0, policy);
            let mut cx = Context::from_waker(Waker::noop());
            let mut first = scheduler.suspend();
            let mut cancelled = scheduler.suspend();
            let mut last = scheduler.suspend();

            assert!(!scheduler.release());

            for suspension in [&mut first, &mut cancelled, &mut last] {
                assert_eq!(Pin::new(suspension).poll(&mut cx), Poll::Pending);
            }

            drop(cancelled);

            assert!(scheduler.release());
            assert!(scheduler.release());
            assert!(!scheduler.release());

            for suspension in [&mut first, &mut last] {
                assert_eq!(Pin::new(suspension).poll(&mut cx), Poll::Ready(()));
            }
        }
    }

    #[test]
    fn new_samples_all_policies() {
        let mut seen = Vec::new();

        for seed in 0..64 {
            let policy = Scheduler::new(seed).state.lock().unwrap().policy;
            assert_eq!(Scheduler::new(seed).state.lock().unwrap().policy, policy);
            if !seen.contains(&policy) {
                seen.push(policy);
            }
        }

        for policy in [
            SchedulerPolicy::Uniform,
            SchedulerPolicy::Fifo,
            SchedulerPolicy::Lifo,
            SchedulerPolicy::RandomPriority,
        ] {
            assert!(seen.contains(&policy));
        }
    }

    #[test]
    fn release_order_is_reproducible() {
        fn release_order(scheduler: &Scheduler) -> Vec<usize> {
            let mut cx = Context::from_waker(Waker::noop());
            let mut pending = (0..8)
                .map(|index| (index, scheduler.suspend()))
                .collect::<Vec<_>>();

            for (_, suspension) in &mut pending {
                assert_eq!(Pin::new(suspension).poll(&mut cx), Poll::Pending);
            }

            let mut order = Vec::new();

            while scheduler.release() {
                let index = pending
                    .iter_mut()
                    .position(|(_, suspension)| Pin::new(suspension).poll(&mut cx).is_ready())
                    .expect("a suspension was released");
                let (index, _) = pending.remove(index);
                order.push(index);
            }

            assert!(pending.is_empty());
            order
        }

        for seed in 0..4 {
            let expected = release_order(&Scheduler::new(seed));
            assert_eq!(release_order(&Scheduler::new(seed)), expected);

            for policy in [
                SchedulerPolicy::Uniform,
                SchedulerPolicy::Fifo,
                SchedulerPolicy::Lifo,
                SchedulerPolicy::RandomPriority,
            ] {
                let expected = release_order(&Scheduler::with_policy(seed, policy));
                assert_eq!(
                    release_order(&Scheduler::with_policy(seed, policy)),
                    expected,
                );
            }
        }
    }

    #[test]
    fn disable_suspensions_preserves_faults() {
        let scheduler = Scheduler::new(0);
        let config = TrySuspendConfig {
            suspend: Probability::ALWAYS,
            error: Probability::ALWAYS,
        };

        scheduler.disable_suspensions();

        let error = scheduler
            .try_suspend(&config, || "injected")
            .now_or_never()
            .unwrap()
            .unwrap_err();
        assert_eq!(error, "injected");
        assert!(!scheduler.release());
    }

    #[test]
    fn disable_suspensions_wakes_pending_operations() {
        let scheduler = Scheduler::new(0);
        let wake = Arc::new(RunWaker {
            notified: AtomicBool::new(false),
        });
        let waker = Waker::from(wake.clone());
        let mut cx = Context::from_waker(&waker);
        let mut suspensions = [scheduler.suspend(), scheduler.suspend()];

        for suspension in &mut suspensions {
            assert_eq!(Pin::new(suspension).poll(&mut cx), Poll::Pending);
        }

        scheduler.disable_suspensions();

        assert!(wake.notified.load(Ordering::Relaxed));
        assert!(!scheduler.release());

        for suspension in &mut suspensions {
            assert_eq!(Pin::new(suspension).poll(&mut cx), Poll::Ready(()));
        }
    }

    #[test]
    fn disable_suspensions_preserves_pending_futures() {
        let scheduler = Scheduler::new(0);
        let mut cx = Context::from_waker(Waker::noop());
        let config = WrapConfig {
            suspend_before: Probability::ALWAYS,
            suspend_after: Probability::ALWAYS,
            error: Probability::NEVER,
            error_after: Probability::NEVER,
        };

        scheduler.disable_suspensions();

        let future = future::pending::<Result<(), _>>();
        let mut future = pin!(scheduler.wrap(&config, || "injected", future));

        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        assert!(!scheduler.release());
    }

    #[test]
    fn disable_faults_preserves_behaviour_sampling() {
        let mut cx = Context::from_waker(Waker::noop());
        let scheduler = Scheduler::new(0);

        assert!(scheduler.faults_enabled());
        assert!(scheduler.sample_fault(Probability::ALWAYS));
        assert!(!scheduler.sample_fault(Probability::NEVER));
        assert_eq!(
            scheduler.sample_fault_position(Probability::ALWAYS, 0..=0),
            Some(0),
        );

        scheduler.disable_faults();

        assert!(!scheduler.faults_enabled());
        assert!(!scheduler.sample_fault(Probability::ALWAYS));
        assert_eq!(
            scheduler.sample_fault_position(Probability::ALWAYS, 0..=0),
            None,
        );

        let mut future = pin!(scheduler.maybe_suspend(Probability::ALWAYS));

        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        assert!(scheduler.release());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(()));
        assert!(!scheduler.release());
    }

    #[test]
    fn disable_faults_keeps_scheduling_policy() {
        for policy in [
            SchedulerPolicy::Uniform,
            SchedulerPolicy::Fifo,
            SchedulerPolicy::Lifo,
            SchedulerPolicy::RandomPriority,
        ] {
            let scheduler = Scheduler::with_policy(0, policy);

            scheduler.disable_faults();

            assert_eq!(scheduler.state.lock().unwrap().policy, policy);
        }
    }

    #[test]
    fn begin_recovery_switches_pending_and_new_suspensions_to_fifo() {
        for policy in [SchedulerPolicy::Lifo, SchedulerPolicy::RandomPriority] {
            let scheduler = Scheduler::with_policy(2, policy);
            let mut cx = Context::from_waker(Waker::noop());
            let mut oldest = pin!(scheduler.suspend());
            let mut next = pin!(scheduler.suspend());

            assert_eq!(oldest.as_mut().poll(&mut cx), Poll::Pending);
            assert_eq!(next.as_mut().poll(&mut cx), Poll::Pending);

            scheduler.begin_recovery();

            assert!(!scheduler.faults_enabled());
            assert!(!scheduler.sample_fault(Probability::ALWAYS));

            let mut newest = pin!(scheduler.maybe_suspend(Probability::ALWAYS));
            assert_eq!(newest.as_mut().poll(&mut cx), Poll::Pending);

            assert!(scheduler.release());
            assert_eq!(oldest.as_mut().poll(&mut cx), Poll::Ready(()));
            assert_eq!(next.as_mut().poll(&mut cx), Poll::Pending);
            assert_eq!(newest.as_mut().poll(&mut cx), Poll::Pending);

            assert!(scheduler.release());
            assert_eq!(next.as_mut().poll(&mut cx), Poll::Ready(()));
            assert_eq!(newest.as_mut().poll(&mut cx), Poll::Pending);

            assert!(scheduler.release());
            assert_eq!(newest.as_mut().poll(&mut cx), Poll::Ready(()));
            assert!(!scheduler.release());
        }
    }

    #[test]
    fn begin_recovery_keeps_already_planned_faults() {
        let scheduler = Scheduler::with_policy(0, SchedulerPolicy::Lifo);
        let mut cx = Context::from_waker(Waker::noop());
        let config = TrySuspendConfig {
            suspend: Probability::ALWAYS,
            error: Probability::ALWAYS,
        };
        let mut before_begin_recovery = pin!(scheduler.try_suspend(&config, || "injected"));
        assert_eq!(before_begin_recovery.as_mut().poll(&mut cx), Poll::Pending);

        scheduler.begin_recovery();

        assert!(scheduler.release());
        assert_eq!(
            before_begin_recovery.as_mut().poll(&mut cx),
            Poll::Ready(Err("injected"))
        );

        let mut after_begin_recovery = pin!(scheduler.try_suspend(&config, || "injected"));
        assert_eq!(after_begin_recovery.as_mut().poll(&mut cx), Poll::Pending);
        assert!(scheduler.release());
        assert_eq!(
            after_begin_recovery.as_mut().poll(&mut cx),
            Poll::Ready(Ok(()))
        );
        assert!(!scheduler.release());
    }

    #[test]
    fn sample_fault_position_stays_within_range() {
        let scheduler = Scheduler::new(0);

        for _ in 0..100 {
            let position = scheduler
                .sample_fault_position(Probability::ALWAYS, 0..5)
                .expect("fault always sampled");
            assert!((0..5).contains(&position));

            let position = scheduler
                .sample_fault_position(Probability::ALWAYS, 0..=5)
                .expect("fault always sampled");
            assert!((0..=5).contains(&position));
        }
    }

    #[test]
    fn maybe_suspend_obeys_probability() {
        for suspend in [false, true] {
            let mut cx = Context::from_waker(Waker::noop());
            let scheduler = Scheduler::new(0);

            let mut future = pin!(scheduler.maybe_suspend(probability(suspend)));

            if suspend {
                assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
                assert!(scheduler.release());
            }

            assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(()));
            assert!(!scheduler.release());
        }
    }

    #[test]
    fn try_suspend_preserves_results_unless_error_is_injected() {
        for result in [Ok(42), Err("operation")] {
            for random_error in [false, true] {
                for suspend in [false, true] {
                    let mut cx = Context::from_waker(Waker::noop());
                    let scheduler = Scheduler::new(0);
                    let ran = Cell::new(false);

                    let config = TrySuspendConfig {
                        suspend: probability(suspend),
                        error: probability(random_error),
                    };

                    let mut future = pin!(async {
                        scheduler.try_suspend(&config, || "injected").await?;
                        ran.set(true);
                        result
                    });

                    if suspend {
                        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
                        assert!(!ran.get());
                        assert!(scheduler.release());
                    }

                    let expected_result = if random_error {
                        Err("injected")
                    } else {
                        result
                    };

                    assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(expected_result));
                    assert_eq!(ran.get(), !random_error);
                    assert!(!scheduler.release());
                }
            }
        }
    }

    #[test]
    fn try_suspend_suspends_before_operation() {
        let mut cx = Context::from_waker(Waker::noop());
        let scheduler = Scheduler::new(0);
        let stage = Cell::new(0);

        let config = TrySuspendConfig {
            suspend: Probability::ALWAYS,
            error: Probability::NEVER,
        };

        let mut future = pin!(async {
            scheduler.try_suspend(&config, || "injected").await?;
            stage.set(1);
            scheduler.suspend().await;
            stage.set(2);
            Ok::<_, &str>(42)
        });

        assert_eq!(stage.get(), 0);
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(stage.get(), 0);

        assert!(scheduler.release());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(stage.get(), 1);

        assert!(scheduler.release());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Ok(42)));
        assert_eq!(stage.get(), 2);

        assert!(!scheduler.release());
    }

    #[test]
    fn try_suspend_obeys_fault_switch() {
        let mut cx = Context::from_waker(Waker::noop());
        let scheduler = Scheduler::new(0);

        let config = TrySuspendConfig {
            suspend: Probability::ALWAYS,
            error: Probability::ALWAYS,
        };

        let mut future = pin!(scheduler.try_suspend(&config, || "injected"));

        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        scheduler.disable_faults();
        assert!(scheduler.release());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Err("injected")));
        assert!(!scheduler.release());

        for result in [Ok(42), Err("operation")] {
            let mut future = pin!(async {
                scheduler.try_suspend(&config, || "injected").await?;
                result
            });

            assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
            assert!(scheduler.release());
            assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(result));
            assert!(!scheduler.release());
        }
    }

    #[test]
    fn try_suspend_samples_on_first_poll() {
        let mut cx = Context::from_waker(Waker::noop());
        let scheduler = Scheduler::new(0);

        let config = TrySuspendConfig {
            suspend: Probability::ALWAYS,
            error: Probability::ALWAYS,
        };

        let mut future = pin!(scheduler.try_suspend(&config, || "injected"));
        scheduler.disable_faults();

        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        assert!(scheduler.release());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        assert!(!scheduler.release());
    }

    #[test]
    fn wrap_suspends_around_future() {
        let mut cx = Context::from_waker(Waker::noop());
        let scheduler = Scheduler::new(0);
        let stage = Cell::new(0);

        let config = WrapConfig {
            suspend_before: Probability::ALWAYS,
            suspend_after: Probability::ALWAYS,
            error: Probability::NEVER,
            error_after: Probability::NEVER,
        };

        let mut future = pin!(scheduler.wrap(&config, || "injected", async {
            stage.set(1);
            scheduler.suspend().await;
            stage.set(2);
            Ok(42)
        }));

        assert_eq!(stage.get(), 0);
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(stage.get(), 0);

        assert!(scheduler.release());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(stage.get(), 1);

        assert!(scheduler.release());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(stage.get(), 2);

        assert!(scheduler.release());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Ok(42)));

        assert!(!scheduler.release());
    }

    #[test]
    fn wrap_skips_future_on_before_error() {
        let mut cx = Context::from_waker(Waker::noop());
        let scheduler = Scheduler::new(0);
        let ran = Cell::new(false);

        let config = WrapConfig {
            suspend_before: Probability::ALWAYS,
            suspend_after: Probability::ALWAYS,
            error: Probability::ALWAYS,
            error_after: Probability::NEVER,
        };

        let mut future = pin!(scheduler.wrap(&config, || "injected", async {
            ran.set(true);
            Ok(())
        }));

        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        assert!(!ran.get());

        assert!(scheduler.release());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Err("injected")));
        assert!(!ran.get());

        assert!(!scheduler.release());
    }

    #[test]
    fn wrap_preserves_results_unless_after_error_is_injected() {
        for result in [Ok(42), Err("operation")] {
            for random_error in [None, Some(RandomErrorPosition::After)] {
                for suspend_after in [false, true] {
                    let mut cx = Context::from_waker(Waker::noop());
                    let scheduler = Scheduler::new(0);
                    let ran = Cell::new(false);

                    let config = WrapConfig {
                        suspend_before: Probability::NEVER,
                        suspend_after: probability(suspend_after),
                        error: probability(random_error.is_some()),
                        error_after: Probability::ALWAYS,
                    };

                    let mut future = pin!(scheduler.wrap(&config, || "injected", async {
                        ran.set(true);
                        result
                    }));

                    if suspend_after {
                        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
                        assert!(ran.get());
                        assert!(scheduler.release());
                    }

                    let expected_result = if random_error.is_some() {
                        Err("injected")
                    } else {
                        result
                    };

                    assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(expected_result));
                    assert!(ran.get());
                    assert!(!scheduler.release());
                }
            }
        }
    }

    #[test]
    fn wrap_cancellation_clean_up() {
        for cancel_at_stage in 0..=2 {
            let mut cx = Context::from_waker(Waker::noop());
            let scheduler = Scheduler::new(0);
            let stage = Cell::new(0);

            let config = WrapConfig {
                suspend_before: Probability::ALWAYS,
                suspend_after: Probability::ALWAYS,
                error: Probability::NEVER,
                error_after: Probability::NEVER,
            };

            {
                let mut future = pin!(scheduler.wrap(&config, || "injected", async {
                    stage.set(1);
                    scheduler.suspend().await;
                    stage.set(2);
                    Ok(())
                }));

                assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);

                for _ in 0..cancel_at_stage {
                    assert!(scheduler.release());
                    assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
                }

                assert_eq!(stage.get(), cancel_at_stage);
            }

            assert_eq!(stage.get(), cancel_at_stage);
            assert!(!scheduler.release());
        }
    }

    #[test]
    fn wrap_keeps_errors_sampled_before_disabling_faults() {
        for position in [RandomErrorPosition::Before, RandomErrorPosition::After] {
            let mut cx = Context::from_waker(Waker::noop());
            let scheduler = Scheduler::new(0);
            let ran = Cell::new(false);

            let config = WrapConfig {
                suspend_before: Probability::ALWAYS,
                suspend_after: probability(position == RandomErrorPosition::After),
                error: Probability::ALWAYS,
                error_after: probability(position == RandomErrorPosition::After),
            };

            let mut future = pin!(scheduler.wrap(&config, || "injected", async {
                ran.set(true);
                Ok(())
            }));

            assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
            assert!(!ran.get());
            scheduler.disable_faults();
            assert!(scheduler.release());

            if position == RandomErrorPosition::After {
                assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
                assert!(ran.get());
                assert!(scheduler.release());
            }

            assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Err("injected")));
            assert_eq!(ran.get(), position == RandomErrorPosition::After);
            assert!(!scheduler.release());
        }
    }

    #[test]
    fn wrap_samples_on_first_poll() {
        for result in [Ok(42), Err("operation")] {
            for position in [RandomErrorPosition::Before, RandomErrorPosition::After] {
                let mut cx = Context::from_waker(Waker::noop());
                let scheduler = Scheduler::new(0);
                let ran = Cell::new(false);

                let config = WrapConfig {
                    suspend_before: Probability::ALWAYS,
                    suspend_after: Probability::ALWAYS,
                    error: Probability::ALWAYS,
                    error_after: probability(position == RandomErrorPosition::After),
                };

                let mut future = pin!(scheduler.wrap(&config, || "injected", async {
                    ran.set(true);
                    result
                }));
                scheduler.disable_faults();

                assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
                assert!(!ran.get());

                assert!(scheduler.release());
                assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
                assert!(ran.get());

                assert!(scheduler.release());
                assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(result));
                assert!(!scheduler.release());
            }
        }
    }

    #[test]
    fn suspend_stream_preserves_items_after_suspension() {
        for faults_enabled in [false, true] {
            for suspend in [false, true] {
                let mut cx = Context::from_waker(Waker::noop());
                let scheduler = Scheduler::new(0);
                let items = [Ok(42), Err("operation"), Ok(43)];

                if !faults_enabled {
                    scheduler.disable_faults();
                }

                let mut stream = pin!(scheduler.suspend_stream(probability(suspend), items.into()));

                for item in items {
                    if suspend {
                        assert_eq!(stream.as_mut().poll_next(&mut cx), Poll::Pending);
                        assert!(scheduler.release());
                    }

                    assert_eq!(stream.as_mut().poll_next(&mut cx), Poll::Ready(Some(item)));
                    assert!(!scheduler.release());
                }

                assert_eq!(stream.as_mut().poll_next(&mut cx), Poll::Ready(None));
                assert!(!scheduler.release());
            }
        }
    }

    #[test]
    fn suspend_stream_cancellation_clean_up() {
        for cancel_after_items in 0..3 {
            let mut cx = Context::from_waker(Waker::noop());
            let scheduler = Scheduler::new(0);

            {
                let mut stream =
                    pin!(scheduler.suspend_stream(Probability::ALWAYS, Vec::from([0, 1, 2])));

                for item in 0..cancel_after_items {
                    assert_eq!(stream.as_mut().poll_next(&mut cx), Poll::Pending);
                    assert!(scheduler.release());
                    assert_eq!(stream.as_mut().poll_next(&mut cx), Poll::Ready(Some(item)));
                }

                assert_eq!(stream.as_mut().poll_next(&mut cx), Poll::Pending);
            }

            assert!(!scheduler.release());
        }
    }

    #[test]
    fn fifo_releases_oldest_waiter() {
        let scheduler = Scheduler::with_policy(0, SchedulerPolicy::Fifo);
        let mut cx = Context::from_waker(Waker::noop());
        let mut oldest = pin!(scheduler.suspend());
        let mut next = pin!(scheduler.suspend());

        assert_eq!(oldest.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(next.as_mut().poll(&mut cx), Poll::Pending);

        assert_eq!(oldest.as_mut().poll(&mut cx), Poll::Pending);
        assert!(scheduler.release());
        assert_eq!(oldest.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(next.as_mut().poll(&mut cx), Poll::Pending);

        let mut newest = pin!(scheduler.suspend());
        assert_eq!(newest.as_mut().poll(&mut cx), Poll::Pending);
        assert!(scheduler.release());
        assert_eq!(next.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(newest.as_mut().poll(&mut cx), Poll::Pending);

        assert!(scheduler.release());
        assert_eq!(newest.as_mut().poll(&mut cx), Poll::Ready(()));
        assert!(!scheduler.release());
    }

    #[test]
    fn lifo_releases_newest_waiter() {
        let scheduler = Scheduler::with_policy(0, SchedulerPolicy::Lifo);
        let mut cx = Context::from_waker(Waker::noop());
        let mut oldest = pin!(scheduler.suspend());
        let mut newer = pin!(scheduler.suspend());

        assert_eq!(oldest.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(newer.as_mut().poll(&mut cx), Poll::Pending);

        assert_eq!(oldest.as_mut().poll(&mut cx), Poll::Pending);
        assert!(scheduler.release());
        assert_eq!(newer.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(oldest.as_mut().poll(&mut cx), Poll::Pending);

        let mut newest = pin!(scheduler.suspend());
        assert_eq!(newest.as_mut().poll(&mut cx), Poll::Pending);
        assert!(scheduler.release());
        assert_eq!(newest.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(oldest.as_mut().poll(&mut cx), Poll::Pending);

        assert!(scheduler.release());
        assert_eq!(oldest.as_mut().poll(&mut cx), Poll::Ready(()));
        assert!(!scheduler.release());
    }

    #[test]
    fn random_priority_releases_highest_priority_first() {
        let scheduler = Scheduler::with_policy(0, SchedulerPolicy::RandomPriority);
        let mut cx = Context::from_waker(Waker::noop());
        let mut suspensions = [
            scheduler.suspend(),
            scheduler.suspend(),
            scheduler.suspend(),
        ];

        for suspension in &mut suspensions {
            assert_eq!(Pin::new(suspension).poll(&mut cx), Poll::Pending);
        }

        {
            let state = scheduler.state.lock().unwrap();
            suspensions.sort_unstable_by_key(|suspension| {
                let id = suspension.id.unwrap();
                (state.suspensions[&id].priority, id)
            });
        }
        let [mut low, mut middle, mut high] = suspensions;

        assert_eq!(Pin::new(&mut low).poll(&mut cx), Poll::Pending);
        assert!(scheduler.release());
        assert_eq!(Pin::new(&mut high).poll(&mut cx), Poll::Ready(()));
        assert_eq!(Pin::new(&mut low).poll(&mut cx), Poll::Pending);
        assert_eq!(Pin::new(&mut middle).poll(&mut cx), Poll::Pending);

        assert!(scheduler.release());
        assert_eq!(Pin::new(&mut middle).poll(&mut cx), Poll::Ready(()));
        assert_eq!(Pin::new(&mut low).poll(&mut cx), Poll::Pending);

        assert!(scheduler.release());
        assert_eq!(Pin::new(&mut low).poll(&mut cx), Poll::Ready(()));
        assert!(!scheduler.release());
    }

    #[test]
    fn fifo_uses_first_poll_order_instead_of_creation_order() {
        let scheduler = Scheduler::with_policy(0, SchedulerPolicy::Fifo);
        let mut cx = Context::from_waker(Waker::noop());
        let mut created_first = pin!(scheduler.suspend());
        let mut created_second = pin!(scheduler.suspend());

        assert_eq!(created_second.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(created_first.as_mut().poll(&mut cx), Poll::Pending);

        assert!(scheduler.release());
        assert_eq!(created_second.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(created_first.as_mut().poll(&mut cx), Poll::Pending);

        assert!(scheduler.release());
        assert_eq!(created_first.as_mut().poll(&mut cx), Poll::Ready(()));
        assert!(!scheduler.release());
    }
}
