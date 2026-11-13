//! Storage for [`fleet_core`].
//!
//! Without optional features, the store exposes [`fleet_core::ClientStore`] and
//! [`fleet_core::RuntimeStore`] and does not depend on `fleet-kit`.
//!
//! The `blobs` feature implements `fleet_kit::Blobs` and resumable image uploads. Payloads
//! are raw values in separate tables, not embedded in event JSON. Published blobs are immutable
//! and retained; image-reference validation and agent grants are maintained with all feature sets.
//!
//! The `queries` feature enables the `fleet-kit` query and collection interfaces,
//! including title/inbox mutations and the store's snooze-management methods.
//! Persisted records, idempotency receipts, and index maintenance are shared by
//! both configurations, so a database can be reopened with or without `queries`.
//!
//! The `roles` feature stores versioned roles and implements [`fleet_core::Roles`] and
//! `fleet_kit::RoleStore`. Role records live in their own table, which is created and preserved
//! with all feature sets.

mod agents;
mod blob_records;
#[cfg(feature = "blobs")]
mod blobs;
mod data;
mod metadata;
mod notifications;
mod preview;
mod records;
mod role_records;
#[cfg(feature = "roles")]
mod roles;
mod store;
mod store_error;
mod worker;

#[cfg(feature = "queries")]
mod queries;

#[cfg(feature = "blobs")]
pub use self::blobs::{AgentBlobs, MAX_BLOB_BYTES, MAX_UPLOAD_CHUNK_BYTES, Upload};
pub(crate) use self::{
    notifications::{Notifications, Topic},
    worker::WorkerHandle,
};
pub use self::{store::Store, store_error::StoreError};
