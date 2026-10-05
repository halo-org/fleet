//! A simulator for testing [`fleet_core`].
//!
//! Simulators share a [`Scheduler`] for delays and faults. Replaying a run requires the same seeds,
//! inputs, and order of calls and polls.

mod content;
mod error_handler;
mod idle_handler;
mod models;
mod probability;
mod random_error;
mod roles;
mod scheduler;
mod store;
mod tools;

pub use self::{
    error_handler::ErrorHandlerSimulator,
    idle_handler::IdleHandlerSimulator,
    models::{ModelsSimulator, ModelsSimulatorError},
    roles::{RolesSimulator, RolesSimulatorError},
    scheduler::Scheduler,
    store::{AgentSnapshot, StoreSimulator, StoreSimulatorError, ToolCallSnapshot},
    tools::{ToolsSimulator, ToolsSimulatorError},
};
pub(crate) use self::{
    probability::Probability,
    random_error::RandomError,
    scheduler::{TrySuspendConfig, WrapConfig},
};
