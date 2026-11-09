use crate::{
    WasmCredentials, WasmError, WasmState, WasmWorkspaceHost,
    bindings::workspaces::{WorkspaceProviderPre, exports::fleet::providers::workspaces},
};
use fleet_kit::{Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceOutput, WorkspaceWrite};
use std::sync::Arc;
use wasmtime::{Store, component::InstancePre};
use wasmtime_wasi::filesystem::WasiFilesystemCtx;

/// A WebAssembly workspace provider.
#[derive(Clone)]
pub struct WasmWorkspaces {
    instance: WorkspaceProviderPre<WasmState>,
    config: Arc<str>,
    filesystem: WasiFilesystemCtx,
    credentials: Option<WasmCredentials>,
    workspace_host: Option<WasmWorkspaceHost>,
}

impl WasmWorkspaces {
    pub(crate) fn new(
        instance: InstancePre<WasmState>,
        config: Arc<str>,
        filesystem: WasiFilesystemCtx,
        credentials: Option<WasmCredentials>,
        workspace_host: Option<WasmWorkspaceHost>,
    ) -> Result<Self, WasmError> {
        Ok(Self {
            instance: WorkspaceProviderPre::new(instance).map_err(WasmError::runtime)?,
            config,
            filesystem,
            credentials,
            workspace_host,
        })
    }

    pub(crate) async fn get(&self, name: &str) -> Result<Option<Workspace>, WasmError> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(
                self.config.clone(),
                self.filesystem.clone(),
                self.credentials.clone(),
                self.workspace_host.clone(),
            ),
        );
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        let workspace = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_workspaces()
                    .call_get(accessor, name.into())
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_workspace)?;

        workspace.map(TryInto::try_into).transpose()
    }

    pub(crate) async fn open(
        &self,
        name: &str,
        context: &WorkspaceContext,
    ) -> Result<(), WasmError> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(
                self.config.clone(),
                self.filesystem.clone(),
                self.credentials.clone(),
                self.workspace_host.clone(),
            ),
        );
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_workspaces()
                    .call_open(accessor, workspaces::WorkspaceContext::new(name, context))
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_workspace)
    }

    pub(crate) async fn read(
        &self,
        name: &str,
        context: &WorkspaceContext,
        path: &str,
        limit: u64,
    ) -> Result<Vec<u8>, WasmError> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(
                self.config.clone(),
                self.filesystem.clone(),
                self.credentials.clone(),
                self.workspace_host.clone(),
            ),
        );
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_workspaces()
                    .call_read(
                        accessor,
                        workspaces::WorkspaceContext::new(name, context),
                        path.into(),
                        limit,
                    )
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_workspace)
    }

    pub(crate) async fn write(
        &self,
        name: &str,
        context: &WorkspaceContext,
        file: &WorkspaceWrite,
    ) -> Result<(), WasmError> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(
                self.config.clone(),
                self.filesystem.clone(),
                self.credentials.clone(),
                self.workspace_host.clone(),
            ),
        );
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_workspaces()
                    .call_write(
                        accessor,
                        workspaces::WorkspaceContext::new(name, context),
                        file.into(),
                    )
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_workspace)
    }

    pub(crate) async fn exec(
        &self,
        name: &str,
        context: &WorkspaceContext,
        is_retry: bool,
        command: &WorkspaceCommand,
    ) -> Result<WorkspaceOutput, WasmError> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(
                self.config.clone(),
                self.filesystem.clone(),
                self.credentials.clone(),
                self.workspace_host.clone(),
            ),
        );
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        let output = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_workspaces()
                    .call_exec(
                        accessor,
                        workspaces::WorkspaceContext::new(name, context),
                        is_retry,
                        command.into(),
                    )
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_workspace)?;

        Ok(output.into())
    }
}
