use crate::{
    WasmError,
    bindings::{types, workspaces::exports::fleet::providers::workspaces},
};
use fleet_core::{
    AgentId, ContentBlock, JsonObject, Model, ModelCompaction, ModelCompactionResponse,
    ModelContext, ModelContinuation, ModelInput, ModelOutput, ModelProvenance,
    ModelProviderToolCall, ModelReasoning, ModelResponse, ModelResponseChunk, ModelTool,
    ModelUsage, Prompt, PromptContext, PromptOutput, PromptResponse, Tool, ToolCall, ToolContext,
    ToolOutput, ToolResult,
};
use fleet_kit::{
    Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceExit, WorkspaceId, WorkspaceOutput,
    WorkspaceWrite,
};

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

impl types::ModelContext {
    pub(crate) fn new(name: &str, value: &ModelContext) -> Self {
        Self {
            model: types::Configured {
                name: name.into(),
                config: json(&value.model.config),
            },
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
            types::ModelResponseChunk::ProviderToolCallStart((index, call_id, name)) => {
                Self::ProviderToolCallStart(index, call_id.into(), name.into())
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
            ModelOutput::ProviderToolCall(call) => Self::ProviderToolCall(call.into()),
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
            types::ModelOutput::ProviderToolCall(call) => Self::ProviderToolCall(call.into()),
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

impl From<&ModelProviderToolCall> for types::ModelProviderToolCall {
    fn from(value: &ModelProviderToolCall) -> Self {
        Self {
            call_id: value.call_id.as_ref().into(),
            name: value.name.as_ref().into(),
            content: value.content.clone(),
            payload: value.payload.clone(),
        }
    }
}

impl From<types::ModelProviderToolCall> for ModelProviderToolCall {
    fn from(value: types::ModelProviderToolCall) -> Self {
        Self {
            call_id: value.call_id.into(),
            name: value.name.into(),
            content: value.content,
            payload: value.payload,
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

impl types::ToolContext {
    pub(crate) fn new(name: &str, value: &ToolContext) -> Self {
        Self {
            tool: types::Configured {
                name: name.into(),
                config: json(&value.tool.config),
            },
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

impl types::PromptContext {
    pub(crate) fn new(name: &str, value: &PromptContext) -> Self {
        Self {
            prompt: types::Configured {
                name: name.into(),
                config: json(&value.prompt.config),
            },
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

impl TryFrom<workspaces::Workspace> for Workspace {
    type Error = WasmError;

    fn try_from(value: workspaces::Workspace) -> Result<Self, Self::Error> {
        Ok(Self {
            name: value.name.into(),
            extensions: serde_json::from_str(&value.extensions)?,
        })
    }
}

impl From<WorkspaceId> for workspaces::WorkspaceId {
    fn from(id: WorkspaceId) -> Self {
        let id = u128::from(id);
        Self {
            high: u64::try_from(id >> 64).expect("high bits fit in u64"),
            low: u64::try_from(id & u128::from(u64::MAX)).expect("low bits fit in u64"),
        }
    }
}

impl workspaces::WorkspaceContext {
    pub(crate) fn new(name: &str, value: &WorkspaceContext) -> Self {
        Self {
            workspace: types::Configured {
                name: name.into(),
                config: json(&value.workspace.config),
            },
            id: value.id.into(),
        }
    }
}

impl From<&WorkspaceWrite> for workspaces::WorkspaceWrite {
    fn from(value: &WorkspaceWrite) -> Self {
        Self {
            path: value.path.clone(),
            content: value.content.clone(),
            expected: value.expected.clone(),
            create_parents: value.create_parents,
        }
    }
}

impl From<&WorkspaceCommand> for workspaces::WorkspaceCommand {
    fn from(value: &WorkspaceCommand) -> Self {
        Self {
            id: value.id.clone(),
            program: value.program.clone(),
            args: value.args.clone(),
            env: value
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            cwd: value.cwd.clone(),
            stdin: value.stdin.clone(),
            timeout_ms: value.timeout_ms,
            output_limit: value.output_limit,
        }
    }
}

impl From<workspaces::WorkspaceOutput> for WorkspaceOutput {
    fn from(value: workspaces::WorkspaceOutput) -> Self {
        Self {
            exit: match value.exit {
                workspaces::WorkspaceExit::Code(code) => WorkspaceExit::Code(code),
                workspaces::WorkspaceExit::Terminated => WorkspaceExit::Terminated,
                workspaces::WorkspaceExit::TimedOut => WorkspaceExit::TimedOut,
            },
            stdout: value.stdout,
            stderr: value.stderr,
            stdout_truncated: value.stdout_truncated,
            stderr_truncated: value.stderr_truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::Configured;
    use serde_json::json;

    #[test]
    fn workspace_preserves_extensions() {
        let extensions = json!({
            "title": "Local directory",
            "config_schema": {
                "type": "object",
                "properties": {
                    "cwd": {
                        "type": "string",
                        "x-picker": "directory",
                    },
                },
            },
        });
        let workspace = workspaces::Workspace {
            name: "local".into(),
            extensions: extensions.to_string(),
        };

        let workspace = Workspace::try_from(workspace).unwrap();

        assert_eq!(workspace.name.as_ref(), "local");
        assert_eq!(workspace.extensions, *extensions.as_object().unwrap());
    }

    #[test]
    fn workspace_context_preserves_identity_and_config() {
        let context = WorkspaceContext {
            workspace: Configured {
                name: "example:local".into(),
                config: JsonObject::from_iter([("cwd".into(), json!("/project"))]),
            },
            id: u128::MAX.into(),
        };

        let context = workspaces::WorkspaceContext::new("local", &context);
        let config: JsonObject = serde_json::from_str(&context.workspace.config).unwrap();

        assert_eq!(context.workspace.name, "local");
        assert_eq!(context.id.high, u64::MAX);
        assert_eq!(context.id.low, u64::MAX);
        assert_eq!(config["cwd"], "/project");
    }
}
