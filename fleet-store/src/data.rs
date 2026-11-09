use fleet_core::JsonObject;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub id: u128,
    pub role: Role,
    pub extensions: JsonObject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: u128,
    pub mode: MessageMode,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageMode {
    Steer,
    Queue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub cursor: u128,
    pub payload: EventPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventPayload {
    Input(u128, Vec<ContentBlock>),
    ModelResponse(ModelResponse),
    ToolResult(ToolResult),
    Compaction(Compaction),
    TurnEnd(TurnEnd),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub output: Vec<ModelOutput>,
    pub usage: ModelUsage,
    pub provenance: ModelProvenance,
    pub continuation_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelOutput {
    Text(String),
    Reasoning(ModelReasoning),
    ToolCall(ToolCall),
    ProviderToolCall(ModelProviderToolCall),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReasoning {
    pub content: Option<String>,
    pub redacted_content: Option<String>,
    pub attestation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProviderToolCall {
    pub call_id: String,
    pub name: String,
    pub content: Option<String>,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_write: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProvenance {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compaction {
    pub last_event: u128,
    pub input: Vec<ModelInput>,
    pub pending_tool_calls: Vec<ToolCall>,
    pub usage: ModelUsage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelInput {
    User(Vec<ContentBlock>),
    Assistant(ModelProvenance, Vec<ModelOutput>),
    ToolResult(ToolResult),
    Compaction(ModelProvenance, ModelCompaction),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCompaction {
    pub content: Option<String>,
    pub redacted_content: Option<String>,
    pub attestation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub tool: String,
    pub is_async: bool,
    pub input: JsonObject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output: ToolOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub structured_content: Option<JsonObject>,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnEnd {
    Completed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    pub config: JsonObject,
}

impl From<fleet_core::Agent> for Agent {
    fn from(value: fleet_core::Agent) -> Self {
        Self {
            id: value.id.into(),
            role: value.role.into(),
            extensions: value.extensions,
        }
    }
}

impl From<Agent> for fleet_core::Agent {
    fn from(value: Agent) -> Self {
        Self {
            id: value.id.into(),
            role: value.role.into(),
            extensions: value.extensions,
        }
    }
}

impl From<fleet_core::Message> for Message {
    fn from(value: fleet_core::Message) -> Self {
        Self {
            id: value.id.into(),
            mode: value.mode.into(),
            content: value.content.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<Message> for fleet_core::Message {
    fn from(value: Message) -> Self {
        Self {
            id: value.id.into(),
            mode: value.mode.into(),
            content: value.content.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<fleet_core::ContentBlock> for ContentBlock {
    fn from(value: fleet_core::ContentBlock) -> Self {
        match value {
            fleet_core::ContentBlock::Text(value_0) => Self::Text(value_0),
        }
    }
}

impl From<ContentBlock> for fleet_core::ContentBlock {
    fn from(value: ContentBlock) -> Self {
        match value {
            ContentBlock::Text(value_0) => Self::Text(value_0),
        }
    }
}

impl From<fleet_core::MessageMode> for MessageMode {
    fn from(value: fleet_core::MessageMode) -> Self {
        match value {
            fleet_core::MessageMode::Steer => Self::Steer,
            fleet_core::MessageMode::Queue => Self::Queue,
        }
    }
}

impl From<MessageMode> for fleet_core::MessageMode {
    fn from(value: MessageMode) -> Self {
        match value {
            MessageMode::Steer => Self::Steer,
            MessageMode::Queue => Self::Queue,
        }
    }
}

impl From<fleet_core::Event> for Event {
    fn from(value: fleet_core::Event) -> Self {
        Self {
            cursor: value.cursor.into(),
            payload: value.payload.into(),
        }
    }
}

impl From<Event> for fleet_core::Event {
    fn from(value: Event) -> Self {
        Self {
            cursor: value.cursor.into(),
            payload: value.payload.into(),
        }
    }
}

impl From<fleet_core::EventPayload> for EventPayload {
    fn from(value: fleet_core::EventPayload) -> Self {
        match value {
            fleet_core::EventPayload::Input(value_0, value_1) => Self::Input(
                value_0.into(),
                value_1.into_iter().map(Into::into).collect(),
            ),
            fleet_core::EventPayload::ModelResponse(value_0) => Self::ModelResponse(value_0.into()),
            fleet_core::EventPayload::ToolResult(value_0) => Self::ToolResult(value_0.into()),
            fleet_core::EventPayload::Compaction(value_0) => Self::Compaction(value_0.into()),
            fleet_core::EventPayload::TurnEnd(value_0) => Self::TurnEnd(value_0.into()),
        }
    }
}

impl From<EventPayload> for fleet_core::EventPayload {
    fn from(value: EventPayload) -> Self {
        match value {
            EventPayload::Input(value_0, value_1) => Self::Input(
                value_0.into(),
                value_1.into_iter().map(Into::into).collect(),
            ),
            EventPayload::ModelResponse(value_0) => Self::ModelResponse(value_0.into()),
            EventPayload::ToolResult(value_0) => Self::ToolResult(value_0.into()),
            EventPayload::Compaction(value_0) => Self::Compaction(value_0.into()),
            EventPayload::TurnEnd(value_0) => Self::TurnEnd(value_0.into()),
        }
    }
}

impl From<fleet_core::ModelResponse> for ModelResponse {
    fn from(value: fleet_core::ModelResponse) -> Self {
        Self {
            output: value.output.into_iter().map(Into::into).collect(),
            usage: value.usage.into(),
            provenance: value.provenance.into(),
            continuation_token: value.continuation_token,
        }
    }
}

impl From<ModelResponse> for fleet_core::ModelResponse {
    fn from(value: ModelResponse) -> Self {
        Self {
            output: value.output.into_iter().map(Into::into).collect(),
            usage: value.usage.into(),
            provenance: value.provenance.into(),
            continuation_token: value.continuation_token,
        }
    }
}

impl From<fleet_core::ModelOutput> for ModelOutput {
    fn from(value: fleet_core::ModelOutput) -> Self {
        match value {
            fleet_core::ModelOutput::Text(value_0) => Self::Text(value_0),
            fleet_core::ModelOutput::Reasoning(value_0) => Self::Reasoning(value_0.into()),
            fleet_core::ModelOutput::ToolCall(value_0) => Self::ToolCall(value_0.into()),
            fleet_core::ModelOutput::ProviderToolCall(value_0) => {
                Self::ProviderToolCall(value_0.into())
            }
        }
    }
}

impl From<ModelOutput> for fleet_core::ModelOutput {
    fn from(value: ModelOutput) -> Self {
        match value {
            ModelOutput::Text(value_0) => Self::Text(value_0),
            ModelOutput::Reasoning(value_0) => Self::Reasoning(value_0.into()),
            ModelOutput::ToolCall(value_0) => Self::ToolCall(value_0.into()),
            ModelOutput::ProviderToolCall(value_0) => Self::ProviderToolCall(value_0.into()),
        }
    }
}

impl From<fleet_core::ModelReasoning> for ModelReasoning {
    fn from(value: fleet_core::ModelReasoning) -> Self {
        Self {
            content: value.content,
            redacted_content: value.redacted_content,
            attestation: value.attestation,
        }
    }
}

impl From<ModelReasoning> for fleet_core::ModelReasoning {
    fn from(value: ModelReasoning) -> Self {
        Self {
            content: value.content,
            redacted_content: value.redacted_content,
            attestation: value.attestation,
        }
    }
}

impl From<fleet_core::ModelProviderToolCall> for ModelProviderToolCall {
    fn from(value: fleet_core::ModelProviderToolCall) -> Self {
        Self {
            call_id: value.call_id.into(),
            name: value.name.into(),
            content: value.content,
            payload: value.payload,
        }
    }
}

impl From<ModelProviderToolCall> for fleet_core::ModelProviderToolCall {
    fn from(value: ModelProviderToolCall) -> Self {
        Self {
            call_id: value.call_id.into(),
            name: value.name.into(),
            content: value.content,
            payload: value.payload,
        }
    }
}

impl From<fleet_core::ModelUsage> for ModelUsage {
    fn from(value: fleet_core::ModelUsage) -> Self {
        Self {
            input: value.input,
            output: value.output,
            cache_read: value.cache_read,
            cache_write: value.cache_write,
        }
    }
}

impl From<ModelUsage> for fleet_core::ModelUsage {
    fn from(value: ModelUsage) -> Self {
        Self {
            input: value.input,
            output: value.output,
            cache_read: value.cache_read,
            cache_write: value.cache_write,
        }
    }
}

impl From<fleet_core::ModelProvenance> for ModelProvenance {
    fn from(value: fleet_core::ModelProvenance) -> Self {
        Self {
            provider_id: value.provider_id,
            model_id: value.model_id,
        }
    }
}

impl From<ModelProvenance> for fleet_core::ModelProvenance {
    fn from(value: ModelProvenance) -> Self {
        Self {
            provider_id: value.provider_id,
            model_id: value.model_id,
        }
    }
}

impl From<fleet_core::Compaction> for Compaction {
    fn from(value: fleet_core::Compaction) -> Self {
        Self {
            last_event: value.last_event.into(),
            input: value.input.into_iter().map(Into::into).collect(),
            pending_tool_calls: value
                .pending_tool_calls
                .into_iter()
                .map(Into::into)
                .collect(),
            usage: value.usage.into(),
        }
    }
}

impl From<Compaction> for fleet_core::Compaction {
    fn from(value: Compaction) -> Self {
        Self {
            last_event: value.last_event.into(),
            input: value.input.into_iter().map(Into::into).collect(),
            pending_tool_calls: value
                .pending_tool_calls
                .into_iter()
                .map(Into::into)
                .collect(),
            usage: value.usage.into(),
        }
    }
}

impl From<fleet_core::ModelInput> for ModelInput {
    fn from(value: fleet_core::ModelInput) -> Self {
        match value {
            fleet_core::ModelInput::User(value_0) => {
                Self::User(value_0.into_iter().map(Into::into).collect())
            }
            fleet_core::ModelInput::Assistant(value_0, value_1) => Self::Assistant(
                value_0.into(),
                value_1.into_iter().map(Into::into).collect(),
            ),
            fleet_core::ModelInput::ToolResult(value_0) => Self::ToolResult(value_0.into()),
            fleet_core::ModelInput::Compaction(value_0, value_1) => {
                Self::Compaction(value_0.into(), value_1.into())
            }
        }
    }
}

impl From<ModelInput> for fleet_core::ModelInput {
    fn from(value: ModelInput) -> Self {
        match value {
            ModelInput::User(value_0) => Self::User(value_0.into_iter().map(Into::into).collect()),
            ModelInput::Assistant(value_0, value_1) => Self::Assistant(
                value_0.into(),
                value_1.into_iter().map(Into::into).collect(),
            ),
            ModelInput::ToolResult(value_0) => Self::ToolResult(value_0.into()),
            ModelInput::Compaction(value_0, value_1) => {
                Self::Compaction(value_0.into(), value_1.into())
            }
        }
    }
}

impl From<fleet_core::ModelCompaction> for ModelCompaction {
    fn from(value: fleet_core::ModelCompaction) -> Self {
        Self {
            content: value.content,
            redacted_content: value.redacted_content,
            attestation: value.attestation,
        }
    }
}

impl From<ModelCompaction> for fleet_core::ModelCompaction {
    fn from(value: ModelCompaction) -> Self {
        Self {
            content: value.content,
            redacted_content: value.redacted_content,
            attestation: value.attestation,
        }
    }
}

impl From<fleet_core::ToolCall> for ToolCall {
    fn from(value: fleet_core::ToolCall) -> Self {
        Self {
            id: value.id.into(),
            tool: value.tool.into(),
            is_async: value.is_async,
            input: value.input,
        }
    }
}

impl From<ToolCall> for fleet_core::ToolCall {
    fn from(value: ToolCall) -> Self {
        Self {
            id: value.id.into(),
            tool: value.tool.into(),
            is_async: value.is_async,
            input: value.input,
        }
    }
}

impl From<fleet_core::ToolResult> for ToolResult {
    fn from(value: fleet_core::ToolResult) -> Self {
        Self {
            call_id: value.call_id.into(),
            output: value.output.into(),
        }
    }
}

impl From<ToolResult> for fleet_core::ToolResult {
    fn from(value: ToolResult) -> Self {
        Self {
            call_id: value.call_id.into(),
            output: value.output.into(),
        }
    }
}

impl From<fleet_core::ToolOutput> for ToolOutput {
    fn from(value: fleet_core::ToolOutput) -> Self {
        Self {
            content: value.content.into_iter().map(Into::into).collect(),
            structured_content: value.structured_content,
            is_error: value.is_error,
        }
    }
}

impl From<ToolOutput> for fleet_core::ToolOutput {
    fn from(value: ToolOutput) -> Self {
        Self {
            content: value.content.into_iter().map(Into::into).collect(),
            structured_content: value.structured_content,
            is_error: value.is_error,
        }
    }
}

impl From<fleet_core::TurnEnd> for TurnEnd {
    fn from(value: fleet_core::TurnEnd) -> Self {
        match value {
            fleet_core::TurnEnd::Completed => Self::Completed,
            fleet_core::TurnEnd::Interrupted => Self::Interrupted,
        }
    }
}

impl From<TurnEnd> for fleet_core::TurnEnd {
    fn from(value: TurnEnd) -> Self {
        match value {
            TurnEnd::Completed => Self::Completed,
            TurnEnd::Interrupted => Self::Interrupted,
        }
    }
}

impl From<fleet_core::Configured<fleet_core::RoleName>> for Role {
    fn from(value: fleet_core::Configured<fleet_core::RoleName>) -> Self {
        Self {
            name: value.name.into(),
            config: value.config,
        }
    }
}

impl From<Role> for fleet_core::Configured<fleet_core::RoleName> {
    fn from(value: Role) -> Self {
        Self {
            name: value.name.into(),
            config: value.config,
        }
    }
}
