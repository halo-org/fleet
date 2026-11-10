use crate::{
    WasmEnvironment, WasmError, WasmState, WasmWorkers,
    bindings::workspaces::{WorkspaceProviderPre, exports::fleet::providers::workspaces},
};
use fleet_kit::{Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceOutput, WorkspaceWrite};
use wasmtime::{Store, component::InstancePre};

/// A WebAssembly workspace provider.
#[derive(Clone)]
pub struct WasmWorkspaces {
    instance: WorkspaceProviderPre<WasmState>,
    environment: WasmEnvironment,
    workers: Option<WasmWorkers>,
}

impl WasmWorkspaces {
    pub(crate) fn new(
        instance: InstancePre<WasmState>,
        environment: WasmEnvironment,
        workers: Option<WasmWorkers>,
    ) -> Result<Self, WasmError> {
        Ok(Self {
            instance: WorkspaceProviderPre::new(instance).map_err(WasmError::runtime)?,
            environment,
            workers,
        })
    }

    fn store(&self) -> Store<WasmState> {
        WasmState::new(self.environment.clone(), self.workers.clone())
            .into_store(self.instance.engine())
    }

    pub(crate) async fn get(&self, name: &str) -> Result<Option<Workspace>, WasmError> {
        let mut store = self.store();
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
        let mut store = self.store();
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
        let mut store = self.store();
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
        let mut store = self.store();
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
        let mut store = self.store();
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
