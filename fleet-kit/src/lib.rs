//! Shared capabilities for [`fleet_core`].

#![no_std]

extern crate alloc;

mod agent_access;
mod agents;
mod authentication;
mod blobs;
mod extensions;
mod ports;
mod roles;
mod types;
mod workspace_access;
mod workspaces;

pub use self::{
    agent_access::{Agents, AgentsErrorKind},
    agents::{
        AgentCollection, AgentParent, AgentPosition, AgentSummary, AgentsPage, AgentsQuery,
        AgentsUpdates, CollectionRevision, InboxAction, InboxFilter, InboxState, agent_pinned,
        agent_title,
    },
    authentication::{
        AuthenticationKind, AuthenticationMethod, Connection, Credential, Credentials, OAuthConfig,
        OAuthRegistration,
    },
    blobs::{BlobMetadata, Blobs, BlobsErrorKind, ImageDimensions, NoBlobs},
    extensions::Extension,
    ports::{Queries, QueriesErrorKind},
    roles::{RoleStore, RoleStoreErrorKind, RoleVersion, StoredRole},
    types::{
        AgentActivity, AgentMetadata, AgentUpdates, EventDirection, EventQuery, EventRecord,
        MessageRecord, Preview, PreviewContent, QueryResponse, QueryUpdate, Revision,
        ToolCallRecord, TurnEndEvent, TurnStatus,
    },
    workspace_access::{
        WorkspaceAccess, WorkspaceBinding, WorkspaceResolution, WorkspaceResolvers, WorkspaceSource,
    },
    workspaces::{
        RoleWorkspace, Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceExit, WorkspaceId,
        WorkspaceName, WorkspaceOutput, WorkspaceWrite, Workspaces, WorkspacesErrorKind,
    },
};
