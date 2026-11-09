//! Storage for [`fleet_core`].
//!
//! The `queries` feature enables the `fleet-kit` individual-agent and collection
//! interfaces. Every writer maintains the indexes, regardless of this feature.

mod agents;
mod data;
mod notifications;
mod preview;
mod records;
mod store;
mod store_error;
mod worker;

#[cfg(feature = "queries")]
mod queries;

pub(crate) use self::{
    notifications::{Notifications, Topic},
    worker::WorkerHandle,
};
pub use self::{store::Store, store_error::StoreError};
