use core::{num::NonZeroUsize, str::FromStr};
use fleet_core::{
    Agent, Compaction, Configured, ContentBlock, Event, EventPayload, JsonObject, Message,
    MessageMode, ModelCompaction, ModelInput, ModelOutput, ModelProvenance, ModelProviderToolCall,
    ModelReasoning, ModelResponse, ModelUsage, Role, RoleParameterTarget, ToolCall, ToolOutput,
    ToolResult, TurnEnd,
};
use fleet_kit::{
    AgentActivity, AgentMetadata, AgentUpdates, EventDirection, EventQuery, EventRecord,
    MessageRecord, Preview, PreviewContent, QueryUpdate, ToolCallRecord, TurnStatus,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::value::MapDeserializer};
use std::collections::BTreeMap;

// === Configuration ===

/// A configured provider.
#[derive(Serialize)]
pub struct ApiProvider<'a> {
    /// The provider name.
    pub name: &'a str,
    /// The plugin supplying the provider.
    pub plugin: &'a str,
}

/// An available role.
#[derive(Serialize)]
pub struct ApiRole {
    /// The role name.
    pub name: String,
    /// The display title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A description of the role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether to hide the role from selection.
    pub hidden: bool,
    /// The available parameters.
    pub parameters: BTreeMap<String, ApiRoleParameter>,
}

/// A role parameter.
#[derive(Serialize)]
pub struct ApiRoleParameter {
    /// The display title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A description of the parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The JSON Schema for supplied values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    /// The value configured by the role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RoleMetadata {
    title: Option<String>,
    description: Option<String>,
    hidden: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ParameterMetadata {
    title: Option<String>,
    description: Option<String>,
    schema: Option<serde_json::Value>,
}

// === Requests ===

/// A request to create an agent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAgentRequest {
    /// The agent ID.
    #[serde(deserialize_with = "deserialize_id")]
    pub id: u128,
    /// The agent's role.
    pub role: String,
    /// The role parameters.
    #[serde(default)]
    pub parameters: JsonObject,
    /// The optional display title.
    pub title: Option<String>,
    /// The initial message.
    pub message: Option<ApiMessage>,
}

/// A request to set or clear an agent's display title.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetTitleRequest {
    /// The new title, or null to clear it. The field must be present.
    #[serde(deserialize_with = "Option::deserialize")]
    pub title: Option<String>,
}

/// A request to interrupt an agent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptAgentRequest {
    /// A message to send with the interrupt.
    pub message: Option<ApiMessage>,
}

/// A query for changes after a revision.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinceQuery {
    /// The exclusive starting revision.
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub since: Option<u128>,
}

/// A query for an agent's events.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsQuery {
    /// The exclusive starting cursor.
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub from: Option<u128>,
    /// The exclusive stopping cursor.
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub until: Option<u128>,
    /// The direction to read events.
    #[serde(default)]
    pub direction: ApiEventDirection,
    /// The maximum number of events to return.
    #[serde(
        default = "default_event_limit",
        deserialize_with = "deserialize_limit"
    )]
    pub limit: NonZeroUsize,
    /// The exclusive starting revision.
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub since: Option<u128>,
}

// === Agents ===

/// A stateless collection scan. The SSE endpoint accepts none of these options.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsReadQuery {
    #[serde(default)]
    inbox: ApiInboxFilter,
    #[serde(default, deserialize_with = "deserialize_agent_position")]
    from: Option<fleet_kit::AgentPosition>,
    #[serde(default, deserialize_with = "deserialize_agent_position")]
    until: Option<fleet_kit::AgentPosition>,
    #[serde(default)]
    direction: ApiEventDirection,
    #[serde(
        default = "default_event_limit",
        deserialize_with = "deserialize_limit"
    )]
    limit: NonZeroUsize,
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    since: Option<u128>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ApiInboxFilter {
    #[default]
    All,
    Unsettled,
    Pending,
    Busy,
    Snoozed,
    Settled,
}

impl FromStr for AgentsReadQuery {
    type Err = serde::de::value::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::deserialize(MapDeserializer::new(form_urlencoded::parse(
            value.as_bytes(),
        )))
    }
}

impl From<AgentsReadQuery> for fleet_kit::AgentsQuery {
    fn from(value: AgentsReadQuery) -> Self {
        use fleet_kit::{InboxFilter, InboxState};
        Self {
            inbox: match value.inbox {
                ApiInboxFilter::All => InboxFilter::All,
                ApiInboxFilter::Unsettled => InboxFilter::Unsettled,
                ApiInboxFilter::Pending => InboxFilter::State(InboxState::Pending),
                ApiInboxFilter::Busy => InboxFilter::State(InboxState::Busy),
                ApiInboxFilter::Snoozed => InboxFilter::State(InboxState::Snoozed),
                ApiInboxFilter::Settled => InboxFilter::State(InboxState::Settled),
            },
            from: value.from,
            until: value.until,
            direction: value.direction.into(),
            limit: value.limit,
            since: value.since.map(fleet_kit::CollectionRevision),
        }
    }
}

const fn inbox_name(value: fleet_kit::InboxState) -> &'static str {
    use fleet_kit::InboxState;
    match value {
        InboxState::Pending => "pending",
        InboxState::Busy => "busy",
        InboxState::Snoozed => "snoozed",
        InboxState::Settled => "settled",
    }
}

