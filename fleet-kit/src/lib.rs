//! Shared capabilities for [`fleet_core`].

#![no_std]

extern crate alloc;

mod ports;
mod types;

pub use self::{
    ports::{Queries, QueriesErrorKind},
    types::{
        AgentActivity, AgentUpdates, EventDirection, EventQuery, EventRecord, MessageRecord,
        Preview, PreviewContent, QueryResponse, QueryUpdate, Revision, ToolCallRecord,
        TurnEndEvent, TurnStatus,
    },
};
