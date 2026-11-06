use crate::{WasmError, bindings::types};
use fleet_core::{
    AgentId, Configured, ContentBlock, JsonObject, Model, ModelCompaction, ModelCompactionResponse,
    ModelContext, ModelContinuation, ModelInput, ModelOutput, ModelProvenance, ModelReasoning,
    ModelResponse, ModelResponseChunk, ModelTool, ModelUsage, Prompt, PromptContext, PromptOutput,
    PromptResponse, Role, RoleCompaction, RoleParameter, RoleParameterName, RoleParameterTarget,
    Tool, ToolAlias, ToolCall, ToolContext, ToolOutput, ToolResult,
};
use std::collections::BTreeMap;

fn json(object: &JsonObject) -> String {
    serde_json::to_string(object).expect("JSON objects can be serialized")
}

impl From<AgentId> for types::AgentId {
    fn from(id: AgentId) -> Self {
        let id = u128::from(id);
        Self {
            high: u64::try_from(id >> 64).expect("high bits fit in u64"),
            low: u64::try_from(id & u128::from(u64::MAX)).expect("low bits fit in u64"),
        }
    }
}

impl<N: AsRef<str>> From<&Configured<N>> for types::Configured {
    fn from(value: &Configured<N>) -> Self {
        Self {
            name: value.name.as_ref().into(),
            config: json(&value.config),
        }
    }
}

impl<N: From<String>> TryFrom<types::Configured> for Configured<N> {
    type Error = WasmError;

    fn try_from(value: types::Configured) -> Result<Self, Self::Error> {
        Ok(Self {
            name: value.name.into(),
            config: serde_json::from_str(&value.config)?,
        })
    }
}

impl TryFrom<types::Role> for Role {
    type Error = WasmError;

    fn try_from(value: types::Role) -> Result<Self, Self::Error> {
        let mut parameters = BTreeMap::new();
        for (name, parameter) in value.parameters {
            let name = RoleParameterName::from(name);
            if parameters
                .insert(name.clone(), parameter.try_into()?)
                .is_some()
            {
                return Err(WasmError::duplicate_role_parameter(name));
            }
        }

        let mut tools = BTreeMap::new();
        for (alias, tool) in value.tools {
            let alias = ToolAlias::from(alias);
            if tools.insert(alias.clone(), tool.try_into()?).is_some() {
                return Err(WasmError::duplicate_tool_alias(alias));
            }
        }

        Ok(Self {
            name: value.name.into(),
            parameters,
            model: value.model.try_into()?,
            tools,
            system_prompt: value.system_prompt,
            compaction: value.compaction.map(Into::into),
            extensions: serde_json::from_str(&value.extensions)?,
        })
    }
}

impl TryFrom<types::RoleParameter> for RoleParameter {
    type Error = WasmError;

    fn try_from(value: types::RoleParameter) -> Result<Self, Self::Error> {
        Ok(Self {
            target: match value.target {
                types::RoleParameterTarget::Model(path) => RoleParameterTarget::Model(path),
                types::RoleParameterTarget::Tool((alias, path)) => {
                    RoleParameterTarget::Tool(alias.into(), path)
                }
                types::RoleParameterTarget::Extensions(path) => {
                    RoleParameterTarget::Extensions(path)
                }
            },
            extensions: serde_json::from_str(&value.extensions)?,
        })
    }
}

impl From<types::RoleCompaction> for RoleCompaction {
    fn from(value: types::RoleCompaction) -> Self {
        Self {
            threshold: value.threshold,
            instructions: value.instructions,
        }
    }
}

impl TryFrom<types::Model> for Model {
    type Error = WasmError;

    fn try_from(value: types::Model) -> Result<Self, Self::Error> {
        Ok(Self {
            name: value.name.into(),
            context_window: value.context_window,
            compaction_threshold: value.compaction_threshold,
            extensions: serde_json::from_str(&value.extensions)?,
        })
    }
}

impl From<&ModelContext> for types::ModelContext {
    fn from(value: &ModelContext) -> Self {
        Self {
            model: (&value.model).into(),
            system_prompt: value.system_prompt.clone(),
            tools: value.tools.iter().map(Into::into).collect(),
            role_extensions: json(&value.role_extensions),
        }
    }
}

impl From<&ModelTool> for types::ModelTool {
    fn from(value: &ModelTool) -> Self {
        Self {
            name: value.name.as_ref().into(),
            description: value.description.clone(),
            is_async: value.is_async,
            input_schema: json(&value.input_schema),
        }
    }
}

