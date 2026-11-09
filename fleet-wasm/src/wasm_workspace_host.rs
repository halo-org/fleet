use crate::{WasmState, bindings::workspace_host::fleet::providers::workspaces};
use core::fmt;
use fleet_core::Configured;
use fleet_kit::{WorkspaceContext, WorkspaceExit, Workspaces, WorkspacesErrorKind};
use futures_util::future::BoxFuture;
use std::sync::Arc;
use wasmtime::component::{Accessor, HasSelf};

/// Workspaces available to a WebAssembly provider.
#[derive(Clone)]
pub struct WasmWorkspaceHost(Arc<dyn WorkspacesObject>);

trait WorkspacesObject: Send + Sync {
    fn get(
        &self,
        name: String,
    ) -> BoxFuture<'_, Result<Option<workspaces::Workspace>, workspaces::WorkspacesError>>;
    fn open(
        &self,
        context: workspaces::WorkspaceContext,
    ) -> BoxFuture<'_, Result<(), workspaces::WorkspacesError>>;
    fn read(
        &self,
        context: workspaces::WorkspaceContext,
        path: String,
        limit: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, workspaces::WorkspacesError>>;
    fn write(
        &self,
        context: workspaces::WorkspaceContext,
        file: workspaces::WorkspaceWrite,
    ) -> BoxFuture<'_, Result<(), workspaces::WorkspacesError>>;
    fn exec(
        &self,
        context: workspaces::WorkspaceContext,
        is_retry: bool,
        command: workspaces::WorkspaceCommand,
    ) -> BoxFuture<'_, Result<workspaces::WorkspaceOutput, workspaces::WorkspacesError>>;
}

impl WasmWorkspaceHost {
    /// Wraps a workspace provider.
    pub fn new(workspaces: impl Workspaces + Send + Sync + 'static) -> Self {
        Self(Arc::new(workspaces))
    }
}

impl fmt::Debug for WasmWorkspaceHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmWorkspaceHost").finish_non_exhaustive()
    }
}

impl<W: Workspaces + Send + Sync> WorkspacesObject for W {
    fn get(
        &self,
        name: String,
    ) -> BoxFuture<'_, Result<Option<workspaces::Workspace>, workspaces::WorkspacesError>> {
        Box::pin(async move {
            Workspaces::get(self, &name.into())
                .await
                .map_err(|source| error(&source))?
                .map(|workspace| {
                    Ok(workspaces::Workspace {
                        name: workspace.name.into(),
                        extensions: serde_json::to_string(&workspace.extensions)
                            .map_err(|source| json_error(&source))?,
                    })
                })
                .transpose()
        })
    }

    fn open(
        &self,
        context: workspaces::WorkspaceContext,
    ) -> BoxFuture<'_, Result<(), workspaces::WorkspacesError>> {
        Box::pin(async move {
            Workspaces::open(self, &convert_context(context)?)
                .await
                .map_err(|source| error(&source))
        })
    }

    fn read(
        &self,
        context: workspaces::WorkspaceContext,
        path: String,
        limit: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, workspaces::WorkspacesError>> {
        Box::pin(async move {
            Workspaces::read(self, &convert_context(context)?, &path, limit)
                .await
                .map_err(|source| error(&source))
        })
    }

    fn write(
        &self,
        context: workspaces::WorkspaceContext,
        file: workspaces::WorkspaceWrite,
    ) -> BoxFuture<'_, Result<(), workspaces::WorkspacesError>> {
        Box::pin(async move {
            let file = fleet_kit::WorkspaceWrite {
                path: file.path,
                content: file.content,
                expected: file.expected,
                create_parents: file.create_parents,
            };
            Workspaces::write(self, &convert_context(context)?, &file)
                .await
                .map_err(|source| error(&source))
        })
    }

    fn exec(
        &self,
        context: workspaces::WorkspaceContext,
        is_retry: bool,
        command: workspaces::WorkspaceCommand,
    ) -> BoxFuture<'_, Result<workspaces::WorkspaceOutput, workspaces::WorkspacesError>> {
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
            let output = Workspaces::exec(self, &convert_context(context)?, is_retry, &command)
                .await
                .map_err(|source| error(&source))?;
            Ok(workspaces::WorkspaceOutput {
                exit: match output.exit {
                    WorkspaceExit::Code(code) => workspaces::WorkspaceExit::Code(code),
                    WorkspaceExit::Terminated => workspaces::WorkspaceExit::Terminated,
                    WorkspaceExit::TimedOut => workspaces::WorkspaceExit::TimedOut,
                },
                stdout: output.stdout,
                stderr: output.stderr,
                stdout_truncated: output.stdout_truncated,
                stderr_truncated: output.stderr_truncated,
            })
        })
    }
}