fn agent_position(value: fleet_kit::AgentPosition) -> String {
    format!("{}:{}:{}", inbox_name(value.inbox), value.order, value.id)
}

fn deserialize_agent_position<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<fleet_kit::AgentPosition>, D::Error> {
    use fleet_kit::InboxState;
    let value = String::deserialize(deserializer)?;
    let invalid = || serde::de::Error::custom("invalid agent position");
    let mut parts = value.split(':');
    let inbox = match parts.next() {
        Some("pending") => InboxState::Pending,
        Some("busy") => InboxState::Busy,
        Some("snoozed") => InboxState::Snoozed,
        Some("settled") => InboxState::Settled,
        _ => return Err(invalid()),
    };
    let order = parts
        .next()
        .ok_or_else(invalid)?
        .parse::<u128>()
        .map_err(|_| invalid())?;
    let id = parts
        .next()
        .ok_or_else(invalid)?
        .parse::<u128>()
        .map_err(|_| invalid())?;
    if parts.next().is_some() {
        return Err(invalid());
    }
    Ok(Some(fleet_kit::AgentPosition {
        inbox,
        order,
        id: id.into(),
    }))
}

/// An idle agent's desired inbox state.
#[derive(Debug, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
#[expect(
    clippy::empty_enum_variants_with_brackets,
    reason = "Serde only rejects unknown fields for struct variants"
)]
pub enum SetInboxRequest {
    /// Restore to pending.
    Pending {},
    /// Settle the agent.
    Settled {},
    /// Snooze until Unix milliseconds.
    Snoozed {
        /// Deadline in Unix milliseconds.
        until: u64,
    },
}

impl From<SetInboxRequest> for fleet_kit::InboxAction {
    fn from(value: SetInboxRequest) -> Self {
        match value {
            SetInboxRequest::Pending {} => Self::Pending,
            SetInboxRequest::Settled {} => Self::Settle,
            SetInboxRequest::Snoozed { until } => Self::Snooze(until),
        }
    }
}

/// The collection representation; excludes conversation and preview content.
#[derive(Debug, Serialize)]
pub struct ApiAgentSummary {
    #[serde(serialize_with = "serialize_id")]
    id: u128,
    role: String,
    title: Option<String>,
    #[serde(serialize_with = "serialize_id")]
    metadata_version: u128,
    #[serde(serialize_with = "serialize_id")]
    version: u128,
    inbox: &'static str,
    since: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    until: Option<u64>,
    interrupt_pending: bool,
    position: String,
}

impl From<fleet_kit::AgentSummary> for ApiAgentSummary {
    fn from(value: fleet_kit::AgentSummary) -> Self {
        Self {
            id: value.id.into(),
            role: value.role.to_string(),
            title: value.title,
            metadata_version: value.metadata_version.into(),
            version: value.version.0,
            inbox: inbox_name(value.inbox),
            since: value.since,
            until: value.until,
            interrupt_pending: value.interrupt_pending,
            position: agent_position(value.position),
        }
    }
}

/// One collection page. A non-null next position means more rows were present.
#[derive(Debug, Serialize)]
pub struct ApiAgentsPage {
    agents: Vec<ApiAgentSummary>,
    next: Option<String>,
}

impl From<fleet_kit::AgentsPage> for ApiAgentsPage {
    fn from(value: fleet_kit::AgentsPage) -> Self {
        Self {
            agents: value.agents.into_iter().map(Into::into).collect(),
            next: value.next.map(agent_position),
        }
    }
}

/// A complete collection revision group; its revision is the SSE ID.
#[derive(Debug, Serialize)]
pub struct ApiAgentsUpdates {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    agents: Vec<ApiAgentSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    removed: Vec<String>,
}

impl From<fleet_kit::AgentsUpdates> for ApiAgentsUpdates {
    fn from(value: fleet_kit::AgentsUpdates) -> Self {
        Self {
            agents: value.agents.into_iter().map(Into::into).collect(),
            removed: value.removed.into_iter().map(|id| id.to_string()).collect(),
        }
    }
}

/// The resources changed by an agent update.
#[derive(Debug, Serialize)]
pub struct ApiAgentUpdates {
    /// The agent's metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<ApiAgent>,
    /// The changed messages.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<ApiMessageRecord>,
    /// The agent's activity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<ApiAgentActivity>,
    /// The changed tool calls.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ApiToolCallRecord>,
    /// The changed events.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ApiEvent>,
}

/// An agent's metadata.
#[derive(Debug, Serialize)]
pub struct ApiAgent {
    /// The agent ID.
    #[serde(serialize_with = "serialize_id")]
    pub id: u128,
    /// The role name.
    pub role: String,
    /// The role parameters.
    pub parameters: JsonObject,
    /// The optional display title.
    pub title: Option<String>,
    /// The last metadata change, used by title edits' If-Match precondition.
    #[serde(serialize_with = "serialize_id")]
    pub metadata_version: u128,
}

/// An agent's activity.
#[derive(Debug, Serialize)]
pub struct ApiAgentActivity {
    /// Whether a turn is in progress.
    pub turn: ApiTurnStatus,
    /// Whether an interrupt is pending.
    pub interrupt_pending: bool,
}

/// A turn's current status.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiTurnStatus {
    /// The agent is responding.
    Active,
    /// The agent has no active turn.
    Idle,
}

// === Previews ===