impl From<&ModelInput> for types::ModelInput {
    fn from(value: &ModelInput) -> Self {
        match value {
            ModelInput::User(content) => Self::User(content.iter().map(Into::into).collect()),
            ModelInput::Assistant(provenance, output) => {
                Self::Assistant((provenance.into(), output.iter().map(Into::into).collect()))
            }
            ModelInput::ToolResult(result) => Self::ToolResult(result.into()),
            ModelInput::Compaction(provenance, compaction) => {
                Self::Compaction((provenance.into(), compaction.into()))
            }
        }
    }
}

impl TryFrom<types::ModelInput> for ModelInput {
    type Error = WasmError;

    fn try_from(value: types::ModelInput) -> Result<Self, Self::Error> {
        Ok(match value {
            types::ModelInput::User(content) => {
                Self::User(content.into_iter().map(Into::into).collect())
            }
            types::ModelInput::Assistant((provenance, output)) => Self::Assistant(
                provenance.into(),
                output
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
            ),
            types::ModelInput::ToolResult(result) => Self::ToolResult(result.try_into()?),
            types::ModelInput::Compaction((provenance, compaction)) => {
                Self::Compaction(provenance.into(), compaction.into())
            }
        })
    }
}

impl TryFrom<types::ModelResponse> for ModelResponse {
    type Error = WasmError;

    fn try_from(value: types::ModelResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            output: value
                .output
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            usage: value.usage.into(),
            provenance: value.provenance.into(),
            continuation_token: value.continuation_token,
        })
    }
}

impl TryFrom<types::ModelResponseChunk> for ModelResponseChunk {
    type Error = WasmError;

    fn try_from(value: types::ModelResponseChunk) -> Result<Self, Self::Error> {
        Ok(match value {
            types::ModelResponseChunk::TextStart(index) => Self::TextStart(index),
            types::ModelResponseChunk::ReasoningStart(index) => Self::ReasoningStart(index),
            types::ModelResponseChunk::ToolCallStart((index, id, tool)) => {
                Self::ToolCallStart(index, id.into(), tool.into())
            }
            types::ModelResponseChunk::TextDelta((index, content)) => {
                Self::TextDelta(index, content)
            }
            types::ModelResponseChunk::ReasoningDelta((index, content)) => {
                Self::ReasoningDelta(index, content)
            }
            types::ModelResponseChunk::ToolInputDelta((index, content)) => {
                Self::ToolInputDelta(index, content)
            }
            types::ModelResponseChunk::OutputEnd((index, output)) => {
                Self::OutputEnd(index, output.try_into()?)
            }
            types::ModelResponseChunk::ResponseEnd(response) => {
                Self::ResponseEnd(response.try_into()?)
            }
        })
    }
}

impl From<&ModelOutput> for types::ModelOutput {
    fn from(value: &ModelOutput) -> Self {
        match value {
            ModelOutput::Text(content) => Self::Text(content.clone()),
            ModelOutput::Reasoning(reasoning) => Self::Reasoning(reasoning.into()),
            ModelOutput::ToolCall(call) => Self::ToolCall(call.into()),
        }
    }
}

impl TryFrom<types::ModelOutput> for ModelOutput {
    type Error = WasmError;

    fn try_from(value: types::ModelOutput) -> Result<Self, Self::Error> {
        Ok(match value {
            types::ModelOutput::Text(content) => Self::Text(content),
            types::ModelOutput::Reasoning(reasoning) => Self::Reasoning(reasoning.into()),
            types::ModelOutput::ToolCall(call) => Self::ToolCall(call.try_into()?),
        })
    }
}

