use crate::{Probability, Scheduler};
use fastrand::Rng;
use fleet_core::IdleHandler;

/// An [`IdleHandler`] with simulated delays.
pub struct IdleHandlerSimulator<'sim> {
    scheduler: &'sim Scheduler,
    config: IdleHandlerSimulatorConfig,
}

struct IdleHandlerSimulatorConfig {
    on_idle: Probability,
}

impl<'sim> IdleHandlerSimulator<'sim> {
    /// Creates an idle handler simulator from the given seed.
    pub fn new(scheduler: &'sim Scheduler, seed: u64) -> Self {
        let mut rng = Rng::with_seed(seed);

        Self {
            scheduler,
            config: IdleHandlerSimulatorConfig::generate(&mut rng),
        }
    }
}

impl IdleHandler for IdleHandlerSimulator<'_> {
    async fn on_idle(&self) {
        self.scheduler.maybe_suspend(self.config.on_idle).await;
    }
}

impl IdleHandlerSimulatorConfig {
    fn generate(rng: &mut Rng) -> Self {
        Self {
            on_idle: Probability::generate(rng),
        }
    }
}
