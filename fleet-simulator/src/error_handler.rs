use crate::{Probability, Scheduler};
use fastrand::Rng;
use fleet_core::ErrorHandler;

/// An [`ErrorHandler`] with simulated delays.
///
/// Errors and retry counts are ignored.
pub struct ErrorHandlerSimulator<'sim> {
    scheduler: &'sim Scheduler,
    config: ErrorHandlerSimulatorConfig,
}

struct ErrorHandlerSimulatorConfig {
    handle_error: Probability,
    handle_error_with_backoff: Probability,
}

impl<'sim> ErrorHandlerSimulator<'sim> {
    /// Creates an error handler simulator from the given seed.
    pub fn new(scheduler: &'sim Scheduler, seed: u64) -> Self {
        let mut rng = Rng::with_seed(seed);

        Self {
            scheduler,
            config: ErrorHandlerSimulatorConfig::generate(&mut rng),
        }
    }
}

impl ErrorHandler for ErrorHandlerSimulator<'_> {
    fn handle_error<E>(&self, _error: E) -> impl Future<Output = ()> + Send
    where
        E: core::error::Error,
    {
        self.scheduler.maybe_suspend(self.config.handle_error)
    }

    fn handle_error_with_backoff<E>(
        &self,
        _error: E,
        _retries: u32,
    ) -> impl Future<Output = ()> + Send
    where
        E: core::error::Error,
    {
        self.scheduler
            .maybe_suspend(self.config.handle_error_with_backoff)
    }
}

impl ErrorHandlerSimulatorConfig {
    fn generate(rng: &mut Rng) -> Self {
        Self {
            handle_error: Probability::generate(rng),
            handle_error_with_backoff: Probability::generate(rng),
        }
    }
}
