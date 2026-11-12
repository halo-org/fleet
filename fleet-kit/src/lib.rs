//! Shared capabilities for [`fleet_core`].

#![no_std]

extern crate alloc;

mod agents;
mod authentication;
mod blobs;
mod extensions;
mod ports;
mod types;
mod workspaces;

pub use self::{
    agents::{
        AgentCollection, AgentPosition, AgentSummary, AgentsPage, AgentsQuery, AgentsUpdates,
        CollectionRevision, InboxAction, InboxFilter, InboxState, agent_pinned, agent_title,
    },
    authentication::{
        AuthenticationKind, AuthenticationMethod, Connection, Credential, Credentials, OAuthConfig,
        OAuthRegistration,
    },
    blobs::{BlobMetadata, Blobs, BlobsErrorKind, ImageDimensions, NoBlobs},
    extensions::Extension,
    ports::{Queries, QueriesErrorKind},
    types::{
        AgentActivity, AgentMetadata, AgentUpdates, EventDirection, EventQuery, EventRecord,
        MessageRecord, Preview, PreviewContent, QueryResponse, QueryUpdate, Revision,
        ToolCallRecord, TurnEndEvent, TurnStatus,
    },
    workspaces::{
        RoleWorkspace, Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceExit, WorkspaceId,
        WorkspaceName, WorkspaceOutput, WorkspaceWrite, Workspaces, WorkspacesErrorKind,
    },
};