/// Transient model activity or accumulated response output.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApiPreview {
    /// A response being streamed.
    Streaming {
        /// The event the response is answering.
        #[serde(serialize_with = "serialize_id")]
        input_cursor: u128,
        /// The partial output blocks.
        output: Vec<ApiPreviewBlock>,
    },
    /// A compaction attempt has started and may be waiting to retry.
    Compacting {
        /// The last event included in the compaction attempt.
        #[serde(serialize_with = "serialize_id")]
        input_cursor: u128,
    },
    /// No model activity or response output is available.
    Idle,
}

/// A partial output block.
#[derive(Debug, Serialize)]
pub struct ApiPreviewBlock {
    /// The output index.
    pub index: u32,
    /// The partial content.
    #[serde(flatten)]
    pub content: ApiPreviewContent,
}

/// The partial content of an output block.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiPreviewContent {
    /// Response text.
    Text {
        /// The text received so far.
        text: String,
    },
    /// Reasoning text.
    Reasoning {
        /// The text received so far.
        text: String,
    },
    /// A tool call.
    ToolCall {
        /// The tool call ID.
        id: String,
        /// The tool alias.
        tool: String,
        /// The partial JSON input.
        input: String,
    },
    /// A tool call handled by the model provider.
    ProviderToolCall {
        /// The call ID assigned by the provider.
        call_id: String,
        /// The provider's name for the tool.
        name: String,
        /// The content received so far.
        content: String,
    },
}

/// Selects the block receiving preview text.
#[derive(Debug, Serialize)]
pub struct ApiPreviewSelection<'a> {
    /// The output index.
    pub index: u32,
    /// Metadata for a new block.
    #[serde(flatten)]
    pub kind: Option<ApiPreviewKind<'a>>,
}

/// Metadata for a new preview block.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiPreviewKind<'a> {
    /// Response text.
    Text,
    /// Reasoning text.
    Reasoning,
    /// A tool call.
    ToolCall {
        /// The tool call ID.
        id: &'a str,
        /// The tool alias.
        tool: &'a str,
    },
    /// A tool call handled by the model provider.
    ProviderToolCall {
        /// The call ID assigned by the provider.
        call_id: &'a str,
        /// The provider's name for the tool.
        name: &'a str,
    },
}

// === Models ===

/// Input to a model.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiModelInput {
    /// Content sent by the user.
    User {
        /// The content blocks.
        content: Vec<ApiContentBlock>,
    },
    /// A previous model response.
    Assistant {
        /// The model that produced the response.
        provenance: ApiModelProvenance,
        /// The output blocks.
        output: Vec<ApiModelOutput>,
    },
    /// A tool result.
    ToolResult(ApiToolResult),
    /// Compacted model context.
    Compaction {
        /// The model that produced the compaction.
        provenance: ApiModelProvenance,
        /// The compacted context.
        #[serde(flatten)]
        compaction: ApiModelCompaction,
    },
}

/// A response from a model.
#[derive(Debug, Serialize)]
pub struct ApiModelResponse {
    /// The output blocks.
    pub output: Vec<ApiModelOutput>,
    /// The token usage.
    pub usage: ApiModelUsage,
    /// The model that produced the response.
    pub provenance: ApiModelProvenance,
    /// A token to continue from this response.
    pub continuation_token: Option<String>,
}

/// An output block from a model.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiModelOutput {
    /// Text content.
    Text {
        /// The text.
        text: String,
    },
    /// Reasoning content.
    Reasoning(ApiModelReasoning),
    /// A tool call.
    ToolCall(ApiToolCall),
    /// A tool call handled by the model provider.
    ProviderToolCall(ApiModelProviderToolCall),
}

/// Reasoning output from a model.
#[derive(Debug, Serialize)]
pub struct ApiModelReasoning {
    /// The readable content.
    pub content: Option<String>,
    /// The opaque content.
    pub redacted_content: Option<String>,
    /// The content's signature or ID.
    pub attestation: Option<String>,
}

/// A tool call handled by the model provider.
#[derive(Debug, Serialize)]
pub struct ApiModelProviderToolCall {
    /// The call ID assigned by the provider.
    pub call_id: String,
    /// The provider's name for the tool.
    pub name: String,
    /// The readable content.
    pub content: Option<String>,
    /// The opaque provider data.
    pub payload: String,
}

/// Compacted context from a model.
#[derive(Debug, Serialize)]
pub struct ApiModelCompaction {
    /// The readable summary.
    pub content: Option<String>,
    /// The opaque content.
    pub redacted_content: Option<String>,
    /// The content's signature or ID.
    pub attestation: Option<String>,
}

/// The token usage of a model call.
#[derive(Debug, Serialize)]
pub struct ApiModelUsage {
    /// The input tokens, including cache reads and writes.
    pub input: u32,
    /// The output tokens.
    pub output: u32,
    /// The tokens read from cache.
    pub cache_read: u32,
    /// The tokens written to cache.
    pub cache_write: u32,
}

/// The source of a model response.
#[derive(Debug, Serialize)]
pub struct ApiModelProvenance {
    /// The model provider ID.
    pub provider_id: String,
    /// The model ID.
    pub model_id: String,
}

// === Tools ===

/// A tool call's current state.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApiToolCallRecord {
    /// A tool call without a committed result.
    Pending {
        /// The tool call ID.
        id: String,
        /// The tool alias.
        tool: String,
        /// Whether the agent can continue before the call completes.
        is_async: bool,
    },
    /// A tool call with a committed result.
    Resolved {
        /// The tool call ID.
        id: String,
    },
}

