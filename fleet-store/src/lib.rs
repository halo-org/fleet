//! Storage for [`fleet_core`].
//!
//! Without optional features, the store exposes [`fleet_core::ClientStore`] and
//! [`fleet_core::RuntimeStore`] and does not depend on `fleet-kit`.
//!
//! The `blobs` feature implements `fleet_kit::Blobs` and resumable image uploads. Payloads
//! are raw values in separate tables, not embedded in event JSON. Published blobs are immutable
//! and retained until the agents holding them are deleted. Image-reference validation and agent
//! grants are maintained with all feature sets.
//!
//! The `queries` feature enables the `fleet-kit` query and collection interfaces,
//! including title/inbox mutations and the store's snooze-management methods.
//! Persisted records, idempotency receipts, and index maintenance are shared by
//! both configurations, so a database can be reopened with or without `queries`.
//!
//! The `roles` feature stores versioned roles and implements [`fleet_core::Roles`] and
//! `fleet_kit::RoleStore`. Role records live in their own table, which is created and preserved
//! with all feature sets.
//!
//! The `notifications` feature claims notifications for completed turns and stores the devices
//! that receive them. A notification is recorded in the transaction that completes a turn with no
//! queued messages, with all feature sets.
//!
//! The `subagents` feature provides scoped `fleet_kit::Agents` access. Hosts must run
//! `Store::reconcile_agent_scopes` on startup and after `Store::agent_scopes_changed` to stop
//! execution whose driving call has ended. Scope records are preserved with all feature sets.
//!
//! The `tool-calls` feature provides `ToolCallAccess` to an agent's own tool calls. Consumed message
//! counts are recorded with all feature sets.

#[cfg(feature = "subagents")]
mod agent_access;
mod agents;
mod blob_records;
#[cfg(feature = "blobs")]
mod blobs;
mod data;
mod deletion;
mod listeners;
mod metadata;
mod notification_records;
#[cfg(feature = "notifications")]
mod notifications;
mod preview;
mod records;
mod release_records;
#[cfg(feature = "queries")]
mod releases;
mod role_records;
#[cfg(feature = "roles")]
mod roles;
mod scope_records;
mod stalls;
mod store;
mod store_error;
#[cfg(feature = "tool-calls")]
mod tool_call_access;
mod worker;

#[cfg(feature = "queries")]
mod queries;

#[cfg(feature = "subagents")]
pub use self::agent_access::AgentAccess;
#[cfg(feature = "blobs")]
pub use self::blobs::{AgentBlobs, MAX_BLOB_BYTES, MAX_UPLOAD_CHUNK_BYTES, Upload};
#[cfg(feature = "notifications")]
pub use self::notifications::NotificationClaim;
#[cfg(feature = "queries")]
pub use self::releases::{WorkspaceRelease, WorkspaceReleaseState};
#[cfg(feature = "tool-calls")]
pub use self::tool_call_access::{ToolCallAccess, ToolCallSnapshot};
pub(crate) use self::{
    listeners::{Listeners, Topic},
    worker::WorkerHandle,
};
pub use self::{store::Store, store_error::StoreError};
