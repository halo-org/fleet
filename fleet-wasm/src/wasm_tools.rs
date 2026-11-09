use crate::{
    WasmCredentials, WasmError, WasmState, WasmWorkspaceHost,
    bindings::{tools::ToolProviderPre, types},
};
use fleet_core::{JsonObject, Tool, ToolContext, ToolOutput};
use std::sync::Arc;
use wasmtime::{Store, component::InstancePre};
use wasmtime_wasi::filesystem::WasiFilesystemCtx;

/// A WebAssembly tool provider.
#[derive(Clone)]
pub struct WasmTools {
    instance: ToolProviderPre<WasmState>,
    config: Arc<str>,
    filesystem: WasiFilesystemCtx,
    credentials: Option<WasmCredentials>,
    workspace_host: Option<WasmWorkspaceHost>,
}

impl WasmTools {
    pub(crate) fn new(
        instance: InstancePre<WasmState>,
        config: Arc<str>,
        filesystem: WasiFilesystemCtx,
        credentials: Option<WasmCredentials>,
        workspace_host: Option<WasmWorkspaceHost>,
    ) -> Result<Self, WasmError> {
        Ok(Self {
            instance: ToolProviderPre::new(instance).map_err(WasmError::runtime)?,
            config,
            filesystem,
            credentials,
            workspace_host,
        })
    }

    pub(crate) async fn get(&self, name: &str) -> Result<Option<Tool>, WasmError> {
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
        let tool = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_tools()
                    .call_get(accessor, name.into())
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_tool)?;

        tool.map(TryInto::try_into).transpose()
    }

    pub(crate) async fn execute(
        &self,
        name: &str,
        context: &ToolContext,
        is_retry: bool,
        input: &JsonObject,
    ) -> Result<ToolOutput, WasmError> {
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
        let input = serde_json::to_string(input)?;
        let output = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_tools()
                    .call_execute(
                        accessor,
                        types::ToolContext::new(name, context),
                        is_retry,
                        input,
                    )
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_tool)?;

        output.try_into()
    }

    pub(crate) async fn interrupted_output(
        &self,
        name: &str,
        context: &ToolContext,
        input: &JsonObject,
    ) -> Result<ToolOutput, WasmError> {
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
        let input = serde_json::to_string(input)?;
        let output = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_tools()
                    .call_interrupted_output(
                        accessor,
                        types::ToolContext::new(name, context),
                        input,
                    )
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_tool)?;

        output.try_into()
    }
}
