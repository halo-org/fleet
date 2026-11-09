//! Shared capabilities for [`fleet_core`].

#![no_std]

extern crate alloc;

mod agents;
mod ports;
mod types;

pub use self::{
    agents::{
        AgentCollection, AgentPosition, AgentSummary, AgentsPage, AgentsQuery, AgentsUpdates,
        CollectionRevision, InboxAction, InboxFilter, InboxState,
    },
    ports::{Queries, QueriesErrorKind},
    types::{
        AgentActivity, AgentUpdates, EventDirection, EventQuery, EventRecord, MessageRecord,
        Preview, PreviewContent, QueryResponse, QueryUpdate, Revision, ToolCallRecord,
        TurnEndEvent, TurnStatus,
    },
};
