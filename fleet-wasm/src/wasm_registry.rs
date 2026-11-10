use crate::{
    WasmCapabilities, WasmEnvironment, WasmError, WasmModels, WasmOrigins, WasmPrompts, WasmState,
    WasmTools, WasmWorkers, WasmWorkspaces,
    bindings::authentication::{
        AuthenticationProviderPre, exports::fleet::providers::authentication as auth,
    },
};
use core::future;
use fleet_core::{
    AgentId, ContentBlock, JsonObject, Model, ModelCompactionResponse, ModelContext,
    ModelContinuation, ModelInput, ModelName, ModelResponseChunk, Models, Prompt, PromptContext,
    PromptName, PromptResponse, Prompts, Tool, ToolCall, ToolContext, ToolName, ToolOutput, Tools,
};
use fleet_kit::{
    Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceName, WorkspaceOutput, WorkspaceWrite,
    Workspaces,
};
use futures_util::{Stream, future::Either, stream};
use std::collections::{BTreeMap, btree_map::Entry};
use wasmtime::component::{Component, InstancePre, Linker};

/// Named WebAssembly providers.
///
/// Names use `namespace:item`. Only the first colon separates the namespace.
#[derive(Default)]
pub struct WasmRegistry {
    providers: BTreeMap<String, WasmProvider>,
}

/// A configured WebAssembly provider.
///
/// A provider can export models, tools, prompts, workspaces or any combination of them.
#[derive(Clone)]
pub struct WasmProvider {
    models: Option<WasmModels>,
    tools: Option<WasmTools>,
    prompts: Option<WasmPrompts>,
    workspaces: Option<WasmWorkspaces>,
    authentication: Option<AuthenticationProviderPre<WasmState>>,
    environment: WasmEnvironment,
    workers: Option<WasmWorkers>,
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

    /// Stops private workers and waits for their bounded cleanup.
    ///
    /// New worker connections are rejected after shutdown begins. Call before dropping
    /// the registry for graceful shutdown. Dropping all provider owners, including
    /// in-flight calls, aborts workers instead.
    pub async fn shutdown(&self) {
        futures_util::future::join_all(self.providers.values().map(WasmProvider::shutdown)).await;
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
        worker: Option<InstancePre<WasmState>>,
        config: &JsonObject,
        capabilities: WasmCapabilities,
    ) -> Result<Self, WasmError> {
        let instance = linker
            .instantiate_pre(component)
            .map_err(WasmError::runtime)?;
        let environment = WasmEnvironment {
            config: serde_json::to_string(config)?.into(),
            filesystem: capabilities.open_filesystem()?,
            origins: WasmOrigins::new(capabilities.http),
            credentials: capabilities.credentials,
            workspace_host: capabilities.workspaces,
        };
        let blob_host = capabilities.blobs;
        let workers = worker
            .map(|instance| WasmWorkers::new(instance, environment.clone()))
            .transpose()?;

        let mut models = component
            .get_export_index(None, "fleet:providers/models@0.0.0")
            .map(|_| WasmModels::new(instance.clone(), environment.clone(), workers.clone()))
            .transpose()?;
        let mut tools = component
            .get_export_index(None, "fleet:providers/tools@0.0.0")
            .map(|_| WasmTools::new(instance.clone(), environment.clone(), workers.clone()))
            .transpose()?;
        if let Some(models) = &mut models {
            models.set_blobs(blob_host.clone());
        }
        if let Some(tools) = &mut tools {
            tools.set_blobs(blob_host);
        }
        let prompts = component
            .get_export_index(None, "fleet:providers/prompts@0.0.0")
            .map(|_| WasmPrompts::new(instance.clone(), environment.clone(), workers.clone()))
            .transpose()?;
        let workspaces = component
            .get_export_index(None, "fleet:providers/workspaces@0.0.0")
            .map(|_| WasmWorkspaces::new(instance.clone(), environment.clone(), workers.clone()))
            .transpose()?;
        let authentication = component
            .get_export_index(None, "fleet:providers/authentication@0.0.0")
            .map(|_| AuthenticationProviderPre::new(instance).map_err(WasmError::runtime))
            .transpose()?;

        if models.is_none() && tools.is_none() && prompts.is_none() && workspaces.is_none() {
            return Err(WasmError::no_provider_interfaces());
        }

        Ok(Self {
            models,
            tools,
            prompts,
            workspaces,
            authentication,
            environment,
            workers,
        })
    }

    /// Stops this provider's private workers and rejects new worker connections.
    ///
    /// Clones share the same workers. Ordinary provider calls remain single-use.
    pub async fn shutdown(&self) {
        if let Some(workers) = &self.workers {
            workers.shutdown().await;
        }
    }

