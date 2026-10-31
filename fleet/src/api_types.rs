use core::{num::NonZeroUsize, str::FromStr};
use fleet_core::{
    Agent, AgentParent, Compaction, Configured, ContentBlock, Event, EventDirection, EventPayload,
    JsonObject, Message, MessageMode, ModelCompaction, ModelInput, ModelOutput, ModelProvenance,
    ModelReasoning, ModelResponse, ModelUsage, ToolCall, ToolOutput, ToolResult, TurnEndReason,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::value::MapDeserializer};

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
    /// The initial message.
    pub message: Option<ApiMessage>,
}

/// A request to interrupt an agent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptAgentRequest {
    /// A message to send with the interrupt.
    pub message: Option<ApiMessage>,
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
    pub limit: usize,
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
}

// === Messages ===

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

/// Why a turn ended.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiTurnEndReason {
    /// The agent finished responding.
    Completed,
    /// The turn was interrupted.
    Interrupted,
}

// === Events ===

/// An event in an agent's history.
#[derive(Debug, Serialize)]
pub struct ApiEvent {
    /// The event cursor.
    #[serde(serialize_with = "serialize_id")]
    pub cursor: u128,
    /// The event payload.
    pub payload: ApiEventPayload,
}

/// The payload of an agent event.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiEventPayload {
    /// Input from the agent's parent.
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
        reason: ApiTurnEndReason,
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

impl CreateAgentRequest {
    /// Converts the request to an agent and initial message.
    pub fn into_parts(self) -> (Agent, Option<Message>) {
        let Self {
            id,
            role,
            parameters,
            message,
        } = self;
        let agent = Agent {
            id: id.into(),
            parent: AgentParent::User,
            role: Configured {
                name: role.into(),
                config: parameters,
            },
        };

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
        }
    }
}

impl From<ApiContentBlock> for ContentBlock {
    fn from(content: ApiContentBlock) -> Self {
        match content {
            ApiContentBlock::Text { text } => Self::Text(text),
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

impl From<TurnEndReason> for ApiTurnEndReason {
    fn from(reason: TurnEndReason) -> Self {
        match reason {
            TurnEndReason::Completed => Self::Completed,
            TurnEndReason::Interrupted => Self::Interrupted,
        }
    }
}

impl From<Event> for ApiEvent {
    fn from(event: Event) -> Self {
        let Event { cursor, payload } = event;
        Self {
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
            EventPayload::TurnEnd(reason) => Self::TurnEnd {
                reason: reason.into(),
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

fn deserialize_limit<'de, D: Deserializer<'de>>(deserializer: D) -> Result<usize, D::Error> {
    String::deserialize(deserializer)?
        .parse::<NonZeroUsize>()
        .map(NonZeroUsize::get)
        .map_err(serde::de::Error::custom)
}

const fn default_event_limit() -> usize {
    100
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
        assert_eq!(agent.parent, AgentParent::User);
        assert_eq!(agent.role.name.as_ref(), "assistant");
        assert_eq!(agent.role.config["language"], "english");
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
        assert_eq!(query.limit, 100);
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
        assert_eq!(query.limit, 10);
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

        for (reason, name) in [
            (TurnEndReason::Completed, "completed"),
            (TurnEndReason::Interrupted, "interrupted"),
        ] {
            let event = Event {
                cursor: 22.into(),
                payload: EventPayload::TurnEnd(reason),
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
