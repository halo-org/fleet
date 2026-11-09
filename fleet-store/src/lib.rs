//! Storage for [`fleet_core`].
//!
//! Without optional features, the store exposes [`fleet_core::ClientStore`] and
//! [`fleet_core::RuntimeStore`] and does not depend on `fleet-kit`.
//!
//! The `queries` feature enables the `fleet-kit` query and collection interfaces,
//! including title/inbox mutations and the store's snooze-management methods.
//! Persisted records, idempotency receipts, and index maintenance are shared by
//! both configurations, so a database can be reopened with or without `queries`.

mod agents;
mod data;
mod metadata;
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
