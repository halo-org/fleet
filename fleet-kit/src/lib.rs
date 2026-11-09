//! Shared capabilities for [`fleet_core`].

#![no_std]

extern crate alloc;

mod agents;
mod authentication;
mod ports;
mod types;
mod workspaces;

pub use self::{
    agents::{
        AgentCollection, AgentPosition, AgentSummary, AgentsPage, AgentsQuery, AgentsUpdates,
        CollectionRevision, InboxAction, InboxFilter, InboxState, agent_title,
    },
    authentication::{
        AuthenticationKind, AuthenticationMethod, Connection, Credential, Credentials, OAuthConfig,
        OAuthRegistration,
    },
    ports::{Queries, QueriesErrorKind},
    types::{
        AgentActivity, AgentMetadata, AgentUpdates, EventDirection, EventQuery, EventRecord,
        MessageRecord, Preview, PreviewContent, QueryResponse, QueryUpdate, Revision,
        ToolCallRecord, TurnEndEvent, TurnStatus,
    },
    workspaces::{
        Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceExit, WorkspaceId, WorkspaceName,
        WorkspaceOutput, WorkspaceWrite, Workspaces, WorkspacesErrorKind,
    },
};
