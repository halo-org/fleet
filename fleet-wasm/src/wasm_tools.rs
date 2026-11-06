use crate::{
    WasmCapabilities, WasmError, WasmState,
    bindings::{tools::ToolProviderPre, types},
};
use fleet_core::{
    AgentId, ContentBlock, JsonObject, Tool, ToolCall, ToolContext, ToolName, ToolOutput, Tools,
};
use wasmtime::{
    Store,
    component::{Component, Linker},
};
use wasmtime_wasi::filesystem::WasiFilesystemCtx;

/// A WebAssembly tool provider.
///
/// Unknown tool output uses a host fallback if the component fails.
pub struct WasmTools {
    instance: ToolProviderPre<WasmState>,
    filesystem: WasiFilesystemCtx,
}

impl WasmTools {
    pub(crate) fn new(
        component: &Component,
        linker: &Linker<WasmState>,
        capabilities: WasmCapabilities,
    ) -> Result<Self, WasmError> {
        let instance = linker
            .instantiate_pre(component)
            .map_err(WasmError::runtime)?;
        Ok(Self {
            instance: ToolProviderPre::new(instance).map_err(WasmError::runtime)?,
            filesystem: capabilities.open_filesystem()?,
        })
    }
}

impl Tools for WasmTools {
    type Error = WasmError;

    async fn get(&self, name: &ToolName) -> Result<Option<Tool>, Self::Error> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(self.filesystem.clone()),
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
                    .call_get(accessor, name.as_ref().into())
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_tool)?;

        tool.map(TryInto::try_into).transpose()
    }

    async fn execute(
        &self,
        context: &ToolContext,
        is_retry: bool,
        input: &JsonObject,
    ) -> Result<ToolOutput, Self::Error> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(self.filesystem.clone()),
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
                    .call_execute(accessor, context.into(), is_retry, input)
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_tool)?;

        output.try_into()
    }

    async fn interrupted_output(
        &self,
        context: &ToolContext,
        input: &JsonObject,
    ) -> Result<ToolOutput, Self::Error> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(self.filesystem.clone()),
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
                    .call_interrupted_output(accessor, context.into(), input)
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_tool)?;

        output.try_into()
    }

    fn unknown_tool_output(&self, agent_id: AgentId, call: &ToolCall) -> ToolOutput {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(self.filesystem.clone()),
        );
        let output = self
            .instance
            .instantiate(&mut store)
            .map_err(WasmError::runtime)
            .and_then(|instance| {
                instance
                    .fleet_providers_tools()
                    .call_unknown_tool_output(
                        &mut store,
                        types::AgentId::from(agent_id),
                        &call.into(),
                    )
                    .map_err(WasmError::runtime)
            })
            .and_then(TryInto::try_into);

        output.unwrap_or_else(|_| ToolOutput {
            content: vec![ContentBlock::Text(format!("Unknown tool: {}", call.tool))],
            structured_content: None,
            is_error: true,
        })
    }
}
