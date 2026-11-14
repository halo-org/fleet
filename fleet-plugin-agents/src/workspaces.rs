use crate::{
    AgentId, Agents, PARENT_WORKSPACE,
    bindings::{
        exports::fleet::providers::workspaces::{
            Guest, Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceExit, WorkspaceOutput,
            WorkspaceWrite, WorkspacesError, WorkspacesErrorKind,
        },
        fleet::providers::{agents, types::Configured, workspaces},
    },
};
use fleet_kit::RoleWorkspace;
use serde::Deserialize as _;
use serde_json::{Map, Value};

impl Guest for Agents {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn get(name: String) -> Result<Option<Workspace>, WorkspacesError> {
        if name != "parent" {
            return Ok(None);
        }
        Ok(Some(Workspace {
            name,
            extensions: serde_json::json!({
                "fleet.title": "Parent workspace",
                "fleet.description": "Shares the workspace of the agent that created this one.",
            })
            .to_string(),
        }))
    }

    async fn open(context: WorkspaceContext) -> Result<(), WorkspacesError> {
        workspaces::open(forward(&context).await?)
            .await
            .map_err(Into::into)
    }

    async fn read(
        context: WorkspaceContext,
        path: String,
        limit: u64,
    ) -> Result<Vec<u8>, WorkspacesError> {
        workspaces::read(forward(&context).await?, path, limit)
            .await
            .map_err(Into::into)
    }

    async fn write(context: WorkspaceContext, file: WorkspaceWrite) -> Result<(), WorkspacesError> {
        let file = workspaces::WorkspaceWrite {
            path: file.path,
            content: file.content,
            expected: file.expected,
            create_parents: file.create_parents,
        };
        workspaces::write(forward(&context).await?, file)
            .await
            .map_err(Into::into)
    }

    async fn exec(
        context: WorkspaceContext,
        is_retry: bool,
        command: WorkspaceCommand,
    ) -> Result<WorkspaceOutput, WorkspacesError> {
        let command = workspaces::WorkspaceCommand {
            id: command.id,
            program: command.program,
            args: command.args,
            env: command.env,
            cwd: command.cwd,
            stdin: command.stdin,
            timeout_ms: command.timeout_ms,
            output_limit: command.output_limit,
        };
        let output = workspaces::exec(forward(&context).await?, is_retry, command).await?;
        Ok(WorkspaceOutput {
            exit: match output.exit {
                workspaces::WorkspaceExit::Code(code) => WorkspaceExit::Code(code),
                workspaces::WorkspaceExit::Terminated => WorkspaceExit::Terminated,
                workspaces::WorkspaceExit::TimedOut => WorkspaceExit::TimedOut,
            },
            stdout: output.stdout,
            stderr: output.stderr,
            stdout_truncated: output.stdout_truncated,
            stderr_truncated: output.stderr_truncated,
        })
    }
}

/// Returns the workspace an agent inherited from its parent.
pub async fn parent(agent: AgentId) -> Result<Option<RoleWorkspace>, WorkspacesError> {
    let agent = agents::get(agent).await.map_err(|error| WorkspacesError {
        kind: match error.kind {
            agents::AgentsErrorKind::AgentNotFound => WorkspacesErrorKind::NotFound,
            _ => WorkspacesErrorKind::Other,
        },
        message: error.message,
    })?;
    let extensions: Map<String, Value> =
        serde_json::from_str(&agent.extensions).map_err(invalid)?;
    extensions
        .get(PARENT_WORKSPACE)
        .map(|workspace| RoleWorkspace::deserialize(workspace).map_err(invalid))
        .transpose()
}

async fn forward(
    context: &WorkspaceContext,
) -> Result<workspaces::WorkspaceContext, WorkspacesError> {
    if context.workspace.name != "parent" {
        return Err(WorkspacesError {
            kind: WorkspacesErrorKind::NotFound,
            message: format!("unknown workspace: {}", context.workspace.name),
        });
    }
    let workspace = parent(context.agent_id)
        .await?
        .ok_or_else(|| WorkspacesError {
            kind: WorkspacesErrorKind::InvalidConfiguration,
            message: "the agent has no parent workspace".into(),
        })?;
    let id = workspace
        .id
        .map_or_else(|| u128::from(context.agent_id), u128::from);
    Ok(workspaces::WorkspaceContext {
        workspace: Configured {
            name: workspace.name.into(),
            config: Value::Object(workspace.config).to_string(),
        },
        agent_id: context.agent_id,
        id: workspaces::WorkspaceId {
            high: u64::try_from(id >> 64).expect("high bits fit in u64"),
            low: u64::try_from(id & u128::from(u64::MAX)).expect("low bits fit in u64"),
        },
    })
}

fn invalid(error: impl core::fmt::Display) -> WorkspacesError {
    WorkspacesError {
        kind: WorkspacesErrorKind::InvalidConfiguration,
        message: format!("invalid parent workspace: {error}"),
    }
}

impl From<workspaces::WorkspacesError> for WorkspacesError {
    fn from(error: workspaces::WorkspacesError) -> Self {
        Self {
            kind: match error.kind {
                workspaces::WorkspacesErrorKind::NotFound => WorkspacesErrorKind::NotFound,
                workspaces::WorkspacesErrorKind::InvalidConfiguration => {
                    WorkspacesErrorKind::InvalidConfiguration
                }
                workspaces::WorkspacesErrorKind::FileNotFound => WorkspacesErrorKind::FileNotFound,
                workspaces::WorkspacesErrorKind::FileTooLarge => WorkspacesErrorKind::FileTooLarge,
                workspaces::WorkspacesErrorKind::FileChanged => WorkspacesErrorKind::FileChanged,
                workspaces::WorkspacesErrorKind::UnsafeToRetry => {
                    WorkspacesErrorKind::UnsafeToRetry
                }
                workspaces::WorkspacesErrorKind::Other => WorkspacesErrorKind::Other,
            },
            message: error.message,
        }
    }
}
