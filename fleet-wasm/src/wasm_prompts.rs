use crate::{
    WasmCredentials, WasmError, WasmState,
    bindings::{prompts::PromptProviderPre, types},
};
use fleet_core::{JsonObject, Prompt, PromptContext, PromptResponse};
use std::sync::Arc;
use wasmtime::{Store, component::InstancePre};
use wasmtime_wasi::filesystem::WasiFilesystemCtx;

/// A WebAssembly prompt provider.
pub struct WasmPrompts {
    instance: PromptProviderPre<WasmState>,
    config: Arc<str>,
    filesystem: WasiFilesystemCtx,
    credentials: Option<WasmCredentials>,
}

impl WasmPrompts {
    pub(crate) fn new(
        instance: InstancePre<WasmState>,
        config: Arc<str>,
        filesystem: WasiFilesystemCtx,
        credentials: Option<WasmCredentials>,
    ) -> Result<Self, WasmError> {
        Ok(Self {
            instance: PromptProviderPre::new(instance).map_err(WasmError::runtime)?,
            config,
            filesystem,
            credentials,
        })
    }

    pub(crate) async fn get(&self, name: &str) -> Result<Option<Prompt>, WasmError> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(
                self.config.clone(),
                self.filesystem.clone(),
                self.credentials.clone(),
            ),
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
                    .call_get(accessor, name.into())
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::provider)?;

        prompt.map(TryInto::try_into).transpose()
    }

    pub(crate) async fn render(
        &self,
        name: &str,
        context: &PromptContext,
        input: &JsonObject,
    ) -> Result<PromptResponse, WasmError> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(
                self.config.clone(),
                self.filesystem.clone(),
                self.credentials.clone(),
            ),
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
                    .call_render(accessor, types::PromptContext::new(name, context), input)
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::provider)?;

        Ok(response.into())
    }
}