    /// Returns the connections used by this provider.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to describe its connections.
    pub async fn connections(
        &self,
        host_id: &str,
    ) -> Result<Vec<fleet_kit::Connection>, WasmError> {
        let Some(instance) = &self.authentication else {
            return Ok(Vec::new());
        };
        let mut store = WasmState::new(self.environment.clone(), self.workers.clone())
            .into_store(instance.engine());
        let instance = instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        let connections = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_authentication()
                    .call_connections(accessor, host_id.into())
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(|message| WasmError::runtime(wasmtime::Error::msg(message)))?;
        Ok(connections
            .into_iter()
            .map(|connection| fleet_kit::Connection {
                name: connection.name,
                title: connection.title,
                description: connection.description,
                manage_url: connection.manage_url,
                methods: connection
                    .methods
                    .into_iter()
                    .map(|method| fleet_kit::AuthenticationMethod {
                        name: method.name,
                        title: method.title,
                        kind: match method.kind {
                            auth::AuthenticationKind::ApiKey => {
                                fleet_kit::AuthenticationKind::ApiKey
                            }
                            auth::AuthenticationKind::Oauth(config) => {
                                fleet_kit::AuthenticationKind::OAuth(Box::new(
                                    fleet_kit::OAuthConfig {
                                        authorization_url: config.authorization_url,
                                        token_url: config.token_url,
                                        client_id: config.client_id,
                                        redirect_uri: config.redirect_uri,
                                        scopes: config.scopes,
                                        parameters: config.parameters,
                                        resource: config.resource,
                                        issuer: config.issuer,
                                        registration: config.registration.map(|registration| {
                                            fleet_kit::OAuthRegistration {
                                                client_id_parameter: registration
                                                    .client_id_parameter,
                                                parameters: registration.parameters,
                                            }
                                        }),
                                        required_scopes: config.required_scopes,
                                    },
                                ))
                            }
                        },
                    })
                    .collect(),
            })
            .collect())
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

impl Workspaces for WasmRegistry {
    type Error = WasmError;

    async fn get(&self, name: &WorkspaceName) -> Result<Option<Workspace>, Self::Error> {
        if let Some((namespace, name, provider)) = self.provider(name.as_ref())
            && let Some(workspaces) = &provider.workspaces
        {
            let mut workspace = workspaces.get(name).await?;
            if let Some(workspace) = &mut workspace {
                workspace.name = format!("{namespace}:{}", workspace.name).into();
            }
            Ok(workspace)
        } else {
            Ok(None)
        }
    }

    async fn open(&self, context: &WorkspaceContext) -> Result<(), Self::Error> {
        if let Some((_, name, provider)) = self.provider(context.workspace.name.as_ref())
            && let Some(workspaces) = &provider.workspaces
        {
            workspaces.open(name, context).await
        } else {
            Err(WasmError::workspace_not_found(&context.workspace.name))
        }
    }

    async fn read(
        &self,
        context: &WorkspaceContext,
        path: &str,
        limit: u64,
    ) -> Result<Vec<u8>, Self::Error> {
        if let Some((_, name, provider)) = self.provider(context.workspace.name.as_ref())
            && let Some(workspaces) = &provider.workspaces
        {
            workspaces.read(name, context, path, limit).await
        } else {
            Err(WasmError::workspace_not_found(&context.workspace.name))
        }
    }

    async fn write(
        &self,
        context: &WorkspaceContext,
        file: &WorkspaceWrite,
    ) -> Result<(), Self::Error> {
        if let Some((_, name, provider)) = self.provider(context.workspace.name.as_ref())
            && let Some(workspaces) = &provider.workspaces
        {
            workspaces.write(name, context, file).await
        } else {
            Err(WasmError::workspace_not_found(&context.workspace.name))
        }
    }

    async fn exec(
        &self,
        context: &WorkspaceContext,
        is_retry: bool,
        command: &WorkspaceCommand,
    ) -> Result<WorkspaceOutput, Self::Error> {
        if let Some((_, name, provider)) = self.provider(context.workspace.name.as_ref())
            && let Some(workspaces) = &provider.workspaces
        {
            workspaces.exec(name, context, is_retry, command).await
        } else {
            Err(WasmError::workspace_not_found(&context.workspace.name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr;
    use fleet_core::{Configured, Error as _};
    use fleet_kit::WorkspacesErrorKind;
    use futures_util::FutureExt as _;

    fn provider() -> WasmProvider {
        WasmProvider {
            models: None,
            tools: None,
            prompts: None,
            workspaces: None,
            authentication: None,
            environment: WasmEnvironment::default(),
            workers: None,
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

            let workspace = Workspaces::get(&registry, &name.into())
                .now_or_never()
                .unwrap()
                .unwrap();
            assert_eq!(workspace, None);
        }
    }

    #[test]
    fn open_reports_missing_workspaces() {
        let mut registry = WasmRegistry::new();
        registry.insert("example", provider()).unwrap();

        for name in ["example:item", "missing:item", "item"] {
            let context = WorkspaceContext {
                workspace: Configured {
                    name: name.into(),
                    config: JsonObject::new(),
                },
                id: 1.into(),
            };

            let error = registry.open(&context).now_or_never().unwrap().unwrap_err();
            let kind: WorkspacesErrorKind = error.kind();

            assert_eq!(kind, WorkspacesErrorKind::NotFound);
            assert_eq!(error.to_string(), format!("workspace {name} was not found"));
        }
    }
}
