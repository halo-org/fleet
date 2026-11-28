use crate::{
    WasmState,
    bindings::{types, workspace_access},
};
use core::fmt;
use fleet_core::{AgentId, Configured};
use fleet_kit::{
    CapturedOutput, WorkspaceAccess, WorkspaceBinding, WorkspaceContext, WorkspaceExit,
    WorkspaceOutput, WorkspacesErrorKind,
};
use futures_util::future::BoxFuture;
use std::sync::Arc;
use wasmtime::component::{Accessor, HasSelf};

/// Role-driven workspace access available to WebAssembly tool calls.
#[derive(Clone)]
pub struct WasmWorkspaceAccess {
    factory: Arc<dyn Fn(AgentId) -> Arc<dyn AccessObject> + Send + Sync>,
    access: Option<Arc<dyn AccessObject>>,
}

trait AccessObject: Send + Sync {
    fn open(
        &self,
    ) -> BoxFuture<'_, Result<workspace_access::WorkspaceBinding, types::WorkspacesError>>;
    fn read(
        &self,
        binding: workspace_access::WorkspaceBinding,
        path: String,
        limit: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, types::WorkspacesError>>;
    fn write(
        &self,
        binding: workspace_access::WorkspaceBinding,
        file: types::WorkspaceWrite,
    ) -> BoxFuture<'_, Result<(), types::WorkspacesError>>;
    fn exec(
        &self,
        binding: workspace_access::WorkspaceBinding,
        is_retry: bool,
        command: types::WorkspaceCommand,
    ) -> BoxFuture<'_, Result<types::WorkspaceOutput, types::WorkspacesError>>;
}

impl WasmWorkspaceAccess {
    /// Supplies a factory for an acting agent's workspace access.
    pub fn new<W: WorkspaceAccess + Send + Sync + 'static>(
        factory: impl Fn(AgentId) -> W + Send + Sync + 'static,
    ) -> Self {
        Self {
            factory: Arc::new(move |agent| Arc::new(factory(agent))),
            access: None,
        }
    }

    pub(crate) fn for_agent(&self, agent: AgentId) -> Self {
        Self {
            factory: self.factory.clone(),
            access: Some((self.factory)(agent)),
        }
    }
}

impl fmt::Debug for WasmWorkspaceAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmWorkspaceAccess")
            .finish_non_exhaustive()
    }
}

impl<W: WorkspaceAccess + Send + Sync> AccessObject for W {
    fn open(
        &self,
    ) -> BoxFuture<'_, Result<workspace_access::WorkspaceBinding, types::WorkspacesError>> {
        Box::pin(async move {
            let binding = WorkspaceAccess::open(self)
                .await
                .map_err(|source| error(&source))?;
            let id = u128::from(binding.context.id);
            Ok(workspace_access::WorkspaceBinding {
                context: types::WorkspaceContext {
                    workspace: types::Configured {
                        name: binding.context.workspace.name.into(),
                        config: crate::convert::json(&binding.context.workspace.config),
                    },
                    agent_id: binding.context.agent_id.into(),
                    id: types::WorkspaceId {
                        high: u64::try_from(id >> 64).expect("high bits fit"),
                        low: u64::try_from(id & u128::from(u64::MAX)).expect("low bits fit"),
                    },
                },
                directory: binding.directory,
            })
        })
    }

    fn read(
        &self,
        binding: workspace_access::WorkspaceBinding,
        path: String,
        limit: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, types::WorkspacesError>> {
        Box::pin(async move {
            WorkspaceAccess::read(self, &convert_binding(binding)?, &path, limit)
                .await
                .map_err(|source| error(&source))
        })
    }

    fn write(
        &self,
        binding: workspace_access::WorkspaceBinding,
        file: types::WorkspaceWrite,
    ) -> BoxFuture<'_, Result<(), types::WorkspacesError>> {
        Box::pin(async move {
            let file = fleet_kit::WorkspaceWrite {
                path: file.path,
                content: file.content,
                expected: file.expected,
                create_parents: file.create_parents,
            };
            WorkspaceAccess::write(self, &convert_binding(binding)?, &file)
                .await
                .map_err(|source| error(&source))
        })
    }

    fn exec(
        &self,
        binding: workspace_access::WorkspaceBinding,
        is_retry: bool,
        command: types::WorkspaceCommand,
    ) -> BoxFuture<'_, Result<types::WorkspaceOutput, types::WorkspacesError>> {
        Box::pin(async move {
            let command = fleet_kit::WorkspaceCommand {
                id: command.id,
                program: command.program,
                args: command.args,
                env: command.env.into_iter().collect(),
                cwd: command.cwd,
                stdin: command.stdin,
                timeout_ms: command.timeout_ms,
                output_limit: command.output_limit,
            };
            let output =
                WorkspaceAccess::exec(self, &convert_binding(binding)?, is_retry, &command)
                    .await
                    .map_err(|source| error(&source))?;
            Ok(convert_output(output))
        })
    }
}

