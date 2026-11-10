use crate::{
    WasmEnvironment, WasmError, WasmState, WasmWorkers,
    bindings::{prompts::PromptProviderPre, types},
};
use fleet_core::{JsonObject, Prompt, PromptContext, PromptResponse};
use wasmtime::{Store, component::InstancePre};

/// A WebAssembly prompt provider.
#[derive(Clone)]
pub struct WasmPrompts {
    instance: PromptProviderPre<WasmState>,
    environment: WasmEnvironment,
    workers: Option<WasmWorkers>,
}

impl WasmPrompts {
    pub(crate) fn new(
        instance: InstancePre<WasmState>,
        environment: WasmEnvironment,
        workers: Option<WasmWorkers>,
    ) -> Result<Self, WasmError> {
        Ok(Self {
            instance: PromptProviderPre::new(instance).map_err(WasmError::runtime)?,
            environment,
            workers,
        })
    }

    fn store(&self) -> Store<WasmState> {
        WasmState::new(self.environment.clone(), self.workers.clone())
            .into_store(self.instance.engine())
    }

    pub(crate) async fn get(&self, name: &str) -> Result<Option<Prompt>, WasmError> {
        let mut store = self.store();
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
        let mut store = self.store();
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
