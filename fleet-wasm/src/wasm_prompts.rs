use crate::{WasmCapabilities, WasmError, WasmState, bindings::prompts::PromptProviderPre};
use fleet_core::{JsonObject, Prompt, PromptContext, PromptName, PromptResponse, Prompts};
use wasmtime::{
    Store,
    component::{Component, Linker},
};
use wasmtime_wasi::filesystem::WasiFilesystemCtx;

/// A WebAssembly prompt provider.
pub struct WasmPrompts {
    instance: PromptProviderPre<WasmState>,
    filesystem: WasiFilesystemCtx,
}

impl WasmPrompts {
    pub(crate) fn new(
        component: &Component,
        linker: &Linker<WasmState>,
        capabilities: WasmCapabilities,
    ) -> Result<Self, WasmError> {
        let instance = linker
            .instantiate_pre(component)
            .map_err(WasmError::runtime)?;
        Ok(Self {
            instance: PromptProviderPre::new(instance).map_err(WasmError::runtime)?,
            filesystem: capabilities.open_filesystem()?,
        })
    }
}

impl Prompts for WasmPrompts {
    type Error = WasmError;

    async fn get(&self, name: &PromptName) -> Result<Option<Prompt>, Self::Error> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(self.filesystem.clone()),
        );
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        let prompt = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_prompts()
                    .call_get(accessor, name.as_ref().into())
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::provider)?;

        prompt.map(TryInto::try_into).transpose()
    }

    async fn render(
        &self,
        context: &PromptContext,
        input: &JsonObject,
    ) -> Result<PromptResponse, Self::Error> {
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
        let response = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_prompts()
                    .call_render(accessor, context.into(), input)
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::provider)?;

        Ok(response.into())
    }
}
