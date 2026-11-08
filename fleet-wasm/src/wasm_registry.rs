use crate::{WasmCapabilities, WasmError, WasmModels, WasmPrompts, WasmState, WasmTools};
use core::future;
use fleet_core::{
    AgentId, ContentBlock, JsonObject, Model, ModelCompactionResponse, ModelContext,
    ModelContinuation, ModelInput, ModelName, ModelResponseChunk, Models, Prompt, PromptContext,
    PromptName, PromptResponse, Prompts, Tool, ToolCall, ToolContext, ToolName, ToolOutput, Tools,
};
use futures_util::{Stream, future::Either, stream};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    sync::Arc,
};
use wasmtime::component::{Component, Linker};

/// Named WebAssembly providers.
///
/// Names use `namespace:item`. Only the first colon separates the namespace.
#[derive(Default)]
pub struct WasmRegistry {
    providers: BTreeMap<String, WasmProvider>,
}

/// A configured WebAssembly provider.
///
/// A provider can export models, tools, prompts or any combination of them.
pub struct WasmProvider {
    models: Option<WasmModels>,
    tools: Option<WasmTools>,
    prompts: Option<WasmPrompts>,
}

impl WasmRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            providers: BTreeMap::new(),
        }
    }

    /// Registers a provider under a namespace.
    ///
    /// Namespaces contain ASCII letters, digits, hyphens or underscores.
    ///
    /// # Errors
    ///
    /// Returns an error if the namespace is invalid or already registered.
    pub fn insert(&mut self, namespace: &str, provider: WasmProvider) -> Result<(), WasmError> {
        if namespace.is_empty()
            || !namespace
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(WasmError::invalid_namespace(namespace));
        }

        match self.providers.entry(namespace.into()) {
            Entry::Vacant(entry) => {
                entry.insert(provider);
                Ok(())
            }
            Entry::Occupied(entry) => Err(WasmError::duplicate_namespace(entry.key().clone())),
        }
    }

    fn provider<'a>(&self, name: &'a str) -> Option<(&'a str, &'a str, &WasmProvider)> {
        let (namespace, name) = name.split_once(':')?;
        if name.is_empty() {
            return None;
        }

        Some((namespace, name, self.providers.get(namespace)?))
    }
}

impl WasmProvider {
    pub(crate) fn new(
        component: &Component,
        linker: &Linker<WasmState>,
        config: &JsonObject,
        capabilities: WasmCapabilities,
    ) -> Result<Self, WasmError> {
        let instance = linker
            .instantiate_pre(component)
            .map_err(WasmError::runtime)?;
        let config: Arc<str> = serde_json::to_string(config)?.into();
        let filesystem = capabilities.open_filesystem()?;

        let models = component
            .get_export_index(None, "fleet:providers/models@0.0.0")
            .map(|_| WasmModels::new(instance.clone(), config.clone(), filesystem.clone()))
            .transpose()?;
        let tools = component
            .get_export_index(None, "fleet:providers/tools@0.0.0")
            .map(|_| WasmTools::new(instance.clone(), config.clone(), filesystem.clone()))
            .transpose()?;
        let prompts = component
            .get_export_index(None, "fleet:providers/prompts@0.0.0")
            .map(|_| WasmPrompts::new(instance, config, filesystem))
            .transpose()?;

        if models.is_none() && tools.is_none() && prompts.is_none() {
            return Err(WasmError::no_provider_interfaces());
        }

        Ok(Self {
            models,
            tools,
            prompts,
        })
    }
}

impl Models for WasmRegistry {
    type Error = WasmError;

    async fn get(&self, name: &ModelName) -> Result<Option<Model>, Self::Error> {
        if let Some((namespace, name, provider)) = self.provider(name.as_ref())
            && let Some(models) = &provider.models
        {
            let mut model = models.get(name).await?;
            if let Some(model) = &mut model {
                model.name = format!("{namespace}:{}", model.name).into();
            }
            Ok(model)
        } else {
            Ok(None)
        }
    }

    fn generate(
        &self,
        context: &ModelContext,
        continuation: Option<&ModelContinuation>,
        input: &[ModelInput],
    ) -> impl Stream<Item = Result<ModelResponseChunk, Self::Error>> + Send {
        if let Some((_, name, provider)) = self.provider(context.model.name.as_ref())
            && let Some(models) = &provider.models
        {
            Either::Left(models.generate(name, context, continuation, input))
        } else {
            Either::Right(stream::once(future::ready(Err(
                WasmError::model_not_found(&context.model.name),
            ))))
        }
    }

    async fn compact(
        &self,
        context: &ModelContext,
        continuation: Option<&ModelContinuation>,
        input: &[ModelInput],
        instructions: Option<&str>,
    ) -> Result<ModelCompactionResponse, Self::Error> {
        if let Some((_, name, provider)) = self.provider(context.model.name.as_ref())
            && let Some(models) = &provider.models
        {
            models
                .compact(name, context, continuation, input, instructions)
                .await
        } else {
            Err(WasmError::model_not_found(&context.model.name))
        }
    }
}

