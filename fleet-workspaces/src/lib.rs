//! Role-driven workspace resolution and directory views.

use core::{error::Error, fmt};
use fleet_core::{AgentId, Configured, RoleInstance, Roles};
use fleet_kit::{
    Agents, Extension as _, RoleWorkspace, WorkspaceAccess, WorkspaceBinding, WorkspaceCommand,
    WorkspaceContext, WorkspaceOutput, WorkspaceResolvers, WorkspaceSource, WorkspaceWrite,
    Workspaces, WorkspacesErrorKind,
};
use std::collections::BTreeSet;

/// Workspace access for one acting agent.
pub struct AgentWorkspaces<A, R, W> {
    agent: AgentId,
    agents: A,
    roles: R,
    workspaces: W,
}

/// A workspace resolution or access error.
#[derive(Debug)]
pub struct WorkspaceError {
    kind: WorkspacesErrorKind,
    message: &'static str,
    source: Option<Box<dyn Error + Send>>,
}

impl<A, R, W> AgentWorkspaces<A, R, W> {
    /// Uses the supplied, already authorized agent and workspace providers.
    pub const fn new(agent: AgentId, agents: A, roles: R, workspaces: W) -> Self {
        Self {
            agent,
            agents,
            roles,
            workspaces,
        }
    }

    fn check(&self, binding: &WorkspaceBinding) -> Result<(), WorkspaceError> {
        if binding.context.agent_id != self.agent {
            return Err(invalid("workspace belongs to another acting agent"));
        }
        directory(binding.directory.as_deref())
    }
}

impl<A, R, W> AgentWorkspaces<A, R, W>
where
    A: Agents + Sync,
    R: Roles<Error: Send> + Sync,
    W: WorkspaceResolvers<Error: Send> + Sync,
{
    /// Resolves the environment the agent owns under its current role, without opening it.
    ///
    /// The agent owns the environment of a provider workspace configured without an ID. Returns
    /// `None` when the role configures no workspace, a shared ID or a resolver, such as one
    /// selecting a parent's workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the role or its workspace cannot be loaded.
    pub async fn owned_environment(&self) -> Result<Option<WorkspaceContext>, WorkspaceError> {
        let Some(workspace) = self.workspace(self.agent).await? else {
            return Ok(None);
        };
        if workspace.id.is_some() {
            return Ok(None);
        }
        let selection = Configured {
            name: workspace.name,
            config: workspace.config,
        };
        let resolver = WorkspaceResolvers::get(&self.workspaces, &selection.name)
            .await
            .map_err(|source| provider_error("could not look up workspace resolver", source))?;
        Ok(resolver.is_none().then(|| WorkspaceContext {
            workspace: selection,
            agent_id: self.agent,
            id: u128::from(self.agent).into(),
        }))
    }

    async fn workspace(&self, subject: AgentId) -> Result<Option<RoleWorkspace>, WorkspaceError> {
        let agent = self
            .agents
            .get(subject)
            .await
            .map_err(|source| failure("could not read workspace agent", source))?;
        let role = self
            .roles
            .get(&agent.role.name)
            .await
            .map_err(|source| failure("could not load workspace role", source))?
            .ok_or_else(|| invalid("workspace role does not exist"))?;
        let extensions = RoleInstance::new(&role, &agent.role.config)
            .extensions()
            .map_err(|source| failure("could not configure workspace role", source))?;
        RoleWorkspace::get(&extensions)
            .map_err(|source| failure("could not read role workspace", source))
    }
}