/// A request to call a tool.
#[derive(Debug, Serialize)]
pub struct ApiToolCall {
    /// The tool call ID.
    pub id: String,
    /// The tool alias.
    pub tool: String,
    /// Whether the model may continue before the tool completes.
    pub is_async: bool,
    /// The tool input.
    pub input: JsonObject,
}

/// The result of a tool call.
#[derive(Debug, Serialize)]
pub struct ApiToolResult {
    /// The tool call ID.
    pub call_id: String,
    /// The tool output.
    pub output: ApiToolOutput,
}

/// The output of a tool call.
#[derive(Debug, Serialize)]
pub struct ApiToolOutput {
    /// The content blocks.
    pub content: Vec<ApiContentBlock>,
    /// The structured output.
    pub structured_content: Option<JsonObject>,
    /// Whether the tool returned an error.
    pub is_error: bool,
}

// === Content ===

/// A content block.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApiContentBlock {
    /// Text content.
    Text {
        /// The text.
        text: String,
    },
    /// An image stored as an immutable blob.
    Image {
        /// The blob identity.
        #[serde(serialize_with = "serialize_id", deserialize_with = "deserialize_id")]
        blob_id: u128,
    },
}

// === Messages ===

/// A message's current state.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApiMessageRecord {
    /// A message waiting to be consumed.
    Pending {
        /// The stable position in the queue.
        #[serde(serialize_with = "serialize_id")]
        position: u128,
        /// The message.
        #[serde(flatten)]
        message: ApiMessage,
    },
    /// A message that was consumed.
    Consumed {
        /// The message ID.
        #[serde(serialize_with = "serialize_id")]
        id: u128,
    },
    /// A message that was deleted.
    Deleted {
        /// The message ID.
        #[serde(serialize_with = "serialize_id")]
        id: u128,
    },
}

/// A message to an agent.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiMessage {
    /// The message ID.
    #[serde(deserialize_with = "deserialize_id", serialize_with = "serialize_id")]
    pub id: u128,
    /// How the message should be delivered.
    pub mode: ApiMessageMode,
    /// The content blocks.
    pub content: Vec<ApiContentBlock>,
}

/// How a message should be delivered.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiMessageMode {
    /// Steers the current turn.
    Steer,
    /// Waits for the next turn.
    Queue,
}

// === Compaction ===

/// A compaction of an agent's model context.
#[derive(Debug, Serialize)]
pub struct ApiCompaction {
    /// The last event included in the compaction.
    #[serde(serialize_with = "serialize_id")]
    pub last_event: u128,
    /// The replacement model input.
    pub input: Vec<ApiModelInput>,
    /// The unresolved tool calls.
    pub pending_tool_calls: Vec<ApiToolCall>,
    /// The token usage.
    pub usage: ApiModelUsage,
}

// === Turns ===

/// The end of a turn.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiTurnEnd {
    /// The agent finished responding.
    Completed,
    /// The turn was interrupted.
    Interrupted,
}

// === Events ===

/// An event in an agent's history.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApiEvent {
    /// An event in the log.
    Settled {
        /// The event cursor.
        #[serde(serialize_with = "serialize_id")]
        cursor: u128,
        /// The event payload.
        payload: ApiEventPayload,
    },
    /// An event that was deleted.
    Deleted {
        /// The event cursor.
        #[serde(serialize_with = "serialize_id")]
        cursor: u128,
    },
}

/// The payload of an agent event.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiEventPayload {
    /// Input sent to the agent.
    Input {
        /// The message ID.
        #[serde(serialize_with = "serialize_id")]
        message_id: u128,
        /// The content blocks.
        content: Vec<ApiContentBlock>,
    },
    /// A model response.
    ModelResponse(ApiModelResponse),
    /// A tool result.
    ToolResult(ApiToolResult),
    /// Compacted model context.
    Compaction(ApiCompaction),
    /// The end of a turn.
    TurnEnd {
        /// Why the turn ended.
        reason: ApiTurnEnd,
    },
}

/// The direction to read events.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiEventDirection {
    /// Reads from oldest to newest.
    #[default]
    Forward,
    /// Reads from newest to oldest.
    Backward,
}

// === Errors ===

/// An API error response.
#[derive(Debug, Serialize)]
pub struct ErrorResponse<'a> {
    /// The error details.
    pub error: ErrorDetails<'a>,
}

/// The details of an API error.
#[derive(Debug, Serialize)]
pub struct ErrorDetails<'a> {
    /// The error code.
    pub code: &'a str,
    /// The error message.
    pub message: &'a str,
}

// === Impls ===

impl TryFrom<Role> for ApiRole {
    type Error = serde_json::Error;

    fn try_from(role: Role) -> Result<Self, Self::Error> {
        let metadata = role
            .extensions
            .get("fleet")
            .map(RoleMetadata::deserialize)
            .transpose()?
            .unwrap_or_default();
        let parameters = role
            .parameters
            .iter()
            .map(|(name, parameter)| {
                let metadata = parameter
                    .extensions
                    .get("fleet")
                    .map(ParameterMetadata::deserialize)
                    .transpose()?
                    .unwrap_or_default();
                let default = if metadata
                    .schema
                    .as_ref()
                    .is_some_and(|schema| schema.get("writeOnly") == Some(&true.into()))
                {
                    None
                } else {
                    parameter_default(&role, &parameter.target)
                };
                Ok((
                    name.to_string(),
                    ApiRoleParameter {
                        title: metadata.title,
                        description: metadata.description,
                        schema: metadata.schema,
                        default,
                    },
                ))
            })
            .collect::<Result<_, Self::Error>>()?;

        Ok(Self {
            name: role.name.to_string(),
            title: metadata.title,
            description: metadata.description,
            hidden: metadata.hidden,
            parameters,
        })
    }
}

