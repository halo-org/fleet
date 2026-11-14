use crate::bindings::fleet::providers::types;
use fleet_core::{
    AgentId, BlobId, Configured, ContentBlock, JsonObject, Model, ModelCompaction,
    ModelCompactionResponse, ModelContext, ModelContinuation, ModelInput, ModelOutput,
    ModelProvenance, ModelProviderToolCall, ModelReasoning, ModelResponse, ModelResponseChunk,
    ModelTool, ModelUsage, ToolCall, ToolOutput, ToolResult,
};

fn json(value: &JsonObject) -> String {
    serde_json::to_string(value).expect("JSON objects can be serialized")
}

impl From<Model> for types::Model {
    fn from(value: Model) -> Self {
        Self {
            name: value.name.into(),
            context_window: value.context_window,
            compaction_threshold: value.compaction_threshold,
            extensions: json(&value.extensions),
        }
    }
}

impl TryFrom<types::ModelContext> for ModelContext {
    type Error = serde_json::Error;

    fn try_from(value: types::ModelContext) -> Result<Self, Self::Error> {
        Ok(Self {
            model: Configured {
                name: value.model.name.into(),
                config: serde_json::from_str(&value.model.config)?,
            },
            agent_id: value.agent_id.into(),
            system_prompt: value.system_prompt,
            tools: value
                .tools
                .into_iter()
                .map(|tool| {
                    Ok(ModelTool {
                        name: tool.name.into(),
                        description: tool.description,
                        is_async: tool.is_async,
                        input_schema: serde_json::from_str(&tool.input_schema)?,
                    })
                })
                .collect::<Result<_, Self::Error>>()?,
            role_extensions: serde_json::from_str(&value.role_extensions)?,
        })
    }
}

impl From<types::ModelProvenance> for ModelProvenance {
    fn from(value: types::ModelProvenance) -> Self {
        Self {
            provider_id: value.provider_id,
            model_id: value.model_id,
        }
    }
}

impl From<ModelProvenance> for types::ModelProvenance {
    fn from(value: ModelProvenance) -> Self {
        Self {
            provider_id: value.provider_id,
            model_id: value.model_id,
        }
    }
}

impl From<types::ModelContinuation> for ModelContinuation {
    fn from(value: types::ModelContinuation) -> Self {
        Self {
            token: value.token,
            provenance: value.provenance.into(),
        }
    }
}

impl From<types::ModelReasoning> for ModelReasoning {
    fn from(value: types::ModelReasoning) -> Self {
        Self {
            content: value.content,
            redacted_content: value.redacted_content,
            attestation: value.attestation,
        }
    }
}

impl From<ModelReasoning> for types::ModelReasoning {
    fn from(value: ModelReasoning) -> Self {
        Self {
            content: value.content,
            redacted_content: value.redacted_content,
            attestation: value.attestation,
        }
    }
}

impl From<ModelUsage> for types::ModelUsage {
    fn from(value: ModelUsage) -> Self {
        Self {
            input: value.input,
            output: value.output,
            cache_read: value.cache_read,
            cache_write: value.cache_write,
        }
    }
}

impl From<types::ContentBlock> for ContentBlock {
    fn from(value: types::ContentBlock) -> Self {
        match value {
            types::ContentBlock::Text(text) => Self::Text(text),
            types::ContentBlock::Image(id) => Self::Image(id.into()),
        }
    }
}

impl From<ContentBlock> for types::ContentBlock {
    fn from(value: ContentBlock) -> Self {
        match value {
            ContentBlock::Text(text) => Self::Text(text),
            ContentBlock::Image(id) => Self::Image(id.into()),
        }
    }
}

impl TryFrom<types::ToolCall> for ToolCall {
    type Error = serde_json::Error;

    fn try_from(value: types::ToolCall) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id.into(),
            tool: value.tool.into(),
            is_async: value.is_async,
            input: serde_json::from_str(&value.input)?,
        })
    }
}

impl From<ToolCall> for types::ToolCall {
    fn from(value: ToolCall) -> Self {
        Self {
            id: value.id.into(),
            tool: value.tool.into(),
            is_async: value.is_async,
            input: json(&value.input),
        }
    }
}

impl TryFrom<types::ToolResult> for ToolResult {
    type Error = serde_json::Error;

    fn try_from(value: types::ToolResult) -> Result<Self, Self::Error> {
        Ok(Self {
            call_id: value.call_id.into(),
            output: ToolOutput {
                content: value.output.content.into_iter().map(Into::into).collect(),
                structured_content: value
                    .output
                    .structured_content
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?,
                is_error: value.output.is_error,
            },
        })
    }
}

impl From<ToolResult> for types::ToolResult {
    fn from(value: ToolResult) -> Self {
        Self {
            call_id: value.call_id.into(),
            output: types::ToolOutput {
                content: value.output.content.into_iter().map(Into::into).collect(),
                structured_content: value.output.structured_content.as_ref().map(json),
                is_error: value.output.is_error,
            },
        }
    }
}

impl TryFrom<types::ModelInput> for ModelInput {
    type Error = serde_json::Error;