impl<A, R, W> WorkspaceAccess for AgentWorkspaces<A, R, W>
where
    A: Agents + Sync,
    R: Roles<Error: Send> + Sync,
    W: Workspaces<Error: Send> + WorkspaceResolvers<Error: Send> + Sync,
{
    type Error = WorkspaceError;

    async fn open(&self) -> Result<WorkspaceBinding, WorkspaceError> {
        let mut subject = self.agent;
        let mut visited = BTreeSet::new();
        let mut directories = Vec::new();
        let context = loop {
            if !visited.insert(subject) {
                return Err(invalid("workspace references contain a cycle"));
            }
            let workspace = self
                .workspace(subject)
                .await?
                .ok_or_else(|| invalid("no workspace is configured in the role"))?;
            let selection = Configured {
                name: workspace.name,
                config: workspace.config,
            };
            let resolver = WorkspaceResolvers::get(&self.workspaces, &selection.name)
                .await
                .map_err(|source| provider_error("could not look up workspace resolver", source))?;
            if resolver.is_none() {
                break WorkspaceContext {
                    workspace: selection,
                    agent_id: self.agent,
                    id: workspace.id.unwrap_or_else(|| u128::from(subject).into()),
                };
            }
            if workspace.id.is_some() {
                return Err(invalid(
                    "a workspace resolver selects its own environment ID",
                ));
            }
            let resolution = self
                .workspaces
                .resolve(subject, &selection)
                .await
                .map_err(|source| provider_error("could not resolve workspace", source))?;
            directory(resolution.directory.as_deref())?;
            if let Some(directory) = resolution
                .directory
                .filter(|directory| !directory.is_empty())
            {
                directories.push(directory);
            }
            match resolution.source {
                WorkspaceSource::Agent(agent) => subject = agent,
                WorkspaceSource::Environment(workspace, id) => {
                    break WorkspaceContext {
                        workspace,
                        id,
                        agent_id: self.agent,
                    };
                }
            }
        };
        self.workspaces
            .open(&context)
            .await
            .map_err(|source| provider_error("could not open workspace", source))?;
        let directory = if directories.is_empty() {
            None
        } else {
            Some(directories.into_iter().rev().collect::<Vec<_>>().join("/"))
        };
        Ok(WorkspaceBinding { context, directory })
    }

    async fn read(
        &self,
        binding: &WorkspaceBinding,
        path: &str,
        limit: u64,
    ) -> Result<Vec<u8>, WorkspaceError> {
        self.check(binding)?;
        self.workspaces
            .read(&binding.context, &view_path(binding, path), limit)
            .await
            .map_err(|source| provider_error("could not read workspace file", source))
    }

    async fn write(
        &self,
        binding: &WorkspaceBinding,
        file: &WorkspaceWrite,
    ) -> Result<(), WorkspaceError> {
        self.check(binding)?;
        let file = WorkspaceWrite {
            path: view_path(binding, &file.path),
            ..file.clone()
        };
        self.workspaces
            .write(&binding.context, &file)
            .await
            .map_err(|source| provider_error("could not write workspace file", source))
    }

    async fn exec(
        &self,
        binding: &WorkspaceBinding,
        is_retry: bool,
        command: &WorkspaceCommand,
    ) -> Result<WorkspaceOutput, WorkspaceError> {
        self.check(binding)?;
        let cwd = command.cwd.as_ref().map_or_else(
            || binding.directory.clone(),
            |cwd| Some(view_path(binding, cwd)),
        );
        let command = WorkspaceCommand {
            cwd,
            ..command.clone()
        };
        self.workspaces
            .exec(&binding.context, is_retry, &command)
            .await
            .map_err(|source| provider_error("could not execute workspace command", source))
    }
}

fn directory(directory: Option<&str>) -> Result<(), WorkspaceError> {
    if directory.is_some_and(|directory| directory.starts_with('/') || directory.contains('\0')) {
        return Err(invalid(
            "workspace view directory must be relative and contain no NUL",
        ));
    }
    Ok(())
}

fn view_path(binding: &WorkspaceBinding, path: &str) -> String {
    match &binding.directory {
        Some(directory) if !directory.is_empty() && !path.starts_with('/') => {
            format!("{directory}/{path}")
        }
        _ => path.into(),
    }
}

fn invalid(message: &'static str) -> WorkspaceError {
    WorkspaceError {
        kind: WorkspacesErrorKind::InvalidConfiguration,
        message,
        source: None,
    }
}

fn failure(message: &'static str, source: impl Error + Send + 'static) -> WorkspaceError {
    WorkspaceError {
        kind: WorkspacesErrorKind::Other,
        message,
        source: Some(Box::new(source)),
    }
}

fn provider_error(
    message: &'static str,
    source: impl fleet_core::Error<WorkspacesErrorKind> + Send + 'static,
) -> WorkspaceError {
    WorkspaceError {
        kind: source.kind(),
        message,
        source: Some(Box::new(source)),
    }
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl Error for WorkspaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

impl fleet_core::Error<WorkspacesErrorKind> for WorkspaceError {
    fn kind(&self) -> WorkspacesErrorKind {
        self.kind
    }
}