fn parameter_default(role: &Role, target: &RoleParameterTarget) -> Option<serde_json::Value> {
    let (config, path) = match target {
        RoleParameterTarget::Model(path) => (&role.model.config, path),
        RoleParameterTarget::Tool(alias, path) => (&role.tools.get(alias)?.config, path),
        RoleParameterTarget::Extensions(path) => (&role.extensions, path),
    };
    let Some((first, rest)) = path.split_first() else {
        return Some(config.clone().into());
    };
    rest.iter()
        .try_fold(config.get(first)?, |value, key| value.get(key))
        .cloned()
}

impl CreateAgentRequest {
    /// Converts the request to an agent and initial message.
    pub fn into_parts(self) -> (Agent, Option<Message>) {
        let Self {
            id,
            role,
            parameters,
            title,
            message,
        } = self;
        let mut agent = Agent {
            id: id.into(),
            role: Configured {
                name: role.into(),
                config: parameters,
            },
            extensions: JsonObject::new(),
        };
        if let Some(title) = title {
            agent
                .extensions
                .insert("fleet".into(), serde_json::json!({ "title": title }));
        }

        (agent, message.map(Into::into))
    }
}

impl FromStr for EventsQuery {
    type Err = serde::de::value::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::deserialize(MapDeserializer::new(form_urlencoded::parse(
            value.as_bytes(),
        )))
    }
}

impl From<ModelInput> for ApiModelInput {
    fn from(input: ModelInput) -> Self {
        match input {
            ModelInput::User(content) => Self::User {
                content: content.into_iter().map(Into::into).collect(),
            },
            ModelInput::Assistant(provenance, output) => Self::Assistant {
                provenance: provenance.into(),
                output: output.into_iter().map(Into::into).collect(),
            },
            ModelInput::ToolResult(result) => Self::ToolResult(result.into()),
            ModelInput::Compaction(provenance, compaction) => Self::Compaction {
                provenance: provenance.into(),
                compaction: compaction.into(),
            },
        }
    }
}

impl From<ModelResponse> for ApiModelResponse {
    fn from(response: ModelResponse) -> Self {
        let ModelResponse {
            output,
            usage,
            provenance,
            continuation_token,
        } = response;

        Self {
            output: output.into_iter().map(Into::into).collect(),
            usage: usage.into(),
            provenance: provenance.into(),
            continuation_token,
        }
    }
}

impl From<ModelOutput> for ApiModelOutput {
    fn from(output: ModelOutput) -> Self {
        match output {
            ModelOutput::Text(text) => Self::Text { text },
            ModelOutput::Reasoning(reasoning) => Self::Reasoning(reasoning.into()),
            ModelOutput::ToolCall(call) => Self::ToolCall(call.into()),
            ModelOutput::ProviderToolCall(call) => Self::ProviderToolCall(call.into()),
        }
    }
}

impl From<ModelReasoning> for ApiModelReasoning {
    fn from(reasoning: ModelReasoning) -> Self {
        let ModelReasoning {
            content,
            redacted_content,
            attestation,
        } = reasoning;
        Self {
            content,
            redacted_content,
            attestation,
        }
    }
}

impl From<ModelProviderToolCall> for ApiModelProviderToolCall {
    fn from(call: ModelProviderToolCall) -> Self {
        let ModelProviderToolCall {
            call_id,
            name,
            content,
            payload,
        } = call;
        Self {
            call_id: call_id.into(),
            name: name.into(),
            content,
            payload,
        }
    }
}

impl From<ModelCompaction> for ApiModelCompaction {
    fn from(compaction: ModelCompaction) -> Self {
        let ModelCompaction {
            content,
            redacted_content,
            attestation,
        } = compaction;
        Self {
            content,
            redacted_content,
            attestation,
        }
    }
}

impl From<ModelUsage> for ApiModelUsage {
    fn from(usage: ModelUsage) -> Self {
        let ModelUsage {
            input,
            output,
            cache_read,
            cache_write,
        } = usage;
        Self {
            input,
            output,
            cache_read,
            cache_write,
        }
    }
}

impl From<ModelProvenance> for ApiModelProvenance {
    fn from(provenance: ModelProvenance) -> Self {
        let ModelProvenance {
            provider_id,
            model_id,
        } = provenance;
        Self {
            provider_id,
            model_id,
        }
    }
}

impl From<ToolCallRecord> for ApiToolCallRecord {
    fn from(record: ToolCallRecord) -> Self {
        match record {
            ToolCallRecord::Pending { id, tool, is_async } => Self::Pending {
                id: id.into(),
                tool: tool.into(),
                is_async,
            },
            ToolCallRecord::Resolved(id) => Self::Resolved { id: id.into() },
        }
    }
}

impl From<ToolCall> for ApiToolCall {
    fn from(call: ToolCall) -> Self {
        let ToolCall {
            id,
            tool,
            is_async,
            input,
        } = call;
        Self {
            id: id.into(),
            tool: tool.into(),
            is_async,
            input,
        }
    }
}