impl workspace_access::Host for WasmState {}

impl workspace_access::HostWithStore<WasmState> for HasSelf<WasmState> {
    async fn open(
        store: &Accessor<WasmState, Self>,
    ) -> Result<workspace_access::WorkspaceBinding, types::WorkspacesError> {
        host(store)?.open().await
    }
    async fn read(
        store: &Accessor<WasmState, Self>,
        binding: workspace_access::WorkspaceBinding,
        path: String,
        limit: u64,
    ) -> Result<Vec<u8>, types::WorkspacesError> {
        host(store)?.read(binding, path, limit).await
    }
    async fn write(
        store: &Accessor<WasmState, Self>,
        binding: workspace_access::WorkspaceBinding,
        file: types::WorkspaceWrite,
    ) -> Result<(), types::WorkspacesError> {
        host(store)?.write(binding, file).await
    }
    async fn exec(
        store: &Accessor<WasmState, Self>,
        binding: workspace_access::WorkspaceBinding,
        is_retry: bool,
        command: types::WorkspaceCommand,
    ) -> Result<types::WorkspaceOutput, types::WorkspacesError> {
        host(store)?.exec(binding, is_retry, command).await
    }
}

fn host(
    store: &Accessor<WasmState, HasSelf<WasmState>>,
) -> Result<Arc<dyn AccessObject>, types::WorkspacesError> {
    store
        .with(|mut access| access.get().workspace_access())
        .and_then(|access| access.access)
        .ok_or_else(|| types::WorkspacesError {
            kind: types::WorkspacesErrorKind::Other,
            message: "workspace access is not available in this operation".into(),
        })
}

fn convert_binding(
    binding: workspace_access::WorkspaceBinding,
) -> Result<WorkspaceBinding, types::WorkspacesError> {
    Ok(WorkspaceBinding {
        context: convert_context(binding.context)?,
        directory: binding.directory,
    })
}

fn convert_context(
    context: types::WorkspaceContext,
) -> Result<WorkspaceContext, types::WorkspacesError> {
    Ok(WorkspaceContext {
        workspace: Configured {
            name: context.workspace.name.into(),
            config: serde_json::from_str(&context.workspace.config)
                .map_err(|source| json_error(&source))?,
        },
        agent_id: context.agent_id.into(),
        id: ((u128::from(context.id.high) << 64) | u128::from(context.id.low)).into(),
    })
}

fn convert_output(output: WorkspaceOutput) -> types::WorkspaceOutput {
    let captured = |output: CapturedOutput| types::CapturedOutput {
        head: output.head,
        omitted: output.omitted,
        tail: output.tail,
    };
    types::WorkspaceOutput {
        exit: match output.exit {
            WorkspaceExit::Code(code) => types::WorkspaceExit::Code(code),
            WorkspaceExit::Terminated => types::WorkspaceExit::Terminated,
            WorkspaceExit::TimedOut => types::WorkspaceExit::TimedOut,
        },
        stdout: captured(output.stdout),
        stderr: captured(output.stderr),
    }
}

fn json_error(error: &serde_json::Error) -> types::WorkspacesError {
    types::WorkspacesError {
        kind: types::WorkspacesErrorKind::InvalidConfiguration,
        message: error.to_string(),
    }
}

fn error(error: &impl fleet_core::Error<WorkspacesErrorKind>) -> types::WorkspacesError {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    types::WorkspacesError {
        kind: match error.kind() {
            WorkspacesErrorKind::NotFound => types::WorkspacesErrorKind::NotFound,
            WorkspacesErrorKind::InvalidConfiguration => {
                types::WorkspacesErrorKind::InvalidConfiguration
            }
            WorkspacesErrorKind::FileNotFound => types::WorkspacesErrorKind::FileNotFound,
            WorkspacesErrorKind::FileTooLarge => types::WorkspacesErrorKind::FileTooLarge,
            WorkspacesErrorKind::FileChanged => types::WorkspacesErrorKind::FileChanged,
            WorkspacesErrorKind::UnsafeToRetry => types::WorkspacesErrorKind::UnsafeToRetry,
            WorkspacesErrorKind::Other => types::WorkspacesErrorKind::Other,
        },
        message,
    }
}