    fn try_from(value: types::ModelInput) -> Result<Self, Self::Error> {
        Ok(match value {
            types::ModelInput::User(content) => {
                Self::User(content.into_iter().map(Into::into).collect())
            }
            types::ModelInput::Assistant((source, output)) => Self::Assistant(
                source.into(),
                output
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
            ),
            types::ModelInput::ToolResult(result) => Self::ToolResult(result.try_into()?),
            types::ModelInput::Compaction((source, value)) => {
                Self::Compaction(source.into(), value.into())
            }
        })
    }
}

impl From<ModelInput> for types::ModelInput {
    fn from(value: ModelInput) -> Self {
        match value {
            ModelInput::User(content) => Self::User(content.into_iter().map(Into::into).collect()),
            ModelInput::Assistant(source, output) => {
                Self::Assistant((source.into(), output.into_iter().map(Into::into).collect()))
            }
            ModelInput::ToolResult(result) => Self::ToolResult(result.into()),
            ModelInput::Compaction(source, value) => {
                Self::Compaction((source.into(), value.into()))
            }
        }
    }
}

impl TryFrom<types::ModelOutput> for ModelOutput {
    type Error = serde_json::Error;

    fn try_from(value: types::ModelOutput) -> Result<Self, Self::Error> {
        Ok(match value {
            types::ModelOutput::Text(text) => Self::Text(text),
            types::ModelOutput::Reasoning(reasoning) => Self::Reasoning(reasoning.into()),
            types::ModelOutput::ToolCall(call) => Self::ToolCall(call.try_into()?),
            types::ModelOutput::ProviderToolCall(call) => {
                Self::ProviderToolCall(ModelProviderToolCall {
                    call_id: call.call_id.into(),
                    name: call.name.into(),
                    content: call.content,
                    payload: call.payload,
                })
            }
        })
    }
}

impl From<ModelOutput> for types::ModelOutput {
    fn from(value: ModelOutput) -> Self {
        match value {
            ModelOutput::Text(text) => Self::Text(text),
            ModelOutput::Reasoning(reasoning) => Self::Reasoning(reasoning.into()),
            ModelOutput::ToolCall(call) => Self::ToolCall(call.into()),
            ModelOutput::ProviderToolCall(call) => {
                Self::ProviderToolCall(types::ModelProviderToolCall {
                    call_id: call.call_id.into(),
                    name: call.name.into(),
                    content: call.content,
                    payload: call.payload,
                })
            }
        }
    }
}

impl From<ModelResponse> for types::ModelResponse {
    fn from(value: ModelResponse) -> Self {
        Self {
            output: value.output.into_iter().map(Into::into).collect(),
            usage: value.usage.into(),
            provenance: value.provenance.into(),
            continuation_token: value.continuation_token,
        }
    }
}

impl From<ModelResponseChunk> for types::ModelResponseChunk {
    fn from(value: ModelResponseChunk) -> Self {
        match value {
            ModelResponseChunk::TextStart(index) => Self::TextStart(index),
            ModelResponseChunk::ReasoningStart(index) => Self::ReasoningStart(index),
            ModelResponseChunk::ToolCallStart(index, id, tool) => {
                Self::ToolCallStart((index, id.into(), tool.into()))
            }
            ModelResponseChunk::ProviderToolCallStart(index, id, name) => {
                Self::ProviderToolCallStart((index, id.into(), name.into()))
            }
            ModelResponseChunk::TextDelta(index, text) => Self::TextDelta((index, text)),
            ModelResponseChunk::ReasoningDelta(index, text) => Self::ReasoningDelta((index, text)),
            ModelResponseChunk::ToolInputDelta(index, text) => Self::ToolInputDelta((index, text)),
            ModelResponseChunk::OutputEnd(index, output) => Self::OutputEnd((index, output.into())),
            ModelResponseChunk::ResponseEnd(response) => Self::ResponseEnd(response.into()),
        }
    }
}

impl From<ModelCompactionResponse> for types::ModelCompactionResponse {
    fn from(value: ModelCompactionResponse) -> Self {
        Self {
            input: value.input.into_iter().map(Into::into).collect(),
            usage: value.usage.into(),
        }
    }
}

impl From<types::AgentId> for AgentId {
    fn from(id: types::AgentId) -> Self {
        ((u128::from(id.high) << 64) | u128::from(id.low)).into()
    }
}

impl From<types::BlobId> for BlobId {
    fn from(id: types::BlobId) -> Self {
        ((u128::from(id.high) << 64) | u128::from(id.low)).into()
    }
}

impl From<BlobId> for types::BlobId {
    fn from(id: BlobId) -> Self {
        let id = u128::from(id);
        Self {
            high: u64::try_from(id >> 64).expect("high bits fit in u64"),
            low: u64::try_from(id & u128::from(u64::MAX)).expect("low bits fit in u64"),
        }
    }
}

impl From<types::ModelCompaction> for ModelCompaction {
    fn from(value: types::ModelCompaction) -> Self {
        Self {
            content: value.content,
            redacted_content: value.redacted_content,
            attestation: value.attestation,
        }
    }
}

impl From<ModelCompaction> for types::ModelCompaction {
    fn from(value: ModelCompaction) -> Self {
        Self {
            content: value.content,
            redacted_content: value.redacted_content,
            attestation: value.attestation,
        }
    }
}
