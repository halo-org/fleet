use crate::{
    WasmEnvironment, WasmError, WasmState, WasmWorkers,
    bindings::{tools::ToolProviderPre, types},
};
use fleet_core::{ContentBlock, JsonObject, Tool, ToolContext, ToolOutput};
use wasmtime::{Store, component::InstancePre};

const OUTPUT_LIMIT: usize = 128 * 1024;
const MARKER_LIMIT: usize = 64;
const HEAD_LIMIT: usize = (OUTPUT_LIMIT - MARKER_LIMIT).div_ceil(2);
const TAIL_LIMIT: usize = (OUTPUT_LIMIT - MARKER_LIMIT).div_euclid(2);

/// A WebAssembly tool provider.
#[derive(Clone)]
pub struct WasmTools {
    blobs: Option<crate::WasmBlobs>,
    roles: Option<crate::WasmRoles>,
    instance: ToolProviderPre<WasmState>,
    environment: WasmEnvironment,
    workers: Option<WasmWorkers>,
}

impl WasmTools {
    pub(crate) fn new(
        instance: InstancePre<WasmState>,
        environment: WasmEnvironment,
        workers: Option<WasmWorkers>,
    ) -> Result<Self, WasmError> {
        Ok(Self {
            blobs: None,
            roles: None,
            instance: ToolProviderPre::new(instance).map_err(WasmError::runtime)?,
            environment,
            workers,
        })
    }

    pub(crate) fn set_blobs(&mut self, blobs: Option<crate::WasmBlobs>) {
        self.blobs = blobs;
    }

    pub(crate) fn set_roles(&mut self, roles: Option<crate::WasmRoles>) {
        self.roles = roles;
    }

    fn store(&self) -> Store<WasmState> {
        WasmState::new(self.environment.clone(), self.workers.clone())
            .into_store(self.instance.engine())
    }

    pub(crate) async fn get(&self, name: &str) -> Result<Option<Tool>, WasmError> {
        let mut store = self.store();
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
        let mut store = self.store();
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        let input = serde_json::to_string(input)?;
        store.data_mut().set_blobs(
            self.blobs
                .as_ref()
                .and_then(|blobs| blobs.for_call(context.agent_id, context.call_id.clone())),
        );
        store.data_mut().set_roles(
            self.roles
                .as_ref()
                .map(|roles| roles.for_agent(context.agent_id)),
        );
        store.data_mut().set_agents(
            self.environment
                .agents
                .as_ref()
                .map(|agents| agents.for_call(context.agent_id, context.call_id.clone())),
        );
        store.data_mut().set_tool_calls(
            self.environment
                .tool_calls
                .as_ref()
                .map(|tool_calls| tool_calls.for_call(context.agent_id, context.call_id.clone())),
        );
        store.data_mut().set_workspace_access(
            self.environment
                .workspace_access
                .as_ref()
                .map(|access| access.for_agent(context.agent_id)),
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

        self.validate_output(context, limit_output(output.try_into()?))
            .await
    }

    pub(crate) async fn interrupted_output(
        &self,
        name: &str,
        context: &ToolContext,
        input: &JsonObject,
    ) -> Result<ToolOutput, WasmError> {
        let mut store = self.store();
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        let input = serde_json::to_string(input)?;
        store.data_mut().set_blobs(
            self.blobs
                .as_ref()
                .and_then(|blobs| blobs.for_call(context.agent_id, context.call_id.clone())),
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

        self.validate_output(context, limit_output(output.try_into()?))
            .await
    }

    async fn validate_output(
        &self,
        context: &ToolContext,
        output: ToolOutput,
    ) -> Result<ToolOutput, WasmError> {
        if !output
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(_)))
        {
            return Ok(output);
        }
        let access = self
            .blobs
            .as_ref()
            .and_then(|blobs| blobs.for_call(context.agent_id, context.call_id.clone()));
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
                content: vec![ContentBlock::Text(format!(
                    "Tool returned an unavailable image: {}",
                    error.message
                ))],
                structured_content: None,
                is_error: true,
            }),
        }
    }
}

fn limit_output(output: ToolOutput) -> ToolOutput {
    let structured_content = output.structured_content.filter(|content| {
        serde_json::to_string(content).is_ok_and(|json| json.len() <= OUTPUT_LIMIT)
    });
    let size = output
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.len(),
            ContentBlock::Image(_) => 0,
        })
        .fold(0, usize::saturating_add);
    if size <= OUTPUT_LIMIT {
        return ToolOutput {
            structured_content,
            ..output
        };
    }
    let tail_start = size.saturating_sub(TAIL_LIMIT);
    let mut content = Vec::new();
    let mut marker = None;
    let mut retained = 0_usize;
    let mut start = 0_usize;
    for block in output.content {
        let ContentBlock::Text(mut head) = block else {
            content.push(block);
            continue;
        };
        let end = start.saturating_add(head.len());
        let tail = if end > tail_start {
            head.split_off(head.ceil_char_boundary(tail_start.saturating_sub(start)))
        } else {
            String::new()
        };
        head.truncate(head.floor_char_boundary(HEAD_LIMIT.saturating_sub(start)));
        retained = retained
            .saturating_add(head.len())
            .saturating_add(tail.len());
        if !head.is_empty() {
            content.push(ContentBlock::Text(head));
        }
        if start <= HEAD_LIMIT && end > HEAD_LIMIT {
            marker = Some(content.len());
            content.push(ContentBlock::Text(String::new()));
        }
        if !tail.is_empty() {
            content.push(ContentBlock::Text(tail));
        }
        start = end;
    }
    if let Some(block) = marker.and_then(|index| content.get_mut(index)) {
        *block = ContentBlock::Text(format!(
            "[Omitted {} bytes of tool output]",
            size.saturating_sub(retained),
        ));
    }
    ToolOutput {
        content,
        structured_content,
        is_error: output.is_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(output: &ToolOutput) -> Vec<&str> {
        output
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text(text) => text.as_str(),
                ContentBlock::Image(_) => "image",
            })
            .collect()
    }

    #[test]
    fn limit_output_keeps_head_and_tail() {
        let output = ToolOutput {
            content: vec![
                ContentBlock::Text("a".repeat(HEAD_LIMIT - 1)),
                ContentBlock::Text("bc".into()),
                ContentBlock::Image(1.into()),
                ContentBlock::Text(format!("{}d", "é".repeat(OUTPUT_LIMIT))),
            ],
            structured_content: Some(JsonObject::from_iter([(
                "text".into(),
                "x".repeat(OUTPUT_LIMIT).into(),
            )])),
            is_error: true,
        };

        let output = limit_output(output);
        let text = text(&output);

        assert_eq!(text.len(), 5);
        assert_eq!(text[0].len(), HEAD_LIMIT - 1);
        assert_eq!(text[1], "b");
        assert_eq!(text[2], "[Omitted 196643 bytes of tool output]");
        assert_eq!(text[3], "image");
        assert_eq!(
            text[4],
            format!("{}d", "é".repeat((TAIL_LIMIT - 1).div_euclid(2)))
        );
        assert_eq!(output.structured_content, None);
        assert!(output.is_error);
        assert_eq!(limit_output(output.clone()), output);
    }
}