impl From<&ModelReasoning> for types::ModelReasoning {
    fn from(value: &ModelReasoning) -> Self {
        Self {
            content: value.content.clone(),
            redacted_content: value.redacted_content.clone(),
            attestation: value.attestation.clone(),
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

impl TryFrom<types::ModelCompactionResponse> for ModelCompactionResponse {
    type Error = WasmError;

    fn try_from(value: types::ModelCompactionResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            input: value
                .input
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            usage: value.usage.into(),
        })
    }
}

impl From<&ModelCompaction> for types::ModelCompaction {
    fn from(value: &ModelCompaction) -> Self {
        Self {
            content: value.content.clone(),
            redacted_content: value.redacted_content.clone(),
            attestation: value.attestation.clone(),
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

impl From<types::ModelUsage> for ModelUsage {
    fn from(value: types::ModelUsage) -> Self {
        Self {
            input: value.input,
            output: value.output,
            cache_read: value.cache_read,
            cache_write: value.cache_write,
        }
    }
}

impl From<&ModelContinuation> for types::ModelContinuation {
    fn from(value: &ModelContinuation) -> Self {
        Self {
            token: value.token.clone(),
            provenance: (&value.provenance).into(),
        }
    }
}

impl From<&ModelProvenance> for types::ModelProvenance {
    fn from(value: &ModelProvenance) -> Self {
        Self {
            provider_id: value.provider_id.clone(),
            model_id: value.model_id.clone(),
        }
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

impl TryFrom<types::Tool> for Tool {
    type Error = WasmError;

    fn try_from(value: types::Tool) -> Result<Self, Self::Error> {
        Ok(Self {
            name: value.name.into(),
            description: value.description,
            is_async: value.is_async,
            input_schema: serde_json::from_str(&value.input_schema)?,
            output_schema: value
                .output_schema
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            extensions: serde_json::from_str(&value.extensions)?,
        })
    }
}

impl From<&ToolContext> for types::ToolContext {
    fn from(value: &ToolContext) -> Self {
        Self {
            tool: (&value.tool).into(),
            agent_id: value.agent_id.into(),
            call_id: value.call_id.as_ref().into(),
            role_extensions: json(&value.role_extensions),
        }
    }
}

impl From<&ToolCall> for types::ToolCall {
    fn from(value: &ToolCall) -> Self {
        Self {
            id: value.id.as_ref().into(),
            tool: value.tool.as_ref().into(),
            is_async: value.is_async,
            input: json(&value.input),
        }
    }
}

impl TryFrom<types::ToolCall> for ToolCall {
    type Error = WasmError;

    fn try_from(value: types::ToolCall) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id.into(),
            tool: value.tool.into(),
            is_async: value.is_async,
            input: serde_json::from_str(&value.input)?,
        })
    }
}

impl From<&ToolResult> for types::ToolResult {
    fn from(value: &ToolResult) -> Self {
        Self {
            call_id: value.call_id.as_ref().into(),
            output: (&value.output).into(),
        }
    }
}

impl TryFrom<types::ToolResult> for ToolResult {
    type Error = WasmError;

    fn try_from(value: types::ToolResult) -> Result<Self, Self::Error> {
        Ok(Self {
            call_id: value.call_id.into(),
            output: value.output.try_into()?,
        })
    }
}

impl From<&ToolOutput> for types::ToolOutput {
    fn from(value: &ToolOutput) -> Self {
        Self {
            content: value.content.iter().map(Into::into).collect(),
            structured_content: value.structured_content.as_ref().map(json),
            is_error: value.is_error,
        }
    }
}

impl TryFrom<types::ToolOutput> for ToolOutput {
    type Error = WasmError;

    fn try_from(value: types::ToolOutput) -> Result<Self, Self::Error> {
        Ok(Self {
            content: value.content.into_iter().map(Into::into).collect(),
            structured_content: value
                .structured_content
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            is_error: value.is_error,
        })
    }
}

impl TryFrom<types::Prompt> for Prompt {
    type Error = WasmError;

    fn try_from(value: types::Prompt) -> Result<Self, Self::Error> {
        Ok(Self {
            name: value.name.into(),
            input_schema: serde_json::from_str(&value.input_schema)?,
            extensions: serde_json::from_str(&value.extensions)?,
        })
    }
}

impl From<&PromptContext> for types::PromptContext {
    fn from(value: &PromptContext) -> Self {
        Self {
            prompt: (&value.prompt).into(),
            agent_id: value.agent_id.into(),
        }
    }
}

impl From<types::PromptResponse> for PromptResponse {
    fn from(value: types::PromptResponse) -> Self {
        Self {
            output: value.output.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<types::PromptOutput> for PromptOutput {
    fn from(value: types::PromptOutput) -> Self {
        match value {
            types::PromptOutput::User(content) => {
                Self::User(content.into_iter().map(Into::into).collect())
            }
            types::PromptOutput::Assistant(content) => {
                Self::Assistant(content.into_iter().map(Into::into).collect())
            }
        }
    }
}

impl From<&ContentBlock> for types::ContentBlock {
    fn from(value: &ContentBlock) -> Self {
        match value {
            ContentBlock::Text(text) => Self::Text(text.clone()),
        }
    }
}

impl From<types::ContentBlock> for ContentBlock {
    fn from(value: types::ContentBlock) -> Self {
        match value {
            types::ContentBlock::Text(text) => Self::Text(text),
        }
    }
}