impl Tools for WasmRegistry {
    type Error = WasmError;

    async fn get(&self, name: &ToolName) -> Result<Option<Tool>, Self::Error> {
        if let Some((namespace, name, provider)) = self.provider(name.as_ref())
            && let Some(tools) = &provider.tools
        {
            let mut tool = tools.get(name).await?;
            if let Some(tool) = &mut tool {
                tool.name = format!("{namespace}:{}", tool.name).into();
            }
            Ok(tool)
        } else {
            Ok(None)
        }
    }

    async fn execute(
        &self,
        context: &ToolContext,
        is_retry: bool,
        input: &JsonObject,
    ) -> Result<ToolOutput, Self::Error> {
        if let Some((_, name, provider)) = self.provider(context.tool.name.as_ref())
            && let Some(tools) = &provider.tools
        {
            tools.execute(name, context, is_retry, input).await
        } else {
            Err(WasmError::tool_not_found(&context.tool.name))
        }
    }

    async fn interrupted_output(
        &self,
        context: &ToolContext,
        input: &JsonObject,
    ) -> Result<ToolOutput, Self::Error> {
        if let Some((_, name, provider)) = self.provider(context.tool.name.as_ref())
            && let Some(tools) = &provider.tools
        {
            tools.interrupted_output(name, context, input).await
        } else {
            Err(WasmError::tool_not_found(&context.tool.name))
        }
    }

    fn unknown_tool_output(&self, _agent_id: AgentId, call: &ToolCall) -> ToolOutput {
        ToolOutput {
            content: vec![ContentBlock::Text(format!("Unknown tool: {}", call.tool))],
            structured_content: None,
            is_error: true,
        }
    }
}

impl Prompts for WasmRegistry {
    type Error = WasmError;

    async fn get(&self, name: &PromptName) -> Result<Option<Prompt>, Self::Error> {
        if let Some((namespace, name, provider)) = self.provider(name.as_ref())
            && let Some(prompts) = &provider.prompts
        {
            let mut prompt = prompts.get(name).await?;
            if let Some(prompt) = &mut prompt {
                prompt.name = format!("{namespace}:{}", prompt.name).into();
            }
            Ok(prompt)
        } else {
            Ok(None)
        }
    }

    async fn render(
        &self,
        context: &PromptContext,
        input: &JsonObject,
    ) -> Result<PromptResponse, Self::Error> {
        if let Some((_, name, provider)) = self.provider(context.prompt.name.as_ref())
            && let Some(prompts) = &provider.prompts
        {
            prompts.render(name, context, input).await
        } else {
            Err(WasmError::prompt_not_found(&context.prompt.name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr;
    use futures_util::FutureExt as _;

    fn provider() -> WasmProvider {
        WasmProvider {
            models: None,
            tools: None,
            prompts: None,
        }
    }

    #[test]
    fn insert_rejects_invalid_and_duplicate_namespaces() {
        let mut registry = WasmRegistry::new();
        for namespace in ["", "a:b", "a b", "a/b"] {
            let error = registry.insert(namespace, provider()).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("invalid provider namespace \"{namespace}\"")
            );
        }
        assert!(registry.providers.is_empty());

        registry.insert("example", provider()).unwrap();
        let error = registry.insert("example", provider()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider namespace example is already registered"
        );
        assert_eq!(registry.providers.len(), 1);
    }

    #[test]
    fn provider_routes_by_first_colon() {
        let mut registry = WasmRegistry::new();
        registry.insert("example", provider()).unwrap();
        registry.insert("other", provider()).unwrap();

        let (namespace, name, provider) = registry.provider("example:server:tool").unwrap();
        assert_eq!(namespace, "example");
        assert_eq!(name, "server:tool");
        assert!(ptr::eq(
            provider,
            registry.providers.get("example").unwrap()
        ));

        for name in ["example", "example:", "missing:tool", ":tool"] {
            assert!(registry.provider(name).is_none());
        }
    }

    #[test]
    fn get_returns_none_for_missing_providers_and_interfaces() {
        let mut registry = WasmRegistry::new();
        registry.insert("example", provider()).unwrap();

        for name in ["example:item", "missing:item", "item"] {
            let model = Models::get(&registry, &name.into())
                .now_or_never()
                .unwrap()
                .unwrap();
            assert_eq!(model, None);

            let tool = Tools::get(&registry, &name.into())
                .now_or_never()
                .unwrap()
                .unwrap();
            assert_eq!(tool, None);

            let prompt = Prompts::get(&registry, &name.into())
                .now_or_never()
                .unwrap()
                .unwrap();
            assert_eq!(prompt, None);
        }
    }
}
