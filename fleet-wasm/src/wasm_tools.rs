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
    blobs: Option<crate::WasmBlobs>,
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
            blobs: None,
            instance: ToolProviderPre::new(instance).map_err(WasmError::runtime)?,
            config,
            filesystem,
            credentials,
            workspace_host,
        })
    }

    pub(crate) fn set_blobs(&mut self, blobs: Option<crate::WasmBlobs>) {
        self.blobs = blobs;
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
        store.data_mut().set_blobs(
            self.blobs
                .as_ref()
                .and_then(|blobs| blobs.for_agent(context.agent_id)),
        );
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

        self.validate_output(context, output.try_into()?).await
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
        store.data_mut().set_blobs(
            self.blobs
                .as_ref()
                .and_then(|blobs| blobs.for_agent(context.agent_id)),
        );
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

        self.validate_output(context, output.try_into()?).await
    }

    async fn validate_output(
        &self,
        context: &ToolContext,
        output: ToolOutput,
    ) -> Result<ToolOutput, WasmError> {
        if !output
            .content
            .iter()
            .any(|block| matches!(block, fleet_core::ContentBlock::Image(_)))
        {
            return Ok(output);
        }
        let access = self
            .blobs
            .as_ref()
            .and_then(|blobs| blobs.for_agent(context.agent_id));
        let result = match access {
            Some(access) => access.validate_content(&output.content).await,
            None => Err(crate::bindings::blobs::BlobsError {
                kind: crate::bindings::blobs::BlobsErrorKind::Denied,
                message: "blob access is not available".into(),
            }),
        };
        match result {
            Ok(()) => Ok(output),
            Err(error) if matches!(error.kind, crate::bindings::blobs::BlobsErrorKind::Other) => {
                Err(WasmError::provider(error.message))
            }
            Err(error) => Ok(ToolOutput {
                content: vec![fleet_core::ContentBlock::Text(format!(
                    "Tool returned an unavailable image: {}",
                    error.message
                ))],
                structured_content: None,
                is_error: true,
            }),
        }
    }
}