impl From<ToolResult> for ApiToolResult {
    fn from(result: ToolResult) -> Self {
        let ToolResult { call_id, output } = result;
        Self {
            call_id: call_id.into(),
            output: output.into(),
        }
    }
}

impl From<ToolOutput> for ApiToolOutput {
    fn from(output: ToolOutput) -> Self {
        let ToolOutput {
            content,
            structured_content,
            is_error,
        } = output;

        Self {
            content: content.into_iter().map(Into::into).collect(),
            structured_content,
            is_error,
        }
    }
}

impl From<ContentBlock> for ApiContentBlock {
    fn from(content: ContentBlock) -> Self {
        match content {
            ContentBlock::Text(text) => Self::Text { text },
            ContentBlock::Image(id) => Self::Image { blob_id: id.into() },
        }
    }
}

impl From<ApiContentBlock> for ContentBlock {
    fn from(content: ApiContentBlock) -> Self {
        match content {
            ApiContentBlock::Text { text } => Self::Text(text),
            ApiContentBlock::Image { blob_id } => Self::Image(blob_id.into()),
        }
    }
}

impl From<ApiMessage> for Message {
    fn from(message: ApiMessage) -> Self {
        let ApiMessage { id, mode, content } = message;

        Self {
            id: id.into(),
            mode: mode.into(),
            content: content.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<ApiMessageMode> for MessageMode {
    fn from(mode: ApiMessageMode) -> Self {
        match mode {
            ApiMessageMode::Steer => Self::Steer,
            ApiMessageMode::Queue => Self::Queue,
        }
    }
}

impl From<Compaction> for ApiCompaction {
    fn from(compaction: Compaction) -> Self {
        let Compaction {
            last_event,
            input,
            pending_tool_calls,
            usage,
        } = compaction;

        Self {
            last_event: last_event.into(),
            input: input.into_iter().map(Into::into).collect(),
            pending_tool_calls: pending_tool_calls.into_iter().map(Into::into).collect(),
            usage: usage.into(),
        }
    }
}

impl From<TurnEnd> for ApiTurnEnd {
    fn from(turn_end: TurnEnd) -> Self {
        match turn_end {
            TurnEnd::Completed => Self::Completed,
            TurnEnd::Interrupted => Self::Interrupted,
        }
    }
}

impl From<Event> for ApiEvent {
    fn from(event: Event) -> Self {
        let Event { cursor, payload } = event;
        Self::Settled {
            cursor: cursor.into(),
            payload: payload.into(),
        }
    }
}

impl From<EventPayload> for ApiEventPayload {
    fn from(payload: EventPayload) -> Self {
        match payload {
            EventPayload::Input(message_id, content) => Self::Input {
                message_id: message_id.into(),
                content: content.into_iter().map(Into::into).collect(),
            },
            EventPayload::ModelResponse(response) => Self::ModelResponse(response.into()),
            EventPayload::ToolResult(result) => Self::ToolResult(result.into()),
            EventPayload::Compaction(compaction) => Self::Compaction(compaction.into()),
            EventPayload::TurnEnd(turn_end) => Self::TurnEnd {
                reason: turn_end.into(),
            },
        }
    }
}

impl From<ApiEventDirection> for EventDirection {
    fn from(direction: ApiEventDirection) -> Self {
        match direction {
            ApiEventDirection::Forward => Self::Forward,
            ApiEventDirection::Backward => Self::Backward,
        }
    }
}

impl FromStr for SinceQuery {
    type Err = serde::de::value::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::deserialize(MapDeserializer::new(form_urlencoded::parse(
            value.as_bytes(),
        )))
    }
}

impl From<EventsQuery> for EventQuery {
    fn from(query: EventsQuery) -> Self {
        Self {
            from: query.from.map(Into::into),
            until: query.until.map(Into::into),
            direction: query.direction.into(),
            limit: query.limit,
            since: query.since.map(Into::into),
        }
    }
}

impl From<AgentUpdates> for ApiAgentUpdates {
    fn from(updates: AgentUpdates) -> Self {
        Self {
            agent: match updates.agent {
                QueryUpdate::Unchanged => None,
                QueryUpdate::Changed(agent) => Some(agent.into()),
            },
            messages: updates.messages.into_iter().map(Into::into).collect(),
            activity: match updates.activity {
                QueryUpdate::Unchanged => None,
                QueryUpdate::Changed(activity) => Some(activity.into()),
            },
            tool_calls: updates.tool_calls.into_iter().map(Into::into).collect(),
            events: updates.events.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<AgentMetadata> for ApiAgent {
    fn from(metadata: AgentMetadata) -> Self {
        let agent = metadata.agent;
        let title = fleet_kit::agent_title(&agent.extensions).map(str::to_owned);
        Self {
            id: agent.id.into(),
            role: agent.role.name.into(),
            parameters: agent.role.config,
            title,
            metadata_version: metadata.version.into(),
        }
    }
}

impl From<AgentActivity> for ApiAgentActivity {
    fn from(activity: AgentActivity) -> Self {
        Self {
            turn: match activity.turn {
                TurnStatus::Active => ApiTurnStatus::Active,
                TurnStatus::Idle => ApiTurnStatus::Idle,
            },
            interrupt_pending: activity.interrupt_pending,
        }
    }
}

impl From<Preview> for ApiPreview {
    fn from(preview: Preview) -> Self {
        match preview {
            Preview::Response {
                input_cursor,
                output,
            } => Self::Streaming {
                input_cursor: input_cursor.into(),
                output: output
                    .into_iter()
                    .map(|(index, content)| ApiPreviewBlock {
                        index,
                        content: content.into(),
                    })
                    .collect(),
            },
            Preview::Compaction { input_cursor } => Self::Compacting {
                input_cursor: input_cursor.into(),
            },
        }
    }
}

impl From<Option<Preview>> for ApiPreview {
    fn from(preview: Option<Preview>) -> Self {
        preview.map_or(Self::Idle, Into::into)
    }
}

impl From<PreviewContent> for ApiPreviewContent {
    fn from(content: PreviewContent) -> Self {
        match content {
            PreviewContent::Text(text) => Self::Text { text },
            PreviewContent::Reasoning(text) => Self::Reasoning { text },
            PreviewContent::ToolCall { id, tool, input } => Self::ToolCall {
                id: id.into(),
                tool: tool.into(),
                input,
            },
            PreviewContent::ProviderToolCall {
                call_id,
                name,
                content,
            } => Self::ProviderToolCall {
                call_id: call_id.into(),
                name: name.into(),
                content,
            },
        }
    }
}

impl From<MessageRecord> for ApiMessageRecord {
    fn from(record: MessageRecord) -> Self {
        match record {
            MessageRecord::Pending { position, message } => Self::Pending {
                position,
                message: message.into(),
            },
            MessageRecord::Consumed(id) => Self::Consumed { id: id.into() },
            MessageRecord::Deleted(id) => Self::Deleted { id: id.into() },
        }
    }
}

impl From<Message> for ApiMessage {
    fn from(message: Message) -> Self {
        Self {
            id: message.id.into(),
            mode: match message.mode {
                MessageMode::Steer => ApiMessageMode::Steer,
                MessageMode::Queue => ApiMessageMode::Queue,
            },
            content: message.content.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<EventRecord> for ApiEvent {
    fn from(record: EventRecord) -> Self {
        match record {
            EventRecord::Settled(event) => event.into(),
            EventRecord::Deleted(cursor) => Self::Deleted {
                cursor: cursor.into(),
            },
        }
    }
}

// === Serialization ===

fn serialize_id<S: Serializer>(id: &u128, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(id)
}

fn deserialize_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}

fn deserialize_optional_id<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u128>, D::Error> {
    deserialize_id(deserializer).map(Some)
}

fn deserialize_limit<'de, D: Deserializer<'de>>(deserializer: D) -> Result<NonZeroUsize, D::Error> {
    String::deserialize(deserializer)?
        .parse::<NonZeroUsize>()
        .map_err(serde::de::Error::custom)
}

const fn default_event_limit() -> NonZeroUsize {
    NonZeroUsize::new(100).expect("limit is positive")
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::assert_matches;
    use serde_json::json;

    #[test]
    fn deserialize_create_agent_converts_to_core_types() {
        let request: CreateAgentRequest = serde_json::from_value(json!({
            "id": "340282366920938463463374607431768211455",
            "role": "assistant",
            "parameters": {
                "language": "english",
            },
            "message": {
                "id": "2",
                "mode": "steer",
                "content": [
                    {
                        "type": "text",
                        "text": "Hello",
                    },
                ],
            },
        }))
        .unwrap();

        let (agent, message) = request.into_parts();
        let message = message.unwrap();

        assert_eq!(agent.id, u128::MAX.into());
        assert_eq!(agent.role.name.as_ref(), "assistant");
        assert_eq!(agent.role.config["language"], "english");
        assert!(agent.extensions.is_empty());
        assert_eq!(message.id, 2.into());
        assert_eq!(message.mode, MessageMode::Steer);
        assert_eq!(message.content, vec![ContentBlock::Text("Hello".into())]);
    }

    #[test]
    fn deserialize_requests_preserves_optional_fields() {
        let request: CreateAgentRequest = serde_json::from_value(json!({
            "id": "1",
            "role": "assistant",
        }))
        .unwrap();
        let interrupt: InterruptAgentRequest = serde_json::from_value(json!({})).unwrap();

        assert!(request.parameters.is_empty());
        assert!(request.message.is_none());
        assert!(interrupt.message.is_none());
    }

    #[test]
    fn parse_events_query_applies_defaults() {
        let query = "".parse::<EventsQuery>().unwrap();

        assert_eq!(query.from, None);
        assert_eq!(query.until, None);
        assert_matches!(query.direction, ApiEventDirection::Forward);
        assert_eq!(query.limit.get(), 100);
    }

    #[test]
    fn parse_events_query_decodes_fields() {
        let query =
            "from=340282366920938463463374607431768211455&until=%32&direction=backward&limit=10"
                .parse::<EventsQuery>()
                .unwrap();

        assert_eq!(query.from, Some(u128::MAX));
        assert_eq!(query.until, Some(2));
        assert_matches!(query.direction, ApiEventDirection::Backward);
        assert_eq!(query.limit.get(), 10);
    }

    #[test]
    fn parse_events_query_rejects_invalid_fields() {
        for query in [
            "from=1&from=2",
            "direction=forward&direction=backward",
            "unknown=1",
            "from=invalid",
            "until=",
            "direction=invalid",
            "limit=0",
        ] {
            assert!(query.parse::<EventsQuery>().is_err(), "{query}");
        }
    }

    #[test]
    fn serialize_event_preserves_model_response_fields() {
        let event = Event {
            cursor: u128::MAX.into(),
            payload: EventPayload::ModelResponse(ModelResponse {
                output: vec![
                    ModelOutput::Text("Hello".into()),
                    ModelOutput::Reasoning(ModelReasoning {
                        content: None,
                        redacted_content: Some("opaque reasoning".into()),
                        attestation: Some("signature".into()),
                    }),
                    ModelOutput::ToolCall(ToolCall {
                        id: "call-1".into(),
                        tool: "shell".into(),
                        is_async: true,
                        input: serde_json::from_value(json!({
                            "command": "pwd",
                        }))
                        .unwrap(),
                    }),
                    ModelOutput::ProviderToolCall(ModelProviderToolCall {
                        call_id: "search-1".into(),
                        name: "search".into(),
                        content: Some("Found results".into()),
                        payload: "opaque call".into(),
                    }),
                ],
                usage: ModelUsage {
                    input: 100,
                    output: 20,
                    cache_read: 80,
                    cache_write: 0,
                },
                provenance: ModelProvenance {
                    provider_id: "provider".into(),
                    model_id: "model".into(),
                },
                continuation_token: Some("continuation".into()),
            }),
        };

        let value = serde_json::to_value(ApiEvent::from(event)).unwrap();

        assert_eq!(
            value,
            json!({
                "cursor": "340282366920938463463374607431768211455",
                "status": "settled",
                "payload": {
                    "type": "model_response",
                    "output": [
                        {
                            "type": "text",
                            "text": "Hello",
                        },
                        {
                            "type": "reasoning",
                            "content": null,
                            "redacted_content": "opaque reasoning",
                            "attestation": "signature",
                        },
                        {
                            "type": "tool_call",
                            "id": "call-1",
                            "tool": "shell",
                            "is_async": true,
                            "input": {
                                "command": "pwd",
                            },
                        },
                        {
                            "type": "provider_tool_call",
                            "call_id": "search-1",
                            "name": "search",
                            "content": "Found results",
                            "payload": "opaque call",
                        },
                    ],
                    "usage": {
                        "input": 100,
                        "output": 20,
                        "cache_read": 80,
                        "cache_write": 0,
                    },
                    "provenance": {
                        "provider_id": "provider",
                        "model_id": "model",
                    },
                    "continuation_token": "continuation",
                },
            })
        );
    }

    #[test]
    fn serialize_event_preserves_compaction_inputs_and_pending_tools() {
        let provenance = ModelProvenance {
            provider_id: "provider".into(),
            model_id: "model".into(),
        };
        let result = ToolResult {
            call_id: "call-1".into(),
            output: ToolOutput {
                content: vec![ContentBlock::Text("result".into())],
                structured_content: Some(
                    serde_json::from_value(json!({
                        "success": false,
                    }))
                    .unwrap(),
                ),
                is_error: true,
            },
        };
        let event = Event {
            cursor: 20.into(),
            payload: EventPayload::Compaction(Compaction {
                last_event: 19.into(),
                input: vec![
                    ModelInput::User(vec![ContentBlock::Text("request".into())]),
                    ModelInput::Assistant(
                        provenance.clone(),
                        vec![ModelOutput::Text("response".into())],
                    ),
                    ModelInput::ToolResult(result.clone()),
                    ModelInput::Compaction(
                        provenance,
                        ModelCompaction {
                            content: Some("summary".into()),
                            redacted_content: Some("opaque context".into()),
                            attestation: Some("signature".into()),
                        },
                    ),
                ],
                pending_tool_calls: vec![ToolCall {
                    id: "call-2".into(),
                    tool: "shell".into(),
                    is_async: true,
                    input: JsonObject::new(),
                }],
                usage: ModelUsage {
                    input: 100,
                    output: 20,
                    cache_read: 0,
                    cache_write: 0,
                },
            }),
        };

        let value = serde_json::to_value(ApiEvent::from(event)).unwrap();
        let payload = &value["payload"];

        assert_eq!(payload["type"], "compaction");
        assert_eq!(payload["last_event"], "19");
        assert_eq!(payload["input"][0]["content"][0]["text"], "request");
        assert_eq!(payload["input"][1]["output"][0]["text"], "response");
        assert_eq!(payload["input"][1]["provenance"]["model_id"], "model");
        assert_eq!(
            payload["input"][2]["output"]["structured_content"]["success"],
            false
        );
        assert_eq!(payload["input"][2]["output"]["is_error"], true);
        assert_eq!(payload["input"][3]["content"], "summary");
        assert_eq!(payload["input"][3]["redacted_content"], "opaque context");
        assert_eq!(payload["input"][3]["attestation"], "signature");
        assert_eq!(payload["pending_tool_calls"][0]["id"], "call-2");
        assert_eq!(payload["pending_tool_calls"][0]["is_async"], true);
        assert_eq!(payload["usage"]["input"], 100);

        let event = Event {
            cursor: 21.into(),
            payload: EventPayload::ToolResult(result),
        };
        let value = serde_json::to_value(ApiEvent::from(event)).unwrap();
        assert_eq!(value["payload"], payload["input"][2]);

        for (turn_end, name) in [
            (TurnEnd::Completed, "completed"),
            (TurnEnd::Interrupted, "interrupted"),
        ] {
            let event = Event {
                cursor: 22.into(),
                payload: EventPayload::TurnEnd(turn_end),
            };
            let value = serde_json::to_value(ApiEvent::from(event)).unwrap();

            assert_eq!(
                value["payload"],
                json!({
                    "type": "turn_end",
                    "reason": name,
                })
            );
        }
    }
}
