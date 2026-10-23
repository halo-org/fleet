use crate::{
    ErrorHandlerSimulator, IdleHandlerSimulator, ModelsSimulator, RolesSimulator, Scheduler,
    StoreSimulator, ToolsSimulator,
};
use fastrand::Rng;

/// The simulated providers for a runtime.
pub struct Simulator<'sim> {
    /// The simulated store.
    pub store: StoreSimulator<'sim>,
    /// The simulated roles.
    pub roles: RolesSimulator<'sim>,
    /// The simulated models.
    pub models: ModelsSimulator<'sim>,
    /// The simulated tools.
    pub tools: ToolsSimulator<'sim>,
    /// The simulated error handler.
    pub error_handler: ErrorHandlerSimulator<'sim>,
    /// The simulated idle handler.
    pub idle_handler: IdleHandlerSimulator<'sim>,
}

impl<'sim> Simulator<'sim> {
    /// Creates simulated providers from the given seed.
    pub fn new(scheduler: &'sim Scheduler, seed: u64) -> Self {
        let mut rng = Rng::with_seed(seed);
        let models = ModelsSimulator::new(scheduler, rng.u64(..));
        let tools = ToolsSimulator::new(scheduler, rng.u64(..));
        let roles = RolesSimulator::new(
            scheduler,
            rng.u64(..),
            &models.model_names(),
            &tools.tool_names(),
        );

        Self {
            store: StoreSimulator::new(scheduler, rng.u64(..)),
            roles,
            models,
            tools,
            error_handler: ErrorHandlerSimulator::new(scheduler, rng.u64(..)),
            idle_handler: IdleHandlerSimulator::new(scheduler, rng.u64(..)),
        }
    }

    /// Runs the runtime using the simulated providers.
    pub fn run(&self) -> impl Future<Output = ()> {
        fleet_core::run(
            &self.idle_handler,
            &self.error_handler,
            &self.store,
            &self.roles,
            &self.models,
            &self.tools,
        )
    }
}