impl workspaces::Host for WasmState {}

impl workspaces::HostWithStore<WasmState> for HasSelf<WasmState> {
    async fn get(
        store: &Accessor<WasmState, Self>,
        name: String,
    ) -> Result<Option<workspaces::Workspace>, workspaces::WorkspacesError> {
        host(store)?.0.get(name).await
    }

    async fn open(
        store: &Accessor<WasmState, Self>,
        context: workspaces::WorkspaceContext,
    ) -> Result<(), workspaces::WorkspacesError> {
        host(store)?.0.open(context).await
    }

    async fn read(
        store: &Accessor<WasmState, Self>,
        context: workspaces::WorkspaceContext,
        path: String,
        limit: u64,
    ) -> Result<Vec<u8>, workspaces::WorkspacesError> {
        host(store)?.0.read(context, path, limit).await
    }

    async fn write(
        store: &Accessor<WasmState, Self>,
        context: workspaces::WorkspaceContext,
        file: workspaces::WorkspaceWrite,
    ) -> Result<(), workspaces::WorkspacesError> {
        host(store)?.0.write(context, file).await
    }

    async fn exec(
        store: &Accessor<WasmState, Self>,
        context: workspaces::WorkspaceContext,
        is_retry: bool,
        command: workspaces::WorkspaceCommand,
    ) -> Result<workspaces::WorkspaceOutput, workspaces::WorkspacesError> {
        host(store)?.0.exec(context, is_retry, command).await
    }
}

fn host(
    store: &Accessor<WasmState, HasSelf<WasmState>>,
) -> Result<WasmWorkspaceHost, workspaces::WorkspacesError> {
    store
        .with(|mut access| access.get().workspace_host())
        .ok_or_else(|| workspaces::WorkspacesError {
            kind: workspaces::WorkspacesErrorKind::Other,
            message: "workspaces are not available".into(),
        })
}

fn convert_context(
    context: workspaces::WorkspaceContext,
) -> Result<WorkspaceContext, workspaces::WorkspacesError> {
    Ok(WorkspaceContext {
        workspace: Configured {
            name: context.workspace.name.into(),
            config: serde_json::from_str(&context.workspace.config)
                .map_err(|source| json_error(&source))?,
        },
        id: ((u128::from(context.id.high) << 64) | u128::from(context.id.low)).into(),
    })
}

fn json_error(error: &serde_json::Error) -> workspaces::WorkspacesError {
    workspaces::WorkspacesError {
        kind: workspaces::WorkspacesErrorKind::InvalidConfiguration,
        message: error.to_string(),
    }
}

fn error(error: &impl fleet_core::Error<WorkspacesErrorKind>) -> workspaces::WorkspacesError {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    workspaces::WorkspacesError {
        kind: match error.kind() {
            WorkspacesErrorKind::NotFound => workspaces::WorkspacesErrorKind::NotFound,
            WorkspacesErrorKind::InvalidConfiguration => {
                workspaces::WorkspacesErrorKind::InvalidConfiguration
            }
            WorkspacesErrorKind::FileNotFound => workspaces::WorkspacesErrorKind::FileNotFound,
            WorkspacesErrorKind::FileTooLarge => workspaces::WorkspacesErrorKind::FileTooLarge,
            WorkspacesErrorKind::FileChanged => workspaces::WorkspacesErrorKind::FileChanged,
            WorkspacesErrorKind::UnsafeToRetry => workspaces::WorkspacesErrorKind::UnsafeToRetry,
            WorkspacesErrorKind::Other => workspaces::WorkspacesErrorKind::Other,
        },
        message,
    }
}
