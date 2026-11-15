use alloc::{string::String, vec::Vec};
use fleet_core::{AgentId, Configured};

use crate::{
    Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceId, WorkspaceName, WorkspaceOutput,
    WorkspaceWrite, WorkspacesErrorKind,
};

/// A concrete workspace and a view into its environment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceBinding {
    /// The environment and acting agent.
    pub context: WorkspaceContext,
    /// A relative directory within the workspace, or `None` for the provider's default.
    ///
    /// A view changes relative paths and the default command directory. It does not isolate files.
    pub directory: Option<String>,
}

/// The source selected by a workspace resolver.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkspaceSource {
    /// Loads the agent's current role, parameters and workspace.
    Agent(AgentId),
    /// Opens a concrete environment without loading another role.
    Environment(Configured<WorkspaceName>, WorkspaceId),
}

/// A workspace source and a view relative to that source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceResolution {
    /// The source to resolve.
    pub source: WorkspaceSource,
    /// A relative directory applied after resolving the source.
    pub directory: Option<String>,
}

/// Selects workspace sources without providing file or process access.
pub trait WorkspaceResolvers {
    /// Provider error.
    type Error: fleet_core::Error<WorkspacesErrorKind> + 'static;

    /// Returns a resolver's metadata.
    fn get(
        &self,
        name: &WorkspaceName,
    ) -> impl Future<Output = Result<Option<Workspace>, Self::Error>> + Send;

    /// Selects a source for the agent whose role is being resolved.
    ///
    /// The subject may differ from the acting agent. Resolution does not grant access to the
    /// source; the host checks references and retains the original acting agent.
    fn resolve(
        &self,
        subject: AgentId,
        workspace: &Configured<WorkspaceName>,
    ) -> impl Future<Output = Result<WorkspaceResolution, Self::Error>> + Send;
}

/// Workspace access granted to an acting agent.
pub trait WorkspaceAccess {
    /// Provider error.
    type Error: fleet_core::Error<WorkspacesErrorKind> + 'static;

    /// Resolves the agent's current workspace and opens its environment.
    ///
    /// Retain the binding for an operation, including read-modify-write. A subsequent open loads
    /// current configuration again. Cancellation and retry follow [`crate::Workspaces::open`].
    fn open(&self) -> impl Future<Output = Result<WorkspaceBinding, Self::Error>> + Send;

    /// Reads through the bound view. See [`crate::Workspaces::read`].
    fn read(
        &self,
        binding: &WorkspaceBinding,
        path: &str,
        limit: u64,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send;

    /// Writes through the bound view. See [`crate::Workspaces::write`].
    fn write(
        &self,
        binding: &WorkspaceBinding,
        file: &WorkspaceWrite,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Executes through the bound view. See [`crate::Workspaces::exec`].
    fn exec(
        &self,
        binding: &WorkspaceBinding,
        is_retry: bool,
        command: &WorkspaceCommand,
    ) -> impl Future<Output = Result<WorkspaceOutput, Self::Error>> + Send;
}
